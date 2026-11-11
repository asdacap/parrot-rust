//! Source-compatible provider composition over the protocol and transport owners.
use super::diagnostics::CallTrace;
use super::http::{HttpDriver, HttpEventsBridge, Protocol};
use super::socket_session::{ResponseCallFactory, ResponsePrepared, ResponseSession};
use super::{Error, EventStream, Provider, Request, Session};
use crate::{
    catalogs::{
        self, CatalogError, LiteLlmModelInfoDecoder, Model, StandardModelDecoder,
        StandardModelError,
    },
    chat_completions,
    http_stream::{self, Endpoint, HttpTransport, StreamOptions},
    image_generation::{
        ImageGenerationClient, ImageGenerationRequest, ImageGenerationResult, ImageHeaders,
        ImageTransport,
    },
    lazy_stream::LazyEventStream,
    responses,
    responses_websocket::{
        self, Connector, HeaderSource, HttpEvents, HttpFallback, WebSocketClient, WebSocketError,
        WebSocketRequest,
    },
};
use parrot_domain::contracts::async_ops::{BoxFuture, Cancellation};
use std::{sync::Arc, time::Duration};

pub trait ApiKeySource: Send + Sync {
    fn api_key<'a>(
        &'a self,
        cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<String, Error>>;
    fn has_credential<'a>(
        &'a self,
        cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<bool, Error>>;
}
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct OAuthAccess {
    pub access_token: String,
    pub account_id: String,
}
pub trait OAuthTokenSource: Send + Sync {
    fn token<'a>(
        &'a self,
        cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<OAuthAccess, Error>>;
    fn has_credential<'a>(
        &'a self,
        cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<bool, Error>>;
}
pub trait ImageTokenCalculator: Send + Sync {
    fn calculate(&self, model: &Model, content: &super::Content) -> i64;
}
pub struct FallbackImageTokenCalculator;
impl ImageTokenCalculator for FallbackImageTokenCalculator {
    fn calculate(&self, _: &Model, _: &super::Content) -> i64 {
        crate::image_inspection::FALLBACK_IMAGE_TOKENS
    }
}
pub type CatalogDecoder = fn(&str, &str) -> Result<Vec<Model>, CatalogError>;

pub enum CatalogDecoderSelection {
    Standard,
    Legacy(CatalogDecoder),
}

pub struct CompatibleOptions {
    pub id: String,
    pub base_url: String,
    pub protocol: Protocol,
    pub api_key_source: Arc<dyn ApiKeySource>,
    pub headers: Vec<(String, String)>,
    pub allow_insecure_localhost: bool,
    pub allow_insecure_remote: bool,
    pub disable_websocket: bool,
    pub maximum_request_bytes: usize,
    pub header_timeout_ms: i64,
    pub stream_idle_timeout_ms: i64,
    pub provider_preferences: String,
    pub session_header: String,
    pub user_agent: String,
    pub models: Vec<Model>,
    pub model_defaults: Vec<Model>,
    pub external_models: Vec<Model>,
    pub decoder: CatalogDecoderSelection,
    pub image_token_calculator: Arc<dyn ImageTokenCalculator>,
}
impl CompatibleOptions {
    pub fn new(
        id: impl Into<String>,
        base_url: impl Into<String>,
        api_key_source: Arc<dyn ApiKeySource>,
    ) -> Self {
        Self {
            id: id.into(),
            base_url: base_url.into(),
            protocol: Protocol::ChatCompletions,
            api_key_source,
            headers: Vec::new(),
            allow_insecure_localhost: false,
            allow_insecure_remote: false,
            disable_websocket: true,
            maximum_request_bytes: 64 << 20,
            header_timeout_ms: 60_000,
            stream_idle_timeout_ms: 300_000,
            provider_preferences: String::new(),
            session_header: String::new(),
            user_agent: format!("parrot/{}", env!("CARGO_PKG_VERSION")),
            models: Vec::new(),
            model_defaults: Vec::new(),
            external_models: Vec::new(),
            decoder: CatalogDecoderSelection::Standard,
            image_token_calculator: Arc::new(FallbackImageTokenCalculator),
        }
    }
}
pub struct ChatGptOptions {
    pub tokens: Arc<dyn OAuthTokenSource>,
    pub disable_websocket: bool,
    pub maximum_request_bytes: usize,
    pub header_timeout_ms: i64,
    pub stream_idle_timeout_ms: i64,
    pub models: Vec<Model>,
    pub model_defaults: Vec<Model>,
    pub external_models: Vec<Model>,
}
impl ChatGptOptions {
    pub fn new(tokens: Arc<dyn OAuthTokenSource>) -> Self {
        Self {
            tokens,
            disable_websocket: true,
            maximum_request_bytes: 64 << 20,
            header_timeout_ms: 60_000,
            stream_idle_timeout_ms: 300_000,
            models: Vec::new(),
            model_defaults: Vec::new(),
            external_models: Vec::new(),
        }
    }
}

