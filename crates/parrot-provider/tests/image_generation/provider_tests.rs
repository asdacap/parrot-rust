use parrot_domain::contracts::async_ops::{BoxFuture, Cancellation, OperationResult};
use parrot_foundation::{
    credentials::Credential,
    oauth::{AuthError, CredentialStorage, StoredApiKeySource},
};
use parrot_provider::{
    catalogs::Model,
    http_stream::{
        ByteStream, Endpoint, HttpError, HttpMethod, HttpRequest, HttpResponse, HttpTransport,
        ProviderHttpClient, ProviderHttpClientCatalog, ReqwestTransport,
    },
    image_generation::{
        ImageGenerationReference, ImageGenerationRequest, ImageHttpRequest, ImageTransport,
        ReqwestImageTransport,
    },
    responses_websocket::{ConnectedSocket, Connector, Headers, WebSocketError},
    sessions::{
        Content, Error, EventStream, Provider, Request, Session,
        decorators::{RetryingProvider, SubscriptionUsage, UsageReporter, UsageReportingProvider},
        provider::{
            ApiKeySource, ChatGptOptions, ChatGptProvider, CompatibleOptions, OAuthAccess,
            OAuthTokenSource, OpenAICompatibleProvider,
        },
    },
};
use std::{
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    time::Duration,
};
use tokio::sync::Notify;

const PNG_BASE64: &str = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR4nGP4z8DwHwAFAAH/iZk9HQAAAABJRU5ErkJggg==";
const JPEG_BASE64: &str = "/9j/4AAQSkZJRgABAQAAAQABAAD/2wBDABALDA4MChAODQ4SERATGCgaGBYWGDEjJR0oOjM9PDkzODdASFxOQERXRTc4UG1RV19iZ2hnPk1xeXBkeFxlZ2P/wAALCAACAAMBAREA/8QAFAABAAAAAAAAAAAAAAAAAAAAAP/EABQQAQAAAAAAAAAAAAAAAAAAAAD/2gAIAQEAAD8AP//Z";
const WEBP_BASE64: &str = "UklGRhwAAABXRUJQVlA4TA8AAAAvAkAAAAcQ/Y/+ByKi/wEA";
const PNG_HEX: &str = "89504E470D0A1A0A0000000D49484452000000010000000108060000001F15C4890000000D49444154789C63F8CFC0F01F00050001FF89993D1D0000000049454E44AE426082";
const JPEG_HEX: &str = "FFD8FFE000104A46494600010100000100010000FFDB004300100B0C0E0C0A100E0D0E1211101318281A181616183123251D283A333D3C3933383740485C4E404457453738506D51575F626768673E4D71797064785C656763FFC0000B080002000301011100FFC40014000100000000000000000000000000000000FFC40014100100000000000000000000000000000000FFDA0008010100003F003FFFD9";
const WEBP_HEX: &str = "524946461C000000574542505650384C0F0000002F024000000710FD8FFE0722A2FF0100";
fn hex(value: &str) -> Vec<u8> {
    value
        .as_bytes()
        .chunks_exact(2)
        .map(|pair| u8::from_str_radix(std::str::from_utf8(pair).unwrap(), 16).unwrap())
        .collect()
}
fn success() -> Vec<u8> {
    format!("{{\"data\":[{{\"b64_json\":\"{PNG_BASE64}\"}}]}}").into_bytes()
}
fn request() -> ImageGenerationRequest {
    ImageGenerationRequest {
        prompt: "prompt".into(),
        references: vec![],
    }
}

