use parrot_domain::contracts::async_ops::{BoxFuture, Cancellation};
use parrot_provider::{
    http_stream::*,
    responses::ResponsesEvent,
    responses_websocket::{HttpEvents, SessionEvent},
    sessions::{
        Error, Event, EventStream, Message, Provider, Request, Session,
        decorators::{RetrySleeper, RetryingProvider},
        diagnostic_session::DiagnosticSession,
        diagnostics::*,
        http::*,
        provider::{ApiKeySource, CompatibleOptions, OpenAICompatibleProvider},
    },
};
use std::{
    collections::VecDeque,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    time::Duration,
};

struct Cancel(AtomicBool);
impl Cancel {
    fn never() -> Self {
        Self(AtomicBool::new(false))
    }
}
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
#[derive(Default)]
struct Wire {
    requests: Mutex<Vec<HttpRequest>>,
    reads: AtomicUsize,
    drops: AtomicUsize,
}
struct Body {
    chunks: VecDeque<Vec<u8>>,
    wire: Arc<Wire>,
}
impl ByteStream for Body {
    fn read<'a>(
        &'a mut self,
        buffer: &'a mut [u8],
        cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<usize, HttpError>> {
        Box::pin(async move {
            if cancellation.is_cancelled() {
                return Err(HttpError::Cancelled);
            }
            self.wire.reads.fetch_add(1, Ordering::SeqCst);
            let Some(mut bytes) = self.chunks.pop_front() else {
                return Ok(0);
            };
            let count = bytes.len().min(buffer.len());
            buffer[..count].copy_from_slice(&bytes[..count]);
            if count < bytes.len() {
                self.chunks.push_front(bytes.split_off(count));
            }
            Ok(count)
        })
    }
}
impl Drop for Body {
    fn drop(&mut self) {
        self.wire.drops.fetch_add(1, Ordering::SeqCst);
    }
}
struct Transport {
    wire: Arc<Wire>,
    status: u16,
    chunks: Mutex<Option<VecDeque<Vec<u8>>>>,
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
            self.wire.requests.lock().unwrap().push(request);
            Ok(HttpResponse {
                status: self.status,
                reason: "synthetic".into(),
                headers: vec![("X-Codex-Turn-State".into(), "turn-state".into())],
                body: Box::new(Body {
                    chunks: self.chunks.lock().unwrap().take().unwrap(),
                    wire: self.wire.clone(),
                }),
            })
        })
    }
}
fn transport(status: u16, records: &[&str]) -> (Arc<Transport>, Arc<Wire>) {
    let wire = Arc::new(Wire::default());
    (
        Arc::new(Transport {
            wire: wire.clone(),
            status,
            chunks: Mutex::new(Some(
                records
                    .iter()
                    .map(|text| text.as_bytes().to_vec())
                    .collect(),
            )),
        }),
        wire,
    )
}
fn driver(
    transport: Arc<dyn HttpTransport>,
    protocol: Protocol,
    body: Vec<u8>,
    maximum: usize,
    trace: Option<Arc<CallTrace>>,
) -> HttpDriver {
    HttpDriver::new(
        transport,
        Endpoint::parse("https://example.test/v1/responses").unwrap(),
        vec![("Authorization".into(), "Bearer synthetic".into())],
        body,
        StreamOptions {
            header_timeout: Duration::ZERO,
            idle_timeout: Duration::ZERO,
            maximum_request_bytes: maximum,
        },
        protocol,
        "p".into(),
        false,
        trace,
    )
}
#[derive(Default)]
struct Sink(Mutex<Vec<DiagnosticEvent>>);
impl DiagnosticSink for Sink {
    fn write(&self, event: DiagnosticEvent) {
        self.0.lock().unwrap().push(event);
    }
}
fn trace(sink: Arc<Sink>, dump: Option<Arc<LastRequestDumper>>) -> Arc<CallTrace> {
    Arc::new(CallTrace::start(
        sink,
        Arc::new(MonotonicDiagnosticClock::default()),
        CallContext {
            agent_session_id: "a".into(),
            correlation_id: "c".into(),
            provider_id: "p".into(),
            model_id: "m".into(),
        },
        dump,
    ))
}
const TEXT: &str = "data: {\"type\":\"response.output_text.delta\",\"delta\":\"hello\"}\n\n";
const DONE: &str = "data: {\"type\":\"response.completed\",\"response\":{\"id\":\"r\",\"output\":[],\"usage\":{\"input_tokens\":7,\"output_tokens\":2}}}\n\n";

