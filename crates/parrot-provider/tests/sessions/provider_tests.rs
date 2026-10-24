use parrot_domain::contracts::async_ops::{BoxFuture, Cancellation};
use parrot_provider::{
    catalogs::{CatalogError, Model, StandardModelDecoder, StandardModelError},
    http_stream::{ByteStream, HttpError, HttpRequest, HttpResponse, HttpTransport},
    responses_websocket::{ConnectedSocket, Connector, Headers, WebSocketError},
    sessions::{
        Content, Error, Event, EventStream, Provider, Request, Tool, http::Protocol, provider::*,
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

struct Never;
impl Cancellation for Never {
    fn is_cancelled(&self) -> bool {
        false
    }
    fn cancelled(&self) -> BoxFuture<'_, ()> {
        Box::pin(std::future::pending())
    }
}
#[derive(Default)]
struct Key {
    calls: AtomicUsize,
    value: Mutex<String>,
}
impl Key {
    fn new(value: &str) -> Arc<Self> {
        Arc::new(Self {
            value: Mutex::new(value.into()),
            ..Default::default()
        })
    }
}
impl ApiKeySource for Key {
    fn api_key<'a>(&'a self, _: &'a dyn Cancellation) -> BoxFuture<'a, Result<String, Error>> {
        Box::pin(async {
            self.calls.fetch_add(1, Ordering::SeqCst);
            Ok(self.value.lock().unwrap().clone())
        })
    }
    fn has_credential<'a>(&'a self, _: &'a dyn Cancellation) -> BoxFuture<'a, Result<bool, Error>> {
        Box::pin(async { Ok(!self.value.lock().unwrap().is_empty()) })
    }
}
struct Tokens {
    calls: AtomicUsize,
    access: Mutex<OAuthAccess>,
}
impl Tokens {
    fn new(token: &str, account: &str) -> Arc<Self> {
        Arc::new(Self {
            calls: AtomicUsize::new(0),
            access: Mutex::new(OAuthAccess {
                access_token: token.into(),
                account_id: account.into(),
            }),
        })
    }
}
impl OAuthTokenSource for Tokens {
    fn token<'a>(&'a self, _: &'a dyn Cancellation) -> BoxFuture<'a, Result<OAuthAccess, Error>> {
        Box::pin(async {
            self.calls.fetch_add(1, Ordering::SeqCst);
            Ok(self.access.lock().unwrap().clone())
        })
    }
    fn has_credential<'a>(&'a self, _: &'a dyn Cancellation) -> BoxFuture<'a, Result<bool, Error>> {
        Box::pin(async { Ok(!self.access.lock().unwrap().access_token.is_empty()) })
    }
}
struct NoSocket;
impl Connector for NoSocket {
    fn connect<'a>(
        &'a self,
        _: &'a str,
        _: &'a Headers,
        _: Option<Duration>,
        _: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<ConnectedSocket, WebSocketError>> {
        Box::pin(async { panic!("HTTP-default provider must not connect a WebSocket") })
    }
}
struct Bytes {
    bytes: Vec<u8>,
    position: usize,
}
impl ByteStream for Bytes {
    fn read<'a>(
        &'a mut self,
        buffer: &'a mut [u8],
        _: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<usize, HttpError>> {
        Box::pin(async move {
            let count = buffer.len().min(self.bytes.len() - self.position);
            buffer[..count].copy_from_slice(&self.bytes[self.position..self.position + count]);
            self.position += count;
            Ok(count)
        })
    }
}
type Reply = (String, Vec<(String, String)>);
#[derive(Default)]
struct Transport {
    requests: Mutex<Vec<HttpRequest>>,
    replies: Mutex<VecDeque<Reply>>,
}
impl Transport {
    fn new(body: &str, count: usize) -> Arc<Self> {
        Arc::new(Self {
            requests: Mutex::new(Vec::new()),
            replies: Mutex::new((0..count).map(|_| (body.into(), Vec::new())).collect()),
        })
    }
}
impl HttpTransport for Transport {
    fn send<'a>(
        &'a self,
        request: HttpRequest,
        cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<HttpResponse, HttpError>> {
        Box::pin(async move {
            if cancellation.is_cancelled() {
                return Err(HttpError::Cancelled);
            }
            self.requests.lock().unwrap().push(request);
            let (body, headers) = self
                .replies
                .lock()
                .unwrap()
                .pop_front()
                .expect("synthetic reply");
            Ok(HttpResponse {
                status: 200,
                reason: "OK".into(),
                headers,
                body: Box::new(Bytes {
                    bytes: body.into_bytes(),
                    position: 0,
                }),
            })
        })
    }
}
const CHAT: &str =
    "data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}]}\n\ndata: [DONE]\n\n";
const RESPONSES: &str = "data: {\"type\":\"response.completed\",\"response\":{\"id\":\"r\",\"output\":[],\"usage\":{\"input_tokens\":2,\"output_tokens\":3}}}\n\n";
fn options(key: Arc<Key>) -> CompatibleOptions {
    CompatibleOptions::new("test", "https://unit.test/v1", key)
}
fn request() -> Request {
    Request {
        model: "model".into(),
        max_output_tokens: 123,
        ..Default::default()
    }
}
async fn drain(stream: &mut dyn EventStream) -> Result<Vec<Event>, Error> {
    let mut events = Vec::new();
    while let Some(event) = stream.next(&Never).await? {
        events.push(event);
    }
    Ok(events)
}
fn header(request: &HttpRequest, name: &str) -> String {
    request
        .headers
        .iter()
        .find(|(key, _)| key == name)
        .map(|(_, value)| value.clone())
        .unwrap_or_default()
}

#[tokio::test]
async fn compatible_is_lazy_and_refreshes_key_for_each_request() {
    let key = Key::new("first");
    let transport = Transport::new(CHAT, 2);
    let provider =
        OpenAICompatibleProvider::new(options(key.clone()), transport.clone(), Arc::new(NoSocket))
            .unwrap();
    let mut first = provider.call(request()).unwrap();
    assert_eq!(key.calls.load(Ordering::SeqCst), 0);
    assert!(transport.requests.lock().unwrap().is_empty());
    let events = drain(first.as_mut()).await.unwrap();
    assert_eq!(events[0], Event::HttpRequestStarted);
    assert_eq!(events[1], Event::HttpResponseHeadersReceived);
    *key.value.lock().unwrap() = "second".into();
    drain(provider.call(request()).unwrap().as_mut())
        .await
        .unwrap();
    let requests = transport.requests.lock().unwrap();
    assert_eq!(
        requests[0].endpoint.as_str(),
        "https://unit.test/v1/chat/completions"
    );
    assert_eq!(header(&requests[0], "Authorization"), "Bearer first");
    assert_eq!(header(&requests[1], "Authorization"), "Bearer second");
    assert_eq!(key.calls.load(Ordering::SeqCst), 2);
}
#[tokio::test]
async fn session_ids_are_random_per_open_session_and_stable_per_direct_provider() {
    let transport = Transport::new(CHAT, 5);
    let mut opts = options(Key::new("key"));
    opts.session_header = "X-Session".into();
    let provider =
        OpenAICompatibleProvider::new(opts, transport.clone(), Arc::new(NoSocket)).unwrap();
    let one = provider.open_session().unwrap();
    let two = provider.open_session().unwrap();
    drain(one.call(request()).unwrap().as_mut()).await.unwrap();
    drain(one.call(request()).unwrap().as_mut()).await.unwrap();
    drain(two.call(request()).unwrap().as_mut()).await.unwrap();
    drain(provider.call(request()).unwrap().as_mut())
        .await
        .unwrap();
    drain(provider.call(request()).unwrap().as_mut())
        .await
        .unwrap();
    let ids: Vec<_> = transport
        .requests
        .lock()
        .unwrap()
        .iter()
        .map(|r| header(r, "X-Session"))
        .collect();
    assert_eq!(ids[0], ids[1]);
    assert_ne!(ids[0], ids[2]);
    assert_eq!(ids[3], ids[4]);
    assert_ne!(ids[0], ids[3]);
    assert!(ids.iter().all(|id| {
        id.len() == 32
            && id
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    }));
}
#[tokio::test]
async fn configured_preferences_replace_request_and_preserve_opaque_numbers() {
    let transport = Transport::new(CHAT, 1);
    let mut opts = options(Key::new("key"));
    opts.provider_preferences =
        "{\"huge\":123456789012345678901234567890,\"route\":\"configured\"}".into();
    opts.user_agent = "synthetic/1".into();
    opts.headers.push(("User-Agent".into(), "custom".into()));
    let provider =
        OpenAICompatibleProvider::new(opts, transport.clone(), Arc::new(NoSocket)).unwrap();
    let mut req = request();
    req.provider_preferences = "{\"route\":\"request\"}".into();
    drain(provider.call(req).unwrap().as_mut()).await.unwrap();
    let requests = transport.requests.lock().unwrap();
    let body = String::from_utf8_lossy(&requests[0].body);
    assert!(body.contains("123456789012345678901234567890"));
    assert!(body.contains("configured"));
    assert!(!body.contains("\"request\""));
    assert_eq!(header(&requests[0], "User-Agent"), "custom");
}
#[tokio::test]
async fn missing_api_key_stops_before_transport_but_after_encode() {
    let key = Key::new("");
    let transport = Transport::new(CHAT, 0);
    let provider =
        OpenAICompatibleProvider::new(options(key.clone()), transport.clone(), Arc::new(NoSocket))
            .unwrap();
    let error = drain(provider.call(request()).unwrap().as_mut())
        .await
        .unwrap_err();
    assert_eq!(error.to_string(), "provider: \"test\" has no API key");
    let mut invalid = request();
    invalid.tools.push(Tool {
        name: "tool".into(),
        parameters_json: "{bad".into(),
        ..Default::default()
    });
    assert!(matches!(
        drain(provider.call(invalid).unwrap().as_mut())
            .await
            .unwrap_err(),
        Error::Encode(_)
    ));
    assert_eq!(key.calls.load(Ordering::SeqCst), 1);
    assert!(transport.requests.lock().unwrap().is_empty());
}
#[test]
fn provider_validates_id_headers_endpoints_and_timeouts_without_credentials() {
    let key = Key::new("");
    for variant in 0..6 {
        let mut opts = options(key.clone());
        match variant {
            0 => opts.id = " ".into(),
            1 => opts.headers.push(("Authorization".into(), "secret".into())),
            2 => opts.base_url = "http://remote.test".into(),
            3 => opts.header_timeout_ms = -1,
            4 => opts.stream_idle_timeout_ms = -1,
            _ => opts.session_header = "bad\nname".into(),
        }
        assert!(
            OpenAICompatibleProvider::new(opts, Transport::new(CHAT, 0), Arc::new(NoSocket))
                .is_err()
        );
    }
    assert_eq!(key.calls.load(Ordering::SeqCst), 0);
}
#[tokio::test]
async fn request_byte_limit_is_actual_encoded_body_limit() {
    let transport = Transport::new(CHAT, 0);
    let mut opts = options(Key::new("key"));
    opts.maximum_request_bytes = 1;
    let provider =
        OpenAICompatibleProvider::new(opts, transport.clone(), Arc::new(NoSocket)).unwrap();
    assert!(matches!(
        drain(provider.call(request()).unwrap().as_mut())
            .await
            .unwrap_err(),
        Error::Http(HttpError::RequestTooLarge(1))
    ));
    assert!(transport.requests.lock().unwrap().is_empty());
}
#[tokio::test]
async fn chatgpt_has_fixed_headers_endpoints_and_omits_max_output_tokens() {
    let tokens = Tokens::new("oauth", "account");
    let transport = Transport::new(RESPONSES, 2);
    let provider = ChatGptProvider::new(
        ChatGptOptions::new(tokens.clone()),
        transport.clone(),
        Arc::new(NoSocket),
    )
    .unwrap();
    let mut stream = provider.call(request()).unwrap();
    assert_eq!(tokens.calls.load(Ordering::SeqCst), 0);
    drain(stream.as_mut()).await.unwrap();
    drain(provider.call(request()).unwrap().as_mut())
        .await
        .unwrap();
    let requests = transport.requests.lock().unwrap();
    let first = &requests[0];
    assert_eq!(
        first.endpoint.as_str(),
        "https://chatgpt.com/backend-api/codex/responses"
    );
    assert_eq!(header(first, "Authorization"), "Bearer oauth");
    assert_eq!(header(first, "originator"), "parrot");
    assert_eq!(header(first, "User-Agent"), "parrot");
    assert_eq!(header(first, "ChatGPT-Account-Id"), "account");
    assert_eq!(
        header(first, "session-id"),
        header(&requests[1], "session-id")
    );
    assert!(!String::from_utf8_lossy(&first.body).contains("max_output_tokens"));
    assert_eq!(tokens.calls.load(Ordering::SeqCst), 2);
}
#[tokio::test]
async fn chatgpt_requires_token_before_encoding_even_for_http_session() {
    let tokens = Tokens::new("", "");
    let transport = Transport::new(RESPONSES, 0);
    let provider = ChatGptProvider::new(
        ChatGptOptions::new(tokens.clone()),
        transport.clone(),
        Arc::new(NoSocket),
    )
    .unwrap();
    let mut invalid = request();
    invalid.tools.push(Tool {
        name: "tool".into(),
        parameters_json: "{bad".into(),
        ..Default::default()
    });
    for session in [false, true] {
        let mut stream = if session {
            provider
                .open_session()
                .unwrap()
                .call(invalid.clone())
                .unwrap()
        } else {
            provider.call(invalid.clone()).unwrap()
        };
        assert_eq!(
            drain(stream.as_mut()).await.unwrap_err().to_string(),
            "provider: ChatGPT credential requires an access token"
        );
    }
    assert_eq!(tokens.calls.load(Ordering::SeqCst), 2);
    assert!(transport.requests.lock().unwrap().is_empty());
}
#[tokio::test]
async fn response_session_keeps_turn_state_and_begin_turn_resets_it() {
    let transport = Transport::new(RESPONSES, 3);
    transport.replies.lock().unwrap()[0]
        .1
        .push(("x-codex-turn-state".into(), "state".into()));
    let mut opts = options(Key::new("key"));
    opts.protocol = Protocol::Responses;
    let provider =
        OpenAICompatibleProvider::new(opts, transport.clone(), Arc::new(NoSocket)).unwrap();
    let session = provider.open_session().unwrap();
    for index in 0..3 {
        if index == 2 {
            session.begin_turn().await.unwrap();
        }
        let events = drain(session.call(request()).unwrap().as_mut())
            .await
            .unwrap();
        assert_eq!(events[0], Event::HttpRequestStarted);
        assert_eq!(events[1], Event::HttpResponseHeadersReceived);
    }
    {
        let requests = transport.requests.lock().unwrap();
        assert_eq!(header(&requests[0], "x-codex-turn-state"), "");
        assert_eq!(header(&requests[1], "x-codex-turn-state"), "state");
        assert_eq!(header(&requests[2], "x-codex-turn-state"), "");
    }
    session.close().await.unwrap();
}
#[tokio::test]
async fn catalogs_delegate_standard_decoder_merge_and_optional_supplement() {
    let transport = Transport::new("{\"data\":[{\"id\":\"remote\"}]}", 1);
    transport.replies.lock().unwrap().push_back((
        "{\"data\":[{\"model_name\":\"remote\",\"model_info\":{\"max_input_tokens\":100}}]}".into(),
        Vec::new(),
    ));
    let mut opts = options(Key::new("key"));
    opts.models.push(Model::new("declared", "test"));
    let provider =
        OpenAICompatibleProvider::new(opts, transport.clone(), Arc::new(NoSocket)).unwrap();
    assert_eq!(provider.seed_models()[0].id, "declared");
    let models = provider.list_models(&Never).await.unwrap();
    assert!(models.iter().any(|m| m.id == "remote"));
    assert!(models.iter().any(|m| m.id == "declared"));
    let requests = transport.requests.lock().unwrap();
    assert_eq!(requests[0].endpoint.as_str(), "https://unit.test/v1/models");
    assert_eq!(
        requests[1].endpoint.as_str(),
        "https://unit.test/v1/model/info"
    );
}
#[tokio::test]
async fn chatgpt_quota_preserves_raw_balance_and_uses_auth_without_session() {
    let transport = Transport::new(
        "{\"plan_type\":\"pro\",\"rate_limit\":{\"primary_window\":{\"used_percent\":\"12.5\",\"reset_at\":0,\"limit_window_seconds\":3600}},\"credits\":{\"has_credits\":true,\"balance\":123456789012345678901234567890}}",
        1,
    );
    let provider = ChatGptProvider::new(
        ChatGptOptions::new(Tokens::new("token", "")),
        transport.clone(),
        Arc::new(NoSocket),
    )
    .unwrap();
    let usage = provider
        .usage_reporter()
        .unwrap()
        .usage(&Never)
        .await
        .unwrap();
    assert_eq!(usage.plan_type, "pro");
    assert_eq!(
        usage.credits.unwrap().balance,
        "123456789012345678901234567890"
    );
    let primary = usage.primary_window.unwrap();
    assert_eq!(primary.used_percent, 12.5);
    assert_eq!(primary.reset_at, "1970-01-01T00:00:00+00:00");
    assert_eq!(primary.limit_window_seconds, 3600);
    let requests = transport.requests.lock().unwrap();
    assert_eq!(
        requests[0].endpoint.as_str(),
        "https://chatgpt.com/backend-api/wham/usage"
    );
    assert_eq!(header(&requests[0], "ChatGPT-Account-Id"), "");
    assert_eq!(header(&requests[0], "session-id"), "");
}
#[test]
fn configured_image_calculator_and_chatgpt_reuse_t33_calculation() {
    struct Custom;
    impl ImageTokenCalculator for Custom {
        fn calculate(&self, _: &Model, _: &Content) -> i64 {
            17
        }
    }
    let mut opts = options(Key::new("key"));
    opts.image_token_calculator = Arc::new(Custom);
    let compatible =
        OpenAICompatibleProvider::new(opts, Transport::new(CHAT, 0), Arc::new(NoSocket)).unwrap();
    let chatgpt = ChatGptProvider::new(
        ChatGptOptions::new(Tokens::new("token", "")),
        Transport::new(RESPONSES, 0),
        Arc::new(NoSocket),
    )
    .unwrap();
    let model = Model::new("gpt-4o", "chatgpt");
    let image = Content::image_link("", "https://unit.test/image", "image/png", 512, 512);
    assert_eq!(compatible.calculate_image_tokens(&model, &image), 17);
    assert_eq!(
        chatgpt.calculate_image_tokens(&model, &image),
        parrot_provider::image_inspection::calculate_image_tokens("gpt-4o", 512, 512)
    );
}

// Exact source method/argument ports from OpenAICompatibleProviderAuthTests.cs.
struct RecordingKeys {
    keys: Mutex<VecDeque<String>>,
    calls: AtomicUsize,
}
impl RecordingKeys {
    fn new(keys: &[&str]) -> Arc<Self> {
        Arc::new(Self {
            keys: Mutex::new(keys.iter().map(|key| (*key).into()).collect()),
            calls: AtomicUsize::new(0),
        })
    }
}
impl ApiKeySource for RecordingKeys {
    fn api_key<'a>(&'a self, _: &'a dyn Cancellation) -> BoxFuture<'a, Result<String, Error>> {
        Box::pin(async {
            self.calls.fetch_add(1, Ordering::SeqCst);
            Ok(self
                .keys
                .lock()
                .unwrap()
                .pop_front()
                .expect("source credential sequence"))
        })
    }
    fn has_credential<'a>(&'a self, _: &'a dyn Cancellation) -> BoxFuture<'a, Result<bool, Error>> {
        Box::pin(async { Ok(true) })
    }
}
#[derive(Default)]
struct CallerCancellation {
    cancelled: std::sync::atomic::AtomicBool,
    notify: tokio::sync::Notify,
}
impl Cancellation for CallerCancellation {
    fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::SeqCst)
    }
    fn cancelled(&self) -> BoxFuture<'_, ()> {
        Box::pin(async move {
            loop {
                let notified = self.notify.notified();
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
enum AuthReply {
    Response(u16, &'static str),
    Failure(HttpError, Option<Arc<CallerCancellation>>),
}
struct AuthTransport {
    replies: Mutex<VecDeque<AuthReply>>,
    requests: Mutex<Vec<HttpRequest>>,
}
impl AuthTransport {
    fn new(replies: impl IntoIterator<Item = AuthReply>) -> Arc<Self> {
        Arc::new(Self {
            replies: Mutex::new(replies.into_iter().collect()),
            requests: Mutex::new(Vec::new()),
        })
    }
    fn paths(&self) -> String {
        self.requests
            .lock()
            .unwrap()
            .iter()
            .map(|request| request.endpoint.as_url().path())
            .collect::<Vec<_>>()
            .join("|")
    }
}
impl HttpTransport for AuthTransport {
    fn send<'a>(
        &'a self,
        request: HttpRequest,
        _: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<HttpResponse, HttpError>> {
        Box::pin(async move {
            self.requests.lock().unwrap().push(request);
            match self
                .replies
                .lock()
                .unwrap()
                .pop_front()
                .expect("source response sequence")
            {
                AuthReply::Response(status, body) => Ok(HttpResponse {
                    status,
                    reason: String::new(),
                    headers: Vec::new(),
                    body: Box::new(Bytes {
                        bytes: body.as_bytes().to_vec(),
                        position: 0,
                    }),
                }),
                AuthReply::Failure(error, caller) => {
                    if let Some(caller) = caller {
                        caller.cancelled.store(true, Ordering::SeqCst);
                        caller.notify.notify_waiters();
                    }
                    Err(error)
                }
            }
        })
    }
}
fn auth_provider(
    base_url: &str,
    source: Arc<RecordingKeys>,
    transport: Arc<AuthTransport>,
) -> OpenAICompatibleProvider {
    let mut options = CompatibleOptions::new("configured", base_url, source);
    options.allow_insecure_remote = base_url.starts_with("http:");
    OpenAICompatibleProvider::new(options, transport, Arc::new(NoSocket)).unwrap()
}
fn auth_request() -> Request {
    Request {
        model: "model-a".into(),
        messages: vec![parrot_provider::sessions::Message::user("hello")],
        ..Default::default()
    }
}
const SOURCE_CHAT: &str = "data: {\"choices\":[{\"index\":0,\"finish_reason\":\"stop\",\"delta\":{}}],\"usage\":{\"prompt_tokens\":1,\"completion_tokens\":1}}\n\ndata: [DONE]\n\n";
const SOURCE_RESPONSES: &str = "data: {\"type\":\"response.completed\",\"response\":{\"usage\":{\"input_tokens\":1,\"output_tokens\":1}}}\n\n";
const SOURCE_COMPLETE_MODEL: &str = r#"{"data":[{"id":"model-a","context_window":2,"max_input_tokens":1,"max_output_tokens":1,"input_cost_per_token":0,"cache_read_input_token_cost":0,"output_cost_per_token":0,"supports_function_calling":true,"supports_reasoning":false,"supported_output_modalities":["text"],"supported_reasoning_efforts":[]}]}"#;
const SOURCE_SERVED_MODEL: &str = r#"{"data":[{"id":"served"}]}"#;

#[tokio::test]
async fn api_key_is_resolved_for_each_request_and_missing_keys_fail_before_http() {
    let source = RecordingKeys::new(&["first-key", "second-key", "", ""]);
    let transport = AuthTransport::new([
        AuthReply::Response(200, SOURCE_COMPLETE_MODEL),
        AuthReply::Response(200, SOURCE_CHAT),
    ]);
    let provider = auth_provider("https://example.test/v1", source, transport.clone());
    let listed = provider.list_models(&Never).await.unwrap();
    let events = drain(provider.call(auth_request()).unwrap().as_mut())
        .await
        .unwrap();
    assert_eq!(listed.len(), 1);
    assert!(matches!(events.last(), Some(Event::Completed(_))));
    {
        let requests = transport.requests.lock().unwrap();
        assert_eq!(header(&requests[0], "Authorization"), "Bearer first-key");
        assert_eq!(header(&requests[1], "Authorization"), "Bearer second-key");
    }
    assert!(matches!(
        provider.list_models(&Never).await,
        Err(Error::Provider(_))
    ));
    assert!(matches!(
        drain(provider.call(auth_request()).unwrap().as_mut()).await,
        Err(Error::Provider(_))
    ));
    assert_eq!(transport.requests.lock().unwrap().len(), 2);
}

#[tokio::test]
async fn caller_cancellation_during_model_info_propagates() {
    let caller = Arc::new(CallerCancellation::default());
    let transport = AuthTransport::new([
        AuthReply::Response(200, SOURCE_SERVED_MODEL),
        AuthReply::Failure(HttpError::Cancelled, Some(caller.clone())),
    ]);
    let provider = auth_provider(
        "https://example.test/v1",
        RecordingKeys::new(&["key"]),
        transport.clone(),
    );
    assert!(matches!(
        provider.list_models(caller.as_ref()).await,
        Err(Error::Cancelled)
    ));
    assert_eq!(transport.requests.lock().unwrap().len(), 2);
}

#[tokio::test]
async fn complete_models_metadata_does_not_probe_model_info() {
    const BODY: &str = r#"{"data":[{"id":"complete","context_window":0,"max_input_tokens":0,"max_output_tokens":0,"input_cost_per_token":0,"cache_read_input_token_cost":0,"output_cost_per_token":0,"supports_function_calling":false,"supports_reasoning":false,"supported_output_modalities":[],"supported_reasoning_efforts":[]}]}"#;
    let transport = AuthTransport::new([AuthReply::Response(200, BODY)]);
    let provider = auth_provider(
        "https://example.test/v1",
        RecordingKeys::new(&["key"]),
        transport.clone(),
    );
    let listed = provider.list_models(&Never).await.unwrap();
    assert_eq!(listed.len(), 1);
    assert_eq!(transport.paths(), "/v1/models");
}

async fn exact_missing_metadata(base_url: &str, expected_paths: &str) {
    const MODELS: &str = r#"{"data":[{"id":"served","max_output_tokens":0,"supports_function_calling":false},{"id":"primary-only"}]}"#;
    const INFO: &str = r#"{"data":[{"model_name":"served","model_info":{"max_input_tokens":512,"max_output_tokens":64,"supports_function_calling":true,"supports_reasoning":true,"reasoning_effort_levels":["low","high"],"default_reasoning_effort":"high"}},{"model_name":"info-only","model_info":{"max_input_tokens":999}}]}"#;
    let transport = AuthTransport::new([
        AuthReply::Response(200, MODELS),
        AuthReply::Response(200, INFO),
    ]);
    let source = RecordingKeys::new(&["one-key"]);
    let mut options = CompatibleOptions::new("configured", base_url, source.clone());
    options.allow_insecure_remote = base_url.starts_with("http:");
    options.headers = vec![("X-Tenant".into(), "tenant".into())];
    let provider =
        OpenAICompatibleProvider::new(options, transport.clone(), Arc::new(NoSocket)).unwrap();
    let listed = provider.list_models(&Never).await.unwrap();
    let served = listed.iter().find(|model| model.id == "served").unwrap();
    assert_eq!(transport.paths(), expected_paths);
    {
        let requests = transport.requests.lock().unwrap();
        assert_eq!(
            requests
                .iter()
                .map(|r| header(r, "Authorization"))
                .collect::<Vec<_>>()
                .join("|"),
            "Bearer one-key|Bearer one-key"
        );
        assert_eq!(
            requests
                .iter()
                .map(|r| header(r, "X-Tenant"))
                .collect::<Vec<_>>()
                .join("|"),
            "tenant|tenant"
        );
    }
    assert_eq!(source.calls.load(Ordering::SeqCst), 1);
    assert_eq!(
        listed
            .iter()
            .map(|m| m.id.as_str())
            .collect::<Vec<_>>()
            .join(","),
        "primary-only,served"
    );
    assert_eq!(served.context_window, 512);
    assert_eq!(served.max_input_tokens, 512);
    assert_eq!(served.max_output_tokens, 0);
    assert!(!served.capabilities.tools);
    assert!(served.capabilities.reasoning);
    assert_eq!(
        served
            .capabilities
            .variants
            .iter()
            .map(|v| v.name.as_str())
            .collect::<Vec<_>>()
            .join(","),
        "high,low"
    );
}
#[tokio::test]
async fn missing_metadata_fetches_sibling_model_info_with_one_header_snapshot_https_v1() {
    exact_missing_metadata("https://example.test/v1", "/v1/models|/v1/model/info").await;
}
#[tokio::test]
async fn missing_metadata_fetches_sibling_model_info_with_one_header_snapshot_http_4000() {
    exact_missing_metadata("http://example.test:4000", "/models|/model/info").await;
}

async fn exact_optional_failure(status: u16, body: &'static str) {
    let transport = AuthTransport::new([
        AuthReply::Response(200, SOURCE_SERVED_MODEL),
        AuthReply::Response(status, body),
    ]);
    let provider = auth_provider(
        "https://example.test/v1",
        RecordingKeys::new(&["key"]),
        transport.clone(),
    );
    let listed = provider.list_models(&Never).await.unwrap();
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].id, "served");
    assert_eq!(transport.paths(), "/v1/models|/v1/model/info");
}
#[tokio::test]
async fn optional_model_info_failure_keeps_models_refresh_not_found_missing() {
    exact_optional_failure(404, "missing").await;
}
#[tokio::test]
async fn optional_model_info_failure_keeps_models_refresh_unauthorized_unauthorized() {
    exact_optional_failure(401, "unauthorized").await;
}
#[tokio::test]
async fn optional_model_info_failure_keeps_models_refresh_internal_server_error_broken() {
    exact_optional_failure(500, "broken").await;
}
#[tokio::test]
async fn optional_model_info_failure_keeps_models_refresh_ok_not_json() {
    exact_optional_failure(200, "not json").await;
}
#[tokio::test]
async fn optional_model_info_failure_keeps_models_refresh_ok_empty_object() {
    exact_optional_failure(200, "{}").await;
}

async fn exact_transport_failure(timeout: bool) {
    let failure = if timeout {
        HttpError::Cancelled
    } else {
        HttpError::Transport("unavailable".into())
    };
    let transport = AuthTransport::new([
        AuthReply::Response(200, SOURCE_SERVED_MODEL),
        AuthReply::Failure(failure, None),
    ]);
    let provider = auth_provider(
        "https://example.test/v1",
        RecordingKeys::new(&["key"]),
        transport.clone(),
    );
    let listed = provider.list_models(&Never).await.unwrap();
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].id, "served");
    assert_eq!(transport.requests.lock().unwrap().len(), 2);
}
#[tokio::test]
async fn optional_model_info_transport_or_timeout_failure_keeps_models_refresh_false() {
    exact_transport_failure(false).await;
}
#[tokio::test]
async fn optional_model_info_transport_or_timeout_failure_keeps_models_refresh_true() {
    exact_transport_failure(true).await;
}