#[derive(Default)]
struct Token {
    stopped: AtomicBool,
    changed: Notify,
}
impl Token {
    fn cancel(&self) {
        self.stopped.store(true, Ordering::Release);
        self.changed.notify_waiters();
    }
}
impl Cancellation for Token {
    fn is_cancelled(&self) -> bool {
        self.stopped.load(Ordering::Acquire)
    }
    fn cancelled(&self) -> BoxFuture<'_, ()> {
        Box::pin(async move {
            loop {
                let notified = self.changed.notified();
                tokio::pin!(notified);
                notified.as_mut().enable();
                if self.is_cancelled() {
                    return;
                }
                notified.await;
            }
        })
    }
}
#[derive(Default)]
struct Keys {
    reads: AtomicUsize,
    missing: AtomicBool,
}
impl ApiKeySource for Keys {
    fn api_key<'a>(&'a self, _: &'a dyn Cancellation) -> BoxFuture<'a, Result<String, Error>> {
        Box::pin(async move {
            let n = self.reads.fetch_add(1, Ordering::SeqCst) + 1;
            Ok(if self.missing.load(Ordering::SeqCst) {
                String::new()
            } else {
                format!("key-{n}")
            })
        })
    }
    fn has_credential<'a>(&'a self, _: &'a dyn Cancellation) -> BoxFuture<'a, Result<bool, Error>> {
        Box::pin(async move { Ok(!self.missing.load(Ordering::SeqCst)) })
    }
}
impl OAuthTokenSource for Keys {
    fn token<'a>(&'a self, ct: &'a dyn Cancellation) -> BoxFuture<'a, Result<OAuthAccess, Error>> {
        Box::pin(async move {
            let access_token = self.api_key(ct).await?;
            Ok(OAuthAccess {
                access_token,
                account_id: format!("account-{}", self.reads.load(Ordering::SeqCst)),
            })
        })
    }
    fn has_credential<'a>(
        &'a self,
        ct: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<bool, Error>> {
        ApiKeySource::has_credential(self, ct)
    }
}
#[derive(Default)]
struct Chat(AtomicUsize);
impl HttpTransport for Chat {
    fn send<'a>(
        &'a self,
        _: HttpRequest,
        _: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<HttpResponse, HttpError>> {
        Box::pin(async move {
            self.0.fetch_add(1, Ordering::SeqCst);
            Err(HttpError::Transport("unexpected chat HTTP".into()))
        })
    }
}
#[derive(Default)]
struct Socket(AtomicUsize);
impl Connector for Socket {
    fn connect<'a>(
        &'a self,
        _: &'a str,
        _: &'a Headers,
        _: Option<Duration>,
        _: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<ConnectedSocket, WebSocketError>> {
        Box::pin(async move {
            self.0.fetch_add(1, Ordering::SeqCst);
            Err(WebSocketError::Transport("unexpected WebSocket".into()))
        })
    }
}
struct Body {
    data: Vec<u8>,
    offset: usize,
}
impl ByteStream for Body {
    fn read<'a>(
        &'a mut self,
        output: &'a mut [u8],
        ct: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<usize, HttpError>> {
        Box::pin(async move {
            if ct.is_cancelled() {
                return Err(HttpError::Cancelled);
            }
            let n = output.len().min(self.data.len() - self.offset);
            output[..n].copy_from_slice(&self.data[self.offset..self.offset + n]);
            self.offset += n;
            Ok(n)
        })
    }
}
struct Images {
    requests: Mutex<Vec<ImageHttpRequest>>,
    status: u16,
    body: Vec<u8>,
    delay: Mutex<Duration>,
    cancel_on_send: Mutex<Option<Arc<Token>>>,
}
impl ImageTransport for Images {
    fn send<'a>(
        &'a self,
        request: ImageHttpRequest,
        ct: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<HttpResponse, Error>> {
        Box::pin(async move {
            self.requests.lock().unwrap().push(request);
            let cancel = self.cancel_on_send.lock().unwrap().clone();
            if let Some(cancel) = cancel {
                cancel.cancel();
            }
            if ct.is_cancelled() {
                return Err(Error::Cancelled);
            }
            let delay = *self.delay.lock().unwrap();
            if !delay.is_zero() {
                tokio::select! {biased;_=ct.cancelled()=>return Err(Error::Cancelled),_=tokio::time::sleep(delay)=>{}}
            }
            Ok(HttpResponse {
                status: self.status,
                reason: String::new(),
                headers: vec![],
                body: Box::new(Body {
                    data: self.body.clone(),
                    offset: 0,
                }),
            })
        })
    }
}
/// Kimi/OpenCode reporter selection and quota decoding remain T32 obligations.
/// This injected capability proves the real usage decorator forwards generation.
#[derive(Default)]
struct UsageBoundary(AtomicUsize);
impl UsageReporter for UsageBoundary {
    fn usage<'a>(
        &'a self,
        _: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<SubscriptionUsage, Error>> {
        Box::pin(async move {
            self.0.fetch_add(1, Ordering::SeqCst);
            Err(Error::Provider("unexpected quota request".into()))
        })
    }
}
struct Fixture {
    provider: Arc<dyn Provider>,
    keys: Arc<Keys>,
    images: Arc<Images>,
    chat: Arc<Chat>,
    socket: Arc<Socket>,
    usage: Arc<UsageBoundary>,
}
impl Fixture {
    fn new(kind: &str, base_url: &str, status: u16, body: Vec<u8>) -> Self {
        let keys = Arc::new(Keys::default());
        let images = Arc::new(Images {
            requests: Mutex::new(vec![]),
            status,
            body,
            delay: Mutex::new(Duration::ZERO),
            cancel_on_send: Mutex::new(None),
        });
        let chat = Arc::new(Chat::default());
        let socket = Arc::new(Socket::default());
        let usage = Arc::new(UsageBoundary::default());
        let provider: Arc<dyn Provider> = if kind == "chatgpt" {
            Arc::new(
                ChatGptProvider::new_with_images(
                    ChatGptOptions::new(keys.clone()),
                    chat.clone(),
                    images.clone(),
                    socket.clone(),
                )
                .unwrap(),
            )
        } else {
            let mut options = CompatibleOptions::new("configured", base_url, keys.clone());
            options.header_timeout_ms = 1;
            options.headers = vec![("X-Tenant".into(), "tenant".into())];
            let inner: Arc<dyn Provider> = Arc::new(
                OpenAICompatibleProvider::new_with_images(
                    options,
                    chat.clone(),
                    images.clone(),
                    socket.clone(),
                )
                .unwrap(),
            );
            let inner = if matches!(kind, "kimi" | "opencode") {
                Arc::new(UsageReportingProvider::new(inner, usage.clone())) as Arc<dyn Provider>
            } else {
                inner
            };
            if kind == "compatible" {
                inner
            } else {
                Arc::new(RetryingProvider::new(inner, 5))
            }
        };
        Self {
            provider,
            keys,
            images,
            chat,
            socket,
            usage,
        }
    }
    fn no_other_dispatch(&self) {
        assert_eq!(self.chat.0.load(Ordering::SeqCst), 0);
        assert_eq!(self.socket.0.load(Ordering::SeqCst), 0);
        assert_eq!(self.usage.0.load(Ordering::SeqCst), 0);
    }
}
fn header<'a>(request: &'a ImageHttpRequest, name: &str) -> &'a str {
    request
        .headers
        .iter()
        .find(|(key, _)| key.eq_ignore_ascii_case(name))
        .map(|(_, value)| value.as_str())
        .unwrap_or("")
}

