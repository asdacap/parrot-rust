use parrot_domain::contracts::async_ops::{BoxFuture, Cancellation};
use parrot_provider::responses::*;
use parrot_provider::responses_websocket::*;
use serde_json::{Value, json};
use std::{
    collections::VecDeque,
    io,
    path::Path,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    time::Duration,
};

#[path = "session_projection.rs"]
pub(super) mod source_session;

struct Cancel(AtomicBool);
impl Cancellation for Cancel {
    fn is_cancelled(&self) -> bool {
        self.0.load(Ordering::SeqCst)
    }
    fn cancelled(&self) -> BoxFuture<'_, ()> {
        Box::pin(async {
            if !self.is_cancelled() {
                std::future::pending::<()>().await;
            }
        })
    }
}
struct HeadersSource(AtomicUsize);
impl HeaderSource for HeadersSource {
    fn headers<'a>(
        &'a self,
        _: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<Headers, WebSocketError>> {
        self.0.fetch_add(1, Ordering::SeqCst);
        Box::pin(async { Ok(vec![("Authorization".into(), "synthetic".into())]) })
    }
}
#[derive(Default)]
struct Wire {
    sent: Mutex<Vec<Value>>,
    reads: AtomicUsize,
    aborts: AtomicUsize,
    connects: AtomicUsize,
    timeouts: Mutex<Vec<Option<Duration>>>,
}
struct ScriptSocket {
    script: VecDeque<Result<String, WebSocketError>>,
    wire: Arc<Wire>,
    aborted: bool,
}
impl Socket for ScriptSocket {
    fn state(&self) -> SocketState {
        if self.aborted {
            SocketState::Aborted
        } else {
            SocketState::Open
        }
    }
    fn send<'a>(
        &'a mut self,
        data: &'a [u8],
        end: bool,
        _: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<(), WebSocketError>> {
        assert!(end);
        self.wire
            .sent
            .lock()
            .unwrap()
            .push(serde_json::from_slice(data).unwrap());
        Box::pin(async { Ok(()) })
    }
    fn receive<'a>(
        &'a mut self,
        buffer: &'a mut [u8],
        cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<ReceivedFrame, WebSocketError>> {
        self.wire.reads.fetch_add(1, Ordering::SeqCst);
        Box::pin(async move {
            if cancellation.is_cancelled() {
                return Err(WebSocketError::Cancelled);
            }
            let message = self.script.pop_front().expect("unexpected socket read")?;
            buffer[..message.len()].copy_from_slice(message.as_bytes());
            Ok(ReceivedFrame {
                count: message.len(),
                kind: FrameKind::Text,
                end_of_message: true,
                close_status: None,
                close_description: None,
            })
        })
    }
    fn abort(&mut self) {
        if !self.aborted {
            self.wire.aborts.fetch_add(1, Ordering::SeqCst);
            self.aborted = true;
        }
    }
}
type Script = Result<Vec<Result<String, WebSocketError>>, WebSocketError>;
struct Scripts {
    scripts: Mutex<VecDeque<Script>>,
    wire: Arc<Wire>,
    turn_state: String,
}
impl Connector for Scripts {
    fn connect<'a>(
        &'a self,
        _: &'a str,
        _: &'a Headers,
        timeout: Option<Duration>,
        _: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<ConnectedSocket, WebSocketError>> {
        self.wire.connects.fetch_add(1, Ordering::SeqCst);
        self.wire.timeouts.lock().unwrap().push(timeout);
        let script = self
            .scripts
            .lock()
            .unwrap()
            .pop_front()
            .expect("unexpected connection");
        Box::pin(async move {
            Ok(ConnectedSocket {
                socket: Box::new(ScriptSocket {
                    script: script?.into(),
                    wire: self.wire.clone(),
                    aborted: false,
                }),
                response_headers: vec![("X-Codex-Turn-State".into(), self.turn_state.clone())],
            })
        })
    }
}
#[derive(Default)]
struct Http {
    calls: Mutex<Vec<(Vec<u8>, String)>>,
}
struct HttpStream {
    sent: bool,
}
impl HttpEvents for HttpStream {
    fn next<'a>(
        &'a mut self,
        _: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<Option<ResponsesEvent>, WebSocketError>> {
        let event = if self.sent {
            None
        } else {
            self.sent = true;
            Some(ResponsesEvent::TextDelta("http".into()))
        };
        Box::pin(async move { Ok(event) })
    }
    fn take_turn_state(&mut self) -> Option<String> {
        Some("http-state".into())
    }
}
impl HttpFallback for Http {
    fn open<'a>(
        &'a self,
        body: Vec<u8>,
        state: String,
        _: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<Box<dyn HttpEvents>, WebSocketError>> {
        self.calls.lock().unwrap().push((body, state));
        Box::pin(async { Ok(Box::new(HttpStream { sent: false }) as Box<dyn HttpEvents>) })
    }
}

