use parrot_domain::contracts::async_ops::{BoxFuture, Cancellation};
use parrot_provider::{
    http_stream::{ByteStream, HttpError, HttpRequest, HttpResponse, HttpTransport},
    image_generation::{
        ImageGenerationReference, ImageGenerationRequest, ImageHttpRequest, ImageTransport,
    },
    responses_websocket::{ConnectedSocket, Connector, Headers, WebSocketError},
    sessions::{
        Error, Provider,
        provider::{
            ApiKeySource, ChatGptOptions, ChatGptProvider, CompatibleOptions, OAuthAccess,
            OAuthTokenSource, OpenAICompatibleProvider,
        },
    },
};
use std::{
    collections::VecDeque,
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};

const PNG: &str = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR4nGP4z8DwHwAFAAH/iZk9HQAAAABJRU5ErkJggg==";
fn response_body() -> Vec<u8> {
    format!("{{\"data\":[{{\"b64_json\":\"{PNG}\"}}]}}").into_bytes()
}
fn png() -> Vec<u8> {
    parrot_provider::image_generation::client::decode_response(&response_body())
        .unwrap()
        .data
}
struct Token(bool);
impl Cancellation for Token {
    fn is_cancelled(&self) -> bool {
        self.0
    }
    fn cancelled(&self) -> BoxFuture<'_, ()> {
        Box::pin(async move {
            if !self.0 {
                std::future::pending::<()>().await;
            }
        })
    }
}
struct Credentials {
    values: Mutex<VecDeque<String>>,
    calls: AtomicUsize,
    account: String,
}
impl Credentials {
    fn new(values: &[&str], account: &str) -> Arc<Self> {
        Arc::new(Self {
            values: Mutex::new(values.iter().map(|v| (*v).into()).collect()),
            calls: AtomicUsize::new(0),
            account: account.into(),
        })
    }
    fn next(&self) -> String {
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.values
            .lock()
            .unwrap()
            .pop_front()
            .expect("scripted credential resolution")
    }
}
impl ApiKeySource for Credentials {
    fn api_key<'a>(&'a self, _: &'a dyn Cancellation) -> BoxFuture<'a, Result<String, Error>> {
        Box::pin(async move { Ok(self.next()) })
    }
    fn has_credential<'a>(&'a self, _: &'a dyn Cancellation) -> BoxFuture<'a, Result<bool, Error>> {
        Box::pin(async { Ok(true) })
    }
}
impl OAuthTokenSource for Credentials {
    fn token<'a>(&'a self, _: &'a dyn Cancellation) -> BoxFuture<'a, Result<OAuthAccess, Error>> {
        Box::pin(async move {
            Ok(OAuthAccess {
                access_token: self.next(),
                account_id: self.account.clone(),
            })
        })
    }
    fn has_credential<'a>(&'a self, _: &'a dyn Cancellation) -> BoxFuture<'a, Result<bool, Error>> {
        Box::pin(async { Ok(true) })
    }
}
#[derive(Default)]
struct StreamingOnly(AtomicUsize);
impl HttpTransport for StreamingOnly {
    fn send<'a>(
        &'a self,
        _: HttpRequest,
        _: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<HttpResponse, HttpError>> {
        Box::pin(async move {
            self.0.fetch_add(1, Ordering::SeqCst);
            panic!("image generation must not use the streaming transport")
        })
    }
}
#[derive(Default)]
struct NoSocket(AtomicUsize);
impl Connector for NoSocket {
    fn connect<'a>(
        &'a self,
        _: &'a str,
        _: &'a Headers,
        _: Option<Duration>,
        _: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<ConnectedSocket, WebSocketError>> {
        Box::pin(async move {
            self.0.fetch_add(1, Ordering::SeqCst);
            panic!("image generation must not use WebSocket/fallback")
        })
    }
}
struct Bytes(Vec<u8>, usize);
impl ByteStream for Bytes {
    fn read<'a>(
        &'a mut self,
        output: &'a mut [u8],
        cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<usize, HttpError>> {
        Box::pin(async move {
            if cancellation.is_cancelled() {
                return Err(HttpError::Cancelled);
            }
            let count = output.len().min(self.0.len() - self.1);
            output[..count].copy_from_slice(&self.0[self.1..self.1 + count]);
            self.1 += count;
            Ok(count)
        })
    }
}
type Reply = Result<(u16, Vec<u8>), Error>;
struct Images {
    requests: Mutex<Vec<ImageHttpRequest>>,
    replies: Mutex<VecDeque<Reply>>,
}
impl Images {
    fn success(count: usize) -> Arc<Self> {
        Self::script((0..count).map(|_| Ok((200, response_body()))))
    }
    fn script(replies: impl IntoIterator<Item = Reply>) -> Arc<Self> {
        Arc::new(Self {
            requests: Mutex::new(Vec::new()),
            replies: Mutex::new(replies.into_iter().collect()),
        })
    }
}
impl ImageTransport for Images {
    fn send<'a>(
        &'a self,
        request: ImageHttpRequest,
        cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<HttpResponse, Error>> {
        Box::pin(async move {
            if cancellation.is_cancelled() {
                return Err(Error::Cancelled);
            }
            self.requests.lock().unwrap().push(request);
            let (status, body) = self
                .replies
                .lock()
                .unwrap()
                .pop_front()
                .expect("scripted image response")?;
            Ok(HttpResponse {
                status,
                reason: String::new(),
                headers: Vec::new(),
                body: Box::new(Bytes(body, 0)),
            })
        })
    }
}
fn request(edit: bool) -> ImageGenerationRequest {
    ImageGenerationRequest {
        prompt: "  An image.\nPreserve this prompt.  ".into(),
        references: if edit {
            vec![ImageGenerationReference {
                media_type: "image/png".into(),
                data: png(),
            }]
        } else {
            Vec::new()
        },
    }
}
fn header(request: &ImageHttpRequest, name: &str) -> Option<String> {
    request
        .headers
        .iter()
        .find(|(key, _)| key.eq_ignore_ascii_case(name))
        .map(|(_, value)| value.clone())
}
fn assert_no_session_headers(request: &ImageHttpRequest) {
    for name in ["x-opencode-session", "session-id", "x-codex-turn-state"] {
        assert_eq!(header(request, name), None, "{name}");
    }
}
fn factory(
    chatgpt: bool,
    enabled: bool,
    credentials: Arc<Credentials>,
    streaming: Arc<StreamingOnly>,
    images: Arc<Images>,
    socket: Arc<NoSocket>,
) -> Arc<dyn Provider> {
    if chatgpt {
        let options = ChatGptOptions::new(credentials);
        if enabled {
            Arc::new(ChatGptProvider::new_with_images(options, streaming, images, socket).unwrap())
        } else {
            Arc::new(ChatGptProvider::new(options, streaming, socket).unwrap())
        }
    } else {
        let options = CompatibleOptions::new("configured", "https://example.test/v1", credentials);
        if enabled {
            Arc::new(
                OpenAICompatibleProvider::new_with_images(options, streaming, images, socket)
                    .unwrap(),
            )
        } else {
            Arc::new(OpenAICompatibleProvider::new(options, streaming, socket).unwrap())
        }
    }
}

