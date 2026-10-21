//! Actual-provider assertion slices delegated by the T29 canonical proof map.
use parrot_domain::contracts::async_ops::{BoxFuture, Cancellation};
use parrot_provider::{
    http_stream::{ByteStream, HttpError, HttpRequest, HttpResponse, HttpTransport},
    responses_websocket as ws,
    sessions::{
        Error, Event, EventStream, Message, Provider, Request, Session,
        decorators::{RetrySleeper, RetryingProvider},
        diagnostic_session::DiagnosticSession,
        diagnostics::{DiagnosticEvent, DiagnosticSink, MonotonicDiagnosticClock},
        http::Protocol,
        provider::{
            ApiKeySource, ChatGptOptions, ChatGptProvider, CompatibleOptions, OAuthAccess,
            OAuthTokenSource, OpenAICompatibleProvider,
        },
    },
};
use serde_json::{Value, json};
use std::{
    collections::{HashSet, VecDeque},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

const COMPLETED: &str = r#"{"type":"response.completed","response":{"id":"resp-1","usage":{"input_tokens":1,"output_tokens":1},"output":[{"type":"message","role":"assistant","content":[{"type":"output_text","text":"answer"}]}]}}"#;
const HTTP_COMPLETED: &str = "data: {\"type\":\"response.completed\",\"response\":{\"usage\":{\"input_tokens\":1,\"output_tokens\":1}}}\n\n";
struct Never;
impl Cancellation for Never {
    fn is_cancelled(&self) -> bool {
        false
    }
    fn cancelled(&self) -> BoxFuture<'_, ()> {
        Box::pin(std::future::pending())
    }
}
struct Credentials;
impl ApiKeySource for Credentials {
    fn api_key<'a>(&'a self, _: &'a dyn Cancellation) -> BoxFuture<'a, Result<String, Error>> {
        Box::pin(async { Ok("test-key".into()) })
    }
    fn has_credential<'a>(&'a self, _: &'a dyn Cancellation) -> BoxFuture<'a, Result<bool, Error>> {
        Box::pin(async { Ok(true) })
    }
}
impl OAuthTokenSource for Credentials {
    fn token<'a>(&'a self, _: &'a dyn Cancellation) -> BoxFuture<'a, Result<OAuthAccess, Error>> {
        Box::pin(async {
            Ok(OAuthAccess {
                access_token: "access-token".into(),
                account_id: "account-id".into(),
            })
        })
    }
    fn has_credential<'a>(&'a self, _: &'a dyn Cancellation) -> BoxFuture<'a, Result<bool, Error>> {
        Box::pin(async { Ok(true) })
    }
}
#[derive(Default)]
struct SocketRecord {
    sent: Mutex<Vec<Vec<u8>>>,
    assembling: Mutex<Vec<u8>>,
    aborted: AtomicBool,
}
struct FakeSocket {
    record: Arc<SocketRecord>,
    frames: VecDeque<Result<String, ws::WebSocketError>>,
}
impl ws::Socket for FakeSocket {
    fn state(&self) -> ws::SocketState {
        if self.record.aborted.load(Ordering::SeqCst) {
            ws::SocketState::Aborted
        } else {
            ws::SocketState::Open
        }
    }
    fn send<'a>(
        &'a mut self,
        bytes: &'a [u8],
        end: bool,
        _: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<(), ws::WebSocketError>> {
        Box::pin(async move {
            let mut message = self.record.assembling.lock().unwrap();
            message.extend_from_slice(bytes);
            if end {
                self.record
                    .sent
                    .lock()
                    .unwrap()
                    .push(std::mem::take(&mut message));
            }
            Ok(())
        })
    }
    fn receive<'a>(
        &'a mut self,
        bytes: &'a mut [u8],
        _: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<ws::ReceivedFrame, ws::WebSocketError>> {
        Box::pin(async move {
            let frame = self
                .frames
                .pop_front()
                .expect("unexpected WebSocket receive")?;
            assert!(
                frame.len() <= bytes.len(),
                "fixture frame must fit caller buffer"
            );
            bytes[..frame.len()].copy_from_slice(frame.as_bytes());
            Ok(ws::ReceivedFrame {
                count: frame.len(),
                kind: ws::FrameKind::Text,
                end_of_message: true,
                close_status: None,
                close_description: None,
            })
        })
    }
    fn abort(&mut self) {
        self.record.aborted.store(true, Ordering::SeqCst);
    }
}
enum Connect {
    Socket(Arc<SocketRecord>, Vec<String>),
    Fail(ws::WebSocketError),
}
#[derive(Clone)]
struct ConnectRecord {
    endpoint: String,
    headers: ws::Headers,
    timeout: Option<Duration>,
}
struct Connector {
    outcomes: Mutex<VecDeque<Connect>>,
    calls: Mutex<Vec<ConnectRecord>>,
}
impl Connector {
    fn new(outcomes: Vec<Connect>) -> Arc<Self> {
        Arc::new(Self {
            outcomes: Mutex::new(outcomes.into()),
            calls: Mutex::new(Vec::new()),
        })
    }
    fn count(&self) -> usize {
        self.calls.lock().unwrap().len()
    }
}
impl ws::Connector for Connector {
    fn connect<'a>(
        &'a self,
        endpoint: &'a str,
        headers: &'a ws::Headers,
        timeout: Option<Duration>,
        _: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<ws::ConnectedSocket, ws::WebSocketError>> {
        self.calls.lock().unwrap().push(ConnectRecord {
            endpoint: endpoint.into(),
            headers: headers.clone(),
            timeout,
        });
        let outcome = self
            .outcomes
            .lock()
            .unwrap()
            .pop_front()
            .expect("unexpected WebSocket connection");
        Box::pin(async move {
            match outcome {
                Connect::Fail(error) => Err(error),
                Connect::Socket(record, frames) => Ok(ws::ConnectedSocket {
                    socket: Box::new(FakeSocket {
                        record,
                        frames: frames.into_iter().map(Ok).collect(),
                    }),
                    response_headers: Vec::new(),
                }),
            }
        })
    }
}
fn socket(frames: Vec<String>) -> (Arc<SocketRecord>, Connect) {
    let record = Arc::new(SocketRecord::default());
    (record.clone(), Connect::Socket(record, frames))
}
fn upgrade(status: u16) -> Connect {
    Connect::Fail(ws::WebSocketError::Upgrade {
        status,
        message: "private-sentinel".into(),
    })
}
struct Bytes {
    data: Vec<u8>,
    offset: usize,
}
impl ByteStream for Bytes {
    fn read<'a>(
        &'a mut self,
        bytes: &'a mut [u8],
        _: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<usize, HttpError>> {
        Box::pin(async move {
            let count = bytes.len().min(self.data.len() - self.offset);
            bytes[..count].copy_from_slice(&self.data[self.offset..self.offset + count]);
            self.offset += count;
            Ok(count)
        })
    }
}
#[derive(Clone)]
struct RequestRecord {
    endpoint: String,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
}
struct Reply {
    status: u16,
    body: String,
    headers: Vec<(String, String)>,
    delay: Duration,
}
impl Reply {
    fn completed() -> Self {
        Self {
            status: 200,
            body: HTTP_COMPLETED.into(),
            headers: Vec::new(),
            delay: Duration::ZERO,
        }
    }
}
struct Http {
    replies: Mutex<VecDeque<Reply>>,
    requests: Mutex<Vec<RequestRecord>>,
}
impl Http {
    fn new(replies: Vec<Reply>) -> Arc<Self> {
        Arc::new(Self {
            replies: Mutex::new(replies.into()),
            requests: Mutex::new(Vec::new()),
        })
    }
    fn completed(count: usize) -> Arc<Self> {
        Self::new((0..count).map(|_| Reply::completed()).collect())
    }
    fn count(&self) -> usize {
        self.requests.lock().unwrap().len()
    }
}
impl HttpTransport for Http {
    fn send<'a>(
        &'a self,
        request: HttpRequest,
        _: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<HttpResponse, HttpError>> {
        self.requests.lock().unwrap().push(RequestRecord {
            endpoint: request.endpoint.as_str().into(),
            headers: request.headers,
            body: request.body,
        });
        let reply = self
            .replies
            .lock()
            .unwrap()
            .pop_front()
            .expect("unexpected HTTP callback");
        Box::pin(async move {
            if !reply.delay.is_zero() {
                tokio::time::sleep(reply.delay).await;
            }
            Ok(HttpResponse {
                status: reply.status,
                reason: String::new(),
                headers: reply.headers,
                body: Box::new(Bytes {
                    data: reply.body.into_bytes(),
                    offset: 0,
                }),
            })
        })
    }
}
fn make_provider(
    chatgpt: bool,
    disable: Option<bool>,
    timeout_ms: i64,
    maximum: usize,
    connector: Arc<Connector>,
    http: Arc<Http>,
) -> Arc<dyn Provider> {
    if chatgpt {
        let mut options = ChatGptOptions::new(Arc::new(Credentials));
        if let Some(disable) = disable {
            options.disable_websocket = disable;
        }
        options.header_timeout_ms = timeout_ms;
        options.maximum_request_bytes = maximum;
        Arc::new(ChatGptProvider::new(options, http, connector).unwrap())
    } else {
        let mut options = CompatibleOptions::new(
            "configured",
            "https://example.test/v1",
            Arc::new(Credentials),
        );
        options.protocol = Protocol::Responses;
        if let Some(disable) = disable {
            options.disable_websocket = disable;
        }
        options.header_timeout_ms = timeout_ms;
        options.maximum_request_bytes = maximum;
        Arc::new(OpenAICompatibleProvider::new(options, http, connector).unwrap())
    }
}
fn configured(chatgpt: bool, connector: Arc<Connector>, http: Arc<Http>) -> Arc<dyn Provider> {
    make_provider(chatgpt, Some(false), 60_000, 64 << 20, connector, http)
}
fn request(text: &str) -> Request {
    Request {
        model: "model".into(),
        messages: vec![Message::user(text)],
        ..Request::default()
    }
}
async fn drain(mut stream: Box<dyn EventStream>) -> (Vec<Event>, Result<(), Error>) {
    let mut events = Vec::new();
    let result = loop {
        match stream.next(&Never).await {
            Ok(Some(event)) => events.push(event),
            Ok(None) => break Ok(()),
            Err(error) => break Err(error),
        }
    };
    let closed = stream.close().await;
    (events, closed.and(result))
}
async fn successful(session: &dyn Session, request: Request) -> Vec<Event> {
    let (events, result) = drain(session.call(request).unwrap()).await;
    result.unwrap();
    assert!(matches!(
        events.last().unwrap().base_event(),
        Event::Completed(_)
    ));
    events
}
fn header(headers: &[(String, String)], name: &str) -> String {
    headers
        .iter()
        .find(|(key, _)| key.eq_ignore_ascii_case(name))
        .map_or_else(String::new, |(_, value)| value.clone())
}
fn sent(record: &SocketRecord) -> Vec<Vec<u8>> {
    record.sent.lock().unwrap().clone()
}
fn retries(events: &[Event]) -> Vec<(i32, u64, String)> {
    events
        .iter()
        .filter_map(|event| {
            if let Event::Retry {
                attempt,
                retry_after,
                reason,
            } = event.base_event()
            {
                Some((*attempt, retry_after.as_millis() as u64, reason.clone()))
            } else {
                None
            }
        })
        .collect()
}
#[derive(Default)]
struct Sleeper(Mutex<Vec<Duration>>);
impl RetrySleeper for Sleeper {
    fn sleep<'a>(
        &'a self,
        delay: Duration,
        _: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<(), Error>> {
        self.0.lock().unwrap().push(delay);
        Box::pin(async { Ok(()) })
    }
}
fn retrying(provider: Arc<dyn Provider>) -> Arc<dyn Provider> {
    Arc::new(RetryingProvider::with_sleeper(
        provider,
        5,
        Arc::new(Sleeper::default()),
    ))
}
#[derive(Default)]
struct Log(Mutex<Vec<DiagnosticEvent>>);
impl DiagnosticSink for Log {
    fn write(&self, event: DiagnosticEvent) {
        self.0.lock().unwrap().push(event);
    }
}
fn diagnostic(provider: &dyn Provider, log: Arc<Log>) -> DiagnosticSession {
    DiagnosticSession::new(
        provider.open_session().unwrap(),
        log,
        Arc::new(MonotonicDiagnosticClock::default()),
        "websocket-agent".into(),
        provider.id().into(),
        None,
    )
}

