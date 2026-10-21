use parrot_domain::contracts::async_ops::{BoxFuture, Cancellation};
use parrot_provider::responses::ResponsesEvent;
use parrot_provider::responses_websocket::*;
use std::sync::{Arc, Mutex};

struct Cancel(bool);
impl Cancellation for Cancel {
    fn is_cancelled(&self) -> bool {
        self.0
    }
    fn cancelled(&self) -> BoxFuture<'_, ()> {
        Box::pin(std::future::pending())
    }
}
#[derive(Default)]
struct Trace(Mutex<Vec<String>>, Mutex<Vec<Vec<u8>>>);
impl Trace {
    fn push(&self, value: impl Into<String>) {
        self.0.lock().unwrap().push(value.into());
    }
    fn values(&self) -> Vec<String> {
        self.0.lock().unwrap().clone()
    }
}
struct Observer(Arc<Trace>);
impl WebSocketObserver for Observer {
    fn start(&self) -> Box<dyn WebSocketAttemptObserver> {
        self.0.push("start");
        Box::new(Observer(self.0.clone()))
    }
}
impl WebSocketAttemptObserver for Observer {
    fn dump_request(&mut self, body: &[u8]) {
        self.0.1.lock().unwrap().push(body.to_vec());
        self.0.push(format!("dump:{}", body.len()));
    }
    fn record_request_bytes(&mut self, bytes: u64) {
        self.0.push(format!("request:{bytes}"));
    }
    fn record_response_bytes(&mut self, bytes: u64) {
        self.0.push(format!("response:{bytes}"));
    }
    fn record_close_status(&mut self, status: u16) {
        self.0.push(format!("close:{status}"));
    }
    fn finish(&mut self, outcome: WebSocketAttemptOutcome<'_>) {
        self.0.push(match outcome {
            WebSocketAttemptOutcome::Completed => "completed".into(),
            WebSocketAttemptOutcome::Disposed => "disposed".into(),
            WebSocketAttemptOutcome::Failed(error) => format!("failed:{error:?}"),
        });
    }
}
struct Auth(Arc<Trace>);
impl HeaderSource for Auth {
    fn headers<'a>(
        &'a self,
        _: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<Headers, WebSocketError>> {
        self.0.push("auth");
        Box::pin(async { Ok(Vec::new()) })
    }
}
struct Connect(Arc<Trace>, Result<(), WebSocketError>, u8);
impl Connector for Connect {
    fn connect<'a>(
        &'a self,
        _: &'a str,
        _: &'a Headers,
        _: Option<std::time::Duration>,
        _: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<ConnectedSocket, WebSocketError>> {
        self.0.push("connect");
        let result = self.1.clone();
        let trace = self.0.clone();
        let mode = if self.2 == 2 && trace.values().iter().filter(|x| *x == "connect").count() > 1 {
            0
        } else {
            self.2
        };
        Box::pin(async move {
            result?;
            Ok(ConnectedSocket {
                socket: Box::new(TestSocket(trace, false, mode)),
                response_headers: Vec::new(),
            })
        })
    }
}
struct TestSocket(Arc<Trace>, bool, u8);
const TERMINAL: &str =
    r#"{"type":"response.completed","response":{"id":"r","status":"completed","output":[]}}"#;
