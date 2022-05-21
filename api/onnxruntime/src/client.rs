use std::{collections::HashMap, sync::Arc};

use ipis::{
    async_trait::async_trait,
    core::{
        anyhow::{bail, Result},
        ndarray,
        value::array::Array,
    },
    env::Infer,
    futures::TryFutureExt,
    path::Path,
    tokio::{io::AsyncReadExt, sync::Mutex},
};
use ipnis_common::{
    model::Model,
    onnxruntime::{environment::Environment, session::Session, tensor::OrtOwnedTensor},
    tensor::{dynamic::DynamicTensorData, Tensor},
    Ipnis,
};
use ipsis_common::Ipsis;

use crate::config::ClientConfig;

pub type IpnisClient = IpnisClientInner<::ipiis_api::client::IpiisClient>;

pub struct IpnisClientInner<IpiisClient> {
    pub ipiis: IpiisClient,
    config: ClientConfig,
    environment: Environment,
    /// ## Thread-safe
    /// It's safe to invoke Run() on the same session object in multiple threads.
    /// No need for any external synchronization.
    ///
    /// * Source: https://github.com/microsoft/onnxruntime/issues/114#issuecomment-444725508
    sessions: Mutex<HashMap<Path, Arc<Session>>>,
}

impl<IpiisClient> AsRef<::ipiis_api::client::IpiisClient> for IpnisClientInner<IpiisClient>
where
    IpiisClient: AsRef<::ipiis_api::client::IpiisClient>,
{
    fn as_ref(&self) -> &::ipiis_api::client::IpiisClient {
        self.ipiis.as_ref()
    }
}

impl<IpiisClient> AsRef<::ipiis_api::server::IpiisServer> for IpnisClientInner<IpiisClient>
where
    IpiisClient: AsRef<::ipiis_api::server::IpiisServer>,
{
    fn as_ref(&self) -> &::ipiis_api::server::IpiisServer {
        self.ipiis.as_ref()
    }
}

#[async_trait]
impl<'a, IpiisClient> Infer<'a> for IpnisClientInner<IpiisClient>
where
    Self: Send,
    IpiisClient: Infer<'a, GenesisResult = IpiisClient> + Send,
    <IpiisClient as Infer<'a>>::GenesisArgs: Sized,
{
    type GenesisArgs = <IpiisClient as Infer<'a>>::GenesisArgs;
    type GenesisResult = Self;

    async fn try_infer() -> Result<Self> {
        IpiisClient::try_infer()
            .and_then(Self::with_ipiis_client)
            .await
    }

    async fn genesis(
        args: <Self as Infer<'a>>::GenesisArgs,
    ) -> Result<<Self as Infer<'a>>::GenesisResult> {
        IpiisClient::genesis(args)
            .and_then(Self::with_ipiis_client)
            .await
    }
}

impl<IpiisClient> IpnisClientInner<IpiisClient> {
    pub async fn with_ipiis_client(ipiis: IpiisClient) -> Result<Self> {
        let config = ClientConfig::try_infer().await?;
        let log_level = config.log_level;

        Ok(Self {
            ipiis,
            config,
            environment: Environment::builder()
                .with_name("ipnis")
                // The ONNX Runtime's log level can be different than the one of the wrapper crate or the application.
                .with_log_level(log_level)
                .build()?,
            sessions: Default::default(),
        })
    }

    async fn load_session(&self, path: &Path) -> Result<Arc<Session>>
    where
        IpiisClient: Ipsis + Send + Sync,
    {
        let mut sessions = self.sessions.lock().await;

        // TODO: hibernate the least used sessions (caching)

        match sessions.get(path) {
            Some(session) => Ok(session.clone()),
            None => {
                let model_bytes = {
                    let mut recv = self.ipiis.get_raw(path).await?;
                    let mut buf = Vec::with_capacity(path.len.try_into()?);

                    let len = recv.read_u64().await?;
                    if len != path.len {
                        bail!("failed to validate the length");
                    }

                    recv.read_to_end(&mut buf).await?;
                    assert_eq!(buf.len(), path.len as usize);
                    buf
                };

                let session = self
                    .environment
                    .new_session_builder()?
                    .with_optimization_level(self.config.optimization_level)?
                    .with_number_threads(self.config.number_threads.into())?
                    .with_model_from_memory(&model_bytes)?;
                let session = Arc::new(session);
                sessions.insert(*path, session.clone());

                Ok(session)
            }
        }
    }
}

#[async_trait]
impl<IpiisClient> Ipnis for IpnisClientInner<IpiisClient>
where
    IpiisClient: Ipsis + Send + Sync,
{
    /// ## Thread-safe
    /// This method is thread-safe: https://github.com/microsoft/onnxruntime/issues/114#issuecomment-444725508
    async fn call_raw(&self, model: &Model, inputs: Vec<Tensor>) -> Result<Vec<Tensor>> {
        // load a model
        let session = self.load_session(&model.path).await?;

        // perform the inference
        let outputs: Vec<OrtOwnedTensor<f32, ndarray::IxDyn>> = session.run(&inputs)?;

        // collect outputs
        let outputs = model
            .outputs
            .iter()
            .zip(outputs)
            .map(|(shape, output)| Tensor {
                name: shape.name.to_string(),
                data: DynamicTensorData::F32(Array(output.to_owned().into_shared())).into(),
            })
            .collect();

        Ok(outputs)
    }

    async fn load_model(&self, path: &Path) -> Result<Model> {
        let session = self.load_session(path).await?;

        Ok(Model {
            path: *path,
            inputs: session
                .inputs
                .iter()
                .map(TryInto::try_into)
                .collect::<Result<_>>()?,
            outputs: session
                .outputs
                .iter()
                .map(TryInto::try_into)
                .collect::<Result<_>>()?,
        })
    }
}