#[tokio::test]
async fn non_success_responses_preserve_their_body() {
    const BODY: &str =
        r#"{"error":{"type":"invalid_request","code":"bad","message":"broken"},"trace":"abc"}"#;
    let transport = AuthTransport::new([AuthReply::Response(400, BODY)]);
    let provider = auth_provider(
        "https://example.test/v1",
        RecordingKeys::new(&["key"]),
        transport,
    );
    let error = drain(provider.call(auth_request()).unwrap().as_mut())
        .await
        .unwrap_err();
    let Error::Http(HttpError::Response(failure)) = error else {
        panic!("expected structured ProviderHttpException equivalent");
    };
    assert_eq!(failure.status, 400);
    assert_eq!(failure.response_body, BODY);
    assert_eq!(failure.error_type, "invalid_request");
    assert_eq!(failure.error_code, "bad");
    assert_eq!(failure.detail, "broken");
}

fn exact_endpoint(base_url: &str, allow_remote: bool, allow_local: bool, accepted: bool) {
    let mut options = CompatibleOptions::new("configured", base_url, RecordingKeys::new(&["key"]));
    options.allow_insecure_remote = allow_remote;
    options.allow_insecure_localhost = allow_local;
    let provider =
        OpenAICompatibleProvider::new(options, AuthTransport::new([]), Arc::new(NoSocket));
    if accepted {
        assert_eq!(provider.unwrap().id(), "configured");
    } else {
        assert!(matches!(provider, Err(Error::Http(_))));
    }
}
#[test]
fn provider_endpoint_requires_an_explicit_plaintext_exception_https_false_false_true() {
    exact_endpoint("https://example.test/v1", false, false, true);
}
#[test]
fn provider_endpoint_requires_an_explicit_plaintext_exception_http_remote_false_false_false() {
    exact_endpoint("http://example.test/v1", false, false, false);
}
#[test]
fn provider_endpoint_requires_an_explicit_plaintext_exception_http_remote_true_false_true() {
    exact_endpoint("http://example.test/v1", true, false, true);
}
#[test]
fn provider_endpoint_requires_an_explicit_plaintext_exception_http_loopback_false_false_false() {
    exact_endpoint("http://127.0.0.1:8000/v1", false, false, false);
}
#[test]
fn provider_endpoint_requires_an_explicit_plaintext_exception_http_loopback_false_true_true() {
    exact_endpoint("http://127.0.0.1:8000/v1", false, true, true);
}