impl Socket for TestSocket {
    fn state(&self) -> SocketState {
        SocketState::Open
    }
    fn send<'a>(
        &'a mut self,
        data: &'a [u8],
        _: bool,
        _: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<(), WebSocketError>> {
        assert_eq!(self.0.1.lock().unwrap().last().unwrap(), data);
        self.0.push("send");
        Box::pin(async { Ok(()) })
    }
    fn receive<'a>(
        &'a mut self,
        buffer: &'a mut [u8],
        _: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<ReceivedFrame, WebSocketError>> {
        self.0.push("receive");
        let (body, kind, end, close_status) = match (self.2, self.1) {
            (1, false) => ("{", FrameKind::Text, false, None),
            (1, true) => ("", FrameKind::Close, true, Some(1009)),
            (2, _) => (
                r#"{"type":"error","code":"previous_response_not_found","message":"missing"}"#,
                FrameKind::Text,
                true,
                None,
            ),
            _ => (TERMINAL, FrameKind::Text, true, None),
        };
        self.1 = true;
        buffer[..body.len()].copy_from_slice(body.as_bytes());
        Box::pin(async move {
            Ok(ReceivedFrame {
                count: body.len(),
                kind,
                end_of_message: end,
                close_status,
                close_description: None,
            })
        })
    }
    fn abort(&mut self) {
        self.0.push("abort");
    }
}
struct Http(Arc<Trace>);
struct Empty;
impl HttpEvents for Empty {
    fn next<'a>(
        &'a mut self,
        _: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<Option<ResponsesEvent>, WebSocketError>> {
        Box::pin(async { Ok(None) })
    }
    fn take_turn_state(&mut self) -> Option<String> {
        None
    }
}
impl HttpFallback for Http {
    fn open_unprepared<'a>(
        &'a self,
        _: String,
        _: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<Box<dyn HttpEvents>, WebSocketError>> {
        self.0.push("http-original");
        Box::pin(async { Ok(Box::new(Empty) as Box<dyn HttpEvents>) })
    }
    fn open<'a>(
        &'a self,
        _: Vec<u8>,
        _: String,
        _: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<Box<dyn HttpEvents>, WebSocketError>> {
        self.0.push("http-prepared");
        Box::pin(async { Ok(Box::new(Empty) as Box<dyn HttpEvents>) })
    }
}
fn session(trace: &Arc<Trace>, http_only: bool, result: Result<(), WebSocketError>) -> Session {
    session_config(trace, http_only, result, usize::MAX, 0)
}
fn session_config(
    trace: &Arc<Trace>,
    http_only: bool,
    result: Result<(), WebSocketError>,
    maximum: usize,
    mode: u8,
) -> Session {
    Session::new(
        "p",
        Arc::new(Auth(trace.clone())),
        WebSocketClient::new(
            Arc::new(Connect(trace.clone(), result, mode)),
            "https://example.test/v1/responses",
            None,
            None,
            maximum,
        )
        .unwrap(),
        http_only,
    )
}
fn request() -> WebSocketRequest {
    super::session_tests::source_session::request(
        &serde_json::json!({"messages":[{"role":"user","text":"hello"}]}),
        None,
        "p",
        false,
    )
}

// Numeric bodies and error types differ between adapters; compare only the
// common stable observer ordering directly with the independent actual Trace.
fn source_case(id: &str) -> serde_json::Value {
    let oracle: serde_json::Value =
        serde_json::from_str(include_str!("fixtures/http-lifecycle/seams/expected.json")).unwrap();
    assert_eq!(
        oracle["source_commit"],
        "5f27a8a92625ee31d8bec094e5c3e09550193781"
    );
    oracle["scenarios"]
        .as_array()
        .unwrap()
        .iter()
        .find(|item| item["case_id"] == id)
        .unwrap()
        .clone()
}
fn assert_source_observer(id: &str, call: usize, values: &[String]) {
    let case = source_case(id);
    let previous = if call == 0 {
        Vec::new()
    } else {
        case["calls"][call - 1]["steps"]
            .as_array()
            .unwrap()
            .last()
            .unwrap()["snapshot"]["effects"]
            .as_array()
            .unwrap()
            .clone()
    };
    let call = &case["calls"][call];
    let before = call["after_call_factory"]["effects"].as_array().unwrap();
    assert_eq!(before, &previous, "source factories have no effects");
    assert_eq!(
        before,
        call["after_enumerator_factory"]["effects"]
            .as_array()
            .unwrap()
    );
    let effects = call["steps"].as_array().unwrap().last().unwrap()["snapshot"]["effects"]
        .as_array()
        .unwrap();
    let current = &effects[before.len()..];
    let position = |name: &str| current.iter().position(|effect| effect["effect"] == name);
    if let (Some(prepare), Some(auth)) = (position("prepare"), position("auth_headers")) {
        let start = current
            .iter()
            .position(|effect| effect["operation"] == "request_started")
            .unwrap();
        assert!(prepare < start && start < auth);
    }
    if let Some(raw) = position("raw_error") {
        let finish = current
            .iter()
            .position(|effect| effect["operation"] == "request_finished")
            .unwrap();
        assert!(raw < finish);
    }
    let mut websocket = false;
    let labels: Vec<_> = effects[before.len()..]
        .iter()
        .filter_map(|effect| match effect["effect"].as_str().unwrap() {
            "http_callback" => {
                assert_eq!(effect["same_request"], true);
                Some("http-original")
            }
            "source_diagnostic" => {
                websocket = effect["transport"] == "websocket";
                if !websocket {
                    return None;
                }
                match effect["operation"].as_str().unwrap() {
                    "request_started" => Some("start"),
                    "request_size" => Some("request"),
                    "request_finished" => effect["outcome"].as_str(),
                    _ => None,
                }
            }
            "dump_request" if websocket => Some("dump"),
            "socket_send_frame" if websocket => Some("send"),
            _ => None,
        })
        .collect();
    let actual: Vec<_> = values
        .iter()
        .filter_map(|value| {
            let label = value.split(':').next().unwrap();
            matches!(
                label,
                "start"
                    | "dump"
                    | "request"
                    | "send"
                    | "failed"
                    | "completed"
                    | "disposed"
                    | "http-original"
            )
            .then_some(label)
        })
        .collect();
    assert_eq!(actual, labels, "source case {id}");
}

