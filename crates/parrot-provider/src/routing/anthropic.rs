//! Runtime composition over the actual Anthropic adapter and owning stream.
use crate::{
    anthropic::{
        self, AnthropicProvider, AnthropicStream, EncodeError, ParserError, ProviderError,
    },
    catalogs::{Model, StandardModelError},
    chat_completions::ProviderFailure,
    lazy_stream::LazyEventStream,
    sessions::{Content, Error, Event, EventStream, Provider, Request, Session},
};
use parrot_domain::contracts::async_ops::{BoxFuture, Cancellation};
use std::sync::Arc;

pub struct AnthropicApiKeySource(Arc<dyn crate::sessions::provider::ApiKeySource>);
impl AnthropicApiKeySource {
    pub fn new(source: Arc<dyn crate::sessions::provider::ApiKeySource>) -> Self {
        Self(source)
    }
}
impl anthropic::ApiKeySource for AnthropicApiKeySource {
    fn api_key<'a>(
        &'a self,
        cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<String, ProviderError>> {
        Box::pin(async move { self.0.api_key(cancellation).await.map_err(credential_error) })
    }
    fn has_credential<'a>(
        &'a self,
        cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<bool, ProviderError>> {
        Box::pin(async move {
            self.0
                .has_credential(cancellation)
                .await
                .map_err(credential_error)
        })
    }
}
fn credential_error(error: Error) -> ProviderError {
    match error {
        Error::Authentication(error) => ProviderError::Authentication(error),
        Error::Cancelled => ProviderError::Http(crate::http_stream::HttpError::Cancelled),
        Error::Http(error) => ProviderError::Http(error),
        Error::Io(message) => {
            ProviderError::Encode(EncodeError::Image(std::io::Error::other(message)))
        }
        Error::Wire(message) => ProviderError::Parse(ParserError::Wire(message)),
        error => ProviderError::Credential(error.to_string()),
    }
}
pub(super) fn map_provider_error(error: ProviderError) -> Error {
    match error {
        ProviderError::Authentication(error) => Error::Authentication(error),
        ProviderError::Http(error) => Error::Http(error),
        ProviderError::Encode(EncodeError::Image(error)) => Error::Io(error.to_string()),
        ProviderError::Encode(error) => Error::Encode(error.to_string()),
        ProviderError::Parse(ParserError::ProviderResponse {
            error_type,
            error_code,
            detail,
            response_body,
        }) => Error::ChatProvider(ProviderFailure {
            error_type,
            error_code,
            message: detail,
            response_body,
        }),
        ProviderError::Parse(ParserError::Wire(message)) => Error::Wire(message),
        ProviderError::Parse(error) => Error::Encode(error.to_string()),
        ProviderError::StandardCatalog(StandardModelError::Json(error)) => Error::Encode(error.0),
        ProviderError::StandardCatalog(StandardModelError::InvalidOperation(error)) => {
            Error::InvalidOperation(error.0)
        }
        ProviderError::StandardCatalog(StandardModelError::Provider(error)) => {
            Error::Provider(error.0)
        }
        ProviderError::Catalog(error) => Error::Encode(error.to_string()),
        ProviderError::Provider(message) | ProviderError::Credential(message) => {
            Error::Provider(message)
        }
    }
}

pub struct AnthropicBridge {
    inner: Arc<AnthropicProvider>,
}
impl AnthropicBridge {
    pub fn new(inner: Arc<AnthropicProvider>) -> Self {
        Self { inner }
    }
}
impl Provider for AnthropicBridge {
    fn id(&self) -> &str {
        self.inner.id()
    }
    fn call(&self, request: Request) -> Result<Box<dyn EventStream>, Error> {
        Ok(lazy(self.inner.clone(), request))
    }
    fn has_credential<'a>(
        &'a self,
        cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<bool, Error>> {
        Box::pin(async move {
            self.inner
                .has_credential(cancellation)
                .await
                .map_err(map_provider_error)
        })
    }
    fn seed_models(&self) -> Vec<Model> {
        self.inner.seed_models()
    }
    fn list_models<'a>(
        &'a self,
        cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<Vec<Model>, Error>> {
        Box::pin(async move {
            self.inner
                .list_models(cancellation)
                .await
                .map_err(map_provider_error)
        })
    }
    fn calculate_image_tokens(&self, _: &Model, _: &Content) -> i64 {
        crate::image_inspection::FALLBACK_IMAGE_TOKENS
    }
    fn open_session(&self) -> Result<Arc<dyn Session>, Error> {
        Ok(Arc::new(Stateless(self.inner.clone())))
    }
}
struct Stateless(Arc<AnthropicProvider>);
impl Session for Stateless {
    fn begin_turn(&self) -> BoxFuture<'_, Result<(), Error>> {
        Box::pin(async { Ok(()) })
    }
    fn call(&self, request: Request) -> Result<Box<dyn EventStream>, Error> {
        Ok(lazy(self.0.clone(), request))
    }
    fn try_fall_back_to_http(&self) -> BoxFuture<'_, Result<bool, Error>> {
        Box::pin(async { Ok(false) })
    }
    fn close(&self) -> BoxFuture<'_, Result<(), Error>> {
        Box::pin(async { Ok(()) })
    }
}
fn lazy(provider: Arc<AnthropicProvider>, request: Request) -> Box<dyn EventStream> {
    Box::new(LazyEventStream::new(request, move |request, stop| {
        let provider = provider.clone();
        Box::pin(async move {
            provider
                .call(&request, stop.as_ref())
                .await
                .map(AnthropicEventStream)
                .map_err(map_provider_error)
        })
    }))
}
struct AnthropicEventStream(AnthropicStream);
impl EventStream for AnthropicEventStream {
    fn next<'a>(
        &'a mut self,
        cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<Option<Event>, Error>> {
        Box::pin(async move {
            self.0
                .next(cancellation)
                .await
                .map(|event| event.map(Event::from))
                .map_err(map_provider_error)
        })
    }
    fn close(&mut self) -> BoxFuture<'_, Result<(), Error>> {
        Box::pin(async { Ok(()) })
    }
}