async fn exact_session_header(protocol: Protocol, session_header: &str) {
    let body = match protocol {
        Protocol::Responses => SOURCE_RESPONSES,
        Protocol::ChatCompletions => SOURCE_CHAT,
    };
    let transport = AuthTransport::new([
        AuthReply::Response(200, body),
        AuthReply::Response(200, body),
        AuthReply::Response(200, body),
    ]);
    let mut options = CompatibleOptions::new(
        "configured",
        "https://example.test/v1",
        RecordingKeys::new(&["key", "key", "key"]),
    );
    options.protocol = protocol;
    options.session_header = session_header.into();
    let provider =
        OpenAICompatibleProvider::new(options, transport.clone(), Arc::new(NoSocket)).unwrap();
    let first = provider.open_session().unwrap();
    let second = provider.open_session().unwrap();
    drain(first.call(auth_request()).unwrap().as_mut())
        .await
        .unwrap();
    drain(first.call(auth_request()).unwrap().as_mut())
        .await
        .unwrap();
    drain(second.call(auth_request()).unwrap().as_mut())
        .await
        .unwrap();
    {
        let requests = transport.requests.lock().unwrap();
        let session_headers: Vec<_> = requests
            .iter()
            .map(|r| header(r, "x-opencode-session"))
            .collect();
        assert_eq!(requests.len(), 3);
        let expected_agent = format!("parrot/{}", env!("CARGO_PKG_VERSION"));
        assert_eq!(
            requests
                .iter()
                .map(|r| header(r, "User-Agent"))
                .collect::<Vec<_>>()
                .join("|"),
            [expected_agent.as_str(); 3].join("|")
        );
        if session_header.is_empty() {
            assert_eq!(session_headers.join("|"), "||");
        } else {
            assert_eq!(session_headers[0].len(), 32);
            assert_eq!(session_headers[1], session_headers[0]);
            assert_eq!(session_headers[2].len(), 32);
            assert_ne!(session_headers[2], session_headers[0]);
        }
    }
    first.close().await.unwrap();
    second.close().await.unwrap();
}
#[tokio::test]
async fn session_header_is_stable_per_session_and_user_agent_identifies_parrot_chat_session() {
    exact_session_header(Protocol::ChatCompletions, "x-opencode-session").await;
}
#[tokio::test]
async fn session_header_is_stable_per_session_and_user_agent_identifies_parrot_responses_session() {
    exact_session_header(Protocol::Responses, "x-opencode-session").await;
}
#[tokio::test]
async fn session_header_is_stable_per_session_and_user_agent_identifies_parrot_chat_no_header() {
    exact_session_header(Protocol::ChatCompletions, "").await;
}