enum Authentication {
    ApiKey {
        source: Arc<dyn ApiKeySource>,
        headers: Vec<(String, String)>,
        user_agent: String,
    },
    OAuth(Arc<dyn OAuthTokenSource>),
}
struct Core {
    id: String,
    protocol: Protocol,
    endpoint: Endpoint,
    models_endpoint: Endpoint,
    model_info_endpoint: Option<Endpoint>,
    auth: Authentication,
    preferences: String,
    session_header: String,
    transport: Arc<dyn HttpTransport>,
    connector: Arc<dyn Connector>,
    options: StreamOptions,
    disable_websocket: bool,
    declared: Vec<Model>,
    defaults: Vec<Model>,
    external: Vec<Model>,
    decoder: CatalogDecoderSelection,
    image_token_calculator: Option<Arc<dyn ImageTokenCalculator>>,
}
impl Core {
    fn prepare(&self, mut request: Request) -> Request {
        match self.auth {
            Authentication::OAuth(_) => request.max_output_tokens = 0,
            Authentication::ApiKey { .. } => {
                request.fast_mode = false;
                request.provider_preferences.clone_from(&self.preferences);
                request.include_router_metadata = !self.preferences.is_empty();
            }
        }
        request
    }
    fn encode(&self, request: &Request) -> Result<Vec<u8>, Error> {
        match self.protocol {
            Protocol::Responses => responses::prepare(request, &self.id)
                .map(|p| p.encode_http())
                .map_err(|e| Error::Encode(e.to_string())),
            Protocol::ChatCompletions => chat_completions::prepare(request, &self.id)
                .map(|p| p.body)
                .map_err(|e| Error::Encode(e.to_string())),
        }
    }
    async fn headers(
        &self,
        session_id: Option<&str>,
        cancellation: &dyn Cancellation,
    ) -> Result<Vec<(String, String)>, Error> {
        let mut headers = match &self.auth {
            Authentication::ApiKey {
                source,
                headers,
                user_agent,
            } => {
                let key = source.api_key(cancellation).await?;
                if key.is_empty() {
                    return Err(Error::Provider(format!(
                        "provider: \"{}\" has no API key",
                        self.id
                    )));
                }
                let mut headers = headers.clone();
                set_header(&mut headers, "Authorization", format!("Bearer {key}"));
                if !headers.iter().any(|(name, _)| name == "User-Agent") {
                    headers.push(("User-Agent".into(), user_agent.clone()));
                }
                headers
            }
            Authentication::OAuth(source) => {
                let access = source.token(cancellation).await?;
                if access.access_token.is_empty() {
                    return Err(Error::Provider(
                        "provider: ChatGPT credential requires an access token".into(),
                    ));
                }
                let mut headers = vec![
                    (
                        "Authorization".into(),
                        format!("Bearer {}", access.access_token),
                    ),
                    ("originator".into(), "parrot".into()),
                    ("User-Agent".into(), "parrot".into()),
                ];
                if !access.account_id.is_empty() {
                    headers.push(("ChatGPT-Account-Id".into(), access.account_id));
                }
                headers
            }
        };
        if let Some(id) = session_id.filter(|_| !self.session_header.is_empty()) {
            set_header(&mut headers, &self.session_header, id.into());
        }
        Ok(headers)
    }
    async fn open_http(
        self: &Arc<Self>,
        request: Request,
        session_id: &str,
        turn_state: &str,
        cancellation: &dyn Cancellation,
        trace: Option<Arc<CallTrace>>,
    ) -> Result<HttpDriver, Error> {
        // ChatGPT checks its token before encoding; API-key providers encode first.
        let prepared = self.prepare(request);
        let (body, mut headers) = match self.auth {
            Authentication::OAuth(_) => {
                let headers = self.headers(Some(session_id), cancellation).await?;
                (self.encode(&prepared)?, headers)
            }
            Authentication::ApiKey { .. } => {
                let body = self.encode(&prepared)?;
                (body, self.headers(Some(session_id), cancellation).await?)
            }
        };
        if matches!(self.auth, Authentication::OAuth(_)) {
            let hint = if prepared.fast_mode {
                format!("model={};tier=priority", prepared.model)
            } else {
                format!("model={}", prepared.model)
            };
            set_header(&mut headers, "x-codex-routing-hint", hint);
        }
        if !turn_state.is_empty() {
            set_header(&mut headers, "x-codex-turn-state", turn_state.into());
        }
        Ok(HttpDriver::new(
            self.transport.clone(),
            self.endpoint.clone(),
            headers,
            body,
            self.options,
            self.protocol,
            self.id.clone(),
            prepared.include_router_metadata,
            trace,
        ))
    }
    fn session(self: &Arc<Self>) -> Result<Arc<dyn Session>, Error> {
        let id = random_session_id()?;
        if matches!(self.protocol, Protocol::ChatCompletions) {
            return Ok(Arc::new(StatelessSession {
                core: self.clone(),
                session_id: id,
            }));
        }
        let client = WebSocketClient::new(
            self.connector.clone(),
            self.endpoint.as_str(),
            nonzero(self.options.header_timeout),
            Some(responses_websocket::DEFAULT_IDLE_TIMEOUT),
            self.options.maximum_request_bytes,
        )?;
        let headers = Arc::new(SessionHeaders {
            core: self.clone(),
            session_id: id.clone(),
        });
        let session =
            responses_websocket::Session::new(&self.id, headers, client, self.disable_websocket);
        Ok(Arc::new(ResponseSession::new(
            session,
            Arc::new(Factory {
                core: self.clone(),
                session_id: id,
            }),
        )))
    }
    async fn has_credential(&self, cancellation: &dyn Cancellation) -> Result<bool, Error> {
        match &self.auth {
            Authentication::ApiKey { source, .. } => source.has_credential(cancellation).await,
            Authentication::OAuth(source) => source.has_credential(cancellation).await,
        }
    }
    fn calculate_image_tokens(&self, model: &Model, content: &super::Content) -> i64 {
        self.image_token_calculator.as_ref().map_or_else(
            || {
                crate::image_inspection::calculate_content_tokens(
                    &model.id,
                    matches!(content, super::Content::Image { .. }),
                    content.image_width(),
                    content.image_height(),
                )
            },
            |calculator| calculator.calculate(model, content),
        )
    }
    fn seed_models(&self) -> Vec<Model> {
        catalogs::merge_models(None, &self.declared, &self.defaults, &self.external)
    }
    async fn list_models(&self, cancellation: &dyn Cancellation) -> Result<Vec<Model>, Error> {
        let headers = self.headers(None, cancellation).await?;
        let body = http_stream::get(
            self.transport.as_ref(),
            &self.models_endpoint,
            headers.clone(),
            http_stream::MODELS_REFRESH_TIMEOUT,
            16 << 20,
            cancellation,
        )
        .await?;
        let mut models = match &self.decoder {
            CatalogDecoderSelection::Standard => StandardModelDecoder::decode_typed(
                &self.id, &body,
            )
            .map_err(|error| match error {
                StandardModelError::Json(CatalogError(message)) => Error::Encode(message),
                StandardModelError::InvalidOperation(CatalogError(message)) => {
                    Error::InvalidOperation(message)
                }
                StandardModelError::Provider(CatalogError(message)) => Error::Provider(message),
            })?,
            CatalogDecoderSelection::Legacy(decoder) => {
                decoder(&self.id, &body).map_err(|error| Error::Provider(error.to_string()))?
            }
        };
        if LiteLlmModelInfoDecoder::needs_supplement(&models)
            && let Some(endpoint) = &self.model_info_endpoint
        {
            let supplement = async {
                let body = http_stream::get(
                    self.transport.as_ref(),
                    endpoint,
                    headers,
                    http_stream::MODELS_REFRESH_TIMEOUT,
                    16 << 20,
                    cancellation,
                )
                .await?;
                LiteLlmModelInfoDecoder::decode(&self.id, &body)
                    .map_err(|e| Error::Provider(e.to_string()))
            }
            .await;
            match supplement {
                Ok(extra) => models = catalogs::supplement_models(&models, &extra),
                Err(error) if cancellation.is_cancelled() => return Err(error),
                Err(_) => {}
            }
        }
        Ok(catalogs::merge_models(
            Some(&models),
            &self.declared,
            &self.defaults,
            &self.external,
        ))
    }
}
impl ImageHeaders for Core {
    fn resolve<'a>(
        &'a self,
        cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<Vec<(String, String)>, Error>> {
        Box::pin(self.headers(None, cancellation))
    }
}