#[tokio::test]
async fn actual_factories_generate_and_edit_all_seven_source_tuples_with_fresh_credentials() {
    for (kind, base_url, edit) in [
        ("compatible", "https://example.test/v1", false),
        ("compatible", "https://example.test/custom/v1/", true),
        ("chatgpt", "https://example.test/unused", false),
        ("chatgpt", "https://example.test/unused", true),
        ("retry", "https://example.test/v1", true),
        ("kimi", "https://example.test/v1", true),
        ("opencode", "https://example.test/v1", true),
    ] {
        let fixture = Fixture::new(kind, base_url, 200, success());
        let mut request = request();
        request.prompt = "  Keep this prompt verbatim.\nSecond line.  ".into();
        if edit {
            request.references = vec![
                ImageGenerationReference {
                    media_type: "image/png".into(),
                    data: hex(PNG_HEX),
                },
                ImageGenerationReference {
                    media_type: "image/jpeg".into(),
                    data: hex(JPEG_HEX),
                },
            ];
        }
        for operation in 1..=2 {
            let result = fixture
                .provider
                .generate_image(request.clone(), &Token::default())
                .await
                .unwrap();
            assert_eq!(result.data, hex(PNG_HEX), "{kind}/{edit}");
            let sent = fixture.images.requests.lock().unwrap();
            assert_eq!(sent.len(), operation);
            let sent = &sent[operation - 1];
            let endpoint = if kind == "chatgpt" {
                "https://chatgpt.com/backend-api/codex"
            } else {
                base_url.trim_end_matches('/')
            };
            assert_eq!(
                sent.endpoint.as_str(),
                format!(
                    "{endpoint}/images/{}",
                    if edit { "edits" } else { "generations" }
                )
            );
            assert_eq!(sent.method, "POST");
            assert_eq!(sent.content_type.as_deref(), Some("application/json"));
            assert!(
                !sent
                    .headers
                    .iter()
                    .any(|(name, _)| name.eq_ignore_ascii_case("session-id")
                        || name.eq_ignore_ascii_case("x-opencode-session"))
            );
            assert_eq!(
                header(sent, "Authorization"),
                format!("Bearer key-{operation}")
            );
            if kind == "chatgpt" {
                assert_eq!(
                    header(sent, "ChatGPT-Account-Id"),
                    format!("account-{operation}")
                );
                assert_eq!(header(sent, "originator"), "parrot");
                assert_eq!(header(sent, "User-Agent"), "parrot");
            } else {
                assert_eq!(header(sent, "X-Tenant"), "tenant");
            }
            let body: serde_json::Value = serde_json::from_slice(&sent.body).unwrap();
            assert_eq!(body["prompt"], request.prompt);
            assert_eq!(body["model"], "gpt-image-2");
            for property in ["background", "quality", "size"] {
                assert_eq!(body[property], "auto");
            }
            assert!(body.get("stream").is_none());
            assert_eq!(body.get("images").is_some(), edit);
            let properties: Vec<_> = body
                .as_object()
                .unwrap()
                .keys()
                .map(String::as_str)
                .collect();
            assert_eq!(
                properties,
                if edit {
                    vec!["prompt", "images", "model", "background", "quality", "size"]
                } else {
                    vec!["prompt", "model", "background", "quality", "size"]
                }
            );
            if edit {
                assert_eq!(body["images"].as_array().unwrap().len(), 2);
                assert_eq!(
                    body["images"][0]["image_url"],
                    format!("data:image/png;base64,{PNG_BASE64}")
                );
                assert_eq!(
                    body["images"][1]["image_url"],
                    format!("data:image/jpeg;base64,{JPEG_BASE64}")
                );
            }
        }
        assert_eq!(fixture.keys.reads.load(Ordering::SeqCst), 2);
        fixture.no_other_dispatch();
    }
}