struct PendingCredential {
    calls: AtomicUsize,
    active: Arc<AtomicUsize>,
    dropped: Arc<AtomicUsize>,
    released: std::sync::atomic::AtomicBool,
    changed: tokio::sync::Notify,
}
impl PendingCredential {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            calls: AtomicUsize::new(0),
            active: Arc::new(AtomicUsize::new(0)),
            dropped: Arc::new(AtomicUsize::new(0)),
            released: std::sync::atomic::AtomicBool::new(false),
            changed: tokio::sync::Notify::new(),
        })
    }
    fn release(&self) {
        self.released.store(true, Ordering::SeqCst);
        self.changed.notify_waiters();
    }
}
struct CredentialGuard {
    active: Arc<AtomicUsize>,
    dropped: Arc<AtomicUsize>,
}
impl Drop for CredentialGuard {
    fn drop(&mut self) {
        self.active.fetch_sub(1, Ordering::SeqCst);
        self.dropped.fetch_add(1, Ordering::SeqCst);
    }
}
impl ApiKeySource for PendingCredential {
    fn api_key<'a>(&'a self, _: &'a dyn Cancellation) -> BoxFuture<'a, Result<String, Error>> {
        Box::pin(async move {
            self.calls.fetch_add(1, Ordering::SeqCst);
            self.active.fetch_add(1, Ordering::SeqCst);
            let _guard = CredentialGuard {
                active: self.active.clone(),
                dropped: self.dropped.clone(),
            };
            loop {
                let changed = self.changed.notified();
                tokio::pin!(changed);
                changed.as_mut().enable();
                if self.released.load(Ordering::SeqCst) {
                    return Ok("pending-key".into());
                }
                changed.await;
            }
        })
    }
    fn has_credential<'a>(&'a self, _: &'a dyn Cancellation) -> BoxFuture<'a, Result<bool, Error>> {
        Box::pin(async { Ok(true) })
    }
}
async fn poll_pending<T>(future: std::pin::Pin<&mut impl std::future::Future<Output = T>>) {
    let mut future = future;
    std::future::poll_fn(|context| {
        assert!(future.as_mut().poll(context).is_pending());
        std::task::Poll::Ready(())
    })
    .await;
}