fn nonzero(value: Duration) -> Option<Duration> {
    (!value.is_zero()).then_some(value)
}
fn set_header(headers: &mut Vec<(String, String)>, name: &str, value: String) {
    if let Some((_, current)) = headers.iter_mut().find(|(key, _)| key == name) {
        *current = value;
    } else {
        headers.push((name.into(), value));
    }
}
fn random_session_id() -> Result<String, Error> {
    let mut bytes = [0_u8; 16];
    getrandom::getrandom(&mut bytes)
        .map_err(|_| Error::Provider("provider: cannot generate session ID".into()))?;
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut id = String::with_capacity(32);
    for byte in bytes {
        id.push(char::from(HEX[usize::from(byte >> 4)]));
        id.push(char::from(HEX[usize::from(byte & 15)]));
    }
    Ok(id)
}
fn stream_options(header: i64, idle: i64, maximum: usize) -> Result<StreamOptions, Error> {
    if idle < 0 {
        return Err(Error::Provider(
            "provider: stream idle timeout cannot be negative".into(),
        ));
    }
    if header < 0 {
        return Err(Error::Provider(
            "provider: header timeout cannot be negative".into(),
        ));
    }
    Ok(StreamOptions {
        header_timeout: Duration::from_millis(header as u64),
        idle_timeout: Duration::from_millis(idle as u64),
        maximum_request_bytes: maximum,
    })
}