#[tokio::test]
async fn actual_wrappers_do_not_retry_four_paid_source_errors_or_expose_their_bodies() {
    for (kind, status) in [
        ("chatgpt", 401),
        ("retry", 429),
        ("kimi", 404),
        ("opencode", 503),
    ] {
        let fixture = Fixture::new(
            kind,
            "https://example.test/v1",
            status,
            format!("request secret key-1 data:image/png;base64,{PNG_BASE64}").into_bytes(),
        );
        let error = fixture
            .provider
            .generate_image(request(), &Token::default())
            .await
            .unwrap_err();
        assert_eq!(
            error,
            Error::Provider(format!("provider: image generation failed (HTTP {status})"))
        );
        assert!(std::error::Error::source(&error).is_none());
        assert_eq!(fixture.images.requests.lock().unwrap().len(), 1);
        assert_eq!(fixture.keys.reads.load(Ordering::SeqCst), 1);
        fixture.no_other_dispatch();
    }
}
#[tokio::test]
async fn actual_factories_reject_missing_credentials_without_http_for_both_source_kinds() {
    for kind in ["compatible", "chatgpt"] {
        let fixture = Fixture::new(kind, "https://example.test/v1", 200, success());
        fixture.keys.missing.store(true, Ordering::SeqCst);
        let error = fixture
            .provider
            .generate_image(request(), &Token::default())
            .await
            .unwrap_err();
        assert_eq!(
            error,
            Error::Provider(
                if kind == "chatgpt" {
                    "provider: ChatGPT credential requires an access token"
                } else {
                    "provider: \"configured\" has no API key"
                }
                .into()
            )
        );
        assert!(fixture.images.requests.lock().unwrap().is_empty());
        assert_eq!(fixture.keys.reads.load(Ordering::SeqCst), 1);
        fixture.no_other_dispatch();
    }
}
#[tokio::test]
async fn actual_retry_wrapper_propagates_cancellation_before_and_during_image_send() {
    for during in [false, true] {
        let fixture = Fixture::new("retry", "https://example.test/v1", 200, success());
        let token = Arc::new(Token::default());
        if during {
            *fixture.images.cancel_on_send.lock().unwrap() = Some(token.clone());
        } else {
            token.cancel();
        }
        assert!(matches!(
            fixture
                .provider
                .generate_image(request(), token.as_ref())
                .await,
            Err(Error::Cancelled) | Err(Error::Http(HttpError::Cancelled))
        ));
        assert_eq!(
            fixture.images.requests.lock().unwrap().len(),
            usize::from(during)
        );
        assert_eq!(
            fixture.keys.reads.load(Ordering::SeqCst),
            usize::from(during)
        );
        fixture.no_other_dispatch();
    }
}
#[tokio::test]
async fn actual_compatible_factory_accepts_five_source_webp_references_in_order() {
    let fixture = Fixture::new("compatible", "https://example.test/v1", 200, success());
    let mut req = request();
    req.references = vec![
        ImageGenerationReference {
            media_type: "image/webp".into(),
            data: hex(WEBP_HEX)
        };
        5
    ];
    assert_eq!(
        fixture
            .provider
            .generate_image(req.clone(), &Token::default())
            .await
            .unwrap()
            .data,
        hex(PNG_HEX)
    );
    let sent = fixture.images.requests.lock().unwrap();
    assert_eq!(sent.len(), 1);
    let body: serde_json::Value = serde_json::from_slice(&sent[0].body).unwrap();
    assert_eq!(body["images"].as_array().unwrap().len(), 5);
    for image in body["images"].as_array().unwrap() {
        assert_eq!(
            image["image_url"],
            format!("data:image/webp;base64,{WEBP_BASE64}")
        );
    }
    fixture.no_other_dispatch();
}
#[tokio::test(start_paused = true)]
async fn actual_image_operation_ignores_one_ms_chat_header_timeout() {
    let fixture = Fixture::new("compatible", "https://example.test/v1", 200, success());
    *fixture.images.delay.lock().unwrap() = Duration::from_millis(50);
    let start = tokio::time::Instant::now();
    assert_eq!(
        fixture
            .provider
            .generate_image(request(), &Token::default())
            .await
            .unwrap()
            .data,
        hex(PNG_HEX)
    );
    assert!(start.elapsed() >= Duration::from_millis(50));
    assert_eq!(fixture.images.requests.lock().unwrap().len(), 1);
    fixture.no_other_dispatch();
}