struct Empty;
impl ResponsesContentView for Empty {
    fn kind(&self) -> ResponsesContentKind {
        ResponsesContentKind::Text
    }
    fn text(&self) -> &str {
        ""
    }
    fn image_path(&self) -> &Path {
        Path::new("")
    }
    fn image_url(&self) -> &str {
        ""
    }
    fn image_media_type(&self) -> &str {
        ""
    }
    fn read_image(&self) -> Result<Vec<u8>, io::Error> {
        Ok(Vec::new())
    }
}
impl ResponsesToolCallView for Empty {
    fn id(&self) -> &str {
        ""
    }
    fn name(&self) -> &str {
        ""
    }
    fn arguments_json(&self) -> &str {
        "{}"
    }
}
impl ResponsesToolDefinitionView for Empty {
    fn name(&self) -> &str {
        ""
    }
    fn description(&self) -> &str {
        ""
    }
    fn parameters_json(&self) -> &str {
        "{}"
    }
}
struct Message {
    text: String,
    replay: bool,
}
impl ResponsesMessageView for Message {
    type Content = Empty;
    type ToolCall = Empty;
    fn role(&self) -> ResponsesRole {
        if self.replay {
            ResponsesRole::Assistant
        } else {
            ResponsesRole::Tool
        }
    }
    fn contents(&self) -> &[Empty] {
        &[]
    }
    fn tool_calls(&self) -> &[Empty] {
        &[]
    }
    fn tool_call_id(&self) -> &str {
        "call"
    }
    fn content(&self) -> &str {
        &self.text
    }
    fn provider_id(&self) -> &str {
        "test"
    }
    fn provider_content(&self) -> &str {
        if self.replay { &self.text } else { "" }
    }
}
struct Request {
    messages: Vec<Message>,
    instructions: String,
}
impl ResponsesRequestView for Request {
    type Message = Message;
    type Tool = Empty;
    fn model(&self) -> &str {
        "model"
    }
    fn messages(&self) -> &[Message] {
        &self.messages
    }
    fn tools(&self) -> &[Empty] {
        &[]
    }
    fn instructions(&self) -> &str {
        &self.instructions
    }
    fn max_output_tokens(&self) -> i32 {
        0
    }
    fn reasoning_effort(&self) -> &str {
        ""
    }
    fn reasoning_summary(&self) -> &str {
        ""
    }
    fn provider_preferences(&self) -> &str {
        ""
    }
    fn include_router_metadata(&self) -> bool {
        false
    }
}
fn request(items: &[&str], replay: Option<&str>, instructions: &str) -> WebSocketRequest {
    let mut messages = Vec::new();
    for (index, text) in items.iter().enumerate() {
        messages.push(Message {
            text: (*text).into(),
            replay: false,
        });
        if index == 0 {
            if let Some(replay) = replay {
                messages.push(Message {
                    text: replay.into(),
                    replay: true,
                });
            }
        }
    }
    WebSocketRequest::new(
        &prepare(
            &Request {
                messages,
                instructions: instructions.into(),
            },
            "test",
        )
        .unwrap(),
    )
    .unwrap()
}
fn completed(id: &str, output: Value) -> Result<String, WebSocketError> {
    Ok(json!({"type":"response.completed", "response":{"id":id,"status":"completed","output":output}}).to_string())
}
fn error(code: &str) -> Result<String, WebSocketError> {
    Ok(json!({"type":"error", "error":{"type":"invalid_request_error","code":code,"message":"synthetic failure"}}).to_string())
}
fn text() -> Result<String, WebSocketError> {
    Ok(json!({"type":"response.output_text.delta","delta":"visible"}).to_string())
}
fn setup(scripts: Vec<Script>, disable: bool) -> (Session, Arc<Wire>, Http, Cancel) {
    setup_options(scripts, disable, "test", None, usize::MAX)
}
fn setup_options(
    scripts: Vec<Script>,
    disable: bool,
    provider_id: &str,
    timeout: Option<Duration>,
    maximum: usize,
) -> (Session, Arc<Wire>, Http, Cancel) {
    let wire = Arc::new(Wire::default());
    let connector = Arc::new(Scripts {
        scripts: Mutex::new(scripts.into()),
        wire: wire.clone(),
        turn_state: "socket-state".into(),
    });
    let client = WebSocketClient::new(
        connector,
        "https://example.invalid/responses",
        timeout,
        None,
        maximum,
    )
    .unwrap();
    (
        Session::new(
            provider_id,
            Arc::new(HeadersSource(AtomicUsize::new(0))),
            client,
            disable,
        ),
        wire,
        Http::default(),
        Cancel(AtomicBool::new(false)),
    )
}
async fn drain(
    call: &mut SessionCall<'_>,
    cancel: &dyn Cancellation,
) -> Result<Vec<SessionEvent>, WebSocketError> {
    let mut events = Vec::new();
    while let Some(event) = call.next(cancel).await? {
        events.push(event);
    }
    Ok(events)
}

