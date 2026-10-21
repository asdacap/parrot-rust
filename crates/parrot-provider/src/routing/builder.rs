//! Configured providers only: external metadata cannot introduce endpoints.
use super::{
    auth::CredentialSources,
    factory::{ProviderBuildContext, ProviderImplementation, build_provider},
    models_dev::ModelsDevInformationProvider,
    registry::ProviderRegistry,
};
use crate::{
    catalogs::filter_chatgpt_external,
    http_stream::{HttpTransport, ProviderHttpClientCatalog, ReqwestTransport},
    image_generation::{
        ImageTransport, ReqwestImageTransport,
        bucket::{BucketConfig, Clock, CredentialSource, ImageBucket, S3ImageBucket},
        linking::ImageLinkingProvider,
    },
    responses_websocket::{Connector, NativeConnector},
    sessions::{Error, Provider, decorators::RetryingProvider},
};
use parrot_domain::contracts::async_ops::Cancellation;
use parrot_foundation::config::{configuration::Configuration, runtime::ImageUploadConfig};
use std::{
    collections::BTreeMap,
    sync::{Arc, Mutex},
};

/// All transports share the provider's authority-scoped client selection.
pub struct SelectedHttpClient {
    pub streaming: Arc<dyn HttpTransport>,
    pub images: Arc<dyn ImageTransport>,
    pub connector: Arc<dyn Connector>,
}
pub trait HttpClients: Send + Sync {
    fn resolve(
        &self,
        base_url: &str,
        allow_invalid_tls_certificate: bool,
    ) -> Result<SelectedHttpClient, Error>;
}
pub struct CatalogHttpClients {
    catalog: Mutex<ProviderHttpClientCatalog>,
}
impl CatalogHttpClients {
    pub fn new(catalog: ProviderHttpClientCatalog) -> Self {
        Self {
            catalog: Mutex::new(catalog),
        }
    }
}
impl HttpClients for CatalogHttpClients {
    fn resolve(
        &self,
        base_url: &str,
        allow_invalid_tls_certificate: bool,
    ) -> Result<SelectedHttpClient, Error> {
        let client = self
            .catalog
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .resolve(base_url, allow_invalid_tls_certificate)?;
        Ok(SelectedHttpClient {
            streaming: Arc::new(ReqwestTransport::new(client.clone())),
            images: Arc::new(ReqwestImageTransport::new(client.clone())),
            connector: Arc::new(NativeConnector::new(client)),
        })
    }
}

pub trait ImageBucketFactory: Send + Sync {
    fn build(
        &self,
        config: &ImageUploadConfig,
        transport: Arc<dyn ImageTransport>,
    ) -> Result<Arc<dyn ImageBucket>, Error>;
}
/// Binds actual T34 upload machinery to injected clocks and credential storage.
pub struct S3ImageBucketFactory {
    clock: Arc<dyn Clock>,
    credentials: Arc<dyn CredentialSource>,
}
impl S3ImageBucketFactory {
    pub fn new(clock: Arc<dyn Clock>, credentials: Arc<dyn CredentialSource>) -> Self {
        Self { clock, credentials }
    }
}
impl ImageBucketFactory for S3ImageBucketFactory {
    fn build(
        &self,
        config: &ImageUploadConfig,
        transport: Arc<dyn ImageTransport>,
    ) -> Result<Arc<dyn ImageBucket>, Error> {
        let bucket = S3ImageBucket::new(
            transport,
            BucketConfig {
                endpoint: config.endpoint.clone(),
                bucket: config.bucket.clone(),
                region: config.region.clone(),
                key_prefix: config.key_prefix.clone(),
                public_base_url: config.public_base_url.clone(),
                access_key_env: config.access_key_env.clone(),
                secret_key_env: config.secret_key_env.clone(),
            },
            self.clock.clone(),
            self.credentials.clone(),
        )?;
        Ok(Arc::new(bucket))
    }
}

pub struct BuildServices {
    pub clients: Arc<dyn HttpClients>,
    pub credentials: Arc<dyn CredentialSources>,
    pub models_dev: ModelsDevInformationProvider,
    pub image_buckets: Arc<dyn ImageBucketFactory>,
}
pub struct ProviderRegistryBuilder {
    configuration: Configuration,
    services: BuildServices,
}
impl ProviderRegistryBuilder {
    pub fn new(configuration: Configuration, services: BuildServices) -> Self {
        Self {
            configuration,
            services,
        }
    }
    pub fn buildable_provider_ids(configuration: &Configuration) -> Vec<String> {
        let mut ids: Vec<_> = configuration
            .models
            .providers
            .iter()
            .filter(|(id, config)| ProviderImplementation::resolve(id, config).can_build(config))
            .map(|(id, _)| id.clone())
            .collect();
        ids.sort_by(|left, right| left.encode_utf16().cmp(right.encode_utf16()));
        ids
    }
    pub async fn build(&self, cancellation: &dyn Cancellation) -> Result<ProviderRegistry, Error> {
        let external = self.services.models_dev.fetch(cancellation).await?;
        let image_bucket = match &self.configuration.runtime.image_upload {
            Some(upload) => {
                let client = self.services.clients.resolve(&upload.endpoint, false)?;
                Some(self.services.image_buckets.build(upload, client.images)?)
            }
            None => None,
        };
        let mut providers = Vec::new();
        let mut catalogues = BTreeMap::new();
        for id in Self::buildable_provider_ids(&self.configuration) {
            let config = &self.configuration.models.providers[&id];
            let clients = self
                .services
                .clients
                .resolve(&config.base_url, config.allow_invalid_tls_certificate)?;
            let external_models = if id == "chatgpt" {
                filter_chatgpt_external(
                    external.get("openai").map_or(&[], Vec::as_slice),
                    "chatgpt",
                )
            } else {
                let key = if config.models_dev_id.is_empty() {
                    &id
                } else {
                    &config.models_dev_id
                };
                external.get(key).cloned().unwrap_or_default()
            };
            let built = build_provider(ProviderBuildContext {
                id: &id,
                config,
                credentials: self.services.credentials.as_ref(),
                transport: clients.streaming,
                images: clients.images,
                connector: clients.connector,
                external_models: &external_models,
                maximum_request_bytes: self
                    .configuration
                    .runtime
                    .request_limits
                    .provider_request_bytes as usize,
            })?;
            let provider: Arc<dyn Provider> = match &image_bucket {
                Some(bucket) if config.image_urls => {
                    Arc::new(ImageLinkingProvider::new(built.provider, bucket.clone()))
                }
                _ => built.provider,
            };
            providers.push(Arc::new(RetryingProvider::new(
                provider,
                config.header_timeout_max_retries as u32,
            )) as Arc<dyn Provider>);
            catalogues.insert(id, built.seed);
        }
        let registry = ProviderRegistry::new(providers, catalogues)?;
        registry.available_models(cancellation).await?;
        Ok(registry)
    }
}