#[tokio::test]
async fn dropped_http_initialization_observer_resumes_one_credential_attempt() {
    for session_call in [false, true] {
        let credentials = PendingCredential::new();
        let transport = Transport::new(CHAT, 1);
        let provider = OpenAICompatibleProvider::new(
            CompatibleOptions::new("test", "https://unit.test/v1", credentials.clone()),
            transport.clone(),
            Arc::new(NoSocket),
        )
        .unwrap();
        let session = provider.open_session().unwrap();
        let mut stream = if session_call {
            session.call(request()).unwrap()
        } else {
            provider.call(request()).unwrap()
        };
        {
            let observer = stream.next(&Never);
            tokio::pin!(observer);
            poll_pending(observer.as_mut()).await;
        }
        assert_eq!(credentials.calls.load(Ordering::SeqCst), 1);
        assert_eq!(credentials.active.load(Ordering::SeqCst), 1);
        assert_eq!(credentials.dropped.load(Ordering::SeqCst), 0);
        {
            let observer = stream.next(&Never);
            tokio::pin!(observer);
            poll_pending(observer.as_mut()).await;
            assert_eq!(credentials.calls.load(Ordering::SeqCst), 1);
            credentials.release();
            assert_eq!(observer.await.unwrap(), Some(Event::HttpRequestStarted));
        }
        drain(stream.as_mut()).await.unwrap();
        assert_eq!(credentials.calls.load(Ordering::SeqCst), 1);
        assert_eq!(credentials.active.load(Ordering::SeqCst), 0);
        assert_eq!(credentials.dropped.load(Ordering::SeqCst), 1);
        assert_eq!(transport.requests.lock().unwrap().len(), 1);
        session.close().await.unwrap();
    }
}