// Pinned-source unit probe (not an independent differential capture):
// ResponsesWebSocketTests.cs:140-178, especially ResponseId/Output assertions177-178.
#[tokio::test(start_paused = true)]
async fn source_pinned_resp_one_single_hello_output_drives_exact_next_suffix() {
    let delta = r#"{"type":"response.output_text.delta","delta":"hello"}"#;
    let completed = r#"{"type":"response.completed","response":{"id":"resp-1","usage":{"input_tokens":7,"output_tokens":2},"output":[{"type":"message","role":"assistant","content":[{"type":"output_text","text":"hello"}]}]}}"#;
    let input = json!({"disable_websocket":false, "provider_id":"test", "outcomes":[{
        "socket_id":"source-pinned", "headers":{}, "messages":[delta,completed,completed]
    }], "actions":[{"action":"call","messages":["hello"]},
        {"action":"call","messages":["hello","assistant:hello","next"]}]});
    let mut completions = Vec::new();
    let actual = source_session::observe_completions(&input, |completion| {
        completions.push(completion.clone())
    })
    .await;
    assert_eq!(actual["actions"][0]["exception"], Value::Null);
    assert_eq!(actual["actions"][1]["exception"], Value::Null);
    let completion = &completions[0];
    assert_eq!(completion.response_id, "resp-1");
    assert_eq!(completion.assistant_text, "hello");
    assert_eq!((completion.input_tokens, completion.output_tokens), (7, 2));
    let replay: Value =
        serde_json::from_str(completion.provider_content.as_deref().unwrap()).unwrap();
    assert_eq!(replay["Output"].as_array().unwrap().len(), 1);
    assert_eq!(
        replay["Output"][0],
        json!({"type":"message","role":"assistant","content":[{"type":"output_text","text":"hello"}]})
    );
    let requests = actual["sockets_before_session_dispose"][0]["requests"]
        .as_array()
        .unwrap();
    assert_eq!(requests.len(), 2);
    let first: Value = serde_json::from_str(requests[0]["raw_json"].as_str().unwrap()).unwrap();
    assert_eq!(first["type"], "response.create");
    assert_eq!(first["stream"], true);
    let next: Value = serde_json::from_str(requests[1]["raw_json"].as_str().unwrap()).unwrap();
    assert_eq!(next["previous_response_id"], completion.response_id);
    assert_eq!(
        next["input"],
        json!([{"type":"message","role":"user","content":[{"type":"input_text","text":"next"}]}])
    );
}

// Pinned-source interface probes: OpenAICompatibleProviderWebSocketTests.cs:750
// and ChatGptProviderWebSocketTests.cs:328,330-331. Outer retry budgets and session-ID
// construction/assertion329 remain outside this test's scope.
#[tokio::test]
async fn source_pinned_live_sessions_share_connector_but_not_http_selection_or_sockets() {
    struct SharedConnector {
        sockets: Mutex<VecDeque<Arc<Wire>>>,
        calls: AtomicUsize,
    }
    impl Connector for SharedConnector {
        fn connect<'a>(
            &'a self,
            _: &'a str,
            _: &'a Headers,
            _: Option<Duration>,
            _: &'a dyn Cancellation,
        ) -> BoxFuture<'a, Result<ConnectedSocket, WebSocketError>> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            let wire = self.sockets.lock().unwrap().pop_front().unwrap();
            Box::pin(async move {
                Ok(ConnectedSocket {
                    socket: Box::new(ScriptSocket {
                        script: vec![completed("resp-1", json!([]))].into(),
                        wire,
                        aborted: false,
                    }),
                    response_headers: vec![],
                })
            })
        }
    }
    let first_wire = Arc::new(Wire::default());
    let second_wire = Arc::new(Wire::default());
    let connector = Arc::new(SharedConnector {
        sockets: Mutex::new(vec![first_wire.clone(), second_wire.clone()].into()),
        calls: AtomicUsize::new(0),
    });
    let headers = Arc::new(HeadersSource(AtomicUsize::new(0)));
    let client = || {
        WebSocketClient::new(
            connector.clone(),
            "https://example.invalid/responses",
            None,
            None,
            usize::MAX,
        )
        .unwrap()
    };
    let mut first = Session::new("configured", headers.clone(), client(), false);
    let mut second = Session::new("configured", headers, client(), false);
    let http = Http::default();
    let cancel = Cancel(AtomicBool::new(false));
    drain(
        &mut first.call(request(&["one"], None, ""), &http).unwrap(),
        &cancel,
    )
    .await
    .unwrap();
    assert!(first.try_fall_back_to_http().unwrap());
    drain(
        &mut first.call(request(&["two"], None, ""), &http).unwrap(),
        &cancel,
    )
    .await
    .unwrap();
    drain(
        &mut second.call(request(&["three"], None, ""), &http).unwrap(),
        &cancel,
    )
    .await
    .unwrap();
    assert_eq!(connector.calls.load(Ordering::SeqCst), 2);
    assert_eq!(first_wire.sent.lock().unwrap().len(), 1);
    assert_eq!(second_wire.sent.lock().unwrap().len(), 1);
    assert_eq!(
        first_wire.sent.lock().unwrap()[0]["input"][0]["output"],
        "one"
    );
    assert_eq!(
        second_wire.sent.lock().unwrap()[0]["input"][0]["output"],
        "three"
    );
    assert!(
        second_wire.sent.lock().unwrap()[0]
            .get("previous_response_id")
            .is_none()
    );
    assert_eq!(http.calls.lock().unwrap().len(), 1);
}

