//! Configured provider/model catalog without constructing local agent capabilities.
//!
//! The caller keeps this owner on the same runtime as its provider operations and
//! closes it after those operations have joined. Workspace paths are not grants.

use std::{io::Write, sync::Arc};

use parrot_domain::contracts::async_ops::{BoxFuture, Cancellation, OperationError, OwnedWork};
use parrot_foundation::{
    config::Configuration,
    credentials::{Credential, CredentialStore, CredentialType},
    oauth::{ApiKeyEnvironment, CredentialStorage, OAuthClient},
};
use parrot_provider::{
    http_stream::{HttpError, ProviderHttpClient, ProviderHttpClientCatalog, ReqwestTransport},
    image_generation::bucket::{CredentialSource, StoredCredential, SystemClock},
    responses_websocket::NativeConnector,
    routing::{
        auth::FoundationCredentials,
        builder::{
            BuildServices, CatalogHttpClients, ProviderRegistryBuilder, S3ImageBucketFactory,
        },
        models_dev::ModelsDevInformationProvider,
        registry::ProviderRegistry,
    },
    sessions::Error as ProviderError,
};
use parrot_runtime::agent_turn::lifecycle::Signal;

use super::{DispatchError, EXIT_SUCCESS};

/// Retains the HTTP catalog and OAuth refresh owner used by this exact registry.
/// A bare registry does not own the catalog's authority-scoped HTTP clients.
/// There is deliberately no security snapshot, filesystem, launcher or agent factory here.
pub struct ProviderCatalog {
    registry: Option<Arc<ProviderRegistry>>,
    credentials: Arc<FoundationCredentials>,
    clients: Option<Arc<CatalogHttpClients>>,
    standard: ProviderHttpClient,
}

impl ProviderCatalog {
    pub fn registry(&self) -> &ProviderRegistry {
        self.registry.as_ref().expect("provider catalog is closed")
    }

    pub fn shared_registry(&self) -> Arc<ProviderRegistry> {
        self.registry
            .as_ref()
            .expect("provider catalog is closed")
            .clone()
    }

    /// Joins accepted OAuth refresh work without caller cancellation. The caller
    /// must already have joined all operations borrowing the registry.
    pub async fn settle(&self) -> Result<(), DispatchError> {
        self.credentials
            .settle()
            .await
            .map_err(|failure| DispatchError::Command(failure.to_string()))
    }

    /// Attempts the refresh join before disposing the registry and its HTTP clients,
    /// including when the refresh join reports a failure. Keep the actual runtime
    /// alive until this returns; dropping an Arc or retaining a Handle is not a join.
    pub async fn close(&mut self) -> Result<(), DispatchError> {
        let result = self.settle().await;
        self.registry.take();
        self.clients.take();
        self.standard.dispose();
        result
    }
}

/// Uses the already-selected concrete credential store. No legacy credential
/// discovery, local agent capability construction, or test backend is involved.
/// Provider endpoints and invalid-certificate opt-ins remain configuration-owned;
/// the existing HTTP catalog preserves each relaxed client's authority boundary.
pub async fn build(
    configuration: Configuration,
    credentials: Arc<CredentialStore>,
    oauth: Arc<dyn OAuthClient>,
    cancellation: &dyn Cancellation,
) -> Result<ProviderCatalog, DispatchError> {
    cancellation.check().map_err(|_| DispatchError::Cancelled)?;
    let standard = ProviderHttpClient::standard(
        reqwest::Client::builder()
            .build()
            .map_err(|failure| DispatchError::Command(failure.to_string()))?,
    );
    let clients = Arc::new(CatalogHttpClients::new(ProviderHttpClientCatalog::new(
        standard.clone(),
    )));
    let environment: Arc<ApiKeyEnvironment> = Arc::new(|name| std::env::var(name).ok());
    let sources = Arc::new(FoundationCredentials::new(
        credentials.clone(),
        environment.clone(),
        oauth,
    ));
    let builder = ProviderRegistryBuilder::new(
        configuration,
        BuildServices {
            clients: clients.clone(),
            credentials: sources.clone(),
            connector: Arc::new(NativeConnector::new(standard.clone())),
            models_dev: ModelsDevInformationProvider::new(Arc::new(ReqwestTransport::new(
                standard.clone(),
            ))),
            image_buckets: Arc::new(S3ImageBucketFactory::new(
                Arc::new(SystemClock),
                Arc::new(ImageCredentials {
                    store: credentials,
                    environment,
                }),
            )),
        },
    );
    let built = builder.build(cancellation).await;
    drop(builder);
    match built {
        Ok(registry) => Ok(ProviderCatalog {
            registry: Some(Arc::new(registry)),
            credentials: sources,
            clients: Some(clients),
            standard,
        }),
        Err(failure) => {
            let settled = sources.settle().await;
            drop(clients);
            standard.dispose();
            match settled {
                Ok(()) => Err(provider_failure(failure)),
                Err(cleanup) => Err(DispatchError::Command(format!(
                    "{failure}; provider credential cleanup failed: {cleanup}"
                ))),
            }
        }
    }
}