#[tokio::test]
async fn lazy_http_and_unpulled_calls_never_prepare_or_observe() {
    let trace = Arc::new(Trace::default());
    let http = Http(trace.clone());
    let mut session = session(&trace, true, Ok(()));
    let mut call = session
        .call_lazy(|| panic!("HTTP must not prepare"), &http)
        .unwrap()
        .with_websocket_observer(Arc::new(Observer(trace.clone())));
    assert_eq!(call.next(&Cancel(false)).await.unwrap(), None);
    drop(call);
    assert_eq!(trace.values(), ["http-original"]);
    assert_source_observer("http-only-original-callback-lazy", 0, &trace.values());
    let mut session = self::session(&trace, false, Ok(()));
    drop(
        session
            .call_lazy(|| panic!("unpulled"), &http)
            .unwrap()
            .with_websocket_observer(Arc::new(Observer(trace.clone()))),
    );
    assert_eq!(trace.values(), ["http-original"]);
}

#[tokio::test]
async fn precancel_and_prepare_failure_do_not_start_attempts() {
    for cancelled in [true, false] {
        let trace = Arc::new(Trace::default());
        let http = Http(trace.clone());
        let mut session = session(&trace, false, Ok(()));
        let prepared = trace.clone();
        let mut call = session
            .call_lazy(
                move || {
                    prepared.push("prepare");
                    Err(WebSocketError::InvalidRequest("bad".into()))
                },
                &http,
            )
            .unwrap()
            .with_websocket_observer(Arc::new(Observer(trace.clone())));
        assert_eq!(
            call.next(&Cancel(cancelled)).await.unwrap_err(),
            if cancelled {
                WebSocketError::Cancelled
            } else {
                WebSocketError::InvalidRequest("bad".into())
            }
        );
        drop(call);
        assert_source_observer(
            if cancelled {
                "pre-cancelled-first-pull"
            } else {
                "websocket-prepare-throws-before-auth"
            },
            0,
            &trace.values(),
        );
        assert_eq!(
            trace.values(),
            if cancelled {
                Vec::<String>::new()
            } else {
                vec!["prepare".into()]
            }
        );
    }
}

