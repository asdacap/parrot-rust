//! OpenRouter Batch provider: one single-request batch job per step.
use crate::{
    catalogs::Model,
    http_stream::{HttpTransport, endpoint_url},
    openrouter_batch::session::{BatchSession, DEFAULT_POLL_INTERVAL},
    sessions::{
        Content, Error, Provider, Session,
        provider::{ApiKeySource, OpenAICompatibleProvider},
    },
};
use parrot_domain::contracts::async_ops::{BoxFuture, Cancellation};
use std::{sync::Arc, time::Duration};

pub struct OpenRouterBatchProvider {
    core: Arc<BatchCore>,
    poll_interval: Duration,
}

pub(crate) struct BatchCore {
    pub(crate) id: String,
    pub(crate) base_url: String,
    pub(crate) allow_insecure_localhost: bool,
    pub(crate) allow_insecure_remote: bool,
    pub(crate) transport: Arc<dyn HttpTransport>,
    pub(crate) api_key: Arc<dyn ApiKeySource>,
    pub(crate) inner: Arc<OpenAICompatibleProvider>,
}

impl OpenRouterBatchProvider {
    pub fn new(
        id: impl Into<String>,
        base_url: impl Into<String>,
        allow_insecure_localhost: bool,
        allow_insecure_remote: bool,
        api_key: Arc<dyn ApiKeySource>,
        transport: Arc<dyn HttpTransport>,
        inner: Arc<OpenAICompatibleProvider>,
    ) -> Result<Self, Error> {
        let core = BatchCore {
            id: id.into(),
            base_url: base_url.into(),
            allow_insecure_localhost,
            allow_insecure_remote,
            transport,
            api_key,
            inner,
        };
        endpoint_url(
            &core.base_url,
            "batches",
            core.allow_insecure_localhost,
            core.allow_insecure_remote,
        )?;
        Ok(Self {
            core: Arc::new(core),
            poll_interval: DEFAULT_POLL_INTERVAL,
        })
    }

    /// Overrides the batch status poll interval. Production uses one minute;
    /// tests use zero to drive the workflow synchronously.
    pub fn with_poll_interval(mut self, interval: Duration) -> Self {
        self.poll_interval = interval;
        self
    }
}
impl Provider for OpenRouterBatchProvider {
    fn id(&self) -> &str {
        &self.core.id
    }
    fn has_credential<'a>(
        &'a self,
        cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<bool, Error>> {
        Box::pin(self.core.inner.has_credential(cancellation))
    }
    fn seed_models(&self) -> Vec<Model> {
        self.core.inner.seed_models()
    }
    fn list_models<'a>(
        &'a self,
        cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<Vec<Model>, Error>> {
        Box::pin(self.core.inner.list_models(cancellation))
    }
    fn calculate_image_tokens(&self, model: &Model, content: &Content) -> i64 {
        self.core.inner.calculate_image_tokens(model, content)
    }
    fn open_session(&self) -> Result<Arc<dyn Session>, Error> {
        Ok(Arc::new(BatchSession::new(
            self.core.clone(),
            self.poll_interval,
        )))
    }
}
