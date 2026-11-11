use parrot_domain::contracts::async_ops::{BoxFuture, Cancellation};
use parrot_provider::sessions::socket_session::*;
use parrot_provider::{responses, responses_websocket as ws, sessions::*};
use serde_json::{Value, json};
use std::{
    collections::VecDeque,
    path::PathBuf,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    time::Duration,
};
use tokio::sync::Notify;

#[derive(Default)]
struct Cancel {
    flag: AtomicBool,
    notify: Notify,
}
impl Cancel {
    fn cancel(&self) {
        self.flag.store(true, Ordering::SeqCst);
        self.notify.notify_waiters();
    }
}
impl Cancellation for Cancel {
    fn is_cancelled(&self) -> bool {
        self.flag.load(Ordering::SeqCst)
    }
    fn cancelled(&self) -> BoxFuture<'_, ()> {
        Box::pin(async move {
            loop {
                let next = self.notify.notified();
                tokio::pin!(next);
                next.as_mut().enable();
                if self.is_cancelled() {
                    return;
                }
                next.await;
            }
        })
    }
}
#[derive(Default)]
struct Wire {
    prepares: AtomicUsize,
    connects: AtomicUsize,
    receives: AtomicUsize,
    fragment_bytes: AtomicUsize,
    receive_error_at: AtomicUsize,
    drops: AtomicUsize,
    aborts: AtomicUsize,
    sent: Mutex<Vec<Value>>,
    bodies: Mutex<Vec<Vec<u8>>>,
}
struct Headers;
impl ws::HeaderSource for Headers {
    fn headers<'a>(&'a self, _: &'a dyn Cancellation) -> BoxFuture<'a, Result<ws::Headers, Error>> {
        Box::pin(async { Ok(Vec::new()) })
    }
}
struct Socket {
    wire: Arc<Wire>,
    script: VecDeque<Vec<u8>>,
    aborted: bool,
}
impl Drop for Socket {
    fn drop(&mut self) {
        self.wire.drops.fetch_add(1, Ordering::SeqCst);
    }
}
impl ws::Socket for Socket {
    fn state(&self) -> ws::SocketState {
        if self.aborted {
            ws::SocketState::Aborted
        } else {
            ws::SocketState::Open
        }
    }
    fn send<'a>(
        &'a mut self,
        data: &'a [u8],
        _: bool,
        _: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<(), ws::WebSocketError>> {
        self.wire.bodies.lock().unwrap().push(data.to_vec());
        self.wire
            .sent
            .lock()
            .unwrap()
            .push(serde_json::from_slice(data).unwrap());
        Box::pin(async { Ok(()) })
    }
    fn receive<'a>(
        &'a mut self,
        bytes: &'a mut [u8],
        cancel: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<ws::ReceivedFrame, ws::WebSocketError>> {
        let receive = self.wire.receives.fetch_add(1, Ordering::SeqCst) + 1;
        Box::pin(async move {
            if self.wire.receive_error_at.load(Ordering::SeqCst) == receive {
                return Err(ws::WebSocketError::Wire("receive failed".into()));
            }
            match self.script.pop_front() {
                Some(message) => {
                    let maximum = self.wire.fragment_bytes.load(Ordering::SeqCst);
                    let count = if maximum == 0 {
                        message.len()
                    } else {
                        message.len().min(maximum)
                    };
                    let end_of_message = count == message.len();
                    bytes[..count].copy_from_slice(&message[..count]);
                    if !end_of_message {
                        self.script.push_front(message[count..].to_vec());
                    }
                    Ok(ws::ReceivedFrame {
                        count,
                        kind: ws::FrameKind::Text,
                        end_of_message,
                        close_status: None,
                        close_description: None,
                    })
                }
                None => {
                    cancel.cancelled().await;
                    Err(ws::WebSocketError::Cancelled)
                }
            }
        })
    }
    fn abort(&mut self) {
        if !self.aborted {
            self.aborted = true;
            self.wire.aborts.fetch_add(1, Ordering::SeqCst);
        }
    }
}
struct Connector {
    wire: Arc<Wire>,
    scripts: Mutex<VecDeque<Result<Vec<String>, ws::WebSocketError>>>,
}
impl ws::Connector for Connector {
    fn connect<'a>(
        &'a self,
        _: &'a str,
        _: &'a ws::Headers,
        _: Option<Duration>,
        _: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<ws::ConnectedSocket, ws::WebSocketError>> {
        self.wire.connects.fetch_add(1, Ordering::SeqCst);
        let script = self
            .scripts
            .lock()
            .unwrap()
            .pop_front()
            .expect("connection script");
        Box::pin(async move {
            Ok(ws::ConnectedSocket {
                socket: Box::new(Socket {
                    wire: self.wire.clone(),
                    script: script?.into_iter().map(String::into_bytes).collect(),
                    aborted: false,
                }),
                response_headers: Vec::new(),
            })
        })
    }
}
#[derive(Default)]
struct Http {
    calls: AtomicUsize,
}
struct HttpEvents {
    sent: bool,
}
impl ws::HttpEvents for HttpEvents {
    fn next_event<'a>(
        &'a mut self,
        _: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<Option<ws::SessionEvent>, Error>> {
        let event = if self.sent {
            None
        } else {
            self.sent = true;
            Some(ws::SessionEvent::Response(
                responses::ResponsesEvent::TextDelta("http".into()),
            ))
        };
        Box::pin(async move { Ok(event) })
    }
    fn take_turn_state(&mut self) -> Option<String> {
        None
    }
}
impl ws::HttpFallback for Http {
    fn open_unprepared<'a>(
        &'a self,
        _: String,
        _: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<Box<dyn ws::HttpEvents>, Error>> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Box::pin(async { Ok(Box::new(HttpEvents { sent: false }) as Box<dyn ws::HttpEvents>) })
    }
}
struct Factory {
    wire: Arc<Wire>,
    http: Arc<Http>,
}
impl ResponseCallFactory for Factory {
    fn prepare(
        &self,
        request: Request,
        _: Option<Arc<diagnostics::CallTrace>>,
    ) -> Result<ResponsePrepared, Error> {
        let wire = self.wire.clone();
        Ok(ResponsePrepared {
            prepare_websocket: Box::new(move || {
                wire.prepares.fetch_add(1, Ordering::SeqCst);
                let prepared = responses::prepare(&request, "fake")
                    .map_err(|error| ws::WebSocketError::InvalidRequest(error.to_string()))?;
                ws::WebSocketRequest::new(&prepared)
            }),
            http: self.http.clone(),
        })
    }
}
fn request(text: &str) -> Request {
    Request {
        model: "model".into(),
        messages: vec![Message::user(text)],
        ..Request::default()
    }
}
fn delta(text: &str) -> String {
    json!({"type":"response.output_text.delta","delta":text}).to_string()
}
fn completed(id: &str) -> String {
    json!({"type":"response.completed","response":{"id":id,"status":"completed","output":[]}})
        .to_string()
}
fn setup(
    scripts: Vec<Result<Vec<String>, ws::WebSocketError>>,
    http_only: bool,
) -> (ResponseSession, Arc<Wire>, Arc<Http>) {
    let wire = Arc::new(Wire::default());
    let http = Arc::new(Http::default());
    let connector = Arc::new(Connector {
        wire: wire.clone(),
        scripts: Mutex::new(scripts.into()),
    });
    let client = ws::WebSocketClient::new(
        connector,
        "https://fake.invalid/responses",
        None,
        None,
        1 << 20,
    )
    .unwrap();
    let raw = ws::Session::new("fake", Arc::new(Headers), client, http_only);
    let factory = Arc::new(Factory {
        wire: wire.clone(),
        http: http.clone(),
    });
    (ResponseSession::new(raw, factory), wire, http)
}
async fn yields() {
    for _ in 0..8 {
        tokio::task::yield_now().await;
    }
}