pub struct OpenAICompatibleProvider {
    core: Arc<Core>,
    images: Option<ImageGenerationClient>,
}
impl OpenAICompatibleProvider {
    /// Streaming-only injected compatibility constructor. Use `new_with_images` for images.
    pub fn new(
        options: CompatibleOptions,
        transport: Arc<dyn HttpTransport>,
        connector: Arc<dyn Connector>,
    ) -> Result<Self, Error> {
        if options.id.trim().is_empty() {
            return Err(Error::Provider(
                "provider: compatible provider ID is required".into(),
            ));
        }
        let stream_options = stream_options(
            options.header_timeout_ms,
            options.stream_idle_timeout_ms,
            options.maximum_request_bytes,
        )?;
        let endpoint_name = match options.protocol {
            Protocol::Responses => "responses",
            Protocol::ChatCompletions => "chat/completions",
        };
        let endpoint = http_stream::endpoint_url(
            &options.base_url,
            endpoint_name,
            options.allow_insecure_localhost,
            options.allow_insecure_remote,
        )?;
        let models_endpoint = http_stream::endpoint_url(
            &options.base_url,
            "models",
            options.allow_insecure_localhost,
            options.allow_insecure_remote,
        )?;
        let model_info_endpoint = http_stream::endpoint_url(
            &options.base_url,
            "model/info",
            options.allow_insecure_localhost,
            options.allow_insecure_remote,
        )?;
        let headers = http_stream::validate_headers(&options.headers)?;
        if !options.session_header.is_empty() {
            http_stream::validate_headers(&[(options.session_header.clone(), String::new())])?;
        }
        Ok(Self {
            images: None,
            core: Arc::new(Core {
                id: options.id,
                protocol: options.protocol,
                endpoint,
                models_endpoint,
                model_info_endpoint: Some(model_info_endpoint),
                auth: Authentication::ApiKey {
                    source: options.api_key_source,
                    headers,
                    user_agent: options.user_agent,
                },
                preferences: options.provider_preferences,
                session_header: options.session_header,
                transport,
                connector,
                options: stream_options,
                disable_websocket: options.disable_websocket,
                declared: options.models,
                defaults: options.model_defaults,
                external: options.external_models,
                decoder: options.decoder,
                image_token_calculator: Some(options.image_token_calculator),
            }),
        })
    }
    pub fn new_with_images(
        options: CompatibleOptions,
        transport: Arc<dyn HttpTransport>,
        images: Arc<dyn ImageTransport>,
        connector: Arc<dyn Connector>,
    ) -> Result<Self, Error> {
        let base_url = options.base_url.clone();
        let allow_local = options.allow_insecure_localhost;
        let allow_remote = options.allow_insecure_remote;
        let mut provider = Self::new(options, transport, connector)?;
        provider.images = Some(ImageGenerationClient::new(
            images,
            http_stream::endpoint_url(&base_url, "images/generations", allow_local, allow_remote)?,
            http_stream::endpoint_url(&base_url, "images/edits", allow_local, allow_remote)?,
            provider.core.clone(),
        ));
        Ok(provider)
    }
    pub fn generate_image<'a>(
        &'a self,
        request: ImageGenerationRequest,
        cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<ImageGenerationResult, Error>> {
        match &self.images {
            Some(images) => images.generate(request, cancellation),
            None => Box::pin(async {
                Err(Error::Provider(
                    "provider: image generation is not supported".into(),
                ))
            }),
        }
    }
    pub async fn has_credential(&self, cancellation: &dyn Cancellation) -> Result<bool, Error> {
        self.core.has_credential(cancellation).await
    }
    pub fn seed_models(&self) -> Vec<Model> {
        self.core.seed_models()
    }
    pub async fn list_models(&self, cancellation: &dyn Cancellation) -> Result<Vec<Model>, Error> {
        self.core.list_models(cancellation).await
    }
}
impl Provider for OpenAICompatibleProvider {
    fn generate_image<'a>(
        &'a self,
        request: ImageGenerationRequest,
        cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<ImageGenerationResult, Error>> {
        Self::generate_image(self, request, cancellation)
    }
    fn has_credential<'a>(
        &'a self,
        cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<bool, Error>> {
        Box::pin(Self::has_credential(self, cancellation))
    }
    fn seed_models(&self) -> Vec<Model> {
        self.core.seed_models()
    }
    fn list_models<'a>(
        &'a self,
        cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<Vec<Model>, Error>> {
        Box::pin(self.core.list_models(cancellation))
    }
    fn calculate_image_tokens(&self, model: &Model, content: &super::Content) -> i64 {
        self.core.calculate_image_tokens(model, content)
    }