/// Matches the existing ModelOperations refresh and source top-level ListModels:
/// provider/model IDs only, in registry response order, without contexts/variants.
pub async fn list(
    registry: &ProviderRegistry,
    output: &mut (dyn Write + Send),
    cancellation: &dyn Cancellation,
) -> Result<i32, DispatchError> {
    cancellation.check().map_err(|_| DispatchError::Cancelled)?;
    let models = registry
        .available_models(cancellation)
        .await
        .map_err(provider_failure)?;
    for selected in &models {
        cancellation.check().map_err(|_| DispatchError::Cancelled)?;
        writeln!(
            output,
            "{}/{}",
            selected.model.provider_id, selected.model.id
        )?;
    }
    if models.is_empty() {
        cancellation.check().map_err(|_| DispatchError::Cancelled)?;
        writeln!(output, "no providers are configured")?;
    }
    Ok(EXIT_SUCCESS)
}

fn provider_failure(failure: ProviderError) -> DispatchError {
    match failure {
        ProviderError::Cancelled
        | ProviderError::Cancellation(_)
        | ProviderError::Http(HttpError::Cancelled) => DispatchError::Cancelled,
        other => DispatchError::Command(other.to_string()),
    }
}

/// Thin adapter from the selected Foundation store to the existing image bucket
/// credential contract. The bucket intentionally loads credentials independently
/// of a caller's canceled upload observer, as its existing contract requires.
struct ImageCredentials {
    store: Arc<CredentialStore>,
    environment: Arc<ApiKeyEnvironment>,
}

impl CredentialSource for ImageCredentials {
    fn stored(&self) -> BoxFuture<'_, Result<Option<StoredCredential>, ProviderError>> {
        Box::pin(async move {
            let uncancelled = Signal::new();
            CredentialStorage::get(self.store.as_ref(), "image_upload", &uncancelled)
                .await
                .map(|credential| credential.map(image_credential))
                .map_err(|failure| match failure {
                    OperationError::Cancelled => ProviderError::Cancelled,
                    OperationError::Failed(failure) => ProviderError::Authentication(failure),
                })
        })
    }

    fn environment(&self, name: &str) -> Option<String> {
        (self.environment)(name)
    }
}