// Pinned-source unit probe: OpenAICompatibleProviderWebSocketTests.cs:30-37,
// thrown-type assertions63/67/71 only. Diagnostics/log fields are owned elsewhere.
#[tokio::test]
async fn source_pinned_session_failure_categories_preserve_or_wrap_exact_types() {
    let failures = [
        WebSocketError::Socket {
            code: SocketErrorCode::ConnectionClosedPrematurely,
            message: "private-sentinel".into(),
        },
        WebSocketError::Socket {
            code: SocketErrorCode::Other,
            message: "private-sentinel".into(),
        },
        WebSocketError::Wire("private-sentinel".into()),
        WebSocketError::Upgrade {
            status: 503,
            message: "private-sentinel".into(),
        },
        WebSocketError::Transport("private-sentinel".into()),
        WebSocketError::RequestTooLarge { maximum: 4096 },
    ];
    for failure in failures {
        let (mut session, wire, http, cancel) = setup(vec![Err(failure.clone())], false);
        let actual = session
            .call(request(&["private-sentinel"], None, ""), &http)
            .unwrap()
            .next(&cancel)
            .await
            .unwrap_err();
        match failure {
            failure @ (WebSocketError::Socket { .. } | WebSocketError::Wire(_)) => assert_eq!(
                actual,
                WebSocketError::TransportFailure {
                    message: "Responses WebSocket transport failed.".into(),
                    cause: Box::new(failure),
                }
            ),
            failure => assert_eq!(actual, failure),
        }
        assert_eq!(wire.connects.load(Ordering::SeqCst), 1);
        assert!(http.calls.lock().unwrap().is_empty());
    }
}

// Supplemental actual-parser regression for the pinned generic catch boundary:
// OpenAICompatibleProviderSession.cs:230-245, WireProtocolException.cs:5 and
// ResponsesAdapter.cs:73-75,203,416. ProviderResponse stays pass-through in the
// independently captured named-error/visible-error session differential cases.
#[tokio::test]
async fn source_generic_catch_wraps_actual_t27_parser_errors_with_their_causes() {
    let cases = [
        ("[DONE]", "wire"),
        ("{", "json"),
        (
            r#"{"type":"response.completed","response":{"id":"resp-1","usage":{"input_tokens":2147483648},"output":[]}}"#,
            "integer",
        ),
    ];
    for (data, category) in cases {
        for visible in [false, true] {
            let mut messages = Vec::new();
            if visible {
                messages.push(text());
            }
            messages.push(Ok(data.into()));
            let (mut session, wire, http, cancel) = setup(
                vec![Ok(messages), Ok(vec![completed("fresh", json!([]))])],
                false,
            );
            let mut call = session.call(request(&["one"], None, ""), &http).unwrap();
            if visible {
                assert!(matches!(
                    call.next(&cancel).await.unwrap(),
                    Some(SessionEvent::Response(ResponsesEvent::TextDelta(_)))
                ));
            }
            let failure = call.next(&cancel).await.unwrap_err();
            let WebSocketError::TransportFailure { message, cause } = failure else {
                panic!("parser {category} was not wrapped");
            };
            assert_eq!(message, "Responses WebSocket transport failed.");
            assert!(match (category, cause.as_ref()) {
                ("wire", WebSocketError::Response(ResponsesError::WireProtocol(_))) => true,
                ("json", WebSocketError::Response(ResponsesError::Json(_))) => true,
                (
                    "integer",
                    WebSocketError::Response(ResponsesError::InvalidInteger { field, value }),
                ) => field == "input_tokens" && value == "2147483648",
                _ => false,
            });
            assert!(http.calls.lock().unwrap().is_empty());
            drop(call);
            drain(
                &mut session.call(request(&["two"], None, ""), &http).unwrap(),
                &cancel,
            )
            .await
            .unwrap();
            assert_eq!(http.calls.lock().unwrap().len(), usize::from(visible));
            assert_eq!(
                wire.connects.load(Ordering::SeqCst),
                if visible { 1 } else { 2 }
            );
        }
    }
}