#[tokio::test]
async fn compatible_image_factory_uses_exact_generation_edit_endpoints_and_core_headers() {
    for base_url in [
        "https://example.test/v1",
        "http://example.test:4000/custom/v1",
    ] {
        let credentials = Credentials::new(&["first-key", "second-key"], "");
        let streaming = Arc::new(StreamingOnly::default());
        let images = Images::success(2);
        let socket = Arc::new(NoSocket::default());
        let mut options = CompatibleOptions::new("configured", base_url, credentials.clone());
        options.allow_insecure_remote = base_url.starts_with("http:");
        options.session_header = "x-opencode-session".into();
        options.headers = vec![
            ("X-Tenant".into(), "tenant".into()),
            ("User-Agent".into(), "custom-agent".into()),
        ];
        options.maximum_request_bytes = 1;
        let provider: Arc<dyn Provider> = Arc::new(
            OpenAICompatibleProvider::new_with_images(
                options,
                streaming.clone(),
                images.clone(),
                socket.clone(),
            )
            .unwrap(),
        );
        assert_eq!(credentials.calls.load(Ordering::SeqCst), 0);
        assert!(images.requests.lock().unwrap().is_empty());
        for edit in [false, true] {
            assert_eq!(
                provider
                    .generate_image(request(edit), &Token(false))
                    .await
                    .unwrap()
                    .data,
                png()
            );
        }
        let requests = images.requests.lock().unwrap();
        assert_eq!(requests.len(), 2);
        for (index, suffix) in ["images/generations", "images/edits"]
            .into_iter()
            .enumerate()
        {
            let recorded = &requests[index];
            assert_eq!(recorded.endpoint.as_str(), format!("{base_url}/{suffix}"));
            assert_eq!(recorded.method, "POST");
            assert_eq!(recorded.content_type.as_deref(), Some("application/json"));
            assert_eq!(
                header(recorded, "Authorization").as_deref(),
                Some(if index == 0 {
                    "Bearer first-key"
                } else {
                    "Bearer second-key"
                })
            );
            assert_eq!(header(recorded, "X-Tenant").as_deref(), Some("tenant"));
            assert_eq!(
                header(recorded, "User-Agent").as_deref(),
                Some("custom-agent")
            );
            assert_no_session_headers(recorded);
            assert!(recorded.body.len() > 1);
            let body: serde_json::Value = serde_json::from_slice(&recorded.body).unwrap();
            assert_eq!(body["prompt"], request(index == 1).prompt);
            assert_eq!(body["model"], "gpt-image-2");
        }
        assert_eq!(credentials.calls.load(Ordering::SeqCst), 2);
        assert_eq!(streaming.0.load(Ordering::SeqCst), 0);
        assert_eq!(socket.0.load(Ordering::SeqCst), 0);
    }
}