#[tokio::test]
async fn opening_and_closing_without_pull_does_not_prepare_or_connect() {
    let (session, wire, _) = setup(vec![], false);
    let mut stream = session.call(request("a")).unwrap();
    stream.close().await.unwrap();
    session.close().await.unwrap();
    assert_eq!(wire.prepares.load(Ordering::SeqCst), 0);
    assert_eq!(wire.connects.load(Ordering::SeqCst), 0);
}
#[tokio::test]
async fn pulls_do_not_prefetch_next_wire_event() {
    let (session, wire, _) = setup(
        vec![Ok(vec![delta("one"), delta("two"), completed("r")])],
        false,
    );
    let mut stream = session.call(request("a")).unwrap();
    let cancel = Cancel::default();
    assert_eq!(
        stream.next(&cancel).await.unwrap(),
        Some(Event::TextDelta("one".into()))
    );
    yields().await;
    assert_eq!(wire.receives.load(Ordering::SeqCst), 1);
    assert_eq!(
        stream.next(&cancel).await.unwrap(),
        Some(Event::TextDelta("two".into()))
    );
    assert_eq!(wire.receives.load(Ordering::SeqCst), 2);
    stream.close().await.unwrap();
    assert_eq!(wire.drops.load(Ordering::SeqCst), 1);
    session.close().await.unwrap();
}
#[tokio::test]
async fn close_cancels_and_joins_an_inflight_read() {
    let (session, wire, _) = setup(vec![Ok(vec![])], false);
    let mut stream = session.call(request("a")).unwrap();
    let cancel = Cancel::default();
    let mut read = stream.next(&cancel);
    tokio::select! { _=&mut read=>panic!("unexpected completion"),_ = yields()=>{} }
    assert_eq!(wire.receives.load(Ordering::SeqCst), 1);
    drop(read);
    session.close().await.unwrap();
    assert_eq!(wire.drops.load(Ordering::SeqCst), 1);
    assert_eq!(wire.aborts.load(Ordering::SeqCst), 1);
    assert_eq!(stream.next(&cancel).await.unwrap_err(), Error::Cancelled);
    session.close().await.unwrap();
    assert!(matches!(
        session.call(request("later")),
        Err(Error::Disposed)
    ));
}
#[tokio::test]
async fn explicit_cancellation_waits_for_borrowed_call_cleanup() {
    let (session, wire, _) = setup(vec![Ok(vec![])], false);
    let mut stream = session.call(request("a")).unwrap();
    let cancel = Cancel::default();
    let mut read = stream.next(&cancel);
    tokio::select! { _=&mut read=>panic!("unexpected completion"),_ = yields()=>{} }
    cancel.cancel();
    assert_eq!(read.await.unwrap_err(), Error::Cancelled);
    assert_eq!(wire.drops.load(Ordering::SeqCst), 1);
    assert_eq!(stream.next(&cancel).await.unwrap(), None);
    session.close().await.unwrap();
}
#[tokio::test]
async fn dropping_stream_signals_cleanup_and_session_close_joins_it() {
    let (session, wire, _) = setup(vec![Ok(vec![delta("first")])], false);
    let mut stream = session.call(request("a")).unwrap();
    assert!(stream.next(&Cancel::default()).await.unwrap().is_some());
    drop(stream);
    session.close().await.unwrap();
    assert_eq!(wire.drops.load(Ordering::SeqCst), 1);
    assert_eq!(wire.aborts.load(Ordering::SeqCst), 1);
}
#[tokio::test]
async fn session_close_cancels_both_active_and_queued_calls() {
    let (session, wire, _) = setup(vec![Ok(vec![delta("first")])], false);
    let cancel = Cancel::default();
    let mut first = session.call(request("a")).unwrap();
    first.next(&cancel).await.unwrap();
    let mut second = session.call(request("b")).unwrap();
    let mut queued = second.next(&cancel);
    tokio::select! {_=&mut queued=>panic!("queued call completed"),_=yields()=>{}}
    drop(queued);
    session.close().await.unwrap();
    assert_eq!(wire.connects.load(Ordering::SeqCst), 1);
    assert_eq!(wire.prepares.load(Ordering::SeqCst), 1);
    assert_eq!(wire.drops.load(Ordering::SeqCst), 1);
}
#[tokio::test]
async fn observer_drop_does_not_issue_another_pull_or_lose_its_event() {
    let (session, wire, _) = setup(vec![Ok(vec![delta("kept"), completed("r")])], false);
    let cancel = Cancel::default();
    let mut stream = session.call(request("a")).unwrap();
    let mut read = stream.next(&cancel);
    // Poll once to queue the pull, then drop only its observation future.
    std::future::poll_fn(|context| {
        assert!(read.as_mut().poll(context).is_pending());
        std::task::Poll::Ready(())
    })
    .await;
    drop(read);
    yields().await;
    assert_eq!(wire.receives.load(Ordering::SeqCst), 1);
    assert_eq!(
        stream.next(&cancel).await.unwrap(),
        Some(Event::TextDelta("kept".into()))
    );
    assert_eq!(wire.receives.load(Ordering::SeqCst), 1);
    stream.close().await.unwrap();
    session.close().await.unwrap();
}
#[tokio::test]
async fn unsupported_upgrade_yields_retry_before_opening_http() {
    let (session, wire, http) = setup(
        vec![Err(ws::WebSocketError::Upgrade {
            status: 426,
            message: "upgrade".into(),
        })],
        false,
    );
    let mut stream = session.call(request("a")).unwrap();
    let cancel = Cancel::default();
    assert!(matches!(
        stream.next(&cancel).await.unwrap(),
        Some(Event::Retry { attempt: 1, .. })
    ));
    assert_eq!(http.calls.load(Ordering::SeqCst), 0);
    assert_eq!(
        stream.next(&cancel).await.unwrap(),
        Some(Event::TextDelta("http".into()))
    );
    assert_eq!(http.calls.load(Ordering::SeqCst), 1);
    assert_eq!(wire.connects.load(Ordering::SeqCst), 1);
    stream.close().await.unwrap();
    session.close().await.unwrap();
}
#[tokio::test]
async fn http_default_does_not_prepare_websocket_request() {
    let (session, wire, http) = setup(vec![], true);
    let mut stream = session.call(request("a")).unwrap();
    assert_eq!(
        stream.next(&Cancel::default()).await.unwrap(),
        Some(Event::TextDelta("http".into()))
    );
    assert_eq!(wire.prepares.load(Ordering::SeqCst), 0);
    assert_eq!(wire.connects.load(Ordering::SeqCst), 0);
    assert_eq!(http.calls.load(Ordering::SeqCst), 1);
    stream.close().await.unwrap();
    session.close().await.unwrap();
}
#[tokio::test]
async fn continuation_uses_actual_t29_lineage_and_begin_turn_resets_it() {
    let (session, wire, _) = setup(
        vec![
            Ok(vec![completed("r1"), completed("r2")]),
            Ok(vec![completed("r3")]),
        ],
        false,
    );
    let cancel = Cancel::default();
    let mut first = session.call(request("a")).unwrap();
    assert!(matches!(
        first.next(&cancel).await.unwrap(),
        Some(Event::Completed(_))
    ));
    assert_eq!(first.next(&cancel).await.unwrap(), None);
    let mut second_request = request("a");
    second_request.messages.push(Message::user("b"));
    let mut second = session.call(second_request).unwrap();
    assert!(matches!(
        second.next(&cancel).await.unwrap(),
        Some(Event::Completed(_))
    ));
    assert_eq!(second.next(&cancel).await.unwrap(), None);
    {
        let sent = wire.sent.lock().unwrap();
        assert_eq!(sent[1]["previous_response_id"], "r1");
        assert_eq!(sent[1]["input"].as_array().unwrap().len(), 1);
    }
    session.begin_turn().await.unwrap();
    let mut third = session.call(request("c")).unwrap();
    assert!(matches!(
        third.next(&cancel).await.unwrap(),
        Some(Event::Completed(_))
    ));
    assert_eq!(wire.connects.load(Ordering::SeqCst), 2);
    assert!(
        wire.sent.lock().unwrap()[2]
            .get("previous_response_id")
            .is_none()
    );
    third.close().await.unwrap();
    session.close().await.unwrap();
}