// Pinned caller-boundary assertions only: OpenAICompatibleProviderWebSocketTests
// 110-111 and ChatGptProviderWebSocketTests36-37; no native timeout policy claim.
#[tokio::test]
async fn source_pinned_zero_connect_timeout_reaches_connector_unchanged() {
    for provider_id in ["configured", "chatgpt"] {
        let (mut session, wire, http, cancel) = setup_options(
            vec![Ok(vec![completed("resp-1", json!([]))])],
            false,
            provider_id,
            Some(Duration::ZERO),
            usize::MAX,
        );
        let request = source_session::request(
            &json!({"messages":["hello"]}),
            None,
            provider_id,
            provider_id == "chatgpt",
        );
        let events = drain(&mut session.call(request, &http).unwrap(), &cancel)
            .await
            .unwrap();
        assert!(matches!(
            events.last(),
            Some(SessionEvent::Response(ResponsesEvent::Completed(_)))
        ));
        assert_eq!(*wire.timeouts.lock().unwrap(), vec![Some(Duration::ZERO)]);
        assert_eq!(wire.connects.load(Ordering::SeqCst), 1);
        assert!(http.calls.lock().unwrap().is_empty());
    }
}

// Exact configured limit1 and prepared model/userhello: OpenAI source269/271/272
// and ChatGPT156/158/159. This exercises WS session rejection, not native HTTP.
#[tokio::test]
async fn source_pinned_one_byte_request_limit_rejects_websocket_before_payload() {
    for provider_id in ["configured", "chatgpt"] {
        let (mut session, wire, http, cancel) =
            setup_options(vec![Ok(vec![])], false, provider_id, None, 1);
        let request = source_session::request(
            &json!({"messages":["hello"]}),
            None,
            provider_id,
            provider_id == "chatgpt",
        );
        let prepared: Value = serde_json::from_slice(request.http_body()).unwrap();
        assert_eq!(prepared["model"], "model");
        assert_eq!(prepared["input"][0]["content"][0]["text"], "hello");
        let failure = session
            .call(request, &http)
            .unwrap()
            .next(&cancel)
            .await
            .unwrap_err();
        assert_eq!(failure, WebSocketError::RequestTooLarge { maximum: 1 });
        assert!(failure.to_string().contains("exceeds 1 bytes"));
        assert_eq!(wire.connects.load(Ordering::SeqCst), 1);
        assert!(wire.sent.lock().unwrap().is_empty());
        assert!(http.calls.lock().unwrap().is_empty());
    }
}

// Route/error/no-replay probes for the same source configured-limit rows. The
// HTTP bound below is synthetic HttpFallback behavior: actual HTTP enforcement
// remains T25/T30 and is deliberately not claimed by this test.
#[tokio::test]
async fn source_pinned_http_limit_error_routes_without_websocket_or_error_replay() {
    struct OneByteHttp(AtomicUsize);
    impl HttpFallback for OneByteHttp {
        fn open<'a>(
            &'a self,
            body: Vec<u8>,
            _: String,
            _: &'a dyn Cancellation,
        ) -> BoxFuture<'a, Result<Box<dyn HttpEvents>, WebSocketError>> {
            self.0.fetch_add(1, Ordering::SeqCst);
            assert!(body.len() > 1);
            Box::pin(async { Err(WebSocketError::RequestTooLarge { maximum: 1 }) })
        }
    }
    for provider_id in ["configured", "chatgpt"] {
        for fallback in [false, true] {
            let outcomes = if fallback {
                vec![Err(WebSocketError::Upgrade {
                    status: 404,
                    message: "missing".into(),
                })]
            } else {
                vec![]
            };
            let (mut session, wire, _, cancel) =
                setup_options(outcomes, !fallback, provider_id, None, 1);
            let http = OneByteHttp(AtomicUsize::new(0));
            let request = source_session::request(
                &json!({"messages":["hello"]}),
                None,
                provider_id,
                provider_id == "chatgpt",
            );
            let mut call = session.call(request, &http).unwrap();
            if fallback {
                assert!(matches!(
                    call.next(&cancel).await.unwrap(),
                    Some(SessionEvent::Retry { attempt: 1, .. })
                ));
            }
            let failure = call.next(&cancel).await.unwrap_err();
            assert_eq!(failure, WebSocketError::RequestTooLarge { maximum: 1 });
            assert!(failure.to_string().contains("exceeds 1 bytes"));
            assert_eq!(call.next(&cancel).await, Ok(None));
            assert_eq!(http.0.load(Ordering::SeqCst), 1);
            assert_eq!(wire.connects.load(Ordering::SeqCst), usize::from(fallback));
            assert!(wire.sent.lock().unwrap().is_empty());
        }
    }
}

