use parrot_domain::contracts::async_ops::{BoxFuture, Cancellation};
use parrot_foundation::oauth::AuthError;
use parrot_provider::{
    anthropic::{AnthropicOptions, AnthropicProvider},
    catalogs::{Model, StandardModelDecoder, StandardModelError},
    http_stream::{ByteStream, HttpError, HttpRequest, HttpResponse, HttpTransport},
    routing::{
        anthropic::{AnthropicApiKeySource, AnthropicBridge},
        registry::ProviderRegistry,
    },
    sessions::{Content, Error, Event, Message, Provider, Request, Role, provider::ApiKeySource},
};
use std::{
    collections::VecDeque,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    time::Duration,
};
use tokio::sync::Notify;
struct Token(bool);
impl Cancellation for Token {
    fn is_cancelled(&self) -> bool {
        self.0
    }
    fn cancelled(&self) -> BoxFuture<'_, ()> {
        Box::pin(async move {
            if !self.0 {
                std::future::pending().await
            }
        })
    }
}
const ACTIVE: Token = Token(false);
const CANCELLED: Token = Token(true);
struct Key {
    calls: AtomicUsize,
    result: Mutex<Result<String, Error>>,
    released: AtomicBool,
    changed: Notify,
    honor_cancel: bool,
}
impl Key {
    fn ready() -> Self {
        Self {
            calls: AtomicUsize::new(0),
            result: Mutex::new(Ok("synthetic-key".into())),
            released: AtomicBool::new(true),
            changed: Notify::new(),
            honor_cancel: true,
        }
    }
    fn release(&self) {
        self.released.store(true, Ordering::SeqCst);
        self.changed.notify_one();
    }
}
impl ApiKeySource for Key {
    fn api_key<'a>(&'a self, ct: &'a dyn Cancellation) -> BoxFuture<'a, Result<String, Error>> {
        Box::pin(async move {
            self.calls.fetch_add(1, Ordering::SeqCst);
            while !self.released.load(Ordering::SeqCst) {
                tokio::select! {_ = self.changed.notified()=>{},_ = ct.cancelled()=>return Err(Error::Cancelled)}
            }
            if self.honor_cancel && ct.is_cancelled() {
                return Err(Error::Cancelled);
            }
            self.result.lock().unwrap().clone()
        })
    }
    fn has_credential<'a>(
        &'a self,
        ct: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<bool, Error>> {
        Box::pin(async move { self.api_key(ct).await.map(|key| !key.is_empty()) })
    }
}
struct Body {
    bytes: Vec<u8>,
    position: usize,
    drops: Arc<AtomicUsize>,
    reads: Arc<AtomicUsize>,
}
impl Drop for Body {
    fn drop(&mut self) {
        self.drops.fetch_add(1, Ordering::SeqCst);
    }
}
impl ByteStream for Body {
    fn read<'a>(
        &'a mut self,
        buffer: &'a mut [u8],
        ct: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<usize, HttpError>> {
        Box::pin(async move {
            if ct.is_cancelled() {
                return Err(HttpError::Cancelled);
            }
            self.reads.fetch_add(1, Ordering::SeqCst);
            let count = buffer.len().min(self.bytes.len() - self.position);
            buffer[..count].copy_from_slice(&self.bytes[self.position..self.position + count]);
            self.position += count;
            Ok(count)
        })
    }
}
struct Transport {
    requests: Mutex<Vec<HttpRequest>>,
    responses: Mutex<VecDeque<(u16, String)>>,
    drops: Arc<AtomicUsize>,
    reads: Arc<AtomicUsize>,
}
impl HttpTransport for Transport {
    fn send<'a>(
        &'a self,
        request: HttpRequest,
        ct: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<HttpResponse, HttpError>> {
        Box::pin(async move {
            if ct.is_cancelled() {
                return Err(HttpError::Cancelled);
            }
            self.requests.lock().unwrap().push(request);
            let (status, body) = self
                .responses
                .lock()
                .unwrap()
                .pop_front()
                .expect("finite response");
            Ok(HttpResponse {
                status,
                reason: "synthetic".into(),
                headers: vec![],
                body: Box::new(Body {
                    bytes: body.into_bytes(),
                    position: 0,
                    drops: self.drops.clone(),
                    reads: self.reads.clone(),
                }),
            })
        })
    }
}
fn fixture(
    key: Arc<dyn ApiKeySource>,
    maximum: usize,
    responses: Vec<(u16, &str)>,
) -> (AnthropicBridge, Arc<Transport>) {
    let transport = Arc::new(Transport {
        requests: Mutex::new(Vec::new()),
        responses: Mutex::new(
            responses
                .into_iter()
                .map(|(status, body)| (status, body.into()))
                .collect(),
        ),
        drops: Arc::new(AtomicUsize::new(0)),
        reads: Arc::new(AtomicUsize::new(0)),
    });
    let mut options = AnthropicOptions::new(
        "anthropic-custom",
        "https://example.test/v1",
        Arc::new(AnthropicApiKeySource::new(key)),
    );
    options.maximum_request_bytes = maximum;
    options.headers = vec![
        ("anthropic-version".into(), "custom-version".into()),
        ("anthropic-beta".into(), "custom-beta".into()),
    ];
    options.models = vec![Model::new("declared", "anthropic-custom")];
    (
        AnthropicBridge::new(Arc::new(
            AnthropicProvider::new(options, transport.clone()).unwrap(),
        )),
        transport,
    )
}
fn request() -> Request {
    Request {
        model: "claude-test".into(),
        max_output_tokens: 64,
        messages: vec![Message::new(Role::User, vec![Content::text("hello")])],
        ..Default::default()
    }
}
const COMPLETED: &str = "data: {\"type\":\"message_stop\"}\n\n";
#[tokio::test]
async fn actual_anthropic_factory_bridge_is_lazy_and_passes_lifecycle_headers_raw_native_history_and_completion()
 {
    let key = Arc::new(Key::ready());
    let (provider, transport) = fixture(key.clone(), 1 << 20, vec![(200, COMPLETED)]);
    let mut req = request();
    let mut native = Message::new(Role::Assistant, vec![]);
    native.provider_id = "anthropic-custom".into();
    native.provider_content =
        r#"[{"type":"thinking","thinking":"native","signature":"opaque"}]"#.into();
    req.messages.push(native);
    let mut stream = provider.call(req).unwrap();
    assert_eq!(key.calls.load(Ordering::SeqCst), 0);
    assert!(transport.requests.lock().unwrap().is_empty());
    assert_eq!(
        stream.next(&ACTIVE).await.unwrap(),
        Some(Event::HttpRequestStarted)
    );
    assert_eq!(key.calls.load(Ordering::SeqCst), 1);
    assert!(transport.requests.lock().unwrap().is_empty());
    assert_eq!(
        stream.next(&ACTIVE).await.unwrap(),
        Some(Event::HttpResponseHeadersReceived)
    );
    assert_eq!(transport.reads.load(Ordering::SeqCst), 0);
    {
        let requests = transport.requests.lock().unwrap();
        let sent = &requests[0];
        assert_eq!(sent.endpoint.as_str(), "https://example.test/v1/messages");
        let header = |name: &str| {
            sent.headers
                .iter()
                .find(|(n, _)| n.eq_ignore_ascii_case(name))
                .unwrap()
                .1
                .clone()
        };
        assert_eq!(header("x-api-key"), "synthetic-key");
        assert_eq!(header("anthropic-version"), "custom-version");
        assert_eq!(
            header("anthropic-beta"),
            "custom-beta,thinking-binding-controls-2026-08-01"
        );
        let body: serde_json::Value = serde_json::from_slice(&sent.body).unwrap();
        assert_eq!(body["model"], "claude-test");
        assert_eq!(body["messages"][1]["content"][0]["signature"], "opaque");
    }
    assert!(matches!(
        stream.next(&ACTIVE).await.unwrap(),
        Some(Event::Completed(_))
    ));
    assert_eq!(stream.next(&ACTIVE).await.unwrap(), None);
    assert_eq!(transport.drops.load(Ordering::SeqCst), 1);
    stream.close().await.unwrap();
    stream.close().await.unwrap();
}
#[tokio::test]
async fn actual_anthropic_stateless_sessions_delegate_calls_and_report_unsupported_fallback_without_consuming_request()
 {
    let (provider, transport) = fixture(Arc::new(Key::ready()), 1 << 20, vec![(200, COMPLETED)]);
    let session = provider.open_session().unwrap();
    session.begin_turn().await.unwrap();
    assert!(!session.try_fall_back_to_http().await.unwrap());
    let mut stream = session.call(request()).unwrap();
    assert_eq!(
        stream.next(&ACTIVE).await.unwrap(),
        Some(Event::HttpRequestStarted)
    );
    assert_eq!(
        stream.next(&ACTIVE).await.unwrap(),
        Some(Event::HttpResponseHeadersReceived)
    );
    stream.close().await.unwrap();
    assert_eq!(transport.drops.load(Ordering::SeqCst), 1);
    session.close().await.unwrap();
    session.close().await.unwrap();
    assert_eq!(transport.requests.lock().unwrap().len(), 1);
}
#[tokio::test]
async fn actual_anthropic_request_limit_fails_before_start_or_transport() {
    let (provider, transport) = fixture(Arc::new(Key::ready()), 1, vec![]);
    let mut stream = provider.call(request()).unwrap();
    assert!(matches!(
        stream.next(&ACTIVE).await,
        Err(Error::Http(HttpError::RequestTooLarge(1)))
    ));
    assert!(transport.requests.lock().unwrap().is_empty());
    assert_eq!(stream.next(&ACTIVE).await.unwrap(), None);
}
#[tokio::test]
async fn anthropic_pending_initialization_survives_dropped_observer_and_resolves_only_once() {
    let mut key = Key::ready();
    key.released = AtomicBool::new(false);
    let key = Arc::new(key);
    let (provider, transport) = fixture(key.clone(), 1 << 20, vec![(200, COMPLETED)]);
    let mut stream = provider.call(request()).unwrap();
    let mut next = stream.next(&ACTIVE);
    assert!(
        tokio::time::timeout(Duration::from_millis(10), next.as_mut())
            .await
            .is_err()
    );
    drop(next);
    key.release();
    assert_eq!(
        stream.next(&ACTIVE).await.unwrap(),
        Some(Event::HttpRequestStarted)
    );
    assert_eq!(key.calls.load(Ordering::SeqCst), 1);
    assert!(transport.requests.lock().unwrap().is_empty());
    stream.close().await.unwrap();
}
#[tokio::test]
async fn anthropic_close_cancels_and_joins_pending_initialization_without_sending() {
    let mut key = Key::ready();
    key.released = AtomicBool::new(false);
    let key = Arc::new(key);
    let (provider, transport) = fixture(key, 1 << 20, vec![]);
    let mut stream = provider.call(request()).unwrap();
    let mut next = stream.next(&ACTIVE);
    assert!(
        tokio::time::timeout(Duration::from_millis(10), next.as_mut())
            .await
            .is_err()
    );
    drop(next);
    tokio::time::timeout(Duration::from_secs(1), stream.close())
        .await
        .unwrap()
        .unwrap();
    assert!(transport.requests.lock().unwrap().is_empty());
    assert_eq!(stream.next(&ACTIVE).await.unwrap(), None);
}
#[tokio::test]
async fn anthropic_close_keeps_polling_cancelled_credential_cleanup_until_it_finishes() {
    struct SettlingKey {
        started: Notify,
        cancelled: Notify,
        cleanup: Notify,
        finished: AtomicBool,
    }
    impl ApiKeySource for SettlingKey {
        fn api_key<'a>(&'a self, ct: &'a dyn Cancellation) -> BoxFuture<'a, Result<String, Error>> {
            Box::pin(async move {
                self.started.notify_one();
                ct.cancelled().await;
                self.cancelled.notify_one();
                self.cleanup.notified().await;
                self.finished.store(true, Ordering::SeqCst);
                Err(Error::Cancelled)
            })
        }
        fn has_credential<'a>(
            &'a self,
            _: &'a dyn Cancellation,
        ) -> BoxFuture<'a, Result<bool, Error>> {
            Box::pin(async { Ok(true) })
        }
    }
    let key = Arc::new(SettlingKey {
        started: Notify::new(),
        cancelled: Notify::new(),
        cleanup: Notify::new(),
        finished: AtomicBool::new(false),
    });
    let (provider, transport) = fixture(key.clone(), 1 << 20, vec![]);
    let mut stream = provider.call(request()).unwrap();
    let mut next = stream.next(&ACTIVE);
    tokio::select! { biased; result=&mut next=>panic!("unexpected start {result:?}"), _=key.started.notified()=>{} }
    drop(next);
    let mut close = stream.close();
    tokio::select! { biased; result=&mut close=>panic!("cleanup abandoned {result:?}"), _=key.cancelled.notified()=>{} }
    assert!(!key.finished.load(Ordering::SeqCst));
    assert!(
        tokio::time::timeout(Duration::from_millis(10), close.as_mut())
            .await
            .is_err()
    );
    drop(close);
    key.cleanup.notify_one();
    stream.close().await.unwrap();
    assert!(key.finished.load(Ordering::SeqCst));
    assert!(transport.requests.lock().unwrap().is_empty());
}
#[tokio::test]
async fn anthropic_authentication_retains_actual_auth_enum_and_http_errors_retain_body() {
    for error in [
        AuthError::Message("auth sentinel"),
        AuthError::Transport,
        AuthError::Io,
    ] {
        let key = Arc::new(Key::ready());
        *key.result.lock().unwrap() = Err(Error::Authentication(error.clone()));
        let (provider, transport) = fixture(key, 1 << 20, vec![]);
        let mut stream = provider.call(request()).unwrap();
        assert_eq!(
            stream.next(&ACTIVE).await.unwrap_err(),
            Error::Authentication(error)
        );
        assert!(transport.requests.lock().unwrap().is_empty());
    }
    let (provider, _) = fixture(
        Arc::new(Key::ready()),
        1 << 20,
        vec![(
            401,
            r#"{"error":{"message":"denied","type":"authentication_error"}}"#,
        )],
    );
    let mut stream = provider.call(request()).unwrap();
    assert!(matches!(
        stream.next(&ACTIVE).await.unwrap(),
        Some(Event::HttpRequestStarted)
    ));
    let error = stream.next(&ACTIVE).await.unwrap_err();
    match error {
        Error::Http(HttpError::Response(failure)) => {
            assert_eq!(failure.status, 401);
            assert_eq!(
                failure.response_body,
                r#"{"error":{"message":"denied","type":"authentication_error"}}"#
            )
        }
        other => panic!("{other:?}"),
    }
}
#[tokio::test]
async fn anthropic_structured_stream_error_keeps_all_response_fields_and_body() {
    let body = "data: {\"type\":\"error\",\"error\":{\"type\":\"overloaded_error\",\"code\":\"overloaded\",\"message\":\"retry later\"}}\n\n";
    let (provider, _) = fixture(Arc::new(Key::ready()), 1 << 20, vec![(200, body)]);
    let mut stream = provider.call(request()).unwrap();
    stream.next(&ACTIVE).await.unwrap();
    stream.next(&ACTIVE).await.unwrap();
    match stream.next(&ACTIVE).await.unwrap_err() {
        Error::ChatProvider(error) => {
            assert_eq!(error.error_type, "overloaded_error");
            assert_eq!(error.error_code, "");
            assert_eq!(error.message, "retry later");
            assert!(error.response_body.contains("retry later"));
        }
        other => panic!("{other:?}"),
    }
}
#[tokio::test]
async fn anthropic_initial_cancellation_preserves_aware_and_ignoring_ready_credential_branches() {
    for aware in [true, false] {
        let mut key = Key::ready();
        key.honor_cancel = aware;
        let key = Arc::new(key);
        let (provider, transport) = fixture(key.clone(), 1 << 20, vec![]);
        let mut stream = provider.call(request()).unwrap();
        let first = stream.next(&CANCELLED).await;
        if aware {
            assert!(matches!(first, Err(Error::Http(HttpError::Cancelled))))
        } else {
            assert_eq!(first.unwrap(), Some(Event::HttpRequestStarted));
            assert!(matches!(
                stream.next(&CANCELLED).await,
                Err(Error::Http(HttpError::Cancelled))
            ))
        }
        assert_eq!(key.calls.load(Ordering::SeqCst), 1);
        assert!(transport.requests.lock().unwrap().is_empty());
    }
}
#[tokio::test]
async fn anthropic_registry_refresh_keeps_declared_seed_for_source_no_usable_models_errors() {
    for body in [
        r#"{"data":[]}"#,
        "{}",
        r#"{"data":null}"#,
        r#"{"data":42}"#,
        r#"{"data":"models"}"#,
        r#"{"data":false}"#,
        r#"{"data":{}}"#,
    ] {
        let key = Arc::new(Key::ready());
        let (provider, transport) = fixture(key.clone(), 1 << 20, vec![(200, body)]);
        let provider: Arc<dyn Provider> = Arc::new(provider);
        let seed = provider.seed_models();
        let registry = ProviderRegistry::new(
            vec![provider.clone()],
            [(provider.id().to_owned(), seed.clone())]
                .into_iter()
                .collect(),
        )
        .unwrap();
        let available = registry.available_models(&ACTIVE).await.unwrap();
        assert_eq!(available.len(), 1);
        assert_eq!(available[0].model, seed[0]);
        assert!(Arc::ptr_eq(&available[0].provider, &provider));
        assert_eq!(registry.models(provider.id()), seed);
        assert_eq!(key.calls.load(Ordering::SeqCst), 2);
        assert_eq!(transport.requests.lock().unwrap().len(), 1);
        assert_eq!(transport.drops.load(Ordering::SeqCst), 1);
    }
}
#[tokio::test]
async fn anthropic_registry_refresh_propagates_wrong_root_and_nonobject_item_native_operations() {
    for body in [
        "[]",
        "null",
        "42",
        r#""models""#,
        "true",
        "false",
        r#"{"data":[null]}"#,
        r#"{"data":[42]}"#,
        r#"{"data":["model"]}"#,
        r#"{"data":[true]}"#,
        r#"{"data":[false]}"#,
        r#"{"data":[[]]}"#,
    ] {
        let key = Arc::new(Key::ready());
        let (provider, transport) = fixture(key.clone(), 1 << 20, vec![(200, body)]);
        let provider: Arc<dyn Provider> = Arc::new(provider);
        let seed = provider.seed_models();
        let registry = ProviderRegistry::new(
            vec![provider.clone()],
            [(provider.id().to_owned(), seed.clone())]
                .into_iter()
                .collect(),
        )
        .unwrap();
        let error = registry.available_models(&ACTIVE).await.unwrap_err();
        assert!(
            matches!(error, Error::InvalidOperation(_)),
            "{body}: {error:?}"
        );
        assert_eq!(registry.models(provider.id()), seed);
        assert_eq!(key.calls.load(Ordering::SeqCst), 2);
        assert_eq!(transport.requests.lock().unwrap().len(), 1);
        assert_eq!(transport.drops.load(Ordering::SeqCst), 1);
    }
}
#[tokio::test]
async fn anthropic_registry_refresh_propagates_malformed_native_json_instead_of_using_seed() {
    let body = "{broken";
    let native = StandardModelDecoder::decode_typed("anthropic-custom", body).unwrap_err();
    let native_display = native.to_string();
    let StandardModelError::Json(native) = native else {
        panic!("malformed fixture did not reach the native JSON parser");
    };
    assert!(!native.0.is_empty());
    assert_eq!(native.to_string(), native_display);
    assert!(std::error::Error::source(&native).is_none());
    let (provider, transport) = fixture(Arc::new(Key::ready()), 1 << 20, vec![(200, body)]);
    let provider: Arc<dyn Provider> = Arc::new(provider);
    let seed = provider.seed_models();
    let registry = ProviderRegistry::new(
        vec![provider.clone()],
        [(provider.id().to_owned(), seed.clone())]
            .into_iter()
            .collect(),
    )
    .unwrap();
    let error = registry.available_models(&ACTIVE).await.unwrap_err();
    assert_eq!(error, Error::Encode(native.0));
    assert_eq!(error.to_string(), native_display);
    assert!(std::error::Error::source(&error).is_none());
    assert_ne!(
        error.to_string(),
        "provider: models response contains no usable models"
    );
    assert_eq!(registry.models(provider.id()), seed);
    assert_eq!(transport.requests.lock().unwrap().len(), 1);
    assert_eq!(transport.drops.load(Ordering::SeqCst), 1);
}
#[tokio::test]
async fn anthropic_catalog_credentials_and_default_image_cost_are_delegated() {
    let (provider, transport) = fixture(
        Arc::new(Key::ready()),
        1 << 20,
        vec![(200, r#"{"data":[{"id":"listed"}]}"#)],
    );
    assert!(provider.has_credential(&ACTIVE).await.unwrap());
    assert_eq!(provider.seed_models()[0].id, "declared");
    let models = provider.list_models(&ACTIVE).await.unwrap();
    assert!(models.iter().any(|m| m.id == "listed"));
    assert_eq!(
        transport.requests.lock().unwrap()[0].endpoint.as_str(),
        "https://example.test/v1/models?limit=1000"
    );
    assert_eq!(
        provider.calculate_image_tokens(
            &Model::new("any", "anthropic-custom"),
            &Content::text("not an image")
        ),
        4096
    );
    assert!(provider.usage_reporter().is_none());
}