    fn id(&self) -> &str {
        &self.core.id
    }
    fn open_session(&self) -> Result<Arc<dyn Session>, Error> {
        self.core.session()
    }
}

pub struct ChatGptProvider {
    core: Arc<Core>,
    usage_reporter: Arc<dyn super::decorators::UsageReporter>,
    images: Option<ImageGenerationClient>,
}
impl ChatGptProvider {
    pub const PROVIDER_ID: &'static str = "chatgpt";
    /// Streaming-only injected compatibility constructor. Use `new_with_images` for images.
    pub fn new(
        options: ChatGptOptions,
        transport: Arc<dyn HttpTransport>,
        connector: Arc<dyn Connector>,
    ) -> Result<Self, Error> {
        let core = Arc::new(Core {
            id: Self::PROVIDER_ID.into(),
            protocol: Protocol::Responses,
            endpoint: Endpoint::parse("https://chatgpt.com/backend-api/codex/responses")
                .expect("compiled endpoint"),
            models_endpoint: Endpoint::parse(
                "https://chatgpt.com/backend-api/codex/models?client_version=0.144.5",
            )
            .expect("compiled endpoint"),
            model_info_endpoint: None,
            auth: Authentication::OAuth(options.tokens),
            preferences: String::new(),
            session_header: "session-id".into(),
            transport,
            connector,
            options: stream_options(
                options.header_timeout_ms,
                options.stream_idle_timeout_ms,
                options.maximum_request_bytes,
            )?,
            disable_websocket: options.disable_websocket,
            declared: options.models,
            defaults: options.model_defaults,
            external: options.external_models,
            decoder: CatalogDecoderSelection::Legacy(decode_chatgpt_models),
            image_token_calculator: None,
        });
        let usage_reporter = Arc::new(ChatGptUsageReporter { core: core.clone() });
        Ok(Self {
            core,
            usage_reporter,
            images: None,
        })
    }
    pub fn new_with_images(
        options: ChatGptOptions,
        transport: Arc<dyn HttpTransport>,
        images: Arc<dyn ImageTransport>,
        connector: Arc<dyn Connector>,
    ) -> Result<Self, Error> {
        let mut provider = Self::new(options, transport, connector)?;
        provider.images = Some(ImageGenerationClient::new(
            images,
            Endpoint::parse("https://chatgpt.com/backend-api/codex/images/generations")
                .expect("compiled endpoint"),
            Endpoint::parse("https://chatgpt.com/backend-api/codex/images/edits")
                .expect("compiled endpoint"),
            provider.core.clone(),
        ));
        Ok(provider)
    }
    pub fn generate_image<'a>(
        &'a self,
        request: ImageGenerationRequest,
        cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<ImageGenerationResult, Error>> {
        match &self.images {
            Some(images) => images.generate(request, cancellation),
            None => Box::pin(async {
                Err(Error::Provider(
                    "provider: image generation is not supported".into(),
                ))
            }),
        }
    }
    pub async fn has_credential(&self, cancellation: &dyn Cancellation) -> Result<bool, Error> {
        self.core.has_credential(cancellation).await
    }
    pub fn seed_models(&self) -> Vec<Model> {
        self.core.seed_models()
    }
    pub async fn list_models(&self, cancellation: &dyn Cancellation) -> Result<Vec<Model>, Error> {
        self.core.list_models(cancellation).await
    }
}
impl Provider for ChatGptProvider {
    fn generate_image<'a>(
        &'a self,
        request: ImageGenerationRequest,
        cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<ImageGenerationResult, Error>> {
        Self::generate_image(self, request, cancellation)
    }
    fn has_credential<'a>(
        &'a self,
        cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<bool, Error>> {
        Box::pin(Self::has_credential(self, cancellation))
    }
    fn seed_models(&self) -> Vec<Model> {
        self.core.seed_models()
    }
    fn list_models<'a>(
        &'a self,
        cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<Vec<Model>, Error>> {
        Box::pin(self.core.list_models(cancellation))
    }
    fn calculate_image_tokens(&self, model: &Model, content: &super::Content) -> i64 {
        self.core.calculate_image_tokens(model, content)
    }
    fn usage_reporter(&self) -> Option<Arc<dyn super::decorators::UsageReporter>> {
        Some(self.usage_reporter.clone())
    }