#[tokio::test]
async fn pending_http_initialization_cancels_closes_and_drops_without_detached_work() {
    for cleanup in ["cancel", "close", "drop"] {
        let credentials = PendingCredential::new();
        let transport = Transport::new(CHAT, 0);
        let provider = OpenAICompatibleProvider::new(
            CompatibleOptions::new("test", "https://unit.test/v1", credentials.clone()),
            transport.clone(),
            Arc::new(NoSocket),
        )
        .unwrap();
        let mut stream = provider.call(request()).unwrap();
        let caller = CallerCancellation::default();
        {
            let observer = stream.next(&caller);
            tokio::pin!(observer);
            poll_pending(observer.as_mut()).await;
            if cleanup == "cancel" {
                caller.cancelled.store(true, Ordering::SeqCst);
                caller.notify.notify_waiters();
                assert!(matches!(observer.await, Err(Error::Cancelled)));
            }
        }
        if cleanup == "close" {
            stream.close().await.unwrap();
        }
        drop(stream);
        assert_eq!(credentials.calls.load(Ordering::SeqCst), 1, "{cleanup}");
        assert_eq!(credentials.active.load(Ordering::SeqCst), 0, "{cleanup}");
        assert_eq!(credentials.dropped.load(Ordering::SeqCst), 1, "{cleanup}");
        assert!(transport.requests.lock().unwrap().is_empty());
    }
}