#[derive(Default)]
struct Sink(Mutex<Vec<diagnostics::DiagnosticEvent>>);
impl diagnostics::DiagnosticSink for Sink {
    fn write(&self, event: diagnostics::DiagnosticEvent) {
        self.0.lock().unwrap().push(event);
    }
}
fn trace(
    sink: Arc<Sink>,
    dumper: Option<Arc<diagnostics::LastRequestDumper>>,
) -> Arc<diagnostics::CallTrace> {
    Arc::new(diagnostics::CallTrace::start(
        sink,
        Arc::new(diagnostics::MonotonicDiagnosticClock::default()),
        diagnostics::CallContext {
            agent_session_id: "agent".into(),
            correlation_id: "call".into(),
            provider_id: "fake".into(),
            model_id: "model".into(),
        },
        dumper,
    ))
}
#[tokio::test]
async fn websocket_trace_dumps_actual_incremental_body_and_counts_wire_bytes() {
    let first_completion = completed("r1");
    let second_completion = completed("r2");
    let (session, wire, _) = setup(
        vec![Ok(vec![
            first_completion.clone(),
            second_completion.clone(),
        ])],
        false,
    );
    wire.fragment_bytes.store(3, Ordering::SeqCst);
    let sink = Arc::new(Sink::default());
    let archive = SocketArchive::new();
    let path = archive.0.join("last_requests.json");
    let dumper = Arc::new(diagnostics::LastRequestDumper::new(&path, sink.clone()));
    let cancel = Cancel::default();
    let first_trace = trace(sink.clone(), Some(dumper.clone()));
    let mut first = session
        .call_with_trace(request("secret-canary"), first_trace)
        .unwrap();
    assert!(matches!(
        first.next(&cancel).await.unwrap(),
        Some(Event::Completed(_))
    ));
    assert_eq!(first.next(&cancel).await.unwrap(), None);
    let mut next_request = request("secret-canary");
    next_request.messages.push(Message::user("next"));
    let second_trace = trace(sink.clone(), Some(dumper));
    let mut second = session.call_with_trace(next_request, second_trace).unwrap();
    assert!(matches!(
        second.next(&cancel).await.unwrap(),
        Some(Event::Completed(_))
    ));
    assert_eq!(second.next(&cancel).await.unwrap(), None);
    let bodies = wire.bodies.lock().unwrap().clone();
    assert_eq!(std::fs::read(&path).unwrap(), bodies[1]);
    assert_eq!(
        serde_json::from_slice::<Value>(&bodies[1]).unwrap()["previous_response_id"],
        "r1"
    );
    let events = sink.0.lock().unwrap().clone();
    let finished: Vec<_> = events
        .iter()
        .filter(|e| e.operation == "request_finished")
        .collect();
    assert_eq!(finished.len(), 2);
    assert_eq!(finished[0].transport.as_deref(), Some("websocket"));
    assert_eq!(finished[0].request_bytes, Some(bodies[0].len() as u64));
    assert_eq!(
        finished[0].response_bytes,
        Some(first_completion.len() as u64)
    );
    assert_eq!(finished[1].request_bytes, Some(bodies[1].len() as u64));
    assert_eq!(
        finished[1].response_bytes,
        Some(second_completion.len() as u64)
    );
    assert_eq!(finished[1].outcome.as_deref(), Some("completed"));
    assert!(!format!("{events:?}").contains("secret-canary"));
    for (number, response) in [(1, first_completion), (2, second_completion)] {
        let record = archive.read(number);
        assert_eq!(
            record.request,
            Some(ArchivedBody::Utf8(
                String::from_utf8(bodies[number as usize - 1].clone()).unwrap()
            ))
        );
        assert_eq!(record.response, vec![ArchivedBody::Utf8(response)]);
        assert_eq!(record.outcome, "completed");
        assert_eq!(record.error_code, None);
    }
    session.close().await.unwrap();
}
#[tokio::test]
async fn websocket_archive_includes_full_tool_context_alongside_incremental_request() {
    let tool_call = json!({
        "type": "function_call",
        "id": "fc1",
        "call_id": "call1",
        "name": "read",
        "arguments": r#"{"path":"secret-canary.rs"}"#,
        "status": "completed"
    });
    let first_completion = json!({
        "type": "response.completed",
        "response": {"id": "r1", "status": "completed", "output": [tool_call]}
    })
    .to_string();
    let (session, wire, _) = setup(vec![Ok(vec![first_completion, completed("r2")])], false);
    let sink = Arc::new(Sink::default());
    let archive = SocketArchive::new();
    let dumper = Arc::new(diagnostics::LastRequestDumper::new(
        archive.0.join("last_requests.json"),
        sink.clone(),
    ));
    let cancel = Cancel::default();
    let mut next_request = request("read the source");
    let mut first = session
        .call_with_trace(
            next_request.clone(),
            trace(sink.clone(), Some(dumper.clone())),
        )
        .unwrap();
    let Some(Event::Completed(completion)) = first.next(&cancel).await.unwrap() else {
        panic!("expected tool-call completion");
    };
    assert_eq!(first.next(&cancel).await.unwrap(), None);
    next_request.messages.push(completion.into_message());
    next_request
        .messages
        .push(Message::tool_result("call1", "source contents"));
    let prepared = responses::prepare(&next_request, "fake").unwrap();
    let expected = prepared.encode_http();
    let mut second = session
        .call_with_trace(next_request, trace(sink.clone(), Some(dumper)))
        .unwrap();
    assert!(matches!(
        second.next(&cancel).await.unwrap(),
        Some(Event::Completed(_))
    ));
    let wire_request = wire.sent.lock().unwrap()[1].clone();
    assert_eq!(wire_request["previous_response_id"], "r1");
    assert_eq!(wire_request["input"].as_array().unwrap().len(), 1);
    assert_eq!(wire_request["input"][0]["type"], "function_call_output");
    let pending: Value =
        serde_json::from_slice(&std::fs::read(archive.0.join("request_2.json")).unwrap()).unwrap();
    assert_eq!(pending["outcome"], "in_progress");
    let full_request: Value =
        serde_json::from_str(pending["full_request"]["data"].as_str().unwrap()).unwrap();
    assert_eq!(full_request["input"].as_array().unwrap().len(), 3);
    assert_eq!(full_request["input"][1], tool_call);
    assert_eq!(full_request["input"][2], wire_request["input"][0]);
    assert!(full_request.get("previous_response_id").is_none());
    assert_eq!(
        pending["full_request"]["data"],
        String::from_utf8(expected.clone()).unwrap()
    );
    assert_eq!(second.next(&cancel).await.unwrap(), None);
    let record = archive.read(2);
    assert_eq!(
        record.request,
        Some(ArchivedBody::Utf8(
            String::from_utf8(wire.bodies.lock().unwrap()[1].clone()).unwrap()
        ))
    );
    assert_eq!(
        record.full_request,
        Some(ArchivedBody::Utf8(String::from_utf8(expected).unwrap()))
    );
    assert!(!format!("{:?}", sink.0.lock().unwrap()).contains("secret-canary"));
    session.close().await.unwrap();
}