    fn id(&self) -> &str {
        Self::PROVIDER_ID
    }
    fn open_session(&self) -> Result<Arc<dyn Session>, Error> {
        self.core.session()
    }
}

struct SessionHeaders {
    core: Arc<Core>,
    session_id: String,
}
impl HeaderSource for SessionHeaders {
    fn headers<'a>(
        &'a self,
        cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<responses_websocket::Headers, Error>> {
        Box::pin(self.core.headers(Some(&self.session_id), cancellation))
    }
}
struct Factory {
    core: Arc<Core>,
    session_id: String,
}
impl ResponseCallFactory for Factory {
    fn prepare(
        &self,
        request: Request,
        trace: Option<Arc<CallTrace>>,
    ) -> Result<ResponsePrepared, Error> {
        let core = self.core.clone();
        let websocket_request = request.clone();
        Ok(ResponsePrepared {
            prepare_websocket: Box::new(move || {
                let prepared = core.prepare(websocket_request);
                let encoded = responses::prepare(&prepared, &core.id)
                    .map_err(|e| WebSocketError::InvalidRequest(e.to_string()))?;
                WebSocketRequest::new(&encoded)
            }),
            http: Arc::new(Fallback {
                core: self.core.clone(),
                session_id: self.session_id.clone(),
                original_request: request,
                trace,
            }),
        })
    }
}
struct Fallback {
    core: Arc<Core>,
    session_id: String,
    original_request: Request,
    trace: Option<Arc<CallTrace>>,
}
impl HttpFallback for Fallback {
    fn open_unprepared<'a>(
        &'a self,
        turn_state: String,
        cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<Box<dyn HttpEvents>, Error>> {
        Box::pin(async move {
            let driver = self
                .core
                .open_http(
                    self.original_request.clone(),
                    &self.session_id,
                    &turn_state,
                    cancellation,
                    self.trace.clone(),
                )
                .await?;
            Ok(Box::new(HttpEventsBridge::new(driver)) as Box<dyn HttpEvents>)
        })
    }
}
struct StatelessSession {
    core: Arc<Core>,
    session_id: String,
}
impl Session for StatelessSession {
    fn begin_turn(&self) -> BoxFuture<'_, Result<(), Error>> {
        Box::pin(async { Ok(()) })
    }
    fn call(&self, request: Request) -> Result<Box<dyn EventStream>, Error> {
        Ok(lazy_http(
            self.core.clone(),
            self.session_id.clone(),
            request,
            None,
        ))
    }
    fn call_with_trace(
        &self,
        request: Request,
        trace: Arc<CallTrace>,
    ) -> Result<Box<dyn EventStream>, Error> {
        Ok(lazy_http(
            self.core.clone(),
            self.session_id.clone(),
            request,
            Some(trace),
        ))
    }
    fn try_fall_back_to_http(&self) -> BoxFuture<'_, Result<bool, Error>> {
        Box::pin(async { Ok(false) })
    }
    fn close(&self) -> BoxFuture<'_, Result<(), Error>> {
        Box::pin(async { Ok(()) })
    }
}
fn lazy_http(
    core: Arc<Core>,
    session_id: String,
    request: Request,
    trace: Option<Arc<CallTrace>>,
) -> Box<dyn EventStream> {
    Box::new(LazyEventStream::new(request, move |request, stop| {
        let core = core.clone();
        let session_id = session_id.clone();
        let trace = trace.clone();
        Box::pin(async move {
            tokio::select! {
                biased;
                result = core.open_http(request, &session_id, "", stop.as_ref(), trace) => result,
                _ = stop.cancelled() => Err(Error::Cancelled),
            }
        })
    }))
}

