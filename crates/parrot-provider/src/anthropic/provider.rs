use super::{AnthropicEvent, AnthropicParser, EncodeError, ParserError, PreparedRequest, encode};
use crate::view::RequestView;
use crate::{
    catalogs::{CatalogError, Model, StandardModelDecoder, StandardModelError, merge_models},
    http_stream::{
        self, Endpoint, HttpError, HttpEventStream, HttpStreamEvent, HttpTransport,
        ResponseByteCounter, StreamOptions,
    },
    sessions::provider::CompatibleOptions,
};
use parrot_domain::contracts::async_ops::Cancellation;
use std::{collections::VecDeque, fmt, sync::Arc};

/// Credential lookup is injected through the shared session seam; this module
/// never reads environment or stores. Session credential failures map through
/// `credential_error`; the adapter keeps its own diagnostic fidelity for parser
/// and encoding failures.
#[derive(Debug)]
pub enum ProviderError {
    Http(HttpError),
    Encode(EncodeError),
    Parse(ParserError),
    Catalog(CatalogError),
    StandardCatalog(StandardModelError),
    Provider(String),
    Credential(String),
    Authentication(parrot_foundation::oauth::AuthError),
}
impl fmt::Display for ProviderError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Http(error) => error.fmt(f),
            Self::Encode(error) => error.fmt(f),
            Self::Parse(error) => error.fmt(f),
            Self::Catalog(error) => error.fmt(f),
            Self::StandardCatalog(error) => error.fmt(f),
            Self::Authentication(error) => error.fmt(f),
            Self::Provider(message) | Self::Credential(message) => f.write_str(message),
        }
    }
}
impl std::error::Error for ProviderError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Authentication(error) => Some(error),
            Self::StandardCatalog(error) => Some(error),
            _ => None,
        }
    }
}
impl From<HttpError> for ProviderError {
    fn from(error: HttpError) -> Self {
        Self::Http(error)
    }
}
impl From<ParserError> for ProviderError {
    fn from(error: ParserError) -> Self {
        Self::Parse(error)
    }
}
impl From<EncodeError> for ProviderError {
    fn from(error: EncodeError) -> Self {
        Self::Encode(error)
    }
}
impl From<CatalogError> for ProviderError {
    fn from(error: CatalogError) -> Self {
        Self::Catalog(error)
    }
}
/// Maps a session credential failure into the adapter error without losing the
/// typed authentication cause or cancellation.
pub(crate) fn credential_error(error: crate::sessions::Error) -> ProviderError {
    match error {
        crate::sessions::Error::Authentication(error) => ProviderError::Authentication(error),
        crate::sessions::Error::Cancelled => ProviderError::Http(HttpError::Cancelled),
        crate::sessions::Error::Http(error) => ProviderError::Http(error),
        crate::sessions::Error::Io(message) => {
            ProviderError::Encode(EncodeError::Image(std::io::Error::other(message)))
        }
        crate::sessions::Error::Wire(message) => ProviderError::Parse(ParserError::Wire(message)),
        error => ProviderError::Credential(error.to_string()),
    }
}