#[tokio::test(start_paused = true)]
async fn configured_header_timeout_reaches_actual_connector_for_both_providers() {
    for chatgpt in [false, true] {
        for milliseconds in [0, 1250] {
            let (_, connection) = socket(vec![COMPLETED.into()]);
            let connector = Connector::new(vec![connection]);
            let http = Http::completed(0);
            let provider = make_provider(
                chatgpt,
                Some(false),
                milliseconds,
                64 << 20,
                connector.clone(),
                http,
            );
            let session = provider.open_session().unwrap();
            successful(session.as_ref(), request("hello")).await;
            assert_eq!(
                connector.calls.lock().unwrap()[0].timeout,
                if milliseconds == 0 {
                    None
                } else {
                    Some(Duration::from_millis(milliseconds as u64))
                }
            );
            session.close().await.unwrap();
        }
    }
}
#[tokio::test(start_paused = true)]
async fn configured_http_header_timeout_reaches_original_request_fallback_for_all_eight_tuples() {
    for chatgpt in [false, true] {
        for milliseconds in [0, 20] {
            for fallback in [false, true] {
                let connector = Connector::new(if fallback { vec![upgrade(404)] } else { vec![] });
                let mut reply = Reply::completed();
                reply.delay = Duration::from_millis(250);
                let http = Http::new(vec![reply]);
                let provider = make_provider(
                    chatgpt,
                    Some(!fallback),
                    milliseconds,
                    64 << 20,
                    connector.clone(),
                    http.clone(),
                );
                let session = provider.open_session().unwrap();
                let (_, result) = drain(session.call(request("hello")).unwrap()).await;
                if milliseconds == 0 {
                    result.unwrap();
                    assert_eq!(http.count(), 1);
                } else {
                    assert!(matches!(
                        result,
                        Err(Error::Http(HttpError::HeaderTimeout(_)))
                    ));
                }
                assert_eq!(connector.count(), usize::from(fallback));
                session.close().await.unwrap();
            }
        }
    }
}
#[tokio::test]
async fn configured_request_byte_limit_reaches_http_websocket_and_fallback_for_all_six_tuples() {
    for chatgpt in [false, true] {
        for (disabled, fallback) in [(true, false), (false, false), (false, true)] {
            let (record, connection) = socket(vec![]);
            let connector = Connector::new(if fallback {
                vec![upgrade(404)]
            } else {
                vec![connection]
            });
            let http = Http::completed(0);
            let provider =
                make_provider(chatgpt, Some(disabled), 60_000, 1, connector, http.clone());
            let session = provider.open_session().unwrap();
            let (_, result) = drain(session.call(request("hello")).unwrap()).await;
            assert!(matches!(
                &result,
                Err(Error::Http(HttpError::RequestTooLarge(1)))
                    | Err(Error::Socket(ws::WebSocketError::RequestTooLarge {
                        maximum: 1
                    }))
            ));
            assert!(result.unwrap_err().to_string().contains("exceeds 1 bytes"));
            assert_eq!(http.count(), 0);
            assert!(sent(&record).is_empty());
            session.close().await.unwrap();
        }
    }
}
#[tokio::test]
async fn actual_chatgpt_headers_endpoint_maximum_and_independent_sessions() {
    let (first_record, first) = socket(vec![
        COMPLETED.into(),
        COMPLETED.replace("resp-1", "resp-2"),
    ]);
    let (second_record, second) = socket(vec![COMPLETED.into()]);
    let connector = Connector::new(vec![first, second]);
    let http = Http::completed(0);
    let provider = configured(true, connector.clone(), http);
    let first = provider.open_session().unwrap();
    let second = provider.open_session().unwrap();
    let initial = Request {
        model: "gpt-5.6-sol".into(),
        max_output_tokens: 4096,
        messages: vec![Message::user("hello")],
        ..Request::default()
    };
    successful(first.as_ref(), initial.clone()).await;
    let continued = Request {
        messages: vec![
            Message::user("hello"),
            Message::assistant("answer", vec![]),
            Message::user("next"),
        ],
        ..initial.clone()
    };
    successful(first.as_ref(), continued).await;
    let calls = connector.calls.lock().unwrap().clone();
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].timeout, Some(Duration::from_secs(60)));
    assert_eq!(
        calls[0].endpoint,
        "wss://chatgpt.com/backend-api/codex/responses"
    );
    for (name, expected) in [
        ("OpenAI-Beta", "responses_websockets=2026-02-06"),
        ("Authorization", "Bearer access-token"),
        ("originator", "parrot"),
        ("User-Agent", "parrot"),
        ("ChatGPT-Account-Id", "account-id"),
    ] {
        assert_eq!(header(&calls[0].headers, name), expected);
    }
    assert!(!header(&calls[0].headers, "session-id").is_empty());
    let first_sent = sent(&first_record);
    let first_body: Value = serde_json::from_slice(&first_sent[0]).unwrap();
    let continued_body: Value = serde_json::from_slice(&first_sent[1]).unwrap();
    assert!(first_body.get("max_output_tokens").is_none());
    assert_eq!(continued_body["previous_response_id"], "resp-1");
    assert_eq!(continued_body["input"].as_array().unwrap().len(), 1);
    assert_eq!(continued_body["input"][0]["content"][0]["text"], "next");
    successful(
        second.as_ref(),
        Request {
            model: "gpt-5.6-sol".into(),
            max_output_tokens: 4096,
            messages: vec![Message::user("two")],
            ..Request::default()
        },
    )
    .await;
    let calls = connector.calls.lock().unwrap().clone();
    assert_eq!(calls.len(), 2);
    assert_ne!(
        header(&calls[0].headers, "session-id"),
        header(&calls[1].headers, "session-id")
    );
    assert_eq!(sent(&second_record).len(), 1);
    first.close().await.unwrap();
    second.close().await.unwrap();
}
#[tokio::test]
async fn compatible_default_timeout_is_wired_to_reused_connection() {
    let (record, connection) = socket(vec![
        COMPLETED.into(),
        COMPLETED.replace("resp-1", "resp-2"),
    ]);
    let connector = Connector::new(vec![connection]);
    let provider = configured(false, connector.clone(), Http::completed(0));
    let session = provider.open_session().unwrap();
    successful(session.as_ref(), request("hello")).await;
    successful(
        session.as_ref(),
        Request {
            messages: vec![
                Message::user("hello"),
                Message::assistant("answer", vec![]),
                Message::user("next"),
            ],
            ..request("hello")
        },
    )
    .await;
    assert_eq!(
        connector.calls.lock().unwrap()[0].timeout,
        Some(Duration::from_secs(60))
    );
    assert_eq!(connector.count(), 1);
    assert_eq!(sent(&record).len(), 2);
    session.close().await.unwrap();
}
#[tokio::test]
async fn disabled_default_and_explicit_chatgpt_http_sessions_have_stable_isolated_ids() {
    let connector = Connector::new(vec![]);
    let http = Http::completed(2);
    let provider = make_provider(
        false,
        None,
        60_000,
        64 << 20,
        connector.clone(),
        http.clone(),
    );
    let session = provider.open_session().unwrap();
    successful(session.as_ref(), request("one")).await;
    successful(session.as_ref(), request("two")).await;
    assert_eq!(connector.count(), 0);
    assert_eq!(http.count(), 2);
    session.close().await.unwrap();
    let connector = Connector::new(vec![]);
    let http = Http::completed(3);
    let provider = make_provider(
        true,
        Some(true),
        60_000,
        64 << 20,
        connector.clone(),
        http.clone(),
    );
    let first = provider.open_session().unwrap();
    let second = provider.open_session().unwrap();
    for (session, prompt) in [(&first, "one"), (&first, "two"), (&second, "three")] {
        successful(
            session.as_ref(),
            Request {
                model: "gpt-5.6-sol".into(),
                max_output_tokens: 4096,
                messages: vec![Message::user(prompt)],
                ..Request::default()
            },
        )
        .await;
    }
    assert_eq!(connector.count(), 0);
    let requests = http.requests.lock().unwrap().clone();
    assert_eq!(requests.len(), 3);
    let ids: Vec<_> = requests
        .iter()
        .map(|r| header(&r.headers, "session-id"))
        .collect();
    assert!(!ids[0].is_empty());
    assert_eq!(ids[0], ids[1]);
    assert_ne!(ids[0], ids[2]);
    first.close().await.unwrap();
    second.close().await.unwrap();
}