#[tokio::test]
async fn default_http_captures_first_turn_state_and_begin_turn_clears_it() {
    let (mut session, wire, http, cancel) = setup(vec![], true);
    for _ in 0..2 {
        drain(
            &mut session.call(request(&["one"], None, ""), &http).unwrap(),
            &cancel,
        )
        .await
        .unwrap();
    }
    session.begin_turn();
    drain(
        &mut session.call(request(&["two"], None, ""), &http).unwrap(),
        &cancel,
    )
    .await
    .unwrap();
    let calls = http.calls.lock().unwrap();
    assert_eq!(
        calls
            .iter()
            .map(|(_, state)| state.as_str())
            .collect::<Vec<_>>(),
        ["", "http-state", ""]
    );
    assert_eq!(wire.connects.load(Ordering::SeqCst), 0);
    assert_eq!(
        serde_json::from_slice::<Value>(&calls[0].0).unwrap()["input"][0]["output"],
        "one"
    );
}

#[tokio::test]
async fn continuation_uses_completed_raw_output_prefix_and_only_sends_suffix() {
    let output = json!([{"type":"message","id":"out","role":"assistant","content":[{"type":"output_text","text":"answer"}]}]);
    let (mut session, wire, http, cancel) = setup(
        vec![Ok(vec![
            completed("r1", output),
            completed("r2", json!([])),
        ])],
        false,
    );
    let events = drain(
        &mut session.call(request(&["one"], None, ""), &http).unwrap(),
        &cancel,
    )
    .await
    .unwrap();
    let replay = events
        .into_iter()
        .find_map(|event| match event {
            SessionEvent::Response(ResponsesEvent::Completed(completion)) => {
                completion.provider_content
            }
            _ => None,
        })
        .unwrap();
    drain(
        &mut session
            .call(request(&["one", "two"], Some(&replay), ""), &http)
            .unwrap(),
        &cancel,
    )
    .await
    .unwrap();
    let sent = wire.sent.lock().unwrap();
    assert!(sent[0].get("previous_response_id").is_none());
    assert_eq!(sent[1]["previous_response_id"], "r1");
    assert_eq!(sent[1]["input"].as_array().unwrap().len(), 1);
    assert_eq!(sent[1]["input"][0]["output"], "two");
    assert_eq!(
        sent[1]["client_metadata"]["x-codex-turn-state"],
        "socket-state"
    );
    assert_eq!(wire.connects.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn changed_settings_or_non_growing_history_sends_full_request() {
    for (items, instructions) in [
        (vec!["one"], ""),
        (vec!["one", "two"], "changed"),
        (vec!["different", "two"], ""),
    ] {
        let (mut session, wire, http, cancel) = setup(
            vec![Ok(vec![
                completed("r1", json!([])),
                completed("r2", json!([])),
            ])],
            false,
        );
        drain(
            &mut session.call(request(&["one"], None, ""), &http).unwrap(),
            &cancel,
        )
        .await
        .unwrap();
        drain(
            &mut session
                .call(request(&items, None, instructions), &http)
                .unwrap(),
            &cancel,
        )
        .await
        .unwrap();
        let sent = wire.sent.lock().unwrap();
        assert!(sent[1].get("previous_response_id").is_none());
        assert_eq!(sent[1]["input"].as_array().unwrap().len(), items.len());
    }
}

#[tokio::test]
async fn named_recovery_is_once_full_and_fresh_without_replaying_output() {
    for code in [
        "previous_response_not_found",
        "websocket_connection_limit_reached",
    ] {
        let (mut session, wire, http, cancel) = setup(
            vec![
                Ok(vec![completed("r1", json!([])), error(code)]),
                Ok(vec![completed("r2", json!([]))]),
            ],
            false,
        );
        drain(
            &mut session.call(request(&["one"], None, ""), &http).unwrap(),
            &cancel,
        )
        .await
        .unwrap();
        let events = drain(
            &mut session
                .call(request(&["one", "two"], None, ""), &http)
                .unwrap(),
            &cancel,
        )
        .await
        .unwrap();
        assert!(matches!(
            events.first(),
            Some(SessionEvent::Retry { attempt: 1, .. })
        ));
        let sent = wire.sent.lock().unwrap();
        assert_eq!(sent[1]["previous_response_id"], "r1");
        assert!(sent[2].get("previous_response_id").is_none());
        assert_eq!(sent[2]["input"].as_array().unwrap().len(), 2);
        assert_eq!(wire.connects.load(Ordering::SeqCst), 2);
        assert!(http.calls.lock().unwrap().is_empty());
    }
}

#[tokio::test]
async fn generic_socket_errors_preserve_cause_and_existing_transport_errors_pass_through() {
    let socket = WebSocketError::Socket {
        code: SocketErrorCode::Faulted,
        message: "synthetic socket failure".into(),
    };
    let wrapped = WebSocketError::TransportFailure {
        message: "existing transport".into(),
        cause: Box::new(socket.clone()),
    };
    for failure in [
        socket,
        WebSocketError::InvalidOperation("synthetic invalid operation".into()),
        wrapped.clone(),
    ] {
        let (mut session, _, http, cancel) = setup(vec![Ok(vec![Err(failure.clone())])], false);
        let actual = session
            .call(request(&["one"], None, ""), &http)
            .unwrap()
            .next(&cancel)
            .await
            .unwrap_err();
        if failure == wrapped {
            assert_eq!(actual, wrapped);
        } else {
            assert_eq!(
                actual,
                WebSocketError::TransportFailure {
                    message: "Responses WebSocket transport failed.".into(),
                    cause: Box::new(failure)
                }
            );
        }
        assert!(http.calls.lock().unwrap().is_empty());
    }
}

#[tokio::test]
async fn second_named_error_is_not_recovered_again() {
    let (mut session, wire, http, cancel) = setup(
        vec![
            Ok(vec![error("previous_response_not_found")]),
            Ok(vec![error("previous_response_not_found")]),
        ],
        false,
    );
    let mut call = session.call(request(&["one"], None, ""), &http).unwrap();
    assert!(matches!(
        call.next(&cancel).await.unwrap(),
        Some(SessionEvent::Retry { attempt: 1, .. })
    ));
    assert!(matches!(
        call.next(&cancel).await,
        Err(WebSocketError::Response(_))
    ));
    assert_eq!(wire.connects.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn unsupported_upgrade_selects_sticky_http_but_other_status_throws() {
    for status in [404, 405, 426, 403, 500] {
        let (mut session, wire, http, cancel) = setup(
            vec![Err(WebSocketError::Upgrade {
                status,
                message: "upgrade failed".into(),
            })],
            false,
        );
        let mut call = session.call(request(&["one"], None, ""), &http).unwrap();
        if matches!(status, 404 | 405 | 426) {
            assert!(matches!(
                call.next(&cancel).await.unwrap(),
                Some(SessionEvent::Retry { attempt: 1, .. })
            ));
            assert!(
                matches!(call.next(&cancel).await.unwrap(), Some(SessionEvent::Response(ResponsesEvent::TextDelta(text))) if text == "http")
            );
            drain(&mut call, &cancel).await.unwrap();
            drop(call);
            drain(
                &mut session.call(request(&["two"], None, ""), &http).unwrap(),
                &cancel,
            )
            .await
            .unwrap();
            assert_eq!(http.calls.lock().unwrap().len(), 2);
        } else {
            assert!(matches!(
                call.next(&cancel).await,
                Err(WebSocketError::Upgrade { .. })
            ));
            assert!(http.calls.lock().unwrap().is_empty());
        }
        assert_eq!(wire.connects.load(Ordering::SeqCst), 1);
    }
}

#[tokio::test]
async fn visible_failure_throws_without_replay_and_only_future_calls_use_http() {
    for failure in [
        error("previous_response_not_found"),
        Err(WebSocketError::Transport("broken".into())),
    ] {
        let (mut session, wire, http, cancel) = setup(vec![Ok(vec![text(), failure])], false);
        let mut call = session.call(request(&["one"], None, ""), &http).unwrap();
        assert!(
            matches!(call.next(&cancel).await.unwrap(), Some(SessionEvent::Response(ResponsesEvent::TextDelta(text))) if text == "visible")
        );
        assert_eq!(
            wire.reads.load(Ordering::SeqCst),
            1,
            "first event must not wait for the later failure"
        );
        assert!(call.next(&cancel).await.is_err());
        assert!(http.calls.lock().unwrap().is_empty());
        drop(call);
        drain(
            &mut session.call(request(&["two"], None, ""), &http).unwrap(),
            &cancel,
        )
        .await
        .unwrap();
        assert_eq!(http.calls.lock().unwrap().len(), 1);
        assert_eq!(wire.connects.load(Ordering::SeqCst), 1);
    }
}

#[tokio::test]
async fn invisible_transport_failure_and_cancellation_keep_websockets_enabled() {
    for cancelled in [false, true] {
        let failure = if cancelled {
            WebSocketError::Cancelled
        } else {
            WebSocketError::Transport("broken".into())
        };
        let (mut session, wire, http, cancel) = setup(
            vec![
                Ok(vec![Err(failure.clone())]),
                Ok(vec![completed("fresh", json!([]))]),
            ],
            false,
        );
        assert_eq!(
            session
                .call(request(&["one"], None, ""), &http)
                .unwrap()
                .next(&cancel)
                .await,
            Err(failure)
        );
        drain(
            &mut session.call(request(&["two"], None, ""), &http).unwrap(),
            &cancel,
        )
        .await
        .unwrap();
        assert_eq!(wire.connects.load(Ordering::SeqCst), 2);
        assert!(http.calls.lock().unwrap().is_empty());
    }
}

#[tokio::test]
async fn cancellation_after_visible_output_does_not_make_http_sticky() {
    let (mut session, wire, http, cancel) = setup(
        vec![Ok(vec![text()]), Ok(vec![completed("fresh", json!([]))])],
        false,
    );
    let mut call = session.call(request(&["one"], None, ""), &http).unwrap();
    call.next(&cancel).await.unwrap();
    cancel.0.store(true, Ordering::SeqCst);
    assert_eq!(call.next(&cancel).await, Err(WebSocketError::Cancelled));
    drop(call);
    cancel.0.store(false, Ordering::SeqCst);
    drain(
        &mut session.call(request(&["two"], None, ""), &http).unwrap(),
        &cancel,
    )
    .await
    .unwrap();
    assert_eq!(wire.connects.load(Ordering::SeqCst), 2);
    assert!(http.calls.lock().unwrap().is_empty());
}

#[tokio::test]
async fn abandoning_active_call_poisoned_connection_without_forcing_http() {
    let (mut session, wire, http, cancel) = setup(
        vec![Ok(vec![text()]), Ok(vec![completed("fresh", json!([]))])],
        false,
    );
    let mut call = session.call(request(&["one"], None, ""), &http).unwrap();
    call.next(&cancel).await.unwrap();
    drop(call);
    assert_eq!(wire.aborts.load(Ordering::SeqCst), 1);
    drain(
        &mut session.call(request(&["two"], None, ""), &http).unwrap(),
        &cancel,
    )
    .await
    .unwrap();
    assert_eq!(wire.connects.load(Ordering::SeqCst), 2);
}

#[tokio::test(start_paused = true)]
async fn begin_turn_and_sixty_second_staleness_reset_without_retry() {
    for begin_turn in [false, true] {
        let (mut session, wire, http, cancel) = setup(
            vec![
                Ok(vec![completed("r1", json!([]))]),
                Ok(vec![completed("r2", json!([]))]),
            ],
            false,
        );
        drain(
            &mut session.call(request(&["one"], None, ""), &http).unwrap(),
            &cancel,
        )
        .await
        .unwrap();
        if begin_turn {
            session.begin_turn();
        } else {
            tokio::time::advance(Duration::from_secs(60)).await;
        }
        let events = drain(
            &mut session
                .call(request(&["one", "two"], None, ""), &http)
                .unwrap(),
            &cancel,
        )
        .await
        .unwrap();
        assert!(
            !events
                .iter()
                .any(|event| matches!(event, SessionEvent::Retry { .. }))
        );
        let sent = wire.sent.lock().unwrap();
        assert!(sent[1].get("previous_response_id").is_none());
        assert_eq!(wire.connects.load(Ordering::SeqCst), 2);
    }
}

#[tokio::test]
async fn local_message_limit_selects_http_before_sending_and_preserves_body() {
    let (mut session, wire, http, cancel) = setup(vec![Ok(vec![])], false);
    let large = "x".repeat(MAXIMUM_MESSAGE_BYTES);
    let request = request(&[&large], None, "");
    let expected = request.http_body().to_vec();
    let mut call = session.call(request, &http).unwrap();
    assert!(
        matches!(call.next(&cancel).await.unwrap(), Some(SessionEvent::Retry { attempt: 1, reason }) if reason.contains("exceeds"))
    );
    drain(&mut call, &cancel).await.unwrap();
    assert!(wire.sent.lock().unwrap().is_empty());
    let calls = http.calls.lock().unwrap();
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].0, expected);
    assert_eq!(calls[0].1, "socket-state");
}

#[tokio::test]
async fn explicit_http_selection_and_disposal_are_sticky_and_idempotent() {
    let (mut session, wire, http, cancel) = setup(vec![], false);
    assert!(session.try_fall_back_to_http().unwrap());
    drain(
        &mut session.call(request(&["one"], None, ""), &http).unwrap(),
        &cancel,
    )
    .await
    .unwrap();
    session.dispose();
    session.dispose();
    assert!(matches!(
        session.call(request(&["two"], None, ""), &http),
        Err(WebSocketError::Disposed)
    ));
    assert_eq!(
        session.try_fall_back_to_http(),
        Err(WebSocketError::Disposed)
    );
    assert_eq!(wire.connects.load(Ordering::SeqCst), 0);
}