pub struct AnthropicProvider {
    options: CompatibleOptions,
    transport: Arc<dyn HttpTransport>,
    endpoint: Endpoint,
    models_endpoint: Endpoint,
    headers: Vec<(String, String)>,
}
impl AnthropicProvider {
    pub fn new(
        options: CompatibleOptions,
        transport: Arc<dyn HttpTransport>,
    ) -> Result<Self, ProviderError> {
        if options.id.trim().is_empty() {
            return Err(ProviderError::Provider(
                "provider: anthropic provider ID is required".into(),
            ));
        }
        let endpoint = http_stream::endpoint_url(
            &options.base_url,
            "messages",
            options.allow_insecure_localhost,
            options.allow_insecure_remote,
        )?;
        let models = http_stream::endpoint_url(
            &options.base_url,
            "models",
            options.allow_insecure_localhost,
            options.allow_insecure_remote,
        )?;
        let models_endpoint = Endpoint::parse(&format!("{}?limit=1000", models.as_str()))
            .map_err(|error| ProviderError::Provider(error.to_string()))?;
        let headers = http_stream::validate_headers(&options.headers)?;
        Ok(Self {
            options,
            transport,
            endpoint,
            models_endpoint,
            headers,
        })
    }
    pub fn id(&self) -> &str {
        &self.options.id
    }
    pub fn seed_models(&self) -> Vec<Model> {
        merge_models(
            None,
            &self.options.models,
            &self.options.model_defaults,
            &self.options.external_models,
        )
    }
    pub async fn has_credential(
        &self,
        cancellation: &dyn Cancellation,
    ) -> Result<bool, ProviderError> {
        self.options
            .api_key_source
            .has_credential(cancellation)
            .await
            .map_err(credential_error)
    }
    pub fn prepare(&self, request: &impl RequestView) -> Result<PreparedRequest, ProviderError> {
        Ok(encode(request, self.id())?)
    }
    pub async fn call(
        &self,
        request: &impl RequestView,
        cancellation: &dyn Cancellation,
    ) -> Result<AnthropicStream, ProviderError> {
        self.call_prepared(self.prepare(request)?, cancellation)
            .await
    }
    pub async fn call_prepared(
        &self,
        prepared: PreparedRequest,
        cancellation: &dyn Cancellation,
    ) -> Result<AnthropicStream, ProviderError> {
        let headers = self.headers(cancellation).await?;
        let options = StreamOptions::from_millis(
            self.options.header_timeout_ms,
            self.options.stream_idle_timeout_ms.max(0),
            self.options.maximum_request_bytes,
        )?;
        let http = HttpEventStream::new(
            self.transport.clone(),
            &self.endpoint,
            prepared.body,
            headers,
            options,
            ResponseByteCounter::default(),
        )?;
        Ok(AnthropicStream {
            http: Some(http),
            parser: AnthropicParser::new(self.id()),
            pending: VecDeque::new(),
            finished: false,
            request_started: false,
            invalid_idle_timeout: self.options.stream_idle_timeout_ms < 0,
        })
    }
    pub async fn list_models(
        &self,
        cancellation: &dyn Cancellation,
    ) -> Result<Vec<Model>, ProviderError> {
        let headers = self.headers(cancellation).await?;
        let body = http_stream::get(
            &*self.transport,
            &self.models_endpoint,
            headers,
            http_stream::MODELS_REFRESH_TIMEOUT,
            16 << 20,
            cancellation,
        )
        .await?;
        let models = StandardModelDecoder::decode_typed(self.id(), &body)
            .map_err(ProviderError::StandardCatalog)?;
        Ok(merge_models(
            Some(&models),
            &self.options.models,
            &self.options.model_defaults,
            &self.options.external_models,
        ))
    }
    async fn headers(
        &self,
        cancellation: &dyn Cancellation,
    ) -> Result<Vec<(String, String)>, ProviderError> {
        let key = self
            .options
            .api_key_source
            .api_key(cancellation)
            .await
            .map_err(credential_error)?;
        if key.is_empty() {
            return Err(ProviderError::Provider(format!(
                "provider: \"{}\" has no API key",
                self.id()
            )));
        }
        let mut headers = self.headers.clone();
        set_header(&mut headers, "x-api-key", key);
        if !headers
            .iter()
            .any(|(name, _)| name.eq_ignore_ascii_case("anthropic-version"))
        {
            set_header(&mut headers, "anthropic-version", "2023-06-01".into());
        }
        let beta = headers
            .iter()
            .find(|(name, _)| name.eq_ignore_ascii_case("anthropic-beta"))
            .map(|(_, value)| value.as_str())
            .filter(|value| !value.is_empty());
        let beta = beta.map_or_else(
            || "thinking-binding-controls-2026-08-01".into(),
            |value| format!("{value},thinking-binding-controls-2026-08-01"),
        );
        set_header(&mut headers, "anthropic-beta", beta);
        if !headers
            .iter()
            .any(|(name, _)| name.eq_ignore_ascii_case("User-Agent"))
        {
            set_header(&mut headers, "User-Agent", self.options.user_agent.clone());
        }
        Ok(headers)
    }
}
fn set_header(headers: &mut Vec<(String, String)>, name: &str, value: String) {
    if let Some((_, previous)) = headers
        .iter_mut()
        .find(|(key, _)| key.eq_ignore_ascii_case(name))
    {
        *previous = value;
    } else {
        headers.push((name.into(), value));
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum AnthropicStreamEvent {
    HttpRequestStarted,
    HttpResponseHeadersReceived,
    Message(AnthropicEvent),
}
/// Owns the HTTP body until exhaustion, terminal error, or caller drop. It
/// exposes real lifecycle observations for T30's single trace layer.
pub struct AnthropicStream {
    http: Option<HttpEventStream>,
    parser: AnthropicParser,
    pending: VecDeque<AnthropicEvent>,
    finished: bool,
    request_started: bool,
    invalid_idle_timeout: bool,
}
impl AnthropicStream {
    pub async fn next(
        &mut self,
        cancellation: &dyn Cancellation,
    ) -> Result<Option<AnthropicStreamEvent>, ProviderError> {
        if let Some(event) = self.pending.pop_front() {
            return Ok(Some(AnthropicStreamEvent::Message(event)));
        }
        if self.finished {
            self.http = None;
            return Ok(None);
        }
        let result = self.read_next(cancellation).await;
        if result.is_err() {
            self.finished = true;
            self.http = None;
            self.pending.clear();
        }
        result
    }
    async fn read_next(
        &mut self,
        cancellation: &dyn Cancellation,
    ) -> Result<Option<AnthropicStreamEvent>, ProviderError> {
        // The source validates idle duration in OpenStream, after publishing
        // HttpRequestStarted; the request-byte guard runs before that event.
        if self.request_started && self.invalid_idle_timeout {
            return Err(HttpError::Provider(
                "provider: stream idle timeout cannot be negative".into(),
            )
            .into());
        }
        loop {
            if self.parser.is_stopped() {
                let event = self.parser.finish_next()?;
                if event.is_none() {
                    self.finished = true;
                    self.http = None;
                }
                return Ok(event.map(AnthropicStreamEvent::Message));
            }
            let event = self
                .http
                .as_mut()
                .expect("open stream until terminal")
                .next(cancellation)
                .await?;
            match event {
                Some(HttpStreamEvent::RequestStarted) => {
                    self.request_started = true;
                    return Ok(Some(AnthropicStreamEvent::HttpRequestStarted));
                }
                Some(HttpStreamEvent::ResponseHeadersReceived) => {
                    return Ok(Some(AnthropicStreamEvent::HttpResponseHeadersReceived));
                }
                Some(HttpStreamEvent::Event(event)) => {
                    self.pending.extend(self.parser.consume(&event.data)?);
                    if let Some(event) = self.pending.pop_front() {
                        return Ok(Some(AnthropicStreamEvent::Message(event)));
                    }
                }
                None => {
                    self.http = None;
                    let event = self.parser.finish_next()?;
                    if event.is_none() {
                        self.finished = true;
                    }
                    return Ok(event.map(AnthropicStreamEvent::Message));
                }
            }
        }
    }
}