#[tokio::test]
async fn suspended_websocket_request_trace_is_cancelled_once_via_shared_call_trace() {
    let response = delta("first");
    let (session, _, _) = setup(vec![Ok(vec![response.clone()])], false);
    let sink = Arc::new(Sink::default());
    let archive = SocketArchive::new();
    let dumper = Arc::new(diagnostics::LastRequestDumper::new(
        archive.0.join("last_requests.json"),
        sink.clone(),
    ));
    let trace = trace(sink.clone(), Some(dumper));
    let mut stream = session
        .call_with_trace(request("a"), trace.clone())
        .unwrap();
    stream.next(&Cancel::default()).await.unwrap();
    trace.cancel();
    let record = archive.read(1);
    assert_eq!(record.response, vec![ArchivedBody::Utf8(response)]);
    assert_eq!(record.outcome, "cancelled");
    assert_eq!(record.error_code.as_deref(), Some("cancelled"));
    {
        let events = sink.0.lock().unwrap();
        let finished: Vec<_> = events
            .iter()
            .filter(|e| e.operation == "request_finished")
            .collect();
        assert_eq!(finished.len(), 1);
        assert_eq!(finished[0].outcome.as_deref(), Some("cancelled"));
    }
    stream.close().await.unwrap();
    session.close().await.unwrap();
    let events = sink.0.lock().unwrap();
    assert_eq!(
        events
            .iter()
            .filter(|e| e.operation == "request_finished")
            .count(),
        1
    );
}
#[tokio::test]
async fn websocket_connect_failure_has_allowlisted_trace_and_no_encoded_request_size() {
    let (session, _, _) = setup(
        vec![Err(ws::WebSocketError::Upgrade {
            status: 426,
            message: "secret-error".into(),
        })],
        false,
    );
    let sink = Arc::new(Sink::default());
    let archive = SocketArchive::new();
    let dumper = Arc::new(diagnostics::LastRequestDumper::new(
        archive.0.join("last_requests.json"),
        sink.clone(),
    ));
    let mut stream = session
        .call_with_trace(request("a"), trace(sink.clone(), Some(dumper)))
        .unwrap();
    assert!(matches!(
        stream.next(&Cancel::default()).await.unwrap(),
        Some(Event::Retry { .. })
    ));
    stream.close().await.unwrap();
    session.close().await.unwrap();
    let events = sink.0.lock().unwrap();
    let finished: Vec<_> = events
        .iter()
        .filter(|e| e.operation == "request_finished")
        .collect();
    assert_eq!(finished.len(), 1);
    assert_eq!(finished[0].error_code.as_deref(), Some("websocket_upgrade"));
    assert_eq!(finished[0].request_bytes, None);
    assert_eq!(finished[0].response_bytes, None);
    assert!(!format!("{events:?}").contains("secret-error"));
    let record = archive.read(1);
    assert_eq!(record.request, None);
    assert!(record.response.is_empty());
    assert_eq!(record.outcome, "failed");
    assert_eq!(record.error_code.as_deref(), Some("websocket_upgrade"));
    assert!(!archive.0.join("last_requests.json").exists());
    assert!(!archive.0.join("request_2.json").exists());
}