fn decode_chatgpt_models(provider_id: &str, body: &str) -> Result<Vec<Model>, CatalogError> {
    use catalogs::{ModelCapabilities, ModelMetadataFields as Fields, ModelVariant};
    use serde_json::value::RawValue;
    use std::collections::HashMap;
    type Object = HashMap<String, Box<RawValue>>;
    let parse_error = |e: serde_json::Error| CatalogError(e.to_string());
    let root: Object = serde_json::from_str(body).map_err(parse_error)?;
    let mut models = Vec::new();
    let mut seen = std::collections::HashSet::new();
    let items: Vec<Box<RawValue>> = root
        .get("models")
        .filter(|v| v.get().trim_start().starts_with('['))
        .map(|v| serde_json::from_str(v.get()))
        .transpose()
        .map_err(parse_error)?
        .unwrap_or_default();
    for raw in items {
        let item: Object = serde_json::from_str(raw.get()).map_err(parse_error)?;
        let string = |name: &str| {
            item.get(name)
                .and_then(|v| serde_json::from_str::<String>(v.get()).ok())
                .unwrap_or_default()
        };
        if string("visibility") != "list" {
            continue;
        }
        let id = string("slug");
        let integer = |name: &str| -> i32 {
            item.get(name)
                .and_then(|v| v.get().parse().ok())
                .unwrap_or(0)
        };
        let mut context = integer("context_window");
        if context == 0 {
            context = integer("max_context_window");
        }
        if id.is_empty() || context <= 0 {
            return Err(CatalogError(format!(
                "provider: model catalog contains invalid listed model \"{id}\""
            )));
        }
        if !seen.insert(id.clone()) {
            continue;
        }
        let mut fields = Fields::CONTEXT_WINDOW;
        let name = item
            .get("display_name")
            .and_then(|v| serde_json::from_str::<String>(v.get()).ok());
        if name.is_some() {
            fields.insert(Fields::NAME);
        }
        let mut efforts: Vec<String> = Vec::new();
        if let Some(raw_levels) = item
            .get("supported_reasoning_levels")
            .filter(|v| v.get().trim_start().starts_with('['))
        {
            fields.insert(Fields::REASONING.union(Fields::VARIANTS));
            let levels: Vec<Object> =
                serde_json::from_str(raw_levels.get()).map_err(parse_error)?;
            for level in levels {
                if let Some(effort) = level
                    .get("effort")
                    .and_then(|v| serde_json::from_str::<String>(v.get()).ok())
                    .filter(|v| !v.is_empty())
                    && !efforts.contains(&effort)
                {
                    efforts.push(effort);
                }
            }
        }
        let mut model = Model::new(&id, provider_id);
        model.name = name.unwrap_or(id);
        model.context_window = context;
        model.fields = fields;
        model.capabilities = ModelCapabilities {
            tools: true,
            reasoning: !efforts.is_empty(),
            output: vec!["text".into()],
            variants: efforts
                .into_iter()
                .map(|e| ModelVariant::new(&e, &e))
                .collect(),
        };
        models.push(model);
    }
    if models.is_empty() {
        Err(CatalogError(
            "provider: models response contains no usable models".into(),
        ))
    } else {
        Ok(models)
    }
}