#[tokio::test]
async fn connect_failure_finishes_raw_after_prepare_and_auth() {
    let trace = Arc::new(Trace::default());
    let http = Http(trace.clone());
    let mut session = session(
        &trace,
        false,
        Err(WebSocketError::Wire("connect failed".into())),
    );
    let prepared = trace.clone();
    let mut call = session
        .call_lazy(
            move || {
                prepared.push("prepare");
                Ok(request())
            },
            &http,
        )
        .unwrap()
        .with_websocket_observer(Arc::new(Observer(trace.clone())));
    assert!(matches!(
        call.next(&Cancel(false)).await,
        Err(WebSocketError::TransportFailure { .. })
    ));
    drop(call);
    assert_source_observer("websocket-connect-failure-raw-trace", 0, &trace.values());
    assert_eq!(
        trace.values(),
        [
            "prepare",
            "start",
            "auth",
            "connect",
            "failed:Wire(\"connect failed\")"
        ]
    );
}

#[tokio::test]
async fn terminal_yield_disposal_differs_from_exhaustion_and_reuses_connection() {
    for exhaust in [false, true] {
        let trace = Arc::new(Trace::default());
        let http = Http(trace.clone());
        let mut session = session(&trace, false, Ok(()));
        for call_index in 0..2 {
            let offset = trace.values().len();
            let mut call = session
                .call_lazy(|| Ok(request()), &http)
                .unwrap()
                .with_websocket_observer(Arc::new(Observer(trace.clone())));
            loop {
                if matches!(
                    call.next(&Cancel(false)).await.unwrap(),
                    Some(SessionEvent::Response(ResponsesEvent::Completed(_)))
                ) {
                    break;
                }
            }
            assert!(!trace.values().last().unwrap().contains("completed"));
            if exhaust {
                assert_eq!(call.next(&Cancel(false)).await.unwrap(), None);
            }
            drop(call);
            if exhaust || call_index == 0 {
                assert_source_observer(
                    if exhaust {
                        "websocket-reuse-exhausted-trace"
                    } else {
                        "terminal-yield-disposed-trace"
                    },
                    call_index,
                    &trace.values()[offset..],
                );
            }
            assert_eq!(
                trace.values().last().unwrap(),
                if exhaust { "completed" } else { "disposed" }
            );
        }
        let values = trace.values();
        assert_eq!(values.iter().filter(|x| *x == "connect").count(), 1);
        assert_eq!(values.iter().filter(|x| *x == "start").count(), 2);
        assert_eq!(
            values.iter().filter(|x| x.starts_with("response:")).count(),
            2
        );
    }
}

#[tokio::test]
async fn partial_close_counts_received_bytes_and_raw_close_before_failure() {
    let trace = Arc::new(Trace::default());
    let http = Http(trace.clone());
    let mut session = session_config(&trace, false, Ok(()), usize::MAX, 1);
    let mut call = session
        .call_lazy(|| Ok(request()), &http)
        .unwrap()
        .with_websocket_observer(Arc::new(Observer(trace.clone())));
    assert!(matches!(
        call.next(&Cancel(false)).await,
        Err(WebSocketError::TransportFailure { .. })
    ));
    drop(call);
    assert_source_observer("partial-message-close-byte-and-status", 0, &trace.values());
    let values = trace.values();
    assert_eq!(
        values
            .iter()
            .filter(|x| x.starts_with("response:"))
            .cloned()
            .collect::<Vec<_>>(),
        ["response:1"]
    );
    let close = values.iter().position(|x| x == "close:1009").unwrap();
    let failed = values
        .iter()
        .position(|x| x.starts_with("failed:Wire("))
        .unwrap();
    assert!(close < failed);
    assert!(values[failed].contains("1009"));
    assert!(!values.iter().any(|x| x == "disposed" || x == "completed"));
}