#[tokio::test]
async fn chatgpt_image_factory_uses_fixed_endpoints_and_fresh_oauth_without_session_headers() {
    for account in ["account", ""] {
        let credentials = Credentials::new(&["first-token", "second-token"], account);
        let streaming = Arc::new(StreamingOnly::default());
        let images = Images::success(2);
        let socket = Arc::new(NoSocket::default());
        let mut options = ChatGptOptions::new(credentials.clone());
        options.maximum_request_bytes = 1;
        let provider: Arc<dyn Provider> = Arc::new(
            ChatGptProvider::new_with_images(
                options,
                streaming.clone(),
                images.clone(),
                socket.clone(),
            )
            .unwrap(),
        );
        assert_eq!(credentials.calls.load(Ordering::SeqCst), 0);
        for edit in [false, true] {
            assert_eq!(
                provider
                    .generate_image(request(edit), &Token(false))
                    .await
                    .unwrap()
                    .data,
                png()
            );
        }
        let requests = images.requests.lock().unwrap();
        assert_eq!(requests.len(), 2);
        for (index, suffix) in ["images/generations", "images/edits"]
            .into_iter()
            .enumerate()
        {
            let recorded = &requests[index];
            assert_eq!(
                recorded.endpoint.as_str(),
                format!("https://chatgpt.com/backend-api/codex/{suffix}")
            );
            assert_eq!(
                header(recorded, "Authorization").as_deref(),
                Some(if index == 0 {
                    "Bearer first-token"
                } else {
                    "Bearer second-token"
                })
            );
            assert_eq!(header(recorded, "originator").as_deref(), Some("parrot"));
            assert_eq!(header(recorded, "User-Agent").as_deref(), Some("parrot"));
            assert_eq!(
                header(recorded, "ChatGPT-Account-Id").as_deref(),
                if account.is_empty() {
                    None
                } else {
                    Some(account)
                }
            );
            assert_no_session_headers(recorded);
            assert!(recorded.body.len() > 1);
        }
        assert_eq!(credentials.calls.load(Ordering::SeqCst), 2);
        assert_eq!(streaming.0.load(Ordering::SeqCst), 0);
        assert_eq!(socket.0.load(Ordering::SeqCst), 0);
    }
}

#[tokio::test]
async fn image_validation_and_initial_cancellation_precede_both_credential_sources() {
    for chatgpt in [false, true] {
        let credentials = Credentials::new(&[], "");
        let streaming = Arc::new(StreamingOnly::default());
        let images = Images::success(0);
        let socket = Arc::new(NoSocket::default());
        let provider = factory(
            chatgpt,
            true,
            credentials.clone(),
            streaming.clone(),
            images.clone(),
            socket.clone(),
        );
        let invalid = [
            ImageGenerationRequest {
                prompt: " \n ".into(),
                references: Vec::new(),
            },
            ImageGenerationRequest {
                prompt: "image".into(),
                references: vec![ImageGenerationReference {
                    media_type: "image/png".into(),
                    data: vec![0],
                }],
            },
        ];
        for request in invalid {
            assert!(matches!(
                provider.generate_image(request, &Token(false)).await,
                Err(Error::Provider(_))
            ));
        }
        assert_eq!(
            provider
                .generate_image(request(false), &Token(true))
                .await
                .unwrap_err(),
            Error::Cancelled
        );
        assert_eq!(credentials.calls.load(Ordering::SeqCst), 0);
        assert!(images.requests.lock().unwrap().is_empty());
        assert_eq!(streaming.0.load(Ordering::SeqCst), 0);
        assert_eq!(socket.0.load(Ordering::SeqCst), 0);
    }
}

#[tokio::test]
async fn image_missing_key_and_token_keep_exact_provider_errors_without_http_or_fallback() {
    for chatgpt in [false, true] {
        let credentials = Credentials::new(&[""], "");
        let streaming = Arc::new(StreamingOnly::default());
        let images = Images::success(0);
        let socket = Arc::new(NoSocket::default());
        let provider = factory(
            chatgpt,
            true,
            credentials.clone(),
            streaming.clone(),
            images.clone(),
            socket.clone(),
        );
        let error = provider
            .generate_image(request(false), &Token(false))
            .await
            .unwrap_err();
        assert_eq!(
            error,
            Error::Provider(
                if chatgpt {
                    "provider: ChatGPT credential requires an access token"
                } else {
                    "provider: \"configured\" has no API key"
                }
                .into()
            )
        );
        assert_eq!(credentials.calls.load(Ordering::SeqCst), 1);
        assert!(images.requests.lock().unwrap().is_empty());
        assert_eq!(streaming.0.load(Ordering::SeqCst), 0);
        assert_eq!(socket.0.load(Ordering::SeqCst), 0);
    }
}