struct FailedAuthHeaders {
    original: Error,
}
impl ws::HeaderSource for FailedAuthHeaders {
    fn headers<'a>(&'a self, _: &'a dyn Cancellation) -> BoxFuture<'a, Result<ws::Headers, Error>> {
        Box::pin(async move { Err(self.original.clone()) })
    }
}
#[tokio::test]
async fn websocket_auth_trace_classifies_original_error_without_consuming_it() {
    for (original, code) in [
        (
            Error::Provider("secret-auth-configuration".into()),
            "unexpected",
        ),
        (Error::Cancelled, "cancelled"),
        (Error::Io("secret-auth-io".into()), "io"),
    ] {
        let wire = Arc::new(Wire::default());
        let headers = Arc::new(FailedAuthHeaders {
            original: original.clone(),
        });
        let connector = Arc::new(Connector {
            wire: wire.clone(),
            scripts: Mutex::new(VecDeque::new()),
        });
        let client = ws::WebSocketClient::new(
            connector,
            "https://fake.invalid/responses",
            None,
            None,
            1 << 20,
        )
        .unwrap();
        let raw = ws::Session::new("fake", headers, client, false);
        let factory = Arc::new(Factory {
            wire: wire.clone(),
            http: Arc::new(Http::default()),
        });
        let session = ResponseSession::new(raw, factory);
        let sink = Arc::new(Sink::default());
        let mut stream = session
            .call_with_trace(request("a"), trace(sink.clone(), None))
            .unwrap();
        let actual = stream.next(&Cancel::default()).await.unwrap_err();
        if matches!(&original, Error::Io(_)) {
            assert_eq!(
                actual,
                Error::WebSocketTransport {
                    message: "Responses WebSocket transport failed.".into(),
                    cause: Box::new(original.clone()),
                }
            );
            assert_eq!(
                std::error::Error::source(&actual).unwrap().to_string(),
                original.to_string()
            );
        } else {
            assert_eq!(actual, original);
        }
        assert_eq!(wire.connects.load(Ordering::SeqCst), 0);
        let events = sink.0.lock().unwrap().clone();
        let finished: Vec<_> = events
            .iter()
            .filter(|event| event.operation == "request_finished")
            .collect();
        assert_eq!(finished.len(), 1);
        assert_eq!(finished[0].error_code.as_deref(), Some(code));
        assert_eq!(finished[0].request_bytes, None);
        assert!(!format!("{events:?}").contains("secret-auth"));
        stream.close().await.unwrap();
        session.close().await.unwrap();
    }
}