#[tokio::test]
async fn configured_limit_precedes_request_counter_and_message_limit_falls_back_original() {
    for configured in [true, false] {
        let trace = Arc::new(Trace::default());
        let http = Http(trace.clone());
        let mut session = session_config(
            &trace,
            false,
            Ok(()),
            if configured { 1 } else { usize::MAX },
            0,
        );
        let mut call = session
            .call_lazy(
                move || {
                    if configured {
                        Ok(request())
                    } else {
                        Ok(super::session_tests::source_session::request(
                            &serde_json::json!({"message_text_bytes":MAXIMUM_MESSAGE_BYTES + 1}),
                            None,
                            "p",
                            false,
                        ))
                    }
                },
                &http,
            )
            .unwrap()
            .with_websocket_observer(Arc::new(Observer(trace.clone())));
        let result = call.next(&Cancel(false)).await;
        assert_source_observer(
            if configured {
                "configured-limit-before-request-byte-record"
            } else {
                "message-limit-after-request-byte-record"
            },
            0,
            &trace.values(),
        );
        if configured {
            assert_eq!(
                result.unwrap_err(),
                WebSocketError::RequestTooLarge { maximum: 1 }
            );
        } else {
            assert!(matches!(result.unwrap(), Some(SessionEvent::Retry { .. })));
            assert_eq!(call.next(&Cancel(false)).await.unwrap(), None);
        }
        drop(call);
        let values = trace.values();
        assert!(values.iter().any(|x| x.starts_with("dump:")));
        assert_eq!(
            values.iter().filter(|x| x.starts_with("request:")).count(),
            usize::from(!configured)
        );
        assert!(!values.iter().any(|x| x == "send"));
        if !configured {
            assert_eq!(values.last().unwrap(), "http-original");
        }
    }
}

#[tokio::test]
async fn named_recovery_gets_fresh_observer_without_repreparation() {
    let trace = Arc::new(Trace::default());
    let http = Http(trace.clone());
    let mut session = session_config(&trace, false, Ok(()), usize::MAX, 2);
    let prepared = trace.clone();
    let mut call = session
        .call_lazy(
            move || {
                prepared.push("prepare");
                Ok(request())
            },
            &http,
        )
        .unwrap()
        .with_websocket_observer(Arc::new(Observer(trace.clone())));
    assert!(matches!(
        call.next(&Cancel(false)).await.unwrap(),
        Some(SessionEvent::Retry { .. })
    ));
    while call.next(&Cancel(false)).await.unwrap().is_some() {}
    drop(call);
    assert_source_observer("named-recovery-full-and-fresh", 1, &trace.values());
    let values = trace.values();
    assert_eq!(values.iter().filter(|x| *x == "prepare").count(), 1);
    assert_eq!(values.iter().filter(|x| *x == "start").count(), 2);
    assert_eq!(
        values
            .iter()
            .filter(|x| x.starts_with("failed:Response("))
            .count(),
        1
    );
    assert_eq!(values.iter().filter(|x| *x == "completed").count(), 1);
}

#[tokio::test]
async fn unsupported_upgrade_falls_back_to_original_http_callback_once() {
    let trace = Arc::new(Trace::default());
    let http = Http(trace.clone());
    let mut session = session(
        &trace,
        false,
        Err(WebSocketError::Upgrade {
            status: 404,
            message: "unsupported".into(),
        }),
    );
    let mut call = session
        .call_lazy(|| Ok(request()), &http)
        .unwrap()
        .with_websocket_observer(Arc::new(Observer(trace.clone())));
    assert!(matches!(
        call.next(&Cancel(false)).await.unwrap(),
        Some(SessionEvent::Retry { .. })
    ));
    assert_eq!(call.next(&Cancel(false)).await.unwrap(), None);
    drop(call);
    assert_source_observer(
        "unsupported-upgrade-original-http-callback",
        0,
        &trace.values(),
    );
    assert_eq!(
        trace.values(),
        [
            "start",
            "auth",
            "connect",
            "failed:Upgrade { status: 404, message: \"unsupported\" }",
            "http-original"
        ]
    );
}

#[tokio::test]
async fn eager_precancel_preserves_existing_connection_path() {
    let trace = Arc::new(Trace::default());
    let http = Http(trace.clone());
    let mut session = session(&trace, false, Err(WebSocketError::Cancelled));
    let mut call = session.call(request(), &http).unwrap();
    assert_eq!(
        call.next(&Cancel(true)).await,
        Err(WebSocketError::Cancelled)
    );
    drop(call);
    assert_eq!(trace.values(), ["auth", "connect"]);
}