#[tokio::test]
async fn unsupported_upgrades_and_chatgpt_oversized_requests_select_sticky_actual_http() {
    for (chatgpt, status, oversized) in [
        (false, 404, false),
        (false, 405, false),
        (false, 426, false),
        (true, 404, false),
        (true, 404, true),
    ] {
        let (record, connection) = socket(vec![]);
        let connector = Connector::new(vec![if oversized {
            connection
        } else {
            upgrade(status)
        }]);
        let http = Http::completed(2);
        let provider = configured(chatgpt, connector.clone(), http.clone());
        let session = provider.open_session().unwrap();
        let prompt = if oversized {
            "a".repeat(ws::MAXIMUM_MESSAGE_BYTES)
        } else {
            "one".into()
        };
        let events = successful(session.as_ref(), request(&prompt)).await;
        let expected = if oversized {
            format!(
                "WebSocket request exceeds {} bytes. Retrying over HTTP.",
                ws::MAXIMUM_MESSAGE_BYTES
            )
        } else {
            format!("WebSocket upgrade is unsupported (HTTP {status}). Retrying over HTTP.")
        };
        assert_eq!(retries(&events), vec![(1, 0, expected)]);
        session.begin_turn().await.unwrap();
        successful(session.as_ref(), request("two")).await;
        assert_eq!(connector.count(), 1);
        assert_eq!(http.count(), 2);
        assert!(sent(&record).is_empty());
        session.close().await.unwrap();
    }
}
#[tokio::test(start_paused = true)]
async fn exhausted_retry_budget_is_sticky_session_local_and_http_gets_a_fresh_budget() {
    for upgrade_failure in [false, true] {
        let (independent_record, independent) = socket(vec![COMPLETED.into()]);
        let mut outcomes: Vec<_> = (0..6)
            .map(|_| {
                if upgrade_failure {
                    upgrade(502)
                } else {
                    Connect::Fail(ws::WebSocketError::Transport("transport".into()))
                }
            })
            .collect();
        outcomes.push(independent);
        let connector = Connector::new(outcomes);
        let http = Http::completed(2);
        let provider = retrying(configured(false, connector.clone(), http.clone()));
        let session = provider.open_session().unwrap();
        let events = successful(session.as_ref(), request("one")).await;
        let retry = retries(&events);
        assert_eq!(
            retry.iter().map(|r| r.0).collect::<Vec<_>>(),
            vec![1, 2, 3, 4, 5, 6]
        );
        assert_eq!(
            retry.iter().map(|r| r.1).collect::<Vec<_>>(),
            vec![200, 400, 800, 1600, 3200, 0]
        );
        assert_eq!(
            retry[5].2,
            "WebSocket retry budget exhausted. Retrying over HTTP."
        );
        successful(session.as_ref(), request("two")).await;
        let independent = provider.open_session().unwrap();
        successful(independent.as_ref(), request("three")).await;
        assert_eq!(connector.count(), 7);
        assert_eq!(http.count(), 2);
        assert_eq!(sent(&independent_record).len(), 1);
        session.close().await.unwrap();
        independent.close().await.unwrap();
    }
    let connector = Connector::new(
        (0..6)
            .map(|_| Connect::Fail(ws::WebSocketError::Transport("transport".into())))
            .collect(),
    );
    let http = Http::new(vec![
        Reply {
            status: 500,
            body: "transient".into(),
            headers: vec![],
            delay: Duration::ZERO,
        },
        Reply::completed(),
    ]);
    let provider = retrying(configured(false, connector.clone(), http.clone()));
    let session = provider.open_session().unwrap();
    successful(session.as_ref(), request("hello")).await;
    assert_eq!(connector.count(), 6);
    assert_eq!(http.count(), 2);
    session.close().await.unwrap();
}
#[tokio::test(start_paused = true)]
async fn visible_websocket_failure_is_not_replayed_and_next_call_uses_actual_http() {
    let (_, connection) = socket(vec![
        r#"{"type":"response.output_text.delta","delta":"partial"}"#.into(),
        r#"{"type":"error","error":{"code":"broken","message":"no"}}"#.into(),
    ]);
    let connector = Connector::new(vec![connection]);
    let http = Http::completed(1);
    let provider = retrying(configured(false, connector.clone(), http.clone()));
    let session = provider.open_session().unwrap();
    let (events, result) = drain(session.call(request("one")).unwrap()).await;
    assert!(matches!(
        result,
        Err(Error::Response(_)) | Err(Error::Socket(ws::WebSocketError::Response(_)))
    ));
    assert_eq!(events.len(), 1);
    assert_eq!(
        events
            .iter()
            .filter(|e| e.is_visible_output())
            .map(|e| e.text())
            .collect::<Vec<_>>(),
        vec!["partial"]
    );
    assert!(retries(&events).is_empty());
    successful(session.as_ref(), request("two")).await;
    assert_eq!(connector.count(), 1);
    assert_eq!(http.count(), 1);
    session.close().await.unwrap();
}
#[tokio::test]
async fn actual_http_fallback_retains_turn_state_and_emits_encrypted_reasoning_once() {
    for fallback in [false, true] {
        let item = json!({"type":"reasoning","id":"rs_1","encrypted_content":"opaque"});
        let body = format!(
            "data: {}\n\ndata: {}\n\n",
            json!({"type":"response.output_item.done","item":item}),
            json!({"type":"response.completed","response":{"id":"resp-1","output":[item]}})
        );
        let connector = Connector::new(if fallback { vec![upgrade(404)] } else { vec![] });
        let http = Http::new(vec![Reply {
            body,
            ..Reply::completed()
        }]);
        let provider = make_provider(
            false,
            Some(!fallback),
            60_000,
            64 << 20,
            connector.clone(),
            http.clone(),
        );
        let session = provider.open_session().unwrap();
        let events = successful(session.as_ref(), request("hello")).await;
        assert_eq!(
            events
                .iter()
                .filter(|e| matches!(e.base_event(), Event::EncryptedReasoningReceived))
                .count(),
            1
        );
        assert_eq!(retries(&events).len(), usize::from(fallback));
        assert_eq!(connector.count(), usize::from(fallback));
        assert_eq!(http.count(), 1);
        session.close().await.unwrap();
    }
    let connector = Connector::new(vec![upgrade(404)]);
    let http = Http::new(vec![
        Reply {
            headers: vec![("X-Codex-Turn-State".into(), "http-state".into())],
            ..Reply::completed()
        },
        Reply::completed(),
    ]);
    let provider = configured(false, connector, http.clone());
    let session = provider.open_session().unwrap();
    successful(session.as_ref(), request("one")).await;
    successful(session.as_ref(), request("two")).await;
    assert_eq!(http.count(), 2);
    let requests = http.requests.lock().unwrap().clone();
    assert!(header(&requests[0].headers, "x-codex-turn-state").is_empty());
    assert_eq!(
        header(&requests[1].headers, "x-codex-turn-state"),
        "http-state"
    );
    session.close().await.unwrap();
}
#[tokio::test]
async fn native_history_selects_real_http_callback_only_for_fallback_across_six_behaviors() {
    for behavior in [
        "incremental",
        "partial-native",
        "reconnect",
        "recovery",
        "fallback",
        "model-change",
    ] {
        let native = json!([{"id":"rs_1","type":"reasoning","summary":[{"type":"summary_text","text":"summary"}],"encrypted_content":"opaque-reasoning"},{"id":"msg_1","type":"message","role":"assistant","content":[{"type":"output_text","text":"answer","annotations":[]}]},{"id":"fc_1","type":"function_call","call_id":"call_1","name":"read","arguments":"{}","status":"completed"}]);
        let mut frames =
            vec![json!({"type":"response.output_text.delta","delta":"answer"}).to_string()];
        for (index, item) in native
            .as_array()
            .unwrap()
            .iter()
            .enumerate()
            .take(if behavior == "partial-native" { 1 } else { 3 })
        {
            frames.push(
                json!({"type":"response.output_item.done","output_index":index,"item":item})
                    .to_string(),
            );
        }
        if behavior == "partial-native" {
            frames.push(json!({"type":"response.output_item.added","item":{"id":"fc_1","type":"function_call","call_id":"call_1","name":"read"}}).to_string());
            frames.push(json!({"type":"response.function_call_arguments.done","item_id":"fc_1","call_id":"call_1","arguments":"{}"}).to_string());
        }
        frames.push(json!({"type":"response.completed","response":{"id":"resp-native","output":if behavior=="partial-native" {json!([])}else{native.clone()}}}).to_string());
        frames.push(if behavior=="recovery" { r#"{"type":"error","error":{"code":"previous_response_not_found","message":"unavailable"}}"#.into() }else{COMPLETED.into()});
        let (_, first) = socket(frames);
        let (_, replacement) = socket(vec![COMPLETED.into()]);
        let connector = Connector::new(vec![
            first,
            if behavior == "fallback" {
                upgrade(426)
            } else {
                replacement
            },
        ]);
        let http = Http::completed(usize::from(behavior == "fallback"));
        let provider = configured(false, connector, http.clone());
        let session = provider.open_session().unwrap();
        let events = successful(session.as_ref(), request("hello")).await;
        let completion = events
            .iter()
            .find_map(|e| {
                if let Event::Completed(completion) = e.base_event() {
                    Some(completion.clone())
                } else {
                    None
                }
            })
            .unwrap();
        let continued = Request {
            model: if behavior == "model-change" {
                "other-model".into()
            } else {
                "model".into()
            },
            messages: vec![
                Message::user("hello"),
                completion.into_message(),
                Message::tool_result("call_1", "result"),
                Message::user("next"),
            ],
            ..Request::default()
        };
        if matches!(behavior, "reconnect" | "fallback") {
            session.begin_turn().await.unwrap();
        }
        successful(session.as_ref(), continued).await;
        assert_eq!(
            http.count(),
            usize::from(behavior == "fallback"),
            "{behavior}"
        );
        if behavior == "fallback" {
            let requests = http.requests.lock().unwrap();
            assert_eq!(requests[0].endpoint, "https://example.test/v1/responses");
            let body: Value = serde_json::from_slice(&requests[0].body).unwrap();
            assert_eq!(body["input"].as_array().unwrap().len(), 6);
        }
        session.close().await.unwrap();
    }
}

fn assert_attempts(
    log: &Log,
    transports: &[&str],
    outcomes: &[&str],
    calls: usize,
) -> Vec<DiagnosticEvent> {
    let entries = log.0.lock().unwrap().clone();
    assert!(
        entries
            .iter()
            .all(|entry| entry.agent_session_id.as_deref() == Some("websocket-agent"))
    );
    let starts: Vec<_> = entries
        .iter()
        .filter(|e| e.operation == "call_started")
        .collect();
    let finishes: Vec<_> = entries
        .iter()
        .filter(|e| e.operation == "call_finished")
        .collect();
    assert_eq!(starts.len(), calls);
    assert_eq!(finishes.len(), calls);
    let correlations: HashSet<_> = starts
        .iter()
        .map(|e| e.correlation_id.as_ref().unwrap().clone())
        .collect();
    assert_eq!(correlations.len(), calls);
    let terminal_correlations: HashSet<_> = finishes
        .iter()
        .map(|e| e.correlation_id.as_ref().unwrap().clone())
        .collect();
    assert_eq!(terminal_correlations, correlations);
    let request_starts: Vec<_> = entries
        .iter()
        .filter(|e| e.operation == "request_started")
        .collect();
    let request_finishes: Vec<_> = entries
        .iter()
        .filter(|e| e.operation == "request_finished")
        .collect();
    assert_eq!(request_starts.len(), transports.len());
    assert_eq!(request_finishes.len(), transports.len());
    let mut ids = HashSet::new();
    for (index, (start, finish)) in request_starts.iter().zip(&request_finishes).enumerate() {
        let id = start.request_id.as_deref().unwrap();
        assert!(!id.is_empty());
        assert!(ids.insert(id));
        assert_eq!(finish.request_id, start.request_id);
        assert_eq!(finish.correlation_id, start.correlation_id);
        assert!(correlations.contains(start.correlation_id.as_ref().unwrap()));
        assert_eq!(start.transport.as_deref(), Some(transports[index]));
        assert_eq!(finish.transport, start.transport);
        assert_eq!(finish.outcome.as_deref(), Some(outcomes[index]));
        assert!(finish.duration_milliseconds.is_some());
        let sizes: Vec<_> = entries
            .iter()
            .filter(|e| e.operation == "request_size" && e.request_id.as_deref() == Some(id))
            .collect();
        assert!(sizes.len() <= 1);
        if let Some(size) = sizes.first() {
            assert_eq!(size.correlation_id, start.correlation_id);
            assert_eq!(size.transport, start.transport);
            assert_eq!(size.request_bytes, finish.request_bytes);
        } else {
            assert_eq!(finish.request_bytes, None);
        }
        assert_eq!(
            entries
                .iter()
                .filter(|e| e.request_id.as_deref() == Some(id))
                .count(),
            2 + sizes.len()
        );
    }
    request_finishes.into_iter().cloned().collect()
}
#[tokio::test]
async fn both_provider_factories_report_physical_attempts_order_exact_bytes_and_secrecy() {
    for chatgpt in [false, true] {
        for behavior in [
            "reuse",
            "fallback",
            "previous_response_not_found",
            "websocket_connection_limit_reached",
        ] {
            let completed = COMPLETED.replace("answer", "private-sentinel-response");
            let error=json!({"type":"error","error":{"code":behavior,"message":"private-sentinel-error"}}).to_string();
            let (first_record, first) = socket(if behavior == "reuse" {
                vec![completed.clone(), completed.clone()]
            } else {
                vec![error.clone()]
            });
            let (second_record, second) = socket(vec![completed.clone()]);
            let connector = Connector::new(if behavior == "fallback" {
                vec![Connect::Fail(ws::WebSocketError::Upgrade {
                    status: 404,
                    message: "private-sentinel-upgrade".into(),
                })]
            } else if behavior == "reuse" {
                vec![first]
            } else {
                vec![first, second]
            });
            let http = Http::completed(usize::from(behavior == "fallback"));
            let provider = configured(chatgpt, connector.clone(), http.clone());
            let log = Arc::new(Log::default());
            let session = diagnostic(provider.as_ref(), log.clone());
            successful(&session, request("one")).await;
            if behavior == "reuse" {
                successful(&session, request("two")).await;
            }
            session.close().await.unwrap();
            let terminals = assert_attempts(
                &log,
                if behavior == "fallback" {
                    &["websocket", "http_sse"]
                } else {
                    &["websocket", "websocket"]
                },
                if behavior == "reuse" {
                    &["completed", "completed"]
                } else {
                    &["failed", "completed"]
                },
                if behavior == "reuse" { 2 } else { 1 },
            );
            assert_eq!(http.count(), usize::from(behavior == "fallback"));
            assert_eq!(
                connector.count(),
                if matches!(behavior, "reuse" | "fallback") {
                    1
                } else {
                    2
                }
            );
            let mut bodies = sent(&first_record);
            bodies.extend(sent(&second_record));
            assert_eq!(bodies.len(), if behavior == "fallback" { 0 } else { 2 });
            if !chatgpt && behavior != "fallback" {
                assert_eq!(
                    terminals
                        .iter()
                        .map(|e| e.request_bytes.unwrap())
                        .collect::<Vec<_>>(),
                    bodies.iter().map(|b| b.len() as u64).collect::<Vec<_>>()
                );
                assert_eq!(
                    terminals
                        .iter()
                        .map(|e| e.response_bytes.unwrap())
                        .collect::<Vec<_>>(),
                    vec![
                        if behavior == "reuse" {
                            completed.len() as u64
                        } else {
                            error.len() as u64
                        },
                        completed.len() as u64
                    ]
                );
            }
            assert!(!format!("{:?}", log.0.lock().unwrap()).contains("private-sentinel"));
        }
    }
}
#[tokio::test]
async fn actual_compatible_transport_errors_keep_typed_classification_and_private_diagnostics() {
    for code in [
        "websocket_closed",
        "websocket",
        "wire_protocol",
        "websocket_upgrade",
        "websocket_transport",
        "request_too_large",
    ] {
        let failure = match code {
            "websocket_closed" => ws::WebSocketError::Socket {
                code: ws::SocketErrorCode::ConnectionClosedPrematurely,
                message: "private-sentinel".into(),
            },
            "websocket" => ws::WebSocketError::Socket {
                code: ws::SocketErrorCode::Other,
                message: "private-sentinel".into(),
            },
            "wire_protocol" => ws::WebSocketError::Wire("private-sentinel".into()),
            "websocket_upgrade" => ws::WebSocketError::Upgrade {
                status: 503,
                message: "private-sentinel".into(),
            },
            "websocket_transport" => ws::WebSocketError::Transport("private-sentinel".into()),
            _ => ws::WebSocketError::RequestTooLarge { maximum: 1 },
        };
        let provider = configured(
            false,
            Connector::new(vec![Connect::Fail(failure)]),
            Http::completed(0),
        );
        let log = Arc::new(Log::default());
        let session = diagnostic(provider.as_ref(), log.clone());
        let (_, result) = drain(session.call(request("private-sentinel")).unwrap()).await;
        let error = result.unwrap_err();
        if code == "websocket_upgrade" {
            assert!(matches!(
                error,
                Error::Socket(ws::WebSocketError::Upgrade { .. })
            ));
        } else if code == "request_too_large" {
            assert!(matches!(
                error,
                Error::Socket(ws::WebSocketError::RequestTooLarge { .. })
            ));
        } else {
            assert!(matches!(
                error,
                Error::Socket(ws::WebSocketError::TransportFailure { .. })
                    | Error::Socket(ws::WebSocketError::Transport(_))
            ));
        }
        session.close().await.unwrap();
        let entries = log.0.lock().unwrap();
        for operation in ["request_finished", "call_finished"] {
            let finishes: Vec<_> = entries
                .iter()
                .filter(|e| e.operation == operation)
                .collect();
            assert_eq!(
                finishes.len(),
                1,
                "{code}: expected exactly one {operation}"
            );
            assert_eq!(finishes[0].error_code.as_deref(), Some(code));
        }
        assert!(!format!("{entries:?}").contains("private-sentinel"));
    }
}

#[tokio::test]
async fn actual_chatgpt_independent_connections_each_send_one_request() {
    let (first_record, first_socket) = socket(vec![COMPLETED.into()]);
    let (second_record, second_socket) = socket(vec![COMPLETED.into()]);
    let connector = Connector::new(vec![first_socket, second_socket]);
    let provider = configured(true, connector.clone(), Http::completed(0));
    let first = provider.open_session().unwrap();
    let second = provider.open_session().unwrap();
    for (session, text) in [(&first, "one"), (&second, "two")] {
        successful(
            session.as_ref(),
            Request {
                model: "gpt-5.6-sol".into(),
                max_output_tokens: 4096,
                messages: vec![Message::user(text)],
                ..Request::default()
            },
        )
        .await;
    }
    let calls = connector.calls.lock().unwrap().clone();
    assert_eq!(calls.len(), 2);
    assert_ne!(
        header(&calls[0].headers, "session-id"),
        header(&calls[1].headers, "session-id")
    );
    assert_eq!(sent(&first_record).len(), 1);
    assert_eq!(sent(&second_record).len(), 1);
    first.close().await.unwrap();
    second.close().await.unwrap();
}