fn image_credential(credential: Credential) -> StoredCredential {
    match (credential.credential_type, credential.s3) {
        (CredentialType::S3, Some(s3)) => StoredCredential::S3 {
            access_key: s3.access_key.value().to_owned(),
            secret_key: s3.secret_key.value().to_owned(),
        },
        _ => StoredCredential::Other,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use parrot_provider::{
        catalogs::Model,
        sessions::{EventStream, Provider, Request, Session},
    };
    use std::{collections::BTreeMap, io};

    struct ListedProvider {
        id: String,
        models: Vec<Model>,
        available: bool,
    }

    impl Provider for ListedProvider {
        fn id(&self) -> &str {
            &self.id
        }
        fn call(&self, _: Request) -> Result<Box<dyn EventStream>, ProviderError> {
            Err(ProviderError::InvalidOperation(
                "not used by model listing".into(),
            ))
        }
        fn has_credential<'a>(
            &'a self,
            _: &'a dyn Cancellation,
        ) -> BoxFuture<'a, Result<bool, ProviderError>> {
            Box::pin(async { Ok(self.available) })
        }
        fn seed_models(&self) -> Vec<Model> {
            self.models.clone()
        }
        fn list_models<'a>(
            &'a self,
            _: &'a dyn Cancellation,
        ) -> BoxFuture<'a, Result<Vec<Model>, ProviderError>> {
            Box::pin(async { Ok(self.models.clone()) })
        }
        fn open_session(&self) -> Result<Arc<dyn Session>, ProviderError> {
            Err(ProviderError::InvalidOperation(
                "not used by model listing".into(),
            ))
        }
    }

    #[tokio::test]
    async fn lists_refreshed_provider_and_model_ids_only_in_registry_order() {
        let mut first = Model::new("with/slash", "response-provider");
        first.name = "not printed".into();
        first.context_window = 123456;
        let providers: Vec<Arc<dyn Provider>> = vec![
            Arc::new(ListedProvider {
                id: "z-provider".into(),
                models: vec![Model::new("last", "z-provider")],
                available: true,
            }),
            Arc::new(ListedProvider {
                id: "a-provider".into(),
                models: vec![first, Model::new("second", "a-provider")],
                available: true,
            }),
        ];
        let registry = ProviderRegistry::new(providers, BTreeMap::new()).unwrap();
        let mut output = Vec::new();
        assert_eq!(
            list(&registry, &mut output, &Signal::new()).await.unwrap(),
            EXIT_SUCCESS
        );
        assert_eq!(
            output,
            b"response-provider/with/slash\na-provider/second\nz-provider/last\n"
        );
    }

    #[tokio::test]
    async fn unavailable_credentials_print_exact_empty_message() {
        let provider = Arc::new(ListedProvider {
            id: "configured-but-unavailable".into(),
            models: vec![Model::new("not-listed", "configured-but-unavailable")],
            available: false,
        });
        let registry = ProviderRegistry::new(vec![provider], BTreeMap::new()).unwrap();
        let mut output = Vec::new();
        assert_eq!(
            list(&registry, &mut output, &Signal::new()).await.unwrap(),
            EXIT_SUCCESS
        );
        assert_eq!(output, b"no providers are configured\n");
    }

    #[tokio::test]
    async fn cancellation_precedes_refresh_or_output() {
        let registry = ProviderRegistry::new(Vec::new(), BTreeMap::new()).unwrap();
        let cancellation = Signal::new();
        cancellation.cancel();
        let mut output = Vec::new();
        assert!(matches!(
            list(&registry, &mut output, &cancellation).await,
            Err(DispatchError::Cancelled)
        ));
        assert!(output.is_empty());
    }

    #[tokio::test]
    async fn output_failure_is_not_reported_as_success() {
        struct Broken;
        impl Write for Broken {
            fn write(&mut self, _: &[u8]) -> io::Result<usize> {
                Err(io::Error::new(
                    io::ErrorKind::BrokenPipe,
                    "synthetic output failure",
                ))
            }
            fn flush(&mut self) -> io::Result<()> {
                Ok(())
            }
        }
        let registry = ProviderRegistry::new(Vec::new(), BTreeMap::new()).unwrap();
        assert!(matches!(
            list(&registry, &mut Broken, &Signal::new()).await,
            Err(DispatchError::Io(_))
        ));
    }

    #[test]
    fn image_adapter_preserves_s3_kind_and_does_not_treat_api_keys_as_s3() {
        match image_credential(Credential::for_s3("synthetic-access", "synthetic-secret")) {
            StoredCredential::S3 {
                access_key,
                secret_key,
            } => {
                assert_eq!(access_key, "synthetic-access");
                assert_eq!(secret_key, "synthetic-secret");
            }
            StoredCredential::Other => panic!("expected source s3 credential"),
        }
        assert!(matches!(
            image_credential(Credential::for_api_key("synthetic-key")),
            StoredCredential::Other
        ));
    }
}