#[derive(Default)]
struct Store(Mutex<Option<Credential>>);
impl CredentialStorage for Store {
    fn get<'a>(
        &'a self,
        _: &'a str,
        _: &'a dyn Cancellation,
    ) -> BoxFuture<'a, OperationResult<Option<Credential>, AuthError>> {
        Box::pin(async move { Ok(self.0.lock().unwrap().clone()) })
    }
    fn set<'a>(
        &'a self,
        _: &'a str,
        value: &'a Credential,
        _: &'a dyn Cancellation,
    ) -> BoxFuture<'a, OperationResult<(), AuthError>> {
        Box::pin(async move {
            *self.0.lock().unwrap() = Some(value.clone());
            Ok(())
        })
    }
}
struct StoredKeys(StoredApiKeySource);
impl ApiKeySource for StoredKeys {
    fn api_key<'a>(&'a self, ct: &'a dyn Cancellation) -> BoxFuture<'a, Result<String, Error>> {
        Box::pin(async move {
            self.0
                .api_key(ct)
                .await
                .map(|key| key.value().to_owned())
                .map_err(Error::from)
        })
    }
    fn has_credential<'a>(
        &'a self,
        ct: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<bool, Error>> {
        Box::pin(async move { self.0.has_credential(ct).await.map_err(Error::from) })
    }
}
#[tokio::test]
async fn stored_and_environment_precedence_is_resolved_again_for_each_source_image_operation() {
    for environment_override in [false, true] {
        let store = Arc::new(Store::default());
        let environment = Arc::new(Mutex::new(None::<String>));
        let lookup = environment.clone();
        let source = Arc::new(StoredKeys(StoredApiKeySource::new(
            "configured",
            "SYNTHETIC_T34_IMAGE_KEY",
            store.clone(),
            Arc::new(move |name| {
                assert_eq!(name, "SYNTHETIC_T34_IMAGE_KEY");
                lookup.lock().unwrap().clone()
            }),
        )));
        let fixture = Fixture::new("compatible", "https://example.test/v1", 200, success());
        let provider = OpenAICompatibleProvider::new_with_images(
            CompatibleOptions::new("configured", "https://example.test/v1", source),
            fixture.chat.clone(),
            fixture.images.clone(),
            fixture.socket.clone(),
        )
        .unwrap();
        for operation in 1..=2 {
            store
                .set(
                    "configured",
                    &Credential::for_api_key(format!("stored-{operation}")),
                    &Token::default(),
                )
                .await
                .unwrap();
            if environment_override {
                *environment.lock().unwrap() = Some(format!("environment-{operation}"));
            }
            assert_eq!(
                provider
                    .generate_image(request(), &Token::default())
                    .await
                    .unwrap()
                    .data,
                hex(PNG_HEX)
            );
            let sent = fixture.images.requests.lock().unwrap();
            assert_eq!(
                header(&sent[operation - 1], "Authorization"),
                format!(
                    "Bearer {}-{operation}",
                    if environment_override {
                        "environment"
                    } else {
                        "stored"
                    }
                )
            );
        }
        fixture.no_other_dispatch();
    }
}