#[derive(Default)]
struct AuthDiagnosticSink(Mutex<Vec<parrot_provider::sessions::diagnostics::DiagnosticEvent>>);
impl parrot_provider::sessions::diagnostics::DiagnosticSink for AuthDiagnosticSink {
    fn write(&self, event: parrot_provider::sessions::diagnostics::DiagnosticEvent) {
        self.0.lock().unwrap().push(event);
    }
}
struct CancelledCredential {
    calls: AtomicUsize,
}
impl ApiKeySource for CancelledCredential {
    fn api_key<'a>(&'a self, _: &'a dyn Cancellation) -> BoxFuture<'a, Result<String, Error>> {
        Box::pin(async {
            self.calls.fetch_add(1, Ordering::SeqCst);
            Err(Error::Cancellation("AUTH_SECRET_CANARY".into()))
        })
    }
    fn has_credential<'a>(&'a self, _: &'a dyn Cancellation) -> BoxFuture<'a, Result<bool, Error>> {
        Box::pin(async { Ok(true) })
    }
}
async fn websocket_auth_failure(provider: Arc<dyn Provider>, expected: Error, expected_code: &str) {
    use parrot_provider::sessions::{
        Session, decorators::RetryingProvider, diagnostic_session::DiagnosticSession,
        diagnostics::MonotonicDiagnosticClock,
    };
    let retrying = RetryingProvider::new(provider, 2);
    let session = retrying.open_session().unwrap();
    let sink = Arc::new(AuthDiagnosticSink::default());
    let diagnostic = DiagnosticSession::new(
        session,
        sink.clone(),
        Arc::new(MonotonicDiagnosticClock::default()),
        "agent".into(),
        retrying.id().into(),
        None,
    );
    let mut stream = diagnostic.call(request()).unwrap();
    assert_eq!(stream.next(&Never).await.unwrap_err(), expected);
    assert_eq!(stream.next(&Never).await.unwrap(), None);
    stream.close().await.unwrap();
    diagnostic.close().await.unwrap();
    let records = sink.0.lock().unwrap();
    assert!(!format!("{records:?}").contains("AUTH_SECRET_CANARY"));
    assert!(!records.iter().any(|event| event.operation == "retry"));
    let request_terminals: Vec<_> = records
        .iter()
        .filter(|event| event.operation == "request_finished")
        .collect();
    assert_eq!(request_terminals.len(), 1);
    assert_eq!(
        request_terminals[0].error_code.as_deref(),
        Some(expected_code)
    );
    let call_terminals: Vec<_> = records
        .iter()
        .filter(|event| event.operation == "call_finished")
        .collect();
    assert_eq!(call_terminals.len(), 1);
    assert_eq!(call_terminals[0].error_code.as_deref(), Some(expected_code));
}
#[tokio::test(start_paused = true)]
async fn websocket_missing_api_key_returns_original_without_retry_or_secret_diagnostics() {
    let credentials = Key::new("");
    let transport = Transport::new(RESPONSES, 0);
    let mut options = options(credentials.clone());
    options.protocol = Protocol::Responses;
    options.disable_websocket = false;
    let provider = Arc::new(
        OpenAICompatibleProvider::new(options, transport.clone(), Arc::new(NoSocket)).unwrap(),
    );
    websocket_auth_failure(
        provider,
        Error::Provider("provider: \"test\" has no API key".into()),
        "unexpected",
    )
    .await;
    assert_eq!(credentials.calls.load(Ordering::SeqCst), 1);
    assert!(transport.requests.lock().unwrap().is_empty());
}
#[tokio::test(start_paused = true)]
async fn websocket_missing_oauth_returns_original_without_retry_or_secret_diagnostics() {
    let credentials = Tokens::new("", "");
    let transport = Transport::new(RESPONSES, 0);
    let mut options = ChatGptOptions::new(credentials.clone());
    options.disable_websocket = false;
    let provider =
        Arc::new(ChatGptProvider::new(options, transport.clone(), Arc::new(NoSocket)).unwrap());
    websocket_auth_failure(
        provider,
        Error::Provider("provider: ChatGPT credential requires an access token".into()),
        "unexpected",
    )
    .await;
    assert_eq!(credentials.calls.load(Ordering::SeqCst), 1);
    assert!(transport.requests.lock().unwrap().is_empty());
}
#[tokio::test(start_paused = true)]
async fn websocket_credential_cancellation_returns_original_without_retry_or_secret_diagnostics() {
    let credentials = Arc::new(CancelledCredential {
        calls: AtomicUsize::new(0),
    });
    let transport = Transport::new(RESPONSES, 0);
    let mut options = CompatibleOptions::new("test", "https://unit.test/v1", credentials.clone());
    options.protocol = Protocol::Responses;
    options.disable_websocket = false;
    let provider = Arc::new(
        OpenAICompatibleProvider::new(options, transport.clone(), Arc::new(NoSocket)).unwrap(),
    );
    websocket_auth_failure(
        provider,
        Error::Cancellation("AUTH_SECRET_CANARY".into()),
        "cancelled",
    )
    .await;
    assert_eq!(credentials.calls.load(Ordering::SeqCst), 1);
    assert!(transport.requests.lock().unwrap().is_empty());
}

#[test]
fn chatgpt_usage_reporter_identity_is_retained_and_forwarded_by_retry() {
    use parrot_provider::sessions::decorators::RetryingProvider;
    let tokens = Tokens::new("unused", "");
    let transport = Transport::new(RESPONSES, 0);
    let provider = Arc::new(
        ChatGptProvider::new(
            ChatGptOptions::new(tokens.clone()),
            transport.clone(),
            Arc::new(NoSocket),
        )
        .unwrap(),
    );
    let reporter = provider.usage_reporter().unwrap();
    assert!(Arc::ptr_eq(&reporter, &provider.usage_reporter().unwrap()));
    let retrying = RetryingProvider::new(provider, 2);
    assert!(Arc::ptr_eq(&reporter, &retrying.usage_reporter().unwrap()));
    assert!(Arc::ptr_eq(
        &retrying.usage_reporter().unwrap(),
        &retrying.usage_reporter().unwrap()
    ));
    assert_eq!(tokens.calls.load(Ordering::SeqCst), 0);
    assert!(transport.requests.lock().unwrap().is_empty());
}

#[tokio::test]
async fn already_cancelled_ready_key_preserves_first_start_for_direct_and_stateless_calls() {
    for session_call in [false, true] {
        let credentials = Key::new("key");
        let transport = Transport::new(CHAT, 0);
        let provider = OpenAICompatibleProvider::new(
            options(credentials.clone()),
            transport.clone(),
            Arc::new(NoSocket),
        )
        .unwrap();
        let session = provider.open_session().unwrap();
        let mut stream = if session_call {
            session.call(request()).unwrap()
        } else {
            provider.call(request()).unwrap()
        };
        let caller = CallerCancellation::default();
        caller.cancelled.store(true, Ordering::SeqCst);
        assert_eq!(
            stream.next(&caller).await.unwrap(),
            Some(Event::HttpRequestStarted)
        );
        assert_eq!(credentials.calls.load(Ordering::SeqCst), 1);
        assert!(transport.requests.lock().unwrap().is_empty());
        assert_eq!(stream.next(&caller).await.unwrap_err(), Error::Cancelled);
        assert_eq!(credentials.calls.load(Ordering::SeqCst), 1);
        assert!(transport.requests.lock().unwrap().is_empty());
        stream.close().await.unwrap();
        session.close().await.unwrap();
    }
}