#[tokio::test]
async fn header_socket_failures_do_not_trigger_native_recovery() {
    for original in [
        Error::Socket(ws::WebSocketError::Upgrade {
            status: 426,
            message: "credential service upgrade failed".into(),
        }),
        Error::Socket(ws::WebSocketError::Response(
            responses::ResponsesError::ProviderResponse {
                error_type: "invalid_request_error".into(),
                error_code: "previous_response_not_found".into(),
                detail: "credential service response is unavailable".into(),
                response_body: "credential failure body".into(),
            },
        )),
    ] {
        let wire = Arc::new(Wire::default());
        let http = Arc::new(Http::default());
        let client = ws::WebSocketClient::new(
            Arc::new(Connector {
                wire: wire.clone(),
                scripts: Mutex::new(VecDeque::new()),
            }),
            "https://fake.invalid/responses",
            None,
            None,
            1 << 20,
        )
        .unwrap();
        let raw = ws::Session::new(
            "fake",
            Arc::new(FailedAuthHeaders {
                original: original.clone(),
            }),
            client,
            false,
        );
        let session = ResponseSession::new(
            raw,
            Arc::new(Factory {
                wire: wire.clone(),
                http: http.clone(),
            }),
        );
        for _ in 0..2 {
            let mut stream = session.call(request("a")).unwrap();
            assert_eq!(stream.next(&Cancel::default()).await, Err(original.clone()));
            assert_eq!(stream.next(&Cancel::default()).await.unwrap(), None);
            stream.close().await.unwrap();
        }
        assert_eq!(wire.connects.load(Ordering::SeqCst), 0);
        assert_eq!(http.calls.load(Ordering::SeqCst), 0);
        session.close().await.unwrap();
    }
}