#[tokio::test]
async fn streaming_only_constructors_explicitly_reject_images_without_any_resolution_or_request() {
    for chatgpt in [false, true] {
        let credentials = Credentials::new(&[], "");
        let streaming = Arc::new(StreamingOnly::default());
        let images = Images::success(0);
        let socket = Arc::new(NoSocket::default());
        let provider = factory(
            chatgpt,
            false,
            credentials.clone(),
            streaming.clone(),
            images.clone(),
            socket.clone(),
        );
        for edit in [false, true] {
            assert_eq!(
                provider
                    .generate_image(request(edit), &Token(false))
                    .await
                    .unwrap_err(),
                Error::Provider("provider: image generation is not supported".into())
            );
        }
        assert_eq!(credentials.calls.load(Ordering::SeqCst), 0);
        assert!(images.requests.lock().unwrap().is_empty());
        assert_eq!(streaming.0.load(Ordering::SeqCst), 0);
        assert_eq!(socket.0.load(Ordering::SeqCst), 0);
    }
}

#[tokio::test]
async fn image_transport_failures_do_not_retry_or_fall_back_to_streaming() {
    for chatgpt in [false, true] {
        let credentials = Credentials::new(&["key"], "");
        let streaming = Arc::new(StreamingOnly::default());
        let socket = Arc::new(NoSocket::default());
        let expected = Error::Http(HttpError::Transport("synthetic failure".into()));
        let images = Images::script([Err(expected.clone()), Ok((200, response_body()))]);
        let provider = factory(
            chatgpt,
            true,
            credentials.clone(),
            streaming.clone(),
            images.clone(),
            socket.clone(),
        );
        assert_eq!(
            provider
                .generate_image(request(false), &Token(false))
                .await
                .unwrap_err(),
            expected
        );
        assert_eq!(credentials.calls.load(Ordering::SeqCst), 1);
        assert_eq!(images.requests.lock().unwrap().len(), 1);
        assert_eq!(images.replies.lock().unwrap().len(), 1);
        assert_eq!(streaming.0.load(Ordering::SeqCst), 0);
        assert_eq!(socket.0.load(Ordering::SeqCst), 0);
    }
}

#[test]
fn image_capable_compatible_constructor_preserves_endpoint_and_header_validation() {
    for forbidden_header in [false, true] {
        let credentials = Credentials::new(&[], "");
        let streaming = Arc::new(StreamingOnly::default());
        let images = Images::success(0);
        let socket = Arc::new(NoSocket::default());
        let mut options = CompatibleOptions::new(
            "configured",
            if forbidden_header {
                "https://example.test/v1"
            } else {
                "http://example.test/v1"
            },
            credentials.clone(),
        );
        if forbidden_header {
            options.headers = vec![("Authorization".into(), "configured-secret".into())];
        }
        assert!(matches!(
            OpenAICompatibleProvider::new_with_images(
                options,
                streaming.clone(),
                images.clone(),
                socket.clone()
            ),
            Err(Error::Http(_))
        ));
        assert_eq!(credentials.calls.load(Ordering::SeqCst), 0);
        assert!(images.requests.lock().unwrap().is_empty());
        assert_eq!(streaming.0.load(Ordering::SeqCst), 0);
        assert_eq!(socket.0.load(Ordering::SeqCst), 0);
    }
}

#[tokio::test]
async fn compatible_image_default_user_agent_reuses_core_policy() {
    let credentials = Credentials::new(&["key"], "");
    let streaming = Arc::new(StreamingOnly::default());
    let images = Images::success(1);
    let provider = OpenAICompatibleProvider::new_with_images(
        CompatibleOptions::new("configured", "https://example.test/v1", credentials.clone()),
        streaming.clone(),
        images.clone(),
        Arc::new(NoSocket::default()),
    )
    .unwrap();
    provider
        .generate_image(request(false), &Token(false))
        .await
        .unwrap();
    let requests = images.requests.lock().unwrap();
    assert_eq!(
        header(&requests[0], "User-Agent"),
        Some(format!("parrot/{}", env!("CARGO_PKG_VERSION")))
    );
    assert_eq!(
        header(&requests[0], "Authorization").as_deref(),
        Some("Bearer key")
    );
    assert_no_session_headers(&requests[0]);
    assert_eq!(credentials.calls.load(Ordering::SeqCst), 1);
    assert_eq!(streaming.0.load(Ordering::SeqCst), 0);
}