struct ChatGptUsageReporter {
    core: Arc<Core>,
}
impl super::decorators::UsageReporter for ChatGptUsageReporter {
    fn usage<'a>(
        &'a self,
        cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<super::decorators::SubscriptionUsage, Error>> {
        Box::pin(async move {
            let headers = self.core.headers(None, cancellation).await?;
            let body = http_stream::get(
                self.core.transport.as_ref(),
                Endpoint::parse("https://chatgpt.com/backend-api/wham/usage")
                    .expect("compiled endpoint"),
                headers,
                http_stream::REQUEST_TIMEOUT,
                http_stream::MAX_ERROR_BYTES,
                cancellation,
            )
            .await?;
            decode_usage(&body)
        })
    }
}
fn decode_usage(body: &str) -> Result<super::decorators::SubscriptionUsage, Error> {
    use super::decorators::{SubscriptionUsage, UsageCredits, UsageWindow};
    use serde_json::value::RawValue;
    use std::collections::HashMap;
    type Object = HashMap<String, Box<RawValue>>;
    fn object(raw: &str) -> Result<Object, Error> {
        serde_json::from_str(raw).map_err(|e| Error::Provider(e.to_string()))
    }
    fn string(value: Option<&RawValue>) -> String {
        value
            .and_then(|v| serde_json::from_str::<String>(v.get()).ok())
            .unwrap_or_default()
    }
    fn window(scope: &Object, name: &str) -> Result<Option<UsageWindow>, Error> {
        let Some(raw) = scope
            .get(name)
            .filter(|v| v.get().trim_start().starts_with('{'))
        else {
            return Ok(None);
        };
        let window = object(raw.get())?;
        let integer = |name: &str| {
            window
                .get(name)
                .and_then(|v| v.get().parse::<i64>().ok())
                .unwrap_or(0)
        };
        let percent = window
            .get("used_percent")
            .and_then(|v| {
                v.get().parse::<f64>().ok().or_else(|| {
                    serde_json::from_str::<String>(v.get())
                        .ok()
                        .and_then(|v| v.trim().parse().ok())
                })
            })
            .unwrap_or(0.0);
        Ok(Some(UsageWindow {
            used_percent: percent,
            reset_at: unix_date(integer("reset_at"))?,
            limit_window_seconds: integer("limit_window_seconds"),
        }))
    }
    let root = object(body)?;
    let mut result = SubscriptionUsage {
        plan_type: string(root.get("plan_type").map(Box::as_ref)),
        ..Default::default()
    };
    if let Some(raw) = root
        .get("rate_limit")
        .filter(|v| v.get().trim_start().starts_with('{'))
    {
        let rate = object(raw.get())?;
        result.primary_window = window(&rate, "primary_window")?;
        result.secondary_window = window(&rate, "secondary_window")?;
    }
    if let Some(raw) = root
        .get("credits")
        .filter(|v| v.get().trim_start().starts_with('{'))
    {
        let credits = object(raw.get())?;
        result.credits = Some(UsageCredits {
            has_credits: credits
                .get("has_credits")
                .is_some_and(|v| v.get() == "true"),
            balance: credits
                .get("balance")
                .map(|v| v.get().trim_matches('"').to_owned())
                .unwrap_or_default(),
        });
    }
    Ok(result)
}
// Gregorian conversion of the source's DateTimeOffset.FromUnixTimeSeconds value.
fn unix_date(seconds: i64) -> Result<String, Error> {
    if !(-62_135_596_800..=253_402_300_799).contains(&seconds) {
        return Err(Error::Provider(
            "usage reset_at is outside DateTimeOffset range".into(),
        ));
    }
    let days = seconds.div_euclid(86_400);
    let time = seconds.rem_euclid(86_400);
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let year = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = mp + if mp < 10 { 3 } else { -9 };
    let year = year + i64::from(month <= 2);
    Ok(format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}+00:00",
        time / 3600,
        time / 60 % 60,
        time % 60
    ))
}