#[tokio::test]
async fn responses_is_pull_driven_uses_actual_prepared_bytes_and_retains_turn_state() {
    let request = Request {
        model: "m".into(),
        messages: vec![Message::user("hello")],
        ..Request::default()
    };
    let body = parrot_provider::responses::prepare(&request, "p")
        .unwrap()
        .encode_http();
    let (transport, wire) = transport(200, &[TEXT, DONE, "not read"]);
    let cancel = Cancel::never();
    let mut stream = driver(
        transport,
        Protocol::Responses,
        body.clone(),
        body.len(),
        None,
    );
    assert!(wire.requests.lock().unwrap().is_empty());
    assert_eq!(
        stream.next(&cancel).await.unwrap(),
        Some(Event::HttpRequestStarted)
    );
    assert!(wire.requests.lock().unwrap().is_empty());
    assert_eq!(
        stream.next(&cancel).await.unwrap(),
        Some(Event::HttpResponseHeadersReceived)
    );
    {
        let requests = wire.requests.lock().unwrap();
        assert_eq!(requests[0].body, body);
        assert!(
            requests[0]
                .headers
                .contains(&("Authorization".into(), "Bearer synthetic".into()))
        );
    }
    assert_eq!(stream.take_turn_state().as_deref(), Some("turn-state"));
    assert_eq!(
        stream.next(&cancel).await.unwrap(),
        Some(Event::TextDelta("hello".into()))
    );
    let Some(Event::Completed(completion)) = stream.next(&cancel).await.unwrap() else {
        panic!("completion");
    };
    assert_eq!((completion.input_tokens, completion.output_tokens), (7, 2));
    assert_eq!(stream.next(&cancel).await.unwrap(), None);
    assert_eq!(stream.response_bytes(), (TEXT.len() + DONE.len()) as u64);
    assert_eq!(wire.reads.load(Ordering::SeqCst), 2);
    assert_eq!(wire.drops.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn chat_completions_uses_actual_chat_parser() {
    let (transport, wire) = transport(
        200,
        &[
            "data: {\"choices\":[{\"delta\":{\"content\":\"hi\",\"reasoning_content\":\"think\"},\"finish_reason\":\"stop\"}],\"usage\":{\"prompt_tokens\":9,\"completion_tokens\":4}}\n\n",
            "data: [DONE]\n\n",
        ],
    );
    let cancel = Cancel::never();
    let mut stream = driver(
        transport,
        Protocol::ChatCompletions,
        b"{}".to_vec(),
        2,
        None,
    );
    assert_eq!(
        stream.next(&cancel).await.unwrap(),
        Some(Event::HttpRequestStarted)
    );
    assert_eq!(
        stream.next(&cancel).await.unwrap(),
        Some(Event::HttpResponseHeadersReceived)
    );
    let mut events = Vec::new();
    while let Some(event) = stream.next(&cancel).await.unwrap() {
        events.push(event);
    }
    assert!(
        events
            .iter()
            .any(|event| matches!(event, Event::TextDelta(text) if text == "hi"))
    );
    assert!(
        events
            .iter()
            .any(|event| matches!(event, Event::ReasoningDelta { text, .. } if text == "think"))
    );
    let Some(Event::Completed(completion)) = events.last() else {
        panic!("completion");
    };
    assert_eq!((completion.input_tokens, completion.output_tokens), (9, 4));
    assert!(completion.provider_content.contains("ReasoningContent"));
    assert_eq!(wire.drops.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn request_limit_precedes_dump_trace_and_transport() {
    let (transport, wire) = transport(200, &[]);
    let sink = Arc::new(Sink::default());
    let mut stream = driver(
        transport,
        Protocol::Responses,
        b"123".to_vec(),
        2,
        Some(trace(sink.clone(), None)),
    );
    assert_eq!(
        stream.next(&Cancel::never()).await.unwrap_err(),
        Error::Http(HttpError::RequestTooLarge(2))
    );
    assert!(wire.requests.lock().unwrap().is_empty());
    assert!(
        !sink
            .0
            .lock()
            .unwrap()
            .iter()
            .any(|event| event.operation == "request_started")
    );
    assert_eq!(stream.next(&Cancel::never()).await.unwrap(), None);
}

#[tokio::test]
async fn exact_dump_and_trace_accounting_do_not_include_auth_or_body() {
    static NEXT: AtomicUsize = AtomicUsize::new(0);
    let path = std::env::temp_dir().join(format!(
        "t30-http-dump-{}-{}.json",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::SeqCst)
    ));
    let sink = Arc::new(Sink::default());
    let dump = Arc::new(LastRequestDumper::new(&path, sink.clone()));
    let bytes = br#"{"exact":9007199254740993,"secret":"body-canary"}"#.to_vec();
    let (transport, _) = transport(200, &[DONE]);
    let mut stream = driver(
        transport,
        Protocol::Responses,
        bytes.clone(),
        1000,
        Some(trace(sink.clone(), Some(dump))),
    );
    assert!(!path.exists());
    while stream.next(&Cancel::never()).await.unwrap().is_some() {}
    assert_eq!(std::fs::read(&path).unwrap(), bytes);
    std::fs::remove_file(path).unwrap();
    stream.close().await.unwrap();
    let events = sink.0.lock().unwrap();
    let finished: Vec<_> = events
        .iter()
        .filter(|event| event.operation == "request_finished")
        .collect();
    assert_eq!(finished.len(), 1);
    assert_eq!(finished[0].outcome.as_deref(), Some("completed"));
    assert_eq!(finished[0].request_bytes, Some(bytes.len() as u64));
    assert_eq!(finished[0].response_bytes, Some(DONE.len() as u64));
    let diagnostic = format!("{events:?}");
    for secret in ["body-canary", "Authorization", "Bearer synthetic"] {
        assert!(!diagnostic.contains(secret));
    }
}

#[tokio::test]
async fn non_success_http_body_and_status_survive_t29_error_bridge() {
    let body = r#"{"error":{"type":"overloaded_error","code":"busy","message":"try later"}}"#;
    let (transport, wire) = transport(503, &[body]);
    let sink = Arc::new(Sink::default());
    let errors: HttpErrorSlot = Arc::new(Mutex::new(None));
    let mut bridge = HttpEventsBridge::new(
        driver(
            transport,
            Protocol::Responses,
            b"{}".to_vec(),
            2,
            Some(trace(sink.clone(), None)),
        ),
        errors.clone(),
    );
    let cancel = Cancel::never();
    assert_eq!(
        bridge.next_event(&cancel).await.unwrap(),
        Some(SessionEvent::HttpRequestStarted)
    );
    assert!(wire.requests.lock().unwrap().is_empty());
    assert!(bridge.next_event(&cancel).await.is_err());
    let Some(Error::Http(HttpError::Response(error))) = errors.lock().unwrap().clone() else {
        panic!("structured original HTTP failure");
    };
    assert_eq!(error.status, 503);
    assert_eq!(error.error_code, "busy");
    assert_eq!(error.response_body, body);
    let events = sink.0.lock().unwrap();
    let terminal = events
        .iter()
        .find(|event| event.operation == "request_finished")
        .unwrap();
    assert_eq!(terminal.response_bytes, Some(body.len() as u64));
    assert_eq!(terminal.outcome.as_deref(), Some("failed"));
    assert_eq!(wire.drops.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn responses_bridge_preserves_lifecycle_without_prefetch() {
    let (transport, wire) = transport(200, &[TEXT, DONE]);
    let errors = Arc::new(Mutex::new(None));
    let mut bridge = HttpEventsBridge::new(
        driver(transport, Protocol::Responses, b"{}".to_vec(), 2, None),
        errors,
    );
    let cancel = Cancel::never();
    assert_eq!(
        bridge.next_event(&cancel).await.unwrap(),
        Some(SessionEvent::HttpRequestStarted)
    );
    assert!(wire.requests.lock().unwrap().is_empty());
    assert_eq!(
        bridge.next_event(&cancel).await.unwrap(),
        Some(SessionEvent::HttpResponseHeadersReceived)
    );
    assert_eq!(wire.reads.load(Ordering::SeqCst), 0);
    assert_eq!(
        bridge.next_event(&cancel).await.unwrap(),
        Some(SessionEvent::Response(ResponsesEvent::TextDelta(
            "hello".into()
        )))
    );
    assert_eq!(wire.reads.load(Ordering::SeqCst), 1);
    assert_eq!(bridge.take_turn_state().as_deref(), Some("turn-state"));
}

#[tokio::test]
async fn cancellation_and_disposal_release_response_and_finish_once() {
    for cancelled in [false, true] {
        let (transport, wire) = transport(200, &[TEXT]);
        let sink = Arc::new(Sink::default());
        let mut stream = driver(
            transport,
            Protocol::Responses,
            b"{}".to_vec(),
            2,
            Some(trace(sink.clone(), None)),
        );
        let cancel = Cancel::never();
        stream.next(&cancel).await.unwrap();
        stream.next(&cancel).await.unwrap();
        if cancelled {
            cancel.0.store(true, Ordering::SeqCst);
            assert_eq!(stream.next(&cancel).await.unwrap_err(), Error::Cancelled);
        }
        stream.close().await.unwrap();
        stream.close().await.unwrap();
        drop(stream);
        assert_eq!(wire.drops.load(Ordering::SeqCst), 1);
        let events = sink.0.lock().unwrap();
        let terminals: Vec<_> = events
            .iter()
            .filter(|event| event.operation == "request_finished")
            .collect();
        assert_eq!(terminals.len(), 1);
        assert_eq!(
            terminals[0].outcome.as_deref(),
            Some(if cancelled { "cancelled" } else { "disposed" })
        );
        assert_eq!(terminals[0].response_bytes, Some(0));
    }
}

#[tokio::test]
async fn already_cancelled_call_keeps_source_start_marker_without_sending() {
    let (transport, wire) = transport(200, &[]);
    let sink = Arc::new(Sink::default());
    let mut stream = driver(
        transport,
        Protocol::Responses,
        b"{}".to_vec(),
        2,
        Some(trace(sink.clone(), None)),
    );
    let cancel = Cancel(AtomicBool::new(true));
    assert_eq!(
        stream.next(&cancel).await.unwrap(),
        Some(Event::HttpRequestStarted)
    );
    assert_eq!(stream.next(&cancel).await.unwrap_err(), Error::Cancelled);
    assert!(wire.requests.lock().unwrap().is_empty());
    let events = sink.0.lock().unwrap();
    let terminal: Vec<_> = events
        .iter()
        .filter(|event| event.operation == "request_finished")
        .collect();
    assert_eq!(terminal.len(), 1);
    assert_eq!(terminal[0].outcome.as_deref(), Some("cancelled"));
    assert_eq!(terminal[0].response_bytes, None);
}

#[tokio::test]
async fn owner_can_cancel_request_trace_while_consumer_is_between_pulls() {
    let (transport, wire) = transport(200, &[TEXT]);
    let sink = Arc::new(Sink::default());
    let mut stream = driver(
        transport,
        Protocol::Responses,
        b"{}".to_vec(),
        2,
        Some(trace(sink.clone(), None)),
    );
    stream.next(&Cancel::never()).await.unwrap();
    stream.next(&Cancel::never()).await.unwrap();
    assert!(stream.request_trace().unwrap().cancel());
    {
        let events = sink.0.lock().unwrap();
        let terminal = events
            .iter()
            .find(|event| event.operation == "request_finished")
            .unwrap();
        assert_eq!(terminal.outcome.as_deref(), Some("cancelled"));
        assert_eq!(terminal.response_bytes, Some(0));
    }
    drop(stream);
    assert_eq!(wire.drops.load(Ordering::SeqCst), 1);
    assert_eq!(
        sink.0
            .lock()
            .unwrap()
            .iter()
            .filter(|event| event.operation == "request_finished")
            .count(),
        1
    );
}

struct FixedKey;
impl ApiKeySource for FixedKey {
    fn api_key<'a>(&'a self, _: &'a dyn Cancellation) -> BoxFuture<'a, Result<String, Error>> {
        Box::pin(async { Ok("synthetic".into()) })
    }
    fn has_credential<'a>(&'a self, _: &'a dyn Cancellation) -> BoxFuture<'a, Result<bool, Error>> {
        Box::pin(async { Ok(true) })
    }
}
struct NoSocket;
impl parrot_provider::responses_websocket::Connector for NoSocket {
    fn connect<'a>(
        &'a self,
        _: &'a str,
        _: &'a parrot_provider::responses_websocket::Headers,
        _: Option<Duration>,
        _: &'a dyn Cancellation,
    ) -> BoxFuture<
        'a,
        Result<
            parrot_provider::responses_websocket::ConnectedSocket,
            parrot_provider::responses_websocket::WebSocketError,
        >,
    > {
        Box::pin(async { panic!("HTTP Chat Completions must not connect a WebSocket") })
    }
}
struct ImmediateSleeper;
impl RetrySleeper for ImmediateSleeper {
    fn sleep<'a>(
        &'a self,
        _: Duration,
        _: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<(), Error>> {
        Box::pin(async { Ok(()) })
    }
}
struct RetryTransport {
    scripts: Mutex<VecDeque<&'static str>>,
    wire: Arc<Wire>,
}
impl HttpTransport for RetryTransport {
    fn send<'a>(
        &'a self,
        request: HttpRequest,
        _: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<HttpResponse, HttpError>> {
        Box::pin(async move {
            self.wire.requests.lock().unwrap().push(request);
            let body = self
                .scripts
                .lock()
                .unwrap()
                .pop_front()
                .expect("unexpected HTTP retry");
            Ok(HttpResponse {
                status: 200,
                reason: "OK".into(),
                headers: Vec::new(),
                body: Box::new(Body {
                    chunks: VecDeque::from([body.as_bytes().to_vec()]),
                    wire: self.wire.clone(),
                }),
            })
        })
    }
}
fn retrying_chat(scripts: VecDeque<&'static str>) -> (RetryingProvider, Arc<Wire>) {
    let wire = Arc::new(Wire::default());
    let transport = Arc::new(RetryTransport {
        scripts: Mutex::new(scripts),
        wire: wire.clone(),
    });
    let options = CompatibleOptions::new("p", "https://example.test/v1", Arc::new(FixedKey));
    let provider = OpenAICompatibleProvider::new(options, transport, Arc::new(NoSocket)).unwrap();
    (
        RetryingProvider::with_sleeper(Arc::new(provider), 5, Arc::new(ImmediateSleeper)),
        wire,
    )
}
const MALFORMED_CHAT: &str = "data: {broken\n\n";
const INVALID_CHAT_INTEGER: &str = "data: {\"usage\":{\"prompt_tokens\":2147483648}}\n\n";
const VALID_CHAT: &str = "data: {\"choices\":[{\"delta\":{\"content\":\"ok\"},\"finish_reason\":\"stop\"}]}\n\ndata: [DONE]\n\n";

#[tokio::test]
async fn composed_chat_parse_failures_retry_before_visible_output_then_complete() {
    for malformed in [MALFORMED_CHAT, INVALID_CHAT_INTEGER] {
        let (provider, wire) = retrying_chat(VecDeque::from([malformed, VALID_CHAT]));
        let mut stream = provider
            .call(Request {
                model: "m".into(),
                messages: vec![Message::user("hello")],
                ..Request::default()
            })
            .unwrap();
        let cancel = Cancel::never();
        let mut retries = 0;
        let mut visible = false;
        let mut completed = false;
        while let Some(event) = stream.next(&cancel).await.unwrap() {
            if let Event::Retry { attempt, .. } = &event {
                assert!(!visible, "parse retry must precede visible output");
                assert_eq!(*attempt, 1);
                retries += 1;
            }
            visible |= event.is_visible_output();
            if let Event::Completed(completion) = event {
                assert_eq!(completion.assistant_text, "ok");
                completed = true;
            }
        }
        assert!(completed);
        assert_eq!(retries, 1);
        assert_eq!(wire.requests.lock().unwrap().len(), 2);
        assert_eq!(wire.drops.load(Ordering::SeqCst), 2);
        stream.close().await.unwrap();
    }
}

#[tokio::test]
async fn composed_chat_parse_failures_exhaust_five_stream_retries() {
    for malformed in [MALFORMED_CHAT, INVALID_CHAT_INTEGER] {
        let (provider, wire) = retrying_chat(vec![malformed; 6].into());
        let mut stream = provider
            .call(Request {
                model: "m".into(),
                ..Request::default()
            })
            .unwrap();
        let cancel = Cancel::never();
        let mut retries = 0;
        let failure = loop {
            match stream.next(&cancel).await {
                Ok(Some(Event::Retry { attempt, .. })) => {
                    retries += 1;
                    assert_eq!(attempt, retries);
                }
                Ok(Some(event)) => assert!(!event.is_visible_output()),
                Err(error) => break error,
                Ok(None) => panic!("malformed stream unexpectedly succeeded"),
            }
        };
        let Error::Encode(message) = failure else {
            panic!("parsing error classification lost");
        };
        if malformed == MALFORMED_CHAT {
            assert!(message.contains("key must be a string"));
        } else {
            assert!(message.contains("prompt_tokens"));
        }
        assert_eq!(retries, 5);
        assert_eq!(wire.requests.lock().unwrap().len(), 6);
        assert_eq!(wire.drops.load(Ordering::SeqCst), 6);
        stream.close().await.unwrap();
    }
}

struct PartialBody {
    first: bool,
    wire: Arc<Wire>,
}
impl ByteStream for PartialBody {
    fn read<'a>(
        &'a mut self,
        buffer: &'a mut [u8],
        _: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<usize, HttpError>> {
        Box::pin(async move {
            self.wire.reads.fetch_add(1, Ordering::SeqCst);
            if self.first {
                self.first = false;
                buffer[..6].copy_from_slice(b"data: ");
                return Ok(6);
            }
            std::future::pending().await
        })
    }
}
impl Drop for PartialBody {
    fn drop(&mut self) {
        self.wire.drops.fetch_add(1, Ordering::SeqCst);
    }
}
struct PartialTransport(Arc<Wire>);
impl HttpTransport for PartialTransport {
    fn send<'a>(
        &'a self,
        request: HttpRequest,
        _: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<HttpResponse, HttpError>> {
        Box::pin(async move {
            self.0.requests.lock().unwrap().push(request);
            Ok(HttpResponse {
                status: 200,
                reason: "OK".into(),
                headers: Vec::new(),
                body: Box::new(PartialBody {
                    first: true,
                    wire: self.0.clone(),
                }),
            })
        })
    }
}

#[tokio::test]
async fn cancellation_snapshots_bytes_consumed_during_pending_partial_sse() {
    for protocol in [Protocol::ChatCompletions, Protocol::Responses] {
        let wire = Arc::new(Wire::default());
        let sink = Arc::new(Sink::default());
        let trace = trace(sink.clone(), None);
        let mut stream = driver(
            Arc::new(PartialTransport(wire.clone())),
            protocol,
            b"{}".to_vec(),
            2,
            Some(trace.clone()),
        );
        let cancel = Cancel::never();
        assert_eq!(
            stream.next(&cancel).await.unwrap(),
            Some(Event::HttpRequestStarted)
        );
        assert_eq!(
            stream.next(&cancel).await.unwrap(),
            Some(Event::HttpResponseHeadersReceived)
        );
        let mut observer = stream.next(&cancel);
        std::future::poll_fn(|context| {
            assert!(observer.as_mut().poll(context).is_pending());
            std::task::Poll::Ready(())
        })
        .await;
        assert_eq!(wire.reads.load(Ordering::SeqCst), 2);
        assert_eq!(wire.drops.load(Ordering::SeqCst), 0);
        assert!(trace.cancel());
        {
            let events = sink.0.lock().unwrap();
            let terminal = events
                .iter()
                .find(|event| event.operation == "request_finished")
                .unwrap();
            assert_eq!(terminal.outcome.as_deref(), Some("cancelled"));
            assert_eq!(terminal.response_bytes, Some(6));
        }
        // Discard only the pending observer before closing its still-owning stream.
        drop(observer);
        assert_eq!(stream.response_bytes(), 6);
        assert_eq!(wire.drops.load(Ordering::SeqCst), 0);
        stream.close().await.unwrap();
        stream.close().await.unwrap();
        drop(stream);
        assert_eq!(wire.drops.load(Ordering::SeqCst), 1);
        let events = sink.0.lock().unwrap();
        let terminals: Vec<_> = events
            .iter()
            .filter(|event| event.operation == "request_finished")
            .collect();
        assert_eq!(terminals.len(), 1);
        assert_eq!(terminals[0].response_bytes, Some(6));
    }
}

struct TypedFailureBody {
    error: HttpError,
    wire: Arc<Wire>,
}
impl ByteStream for TypedFailureBody {
    fn read<'a>(
        &'a mut self,
        _: &'a mut [u8],
        cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<usize, HttpError>> {
        Box::pin(async move {
            assert!(!cancellation.is_cancelled());
            self.wire.reads.fetch_add(1, Ordering::SeqCst);
            Err(self.error.clone())
        })
    }
}
impl Drop for TypedFailureBody {
    fn drop(&mut self) {
        self.wire.drops.fetch_add(1, Ordering::SeqCst);
    }
}
struct TypedFailureTransport {
    error: HttpError,
    body_failure: bool,
    wire: Arc<Wire>,
}
impl HttpTransport for TypedFailureTransport {
    fn send<'a>(
        &'a self,
        request: HttpRequest,
        cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<HttpResponse, HttpError>> {
        Box::pin(async move {
            assert!(!cancellation.is_cancelled());
            self.wire.requests.lock().unwrap().push(request);
            if !self.body_failure {
                return Err(self.error.clone());
            }
            Ok(HttpResponse {
                status: 200,
                reason: "OK".into(),
                headers: Vec::new(),
                body: Box::new(TypedFailureBody {
                    error: self.error.clone(),
                    wire: self.wire.clone(),
                }),
            })
        })
    }
}
fn diagnostic_retrying_http(
    protocol: Protocol,
    error: HttpError,
    body_failure: bool,
) -> (DiagnosticSession, Arc<Wire>, Arc<Sink>) {
    let wire = Arc::new(Wire::default());
    let transport = Arc::new(TypedFailureTransport {
        error,
        body_failure,
        wire: wire.clone(),
    });
    let mut options = CompatibleOptions::new("p", "https://example.test/v1", Arc::new(FixedKey));
    options.protocol = protocol;
    let provider = OpenAICompatibleProvider::new(options, transport, Arc::new(NoSocket)).unwrap();
    let retrying =
        RetryingProvider::with_sleeper(Arc::new(provider), 5, Arc::new(ImmediateSleeper));
    let sink = Arc::new(Sink::default());
    let session = DiagnosticSession::new(
        retrying.open_session().unwrap(),
        sink.clone(),
        Arc::new(MonotonicDiagnosticClock::default()),
        "agent".into(),
        "p".into(),
        None,
    );
    (session, wire, sink)
}
async fn assert_typed_request_cancelled(protocol: Protocol, body_failure: bool) {
    let message = "synthetic request transport cancelled independently of caller";
    let original = HttpError::RequestCancelled(message.into());
    let (session, wire, sink) = diagnostic_retrying_http(protocol, original.clone(), body_failure);
    let mut stream = session
        .call(Request {
            model: "m".into(),
            messages: vec![Message::user("hello")],
            ..Request::default()
        })
        .unwrap();
    let cancel = Cancel::never();
    let failure = loop {
        match stream.next(&cancel).await {
            Ok(Some(event)) => {
                assert!(
                    !matches!(event, Event::Retry { .. }),
                    "typed cancellation must not retry"
                );
                assert!(!event.is_visible_output());
            }
            Err(error) => break error,
            Ok(None) => panic!("typed cancellation unexpectedly succeeded"),
        }
    };
    assert!(!cancel.is_cancelled());
    assert_eq!(failure, Error::Http(original.clone()));
    assert_eq!(failure.to_string(), message);
    assert_eq!(
        std::error::Error::source(&failure)
            .unwrap()
            .downcast_ref::<HttpError>(),
        Some(&original)
    );
    stream.close().await.unwrap();
    session.close().await.unwrap();
    assert_eq!(wire.requests.lock().unwrap().len(), 1);
    assert_eq!(wire.drops.load(Ordering::SeqCst), usize::from(body_failure));
    let events = sink.0.lock().unwrap();
    println!("{protocol:?} body_failure={body_failure}: {events:#?}");
    let terminals: Vec<_> = events
        .iter()
        .filter(|event| {
            matches!(
                event.operation.as_str(),
                "request_finished" | "call_finished"
            )
        })
        .collect();
    assert_eq!(terminals.len(), 2);
    // Check the already-correct outer terminal before the reviewed request terminal.
    for operation in ["call_finished", "request_finished"] {
        let terminal = terminals
            .iter()
            .find(|event| event.operation == operation)
            .unwrap();
        assert_eq!(
            terminal.outcome.as_deref(),
            Some("cancelled"),
            "{operation}"
        );
        assert_eq!(
            terminal.severity,
            DiagnosticSeverity::Information,
            "{operation}"
        );
        assert_eq!(
            terminal.error_code.as_deref(),
            Some("cancelled"),
            "{operation}"
        );
    }
}
#[tokio::test]
async fn typed_request_cancelled_send_chat_has_two_cancelled_terminals() {
    assert_typed_request_cancelled(Protocol::ChatCompletions, false).await;
}
#[tokio::test]
async fn typed_request_cancelled_body_chat_has_two_cancelled_terminals() {
    assert_typed_request_cancelled(Protocol::ChatCompletions, true).await;
}
#[tokio::test]
async fn typed_request_cancelled_send_responses_has_two_cancelled_terminals() {
    assert_typed_request_cancelled(Protocol::Responses, false).await;
}
#[tokio::test]
async fn typed_request_cancelled_body_responses_has_two_cancelled_terminals() {
    assert_typed_request_cancelled(Protocol::Responses, true).await;
}
#[test]
fn fieldless_conversion_maps_only_fieldless_http_cancellation() {
    let converted = Error::from(HttpError::Cancelled);
    println!(
        "direct fieldless conversion before assertion: {converted:?}, message={}, source={:?}",
        converted,
        std::error::Error::source(&converted)
    );
    assert_eq!(converted, Error::Cancelled);
    assert_eq!(converted.to_string(), "The operation was canceled.");
    assert!(std::error::Error::source(&converted).is_none());
}
#[test]
fn fieldless_conversion_preserves_every_payload_and_other_http_variant() {
    let failures = [
        HttpError::Http("operation cancelled: HTTP request failure".into()),
        HttpError::ProviderHttp("operation cancelled: provider HTTP failure".into()),
        HttpError::RequestCancelled("transport's exact cancellation message".into()),
        HttpError::RequestCancelled("The operation was canceled.".into()),
        HttpError::RequestCancelled(" \ttransport cancellation\n".into()),
        HttpError::Provider("operation cancelled: provider failure".into()),
        HttpError::Response(HttpFailure {
            status: 503,
            error_type: "busy".into(),
            error_code: "retry".into(),
            detail: "operation cancelled".into(),
            response_body: "raw body".into(),
        }),
        HttpError::Wire("operation cancelled: wire failure".into()),
        HttpError::InvalidJsonString("operation cancelled: invalid JSON".into()),
        HttpError::RequestTooLarge(7),
        HttpError::HeaderTimeout(Duration::from_millis(11)),
        HttpError::Transport("operation cancelled: network failure".into()),
        HttpError::Disposed,
    ];
    for original in failures {
        let converted = Error::from(original.clone());
        assert_eq!(converted, Error::Http(original.clone()));
        assert_eq!(converted.to_string(), original.to_string());
        assert_eq!(
            std::error::Error::source(&converted)
                .unwrap()
                .downcast_ref::<HttpError>(),
            Some(&original)
        );
    }
}
async fn assert_fieldless_http_cancellation(protocol: Protocol, body_failure: bool) {
    let (session, wire, sink) =
        diagnostic_retrying_http(protocol, HttpError::Cancelled, body_failure);
    let mut stream = session
        .call(Request {
            model: "m".into(),
            ..Request::default()
        })
        .unwrap();
    let cancel = Cancel::never();
    let failure = loop {
        match stream.next(&cancel).await {
            Ok(Some(event)) => {
                assert!(!matches!(event, Event::Retry { .. }));
                assert!(!event.is_visible_output());
            }
            Err(error) => break error,
            Ok(None) => panic!("fieldless HTTP cancellation unexpectedly succeeded"),
        }
    };
    stream.close().await.unwrap();
    session.close().await.unwrap();
    assert!(!cancel.is_cancelled());
    assert_eq!(wire.requests.lock().unwrap().len(), 1);
    assert_eq!(wire.drops.load(Ordering::SeqCst), usize::from(body_failure));
    println!(
        "{protocol:?} body_failure={body_failure}: error={failure:?}, message={}, native_source={:?}",
        failure,
        std::error::Error::source(&failure)
    );
    assert_eq!(failure, Error::Cancelled);
    assert_eq!(failure.to_string(), "The operation was canceled.");
    assert!(std::error::Error::source(&failure).is_none());
    let events = sink.0.lock().unwrap();
    let terminals: Vec<_> = events
        .iter()
        .filter(|event| {
            matches!(
                event.operation.as_str(),
                "request_finished" | "call_finished"
            )
        })
        .collect();
    assert_eq!(terminals.len(), 2);
    for terminal in terminals {
        assert_eq!(terminal.outcome.as_deref(), Some("cancelled"));
        assert_eq!(terminal.severity, DiagnosticSeverity::Information);
        assert_eq!(terminal.error_code.as_deref(), Some("cancelled"));
    }
}
#[tokio::test]
async fn fieldless_conversion_send_chat_uses_session_cancellation() {
    assert_fieldless_http_cancellation(Protocol::ChatCompletions, false).await;
}
#[tokio::test]
async fn fieldless_conversion_body_chat_uses_session_cancellation() {
    assert_fieldless_http_cancellation(Protocol::ChatCompletions, true).await;
}
#[tokio::test]
async fn fieldless_conversion_send_responses_uses_session_cancellation() {
    assert_fieldless_http_cancellation(Protocol::Responses, false).await;
}
#[tokio::test]
async fn fieldless_conversion_body_responses_uses_session_cancellation() {
    assert_fieldless_http_cancellation(Protocol::Responses, true).await;
}
#[tokio::test]
async fn request_cancelled_wording_does_not_reclassify_ordinary_transport_errors() {
    for protocol in [Protocol::ChatCompletions, Protocol::Responses] {
        for body_failure in [false, true] {
            let original =
                HttpError::Transport("operation cancelled: ordinary network failure".into());
            let (session, wire, sink) =
                diagnostic_retrying_http(protocol, original.clone(), body_failure);
            let mut stream = session
                .call(Request {
                    model: "m".into(),
                    ..Request::default()
                })
                .unwrap();
            let cancel = Cancel::never();
            let mut retries = 0;
            let failure = loop {
                match stream.next(&cancel).await {
                    Ok(Some(Event::Retry { .. })) => retries += 1,
                    Ok(Some(event)) => assert!(!event.is_visible_output()),
                    Err(error) => break error,
                    Ok(None) => panic!("network failure unexpectedly succeeded"),
                }
            };
            assert_eq!(failure, Error::Http(original));
            assert!(!cancel.is_cancelled());
            assert_eq!(retries, 5);
            assert_eq!(wire.requests.lock().unwrap().len(), 6);
            stream.close().await.unwrap();
            session.close().await.unwrap();
            let events = sink.0.lock().unwrap();
            let terminals: Vec<_> = events
                .iter()
                .filter(|event| {
                    matches!(
                        event.operation.as_str(),
                        "request_finished" | "call_finished"
                    )
                })
                .collect();
            assert_eq!(terminals.len(), 7);
            for terminal in terminals {
                assert_eq!(terminal.outcome.as_deref(), Some("failed"));
                assert_eq!(terminal.severity, DiagnosticSeverity::Error);
                assert_eq!(terminal.error_code.as_deref(), Some("http"));
            }
        }
    }
}

#[tokio::test]
async fn truncated_and_provider_error_streams_keep_adapter_validation() {
    for protocol in [Protocol::ChatCompletions, Protocol::Responses] {
        let (transport, wire) = transport(200, &[]);
        let mut stream = driver(transport, protocol, b"{}".to_vec(), 2, None);
        let cancel = Cancel::never();
        stream.next(&cancel).await.unwrap();
        stream.next(&cancel).await.unwrap();
        let error = stream.next(&cancel).await.unwrap_err();
        assert!(error.to_string().contains("unexpected provider EOF"));
        assert_eq!(wire.drops.load(Ordering::SeqCst), 1);
    }
    let payload = "data: {\"type\":\"error\",\"error\":{\"type\":\"server_error\",\"code\":\"bad\",\"message\":\"failed\"}}\n\n";
    let (transport, _) = transport(200, &[payload]);
    let mut stream = driver(transport, Protocol::Responses, b"{}".to_vec(), 2, None);
    let cancel = Cancel::never();
    stream.next(&cancel).await.unwrap();
    stream.next(&cancel).await.unwrap();
    let Error::Response(parrot_provider::responses::ResponsesError::ProviderResponse {
        error_code,
        response_body,
        ..
    }) = stream.next(&cancel).await.unwrap_err()
    else {
        panic!("provider response error");
    };
    assert_eq!(error_code, "bad");
    assert!(response_body.contains("failed"));
}