struct SocketArchive(PathBuf);
impl SocketArchive {
    fn new() -> Self {
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        let root = std::env::current_dir().unwrap().join(".tmp").join(format!(
            "socket-archive-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::SeqCst)
        ));
        std::fs::create_dir_all(&root).unwrap();
        Self(root)
    }
    fn read(&self, number: u64) -> ArchivedAttempt {
        let record: ArchivedAttempt = serde_json::from_slice(
            &std::fs::read(self.0.join(format!("request_{number}.json"))).unwrap(),
        )
        .unwrap();
        assert_eq!(record.number, number);
        assert_eq!(record.transport, "websocket");
        record
    }
}
impl Drop for SocketArchive {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}
#[derive(Debug, PartialEq, serde::Deserialize)]
#[serde(tag = "encoding", content = "data", rename_all = "snake_case")]
enum ArchivedBody {
    Utf8(String),
    Bytes(Vec<u8>),
}
#[derive(serde::Deserialize)]
struct ArchivedAttempt {
    number: u64,
    transport: String,
    request: Option<ArchivedBody>,
    full_request: Option<ArchivedBody>,
    response: Vec<ArchivedBody>,
    outcome: String,
    error_code: Option<String>,
}

#[tokio::test]
async fn websocket_archive_retains_malformed_response_before_parser_failure() {
    let (session, _, _) = setup(vec![Ok(vec!["{".into()])], false);
    let sink = Arc::new(Sink::default());
    let archive = SocketArchive::new();
    let dumper = Arc::new(diagnostics::LastRequestDumper::new(
        archive.0.join("last_requests.json"),
        sink.clone(),
    ));
    let mut stream = session
        .call_with_trace(request("a"), trace(sink, Some(dumper)))
        .unwrap();
    assert!(stream.next(&Cancel::default()).await.is_err());
    let record = archive.read(1);
    assert_eq!(record.response, vec![ArchivedBody::Utf8("{".into())]);
    assert_eq!(record.outcome, "failed");
    assert!(record.error_code.is_some());
    stream.close().await.unwrap();
    session.close().await.unwrap();
}