struct Unsupported(AtomicUsize);
impl Provider for Unsupported {
    fn id(&self) -> &str {
        "unsupported"
    }
    fn call(&self, _: Request) -> Result<Box<dyn EventStream>, Error> {
        self.0.fetch_add(1, Ordering::SeqCst);
        Err(Error::Provider("unexpected call".into()))
    }
    fn has_credential<'a>(&'a self, _: &'a dyn Cancellation) -> BoxFuture<'a, Result<bool, Error>> {
        Box::pin(async { Ok(false) })
    }
    fn seed_models(&self) -> Vec<Model> {
        vec![]
    }
    fn list_models<'a>(
        &'a self,
        _: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<Vec<Model>, Error>> {
        Box::pin(async { Ok(vec![]) })
    }
    fn calculate_image_tokens(&self, _: &Model, _: &Content) -> i64 {
        0
    }
    fn open_session(&self) -> Result<Arc<dyn Session>, Error> {
        self.0.fetch_add(1, Ordering::SeqCst);
        Err(Error::Provider("unexpected session".into()))
    }
}
#[tokio::test]
async fn unsupported_default_has_no_http_session_or_fallback_even_when_retry_wrapped() {
    let unsupported = Arc::new(Unsupported(AtomicUsize::new(0)));
    let retry = RetryingProvider::new(unsupported.clone(), 5);
    assert_eq!(
        retry
            .generate_image(request(), &Token::default())
            .await
            .unwrap_err(),
        Error::Provider("provider: image generation is not supported".into())
    );
    assert_eq!(unsupported.0.load(Ordering::SeqCst), 0);
}
#[tokio::test]
async fn both_transport_views_share_the_selected_client_scope_and_disposal_without_network() {
    let standard = ProviderHttpClient::standard(reqwest::Client::new());
    let mut catalog = ProviderHttpClientCatalog::new(standard);
    let selected = catalog.resolve("https://selected.invalid", true).unwrap();
    let same = catalog
        .resolve("https://SELECTED.invalid/another", true)
        .unwrap();
    assert!(selected.same_client(&same));
    assert!(selected.allows_invalid_tls_for(
        Endpoint::parse("https://selected.invalid/images/generations").unwrap()
    ));
    assert!(!selected.allows_invalid_tls_for(
        Endpoint::parse("https://other.invalid/images/generations").unwrap()
    ));
    let stream = ReqwestTransport::new(selected.clone());
    let images = ReqwestImageTransport::new(selected.clone());
    selected.dispose();
    let ct = Token::default();
    assert!(matches!(
        stream
            .send(
                HttpRequest {
                    method: HttpMethod::Get,
                    endpoint: Endpoint::parse("https://selected.invalid/models").unwrap(),
                    headers: vec![],
                    body: vec![]
                },
                &ct
            )
            .await,
        Err(HttpError::Disposed)
    ));
    assert!(matches!(
        images
            .send(
                ImageHttpRequest {
                    method: "POST".into(),
                    endpoint: Endpoint::parse("https://selected.invalid/images/generations")
                        .unwrap(),
                    headers: vec![],
                    content_type: Some("application/json".into()),
                    body: vec![]
                },
                &ct
            )
            .await,
        Err(Error::Http(HttpError::Disposed))
    ));
    assert!(same.is_disposed());
}