#[derive(Default)]
struct CancellationAwareCredential {
    calls: AtomicUsize,
    cancelled_seen: std::sync::atomic::AtomicBool,
}
impl ApiKeySource for CancellationAwareCredential {
    fn api_key<'a>(
        &'a self,
        cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<String, Error>> {
        Box::pin(async move {
            self.calls.fetch_add(1, Ordering::SeqCst);
            self.cancelled_seen
                .store(cancellation.is_cancelled(), Ordering::SeqCst);
            if cancellation.is_cancelled() {
                Err(Error::Cancelled)
            } else {
                Ok("key".into())
            }
        })
    }
    fn has_credential<'a>(&'a self, _: &'a dyn Cancellation) -> BoxFuture<'a, Result<bool, Error>> {
        Box::pin(async { Ok(true) })
    }
}
#[tokio::test]
async fn already_cancelled_aware_key_observes_cancellation_for_direct_and_stateless_calls() {
    for session_call in [false, true] {
        let credentials = Arc::new(CancellationAwareCredential::default());
        let transport = Transport::new(CHAT, 0);
        let provider = OpenAICompatibleProvider::new(
            CompatibleOptions::new("test", "https://unit.test/v1", credentials.clone()),
            transport.clone(),
            Arc::new(NoSocket),
        )
        .unwrap();
        let session = provider.open_session().unwrap();
        let mut stream = if session_call {
            session.call(request()).unwrap()
        } else {
            provider.call(request()).unwrap()
        };
        let caller = CallerCancellation::default();
        caller.cancelled.store(true, Ordering::SeqCst);
        assert_eq!(stream.next(&caller).await.unwrap_err(), Error::Cancelled);
        assert_eq!(credentials.calls.load(Ordering::SeqCst), 1);
        assert!(credentials.cancelled_seen.load(Ordering::SeqCst));
        assert!(transport.requests.lock().unwrap().is_empty());
        assert_eq!(stream.next(&caller).await.unwrap(), None);
        assert_eq!(credentials.calls.load(Ordering::SeqCst), 1);
        stream.close().await.unwrap();
        session.close().await.unwrap();
    }
}

fn compatible_catalog_input(case_id: &str) -> serde_json::Value {
    let inputs: serde_json::Value = serde_json::from_str(include_str!(
        "../routing/fixtures/compatible-catalog-origin-v1/inputs.json"
    ))
    .unwrap();
    inputs["cases"]
        .as_array()
        .unwrap()
        .iter()
        .find(|case| case["case_id"] == case_id)
        .unwrap()
        .clone()
}

async fn assert_compatible_catalog_shape(case_id: &str, category: &str) {
    let input = compatible_catalog_input(case_id);
    let id = input["provider_id"].as_str().unwrap();
    let body = input["live_body"].as_str().unwrap();
    let opts = CompatibleOptions::new(
        id,
        input["base_url"].as_str().unwrap(),
        Key::new(input["credential"].as_str().unwrap()),
    );
    let provider =
        OpenAICompatibleProvider::new(opts, Transport::new(body, 2), Arc::new(NoSocket)).unwrap();
    let actual = provider.list_models(&Never).await;
    match StandardModelDecoder::decode_typed(id, body) {
        Ok(models) => {
            assert_eq!(category, "success");
            assert_eq!(actual.unwrap(), models);
        }
        Err(error) => {
            let message = error.to_string();
            let expected = match category {
                "encode" => {
                    assert!(matches!(error, StandardModelError::Json(_)));
                    Error::Encode(message)
                }
                "invalid_operation" => {
                    assert!(matches!(error, StandardModelError::InvalidOperation(_)));
                    Error::InvalidOperation(message)
                }
                "provider" => {
                    assert!(matches!(error, StandardModelError::Provider(_)));
                    Error::Provider(message)
                }
                _ => panic!("unexpected category"),
            };
            assert_eq!(actual.unwrap_err(), expected, "{case_id}");
        }
    }
}

macro_rules! compatible_catalog_shape {
    ($name:ident, $case:literal, $category:literal) => {
        #[tokio::test]
        async fn $name() {
            assert_compatible_catalog_shape($case, $category).await;
        }
    };
}
compatible_catalog_shape!(compatible_catalog_empty_data, "empty_data", "provider");
compatible_catalog_shape!(compatible_catalog_empty_object, "empty_object", "provider");
compatible_catalog_shape!(compatible_catalog_missing_ids, "missing_ids", "provider");
compatible_catalog_shape!(
    compatible_catalog_wrong_data_type,
    "wrong_data_type",
    "provider"
);
compatible_catalog_shape!(
    compatible_catalog_array_root,
    "array_root",
    "invalid_operation"
);
compatible_catalog_shape!(
    compatible_catalog_null_root,
    "null_root",
    "invalid_operation"
);
compatible_catalog_shape!(
    compatible_catalog_number_root,
    "number_root",
    "invalid_operation"
);
compatible_catalog_shape!(
    compatible_catalog_string_root,
    "string_root",
    "invalid_operation"
);
compatible_catalog_shape!(
    compatible_catalog_bool_root,
    "bool_root",
    "invalid_operation"
);
compatible_catalog_shape!(
    compatible_catalog_nonobject_entry,
    "nonobject_entry",
    "invalid_operation"
);
compatible_catalog_shape!(compatible_catalog_malformed, "malformed", "encode");
compatible_catalog_shape!(compatible_catalog_valid, "valid", "success");

fn legacy_standard_catalog(id: &str, body: &str) -> Result<Vec<Model>, CatalogError> {
    StandardModelDecoder::decode(id, body)
}

#[tokio::test]
async fn compatible_catalog_legacy_same_messages_remain_provider_errors() {
    for case_id in ["array_root", "malformed"] {
        let input = compatible_catalog_input(case_id);
        let id = input["provider_id"].as_str().unwrap();
        let body = input["live_body"].as_str().unwrap();
        let mut opts = CompatibleOptions::new(
            id,
            input["base_url"].as_str().unwrap(),
            Key::new(input["credential"].as_str().unwrap()),
        );
        opts.decoder = CatalogDecoderSelection::Legacy(legacy_standard_catalog);
        let provider =
            OpenAICompatibleProvider::new(opts, Transport::new(body, 1), Arc::new(NoSocket))
                .unwrap();
        let message = StandardModelDecoder::decode_typed(id, body)
            .unwrap_err()
            .to_string();
        assert_eq!(
            provider.list_models(&Never).await.unwrap_err(),
            Error::Provider(message),
            "{case_id}"
        );
    }
}