#[tokio::test]
async fn websocket_archive_separates_recovery_attempt_responses() {
    let error =
        r#"{"type":"error","code":"previous_response_not_found","message":"missing"}"#.to_owned();
    let completion = completed("recovered");
    let (session, wire, _) = setup(
        vec![Ok(vec![error.clone()]), Ok(vec![completion.clone()])],
        false,
    );
    let sink = Arc::new(Sink::default());
    let archive = SocketArchive::new();
    let dumper = Arc::new(diagnostics::LastRequestDumper::new(
        archive.0.join("last_requests.json"),
        sink.clone(),
    ));
    let mut stream = session
        .call_with_trace(request("a"), trace(sink, Some(dumper)))
        .unwrap();
    let cancel = Cancel::default();
    assert!(matches!(
        stream.next(&cancel).await.unwrap(),
        Some(Event::Retry { .. })
    ));
    assert!(matches!(
        stream.next(&cancel).await.unwrap(),
        Some(Event::Completed(_))
    ));
    assert_eq!(stream.next(&cancel).await.unwrap(), None);
    let first = archive.read(1);
    assert_eq!(first.response, vec![ArchivedBody::Utf8(error)]);
    assert_eq!(first.outcome, "failed");
    let second = archive.read(2);
    assert_eq!(second.response, vec![ArchivedBody::Utf8(completion)]);
    assert_eq!(second.outcome, "completed");
    assert!(first.request.is_some());
    assert!(second.request.is_some());
    assert_eq!(wire.connects.load(Ordering::SeqCst), 2);
    stream.close().await.unwrap();
    session.close().await.unwrap();
}

#[tokio::test]
async fn websocket_archive_retains_partial_message_on_receive_failure() {
    let response = delta("partial");
    let (session, wire, _) = setup(vec![Ok(vec![response.clone()])], false);
    wire.fragment_bytes.store(3, Ordering::SeqCst);
    wire.receive_error_at.store(2, Ordering::SeqCst);
    let sink = Arc::new(Sink::default());
    let archive = SocketArchive::new();
    let dumper = Arc::new(diagnostics::LastRequestDumper::new(
        archive.0.join("last_requests.json"),
        sink.clone(),
    ));
    let mut stream = session
        .call_with_trace(request("a"), trace(sink.clone(), Some(dumper)))
        .unwrap();
    assert!(stream.next(&Cancel::default()).await.is_err());
    let record = archive.read(1);
    assert_eq!(
        record.response,
        vec![ArchivedBody::Utf8(response[..3].into())]
    );
    assert_eq!(record.outcome, "failed");
    assert!(record.error_code.is_some());
    let events = sink.0.lock().unwrap().clone();
    assert_eq!(
        events
            .iter()
            .find(|event| event.operation == "request_finished")
            .unwrap()
            .response_bytes,
        Some(3)
    );
    stream.close().await.unwrap();
    session.close().await.unwrap();
}
