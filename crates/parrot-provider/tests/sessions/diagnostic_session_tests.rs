use parrot_domain::contracts::async_ops::{BoxFuture, Cancellation};
use parrot_provider::sessions::{
    Completion, Error, Event, EventStream, Request, Session,
    diagnostic_session::{DiagnosticSession, RequestTraceStream},
    diagnostics::*,
};
use std::{
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    time::Duration,
};
use tokio::sync::Notify;

#[derive(Default)]
struct Sink(Mutex<Vec<DiagnosticEvent>>);
impl DiagnosticSink for Sink {
    fn write(&self, event: DiagnosticEvent) {
        self.0.lock().unwrap().push(event);
    }
}
impl Sink {
    fn entries(&self) -> Vec<DiagnosticEvent> {
        self.0.lock().unwrap().clone()
    }
    fn terminals(&self) -> Vec<DiagnosticEvent> {
        self.entries()
            .into_iter()
            .filter(|e| e.operation == "call_finished")
            .collect()
    }
}
struct Clock;
impl DiagnosticClock for Clock {
    fn now_millis(&self) -> u64 {
        17
    }
}
#[derive(Default)]
struct Token {
    flag: AtomicBool,
    notify: Notify,
}
impl Token {
    fn cancel(&self) {
        self.flag.store(true, Ordering::SeqCst);
        self.notify.notify_waiters();
    }
}
impl Cancellation for Token {
    fn is_cancelled(&self) -> bool {
        self.flag.load(Ordering::SeqCst)
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
#[derive(Default)]
struct Counts {
    calls: AtomicUsize,
    closes: AtomicUsize,
    turns: AtomicUsize,
    fallbacks: AtomicUsize,
    sessions_closed: AtomicUsize,
}
struct Fake {
    counts: Arc<Counts>,
    read_error: Option<Error>,
    close_error: Option<Error>,
    open_error: Option<Error>,
}
impl Session for Fake {
    fn begin_turn(&self) -> BoxFuture<'_, Result<(), Error>> {
        self.counts.turns.fetch_add(1, Ordering::SeqCst);
        Box::pin(async { Ok(()) })
    }
    fn call(&self, _: Request) -> Result<Box<dyn EventStream>, Error> {
        panic!("wrapper must forward the actual CallTrace")
    }
    fn call_with_trace(
        &self,
        request: Request,
        trace: Arc<CallTrace>,
    ) -> Result<Box<dyn EventStream>, Error> {
        assert_eq!(request.model, "synthetic-model");
        self.counts.calls.fetch_add(1, Ordering::SeqCst);
        if let Some(error) = &self.open_error {
            return Err(error.clone());
        }
        trace.dump_prepared_body(b"synthetic prepared request");
        Ok(Box::new(RequestTraceStream::new(
            Box::new(FakeStream {
                counts: self.counts.clone(),
                step: 0,
                read_error: self.read_error.clone(),
                close_error: self.close_error.clone(),
                closed: false,
            }),
            trace,
            "synthetic".into(),
            "request-1".into(),
        )))
    }
    fn try_fall_back_to_http(&self) -> BoxFuture<'_, Result<bool, Error>> {
        self.counts.fallbacks.fetch_add(1, Ordering::SeqCst);
        Box::pin(async { Ok(true) })
    }
    fn close(&self) -> BoxFuture<'_, Result<(), Error>> {
        self.counts.sessions_closed.fetch_add(1, Ordering::SeqCst);
        Box::pin(async { Err(Error::Io("delegated session close".into())) })
    }
}
struct FakeStream {
    counts: Arc<Counts>,
    step: usize,
    read_error: Option<Error>,
    close_error: Option<Error>,
    closed: bool,
}
fn retry() -> Event {
    Event::Retry {
        attempt: 2,
        retry_after: Duration::from_millis(400),
        reason: "synthetic retry".into(),
    }
    .with_text("retry text")
}
fn completed() -> Event {
    Event::Completed(Completion {
        input_tokens: 11,
        cached_input_tokens: 3,
        output_tokens: 7,
        ..Completion::default()
    })
    .with_text("completion text")
}
impl EventStream for FakeStream {
    fn next<'a>(
        &'a mut self,
        _: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<Option<Event>, Error>> {
        Box::pin(async move {
            self.step += 1;
            match self.step {
                1 => Ok(Some(retry())),
                2 => {
                    if let Some(error) = &self.read_error {
                        Err(error.clone())
                    } else {
                        Ok(Some(completed()))
                    }
                }
                _ => Ok(None),
            }
        })
    }
    fn close(&mut self) -> BoxFuture<'_, Result<(), Error>> {
        Box::pin(async move {
            if self.closed {
                return Ok(());
            }
            self.closed = true;
            self.counts.closes.fetch_add(1, Ordering::SeqCst);
            self.close_error.clone().map_or(Ok(()), Err)
        })
    }
}
fn fixture(
    read_error: Option<Error>,
    close_error: Option<Error>,
) -> (DiagnosticSession, Arc<Sink>, Arc<Counts>) {
    let counts = Arc::new(Counts::default());
    let sink = Arc::new(Sink::default());
    let inner = Arc::new(Fake {
        counts: counts.clone(),
        read_error,
        close_error,
        open_error: None,
    });
    (
        DiagnosticSession::new(
            inner,
            sink.clone(),
            Arc::new(Clock),
            "synthetic-agent".into(),
            "synthetic-provider".into(),
            None,
        ),
        sink,
        counts,
    )
}
fn request() -> Request {
    Request {
        model: "synthetic-model".into(),
        ..Request::default()
    }
}
fn assert_terminal(sink: &Sink, outcome: &str, code: Option<&str>) {
    let terminals = sink.terminals();
    assert_eq!(terminals.len(), 1);
    let entry = &terminals[0];
    assert_eq!(entry.outcome.as_deref(), Some(outcome));
    assert_eq!(entry.error_code.as_deref(), code);
    assert_eq!(entry.agent_session_id.as_deref(), Some("synthetic-agent"));
    assert_eq!(entry.provider_id.as_deref(), Some("synthetic-provider"));
    assert_eq!(entry.model_id.as_deref(), Some("synthetic-model"));
}

#[tokio::test]
async fn lazy_call_forwards_wrapped_events_tokens_retries_and_request_context() {
    let (session, sink, counts) = fixture(None, None);
    let mut stream = session.call(request()).unwrap();
    assert!(sink.entries().is_empty());
    assert_eq!(counts.calls.load(Ordering::SeqCst), 0);
    let token = Token::default();
    assert_eq!(stream.next(&token).await.unwrap(), Some(retry()));
    assert_eq!(stream.next(&token).await.unwrap(), Some(completed()));
    assert!(
        sink.terminals().is_empty(),
        "completion event is not stream exhaustion"
    );
    assert_eq!(stream.next(&token).await.unwrap(), None);
    stream.close().await.unwrap();
    assert_eq!(stream.next(&token).await.unwrap(), None);
    assert_terminal(&sink, "completed", None);
    let entries = sink.entries();
    assert_eq!(
        entries
            .iter()
            .map(|e| e.operation.as_str())
            .collect::<Vec<_>>(),
        [
            "call_started",
            "request_started",
            "call_retry",
            "input_tokens",
            "cached_input_tokens",
            "output_tokens",
            "request_finished",
            "call_finished",
        ]
    );
    assert_eq!(entries[2].count, Some(2));
    assert_eq!(entries[2].duration_milliseconds, Some(400));
    assert_eq!(
        entries[3..6].iter().map(|e| e.count).collect::<Vec<_>>(),
        [Some(11), Some(3), Some(7)]
    );
    assert!(
        entries
            .iter()
            .all(|e| e.correlation_id == entries[0].correlation_id)
    );
    assert_eq!(counts.closes.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn owning_cancellation_finishes_nested_request_before_call_while_suspended() {
    let (session, sink, counts) = fixture(None, None);
    let token = Arc::new(Token::default());
    let weak = Arc::downgrade(&token);
    let mut stream = session.call_cancellable(request(), token.clone()).unwrap();
    stream.next(&Token::default()).await.unwrap();
    token.cancel();
    // No next or close: the watcher owns cancellation while the consumer pauses.
    tokio::time::timeout(Duration::from_secs(1), async {
        while sink.terminals().is_empty() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    assert_terminal(&sink, "cancelled", Some("cancelled"));
    let entries = sink.entries();
    let terminal_ops = entries
        .iter()
        .filter(|e| e.operation.ends_with("finished"))
        .collect::<Vec<_>>();
    assert_eq!(
        terminal_ops
            .iter()
            .map(|e| e.operation.as_str())
            .collect::<Vec<_>>(),
        ["request_finished", "call_finished"]
    );
    assert!(
        terminal_ops
            .iter()
            .all(|e| e.outcome.as_deref() == Some("cancelled"))
    );
    assert_eq!(counts.closes.load(Ordering::SeqCst), 0);
    stream.close().await.unwrap();
    stream.close().await.unwrap();
    drop(token);
    assert!(
        weak.upgrade().is_none(),
        "close joins watcher and releases owning token"
    );
    assert_eq!(counts.closes.load(Ordering::SeqCst), 1);
    assert_terminal(&sink, "cancelled", Some("cancelled"));
}

#[tokio::test]
async fn immediate_cancellation_then_close_is_single_terminal_and_preserves_cancel_over_dispose_failure()
 {
    let (session, sink, _) = fixture(None, Some(Error::Io("SECRET dispose".into())));
    let token = Arc::new(Token::default());
    let mut stream = session.call_cancellable(request(), token.clone()).unwrap();
    stream.next(&Token::default()).await.unwrap();
    token.cancel(); // deliberately no scheduling point before close
    assert_eq!(
        stream.close().await.unwrap_err(),
        Error::Io("SECRET dispose".into())
    );
    assert_terminal(&sink, "cancelled", Some("cancelled"));
    assert!(!format!("{:?}", sink.entries()).contains("SECRET"));
    assert_eq!(
        sink.entries()
            .iter()
            .find(|e| e.operation == "request_finished")
            .unwrap()
            .outcome
            .as_deref(),
        Some("cancelled")
    );
}

#[tokio::test]
async fn early_disposal_and_drop_record_disposed_once_and_unstarted_close_stays_lazy() {
    for explicit_close in [false, true] {
        let (session, sink, _) = fixture(None, None);
        let mut stream = session.call(request()).unwrap();
        stream.next(&Token::default()).await.unwrap();
        if explicit_close {
            stream.close().await.unwrap();
            stream.close().await.unwrap();
        }
        drop(stream);
        assert_terminal(&sink, "disposed", None);
    }
    let (session, sink, counts) = fixture(None, None);
    let mut stream = session.call(request()).unwrap();
    stream.close().await.unwrap();
    assert!(sink.entries().is_empty());
    assert_eq!(counts.calls.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn read_and_disposal_failure_precedence_matches_source_finally() {
    for (read, close, expected, code) in [
        (
            Some(Error::Wire("SECRET wire".into())),
            None,
            Error::Wire("SECRET wire".into()),
            "wire_protocol",
        ),
        (
            Some(Error::Wire("SECRET wire".into())),
            Some(Error::Io("SECRET dispose".into())),
            Error::Io("SECRET dispose".into()),
            "io",
        ),
        (
            Some(Error::Cancelled),
            Some(Error::Io("SECRET dispose".into())),
            Error::Io("SECRET dispose".into()),
            "io",
        ),
        (
            None,
            Some(Error::Io("SECRET dispose".into())),
            Error::Io("SECRET dispose".into()),
            "io",
        ),
    ] {
        let (session, sink, counts) = fixture(read, close);
        let mut stream = session.call(request()).unwrap();
        let token = Token::default();
        stream.next(&token).await.unwrap();
        let error = loop {
            match stream.next(&token).await {
                Err(error) => break error,
                Ok(Some(_)) => {}
                Ok(None) => panic!("expected failure"),
            }
        };
        assert_eq!(error, expected);
        stream.close().await.unwrap();
        assert_terminal(&sink, "failed", Some(code));
        assert_eq!(counts.closes.load(Ordering::SeqCst), 1);
        assert!(!format!("{:?}", sink.entries()).contains("SECRET"));
    }
}

#[tokio::test]
async fn ordinary_call_observes_per_pull_cancellation_and_delegates_session_lifecycle() {
    let (session, sink, counts) = fixture(None, None);
    session.begin_turn().await.unwrap();
    assert!(session.try_fall_back_to_http().await.unwrap());
    assert_eq!(
        session.close().await.unwrap_err(),
        Error::Io("delegated session close".into())
    );
    assert_eq!(counts.turns.load(Ordering::SeqCst), 1);
    assert_eq!(counts.fallbacks.load(Ordering::SeqCst), 1);
    assert_eq!(counts.sessions_closed.load(Ordering::SeqCst), 1);
    let mut stream = session.call(request()).unwrap();
    let token = Token::default();
    stream.next(&token).await.unwrap();
    token.cancel();
    assert!(
        sink.terminals().is_empty(),
        "ordinary call has no global watcher"
    );
    assert_eq!(stream.next(&token).await.unwrap_err(), Error::Cancelled);
    assert_terminal(&sink, "cancelled", Some("cancelled"));
}

#[tokio::test]
async fn completion_joins_watcher_and_construction_failure_is_lazy() {
    let (session, sink, _) = fixture(None, None);
    let token = Arc::new(Token::default());
    let weak = Arc::downgrade(&token);
    let mut stream = session.call_cancellable(request(), token.clone()).unwrap();
    drop(token);
    while stream.next(&Token::default()).await.unwrap().is_some() {}
    assert!(weak.upgrade().is_none());
    assert_terminal(&sink, "completed", None);

    let sink = Arc::new(Sink::default());
    let counts = Arc::new(Counts::default());
    let session = DiagnosticSession::new(
        Arc::new(Fake {
            counts: counts.clone(),
            read_error: None,
            close_error: None,
            open_error: Some(Error::InvalidOperation("SECRET open failure".into())),
        }),
        sink.clone(),
        Arc::new(Clock),
        "synthetic-agent".into(),
        "synthetic-provider".into(),
        None,
    );
    let mut stream = session.call(request()).unwrap();
    assert!(sink.entries().is_empty());
    assert_eq!(
        stream.next(&Token::default()).await.unwrap_err(),
        Error::InvalidOperation("SECRET open failure".into())
    );
    stream.close().await.unwrap();
    assert_terminal(&sink, "failed", Some("invalid_operation"));
    assert!(!format!("{:?}", sink.entries()).contains("SECRET"));
    assert_eq!(counts.closes.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn typed_failures_produce_allowlisted_codes_without_error_messages() {
    use parrot_provider::{
        http_stream::HttpError,
        responses::ResponsesError,
        responses_websocket::{SocketErrorCode, WebSocketError},
    };
    let secret = || "SECRET failure payload".to_string();
    for (error, code) in [
        (Error::Cancellation(secret()), "cancelled"),
        (Error::Io(secret()), "io"),
        (Error::Provider(secret()), "unexpected"),
        (Error::Encode(secret()), "unexpected"),
        (Error::Disposed, "invalid_operation"),
        (
            Error::Http(HttpError::HeaderTimeout(Duration::from_secs(5))),
            "unexpected",
        ),
        (Error::Http(HttpError::Transport(secret())), "http"),
        (
            Error::Http(HttpError::RequestTooLarge(100)),
            "request_too_large",
        ),
        (
            Error::Response(ResponsesError::WireProtocol(secret())),
            "wire_protocol",
        ),
        (
            Error::Socket(WebSocketError::Upgrade {
                status: 429,
                message: secret(),
            }),
            "websocket_upgrade",
        ),
        (
            Error::Socket(WebSocketError::Transport(secret())),
            "websocket_transport",
        ),
        (
            Error::Socket(WebSocketError::TransportFailure {
                message: secret(),
                cause: Box::new(WebSocketError::Socket {
                    code: SocketErrorCode::ConnectionClosedPrematurely,
                    message: secret(),
                }),
            }),
            "websocket_closed",
        ),
        (
            Error::Socket(WebSocketError::Socket {
                code: SocketErrorCode::Faulted,
                message: secret(),
            }),
            "websocket",
        ),
        (
            Error::Socket(WebSocketError::MessageTooLarge),
            "websocket_message_too_large",
        ),
        (
            Error::Socket(WebSocketError::InvalidRequest(secret())),
            "argument",
        ),
        (
            Error::Socket(WebSocketError::InvalidOperation(secret())),
            "invalid_operation",
        ),
    ] {
        let (session, sink, _) = fixture(Some(error.clone()), None);
        let mut stream = session.call(request()).unwrap();
        stream.next(&Token::default()).await.unwrap();
        assert_eq!(stream.next(&Token::default()).await.unwrap_err(), error);
        assert_terminal(
            &sink,
            if code == "cancelled" {
                "cancelled"
            } else {
                "failed"
            },
            Some(code),
        );
        assert!(!format!("{:?}", sink.entries()).contains("SECRET"));
    }
}

#[tokio::test]
async fn generic_request_trace_starts_before_body_events_and_dispose_failure_wins_exhaustion() {
    struct BodyEvents {
        events: std::collections::VecDeque<Event>,
        sink: Arc<Sink>,
        fail_close: bool,
    }
    impl EventStream for BodyEvents {
        fn next<'a>(
            &'a mut self,
            _: &'a dyn Cancellation,
        ) -> BoxFuture<'a, Result<Option<Event>, Error>> {
            Box::pin(async move {
                assert!(
                    self.sink
                        .entries()
                        .iter()
                        .any(|e| e.operation == "request_started")
                );
                Ok(self.events.pop_front())
            })
        }
        fn close(&mut self) -> BoxFuture<'_, Result<(), Error>> {
            Box::pin(async move {
                if self.fail_close {
                    Err(Error::Io("SECRET dispose".into()))
                } else {
                    Ok(())
                }
            })
        }
    }
    for fail_close in [false, true] {
        let sink = Arc::new(Sink::default());
        let call = Arc::new(CallTrace::start(
            sink.clone(),
            Arc::new(Clock),
            CallContext {
                agent_session_id: "agent".into(),
                correlation_id: "call".into(),
                provider_id: "provider".into(),
                model_id: "model".into(),
            },
            None,
        ));
        let events = vec![
            Event::HttpRequestStarted.with_text("request text"),
            Event::HttpResponseHeadersReceived,
            completed(),
        ];
        let mut stream = RequestTraceStream::new(
            Box::new(BodyEvents {
                events: events.clone().into(),
                sink: sink.clone(),
                fail_close,
            }),
            call.clone(),
            "synthetic".into(),
            "request-1".into(),
        );
        assert_eq!(sink.entries().len(), 1);
        for event in events {
            assert_eq!(stream.next(&Token::default()).await.unwrap(), Some(event));
        }
        assert_eq!(
            sink.entries().len(),
            2,
            "Completed event does not finish a request"
        );
        let result = stream.next(&Token::default()).await;
        if fail_close {
            assert_eq!(result.unwrap_err(), Error::Io("SECRET dispose".into()));
        } else {
            assert_eq!(result.unwrap(), None);
        }
        stream.close().await.unwrap();
        let entries = sink.entries();
        assert_eq!(
            entries
                .iter()
                .map(|e| e.operation.as_str())
                .collect::<Vec<_>>(),
            ["call_started", "request_started", "request_finished"]
        );
        assert_eq!(
            entries[2].outcome.as_deref(),
            Some(if fail_close { "failed" } else { "completed" })
        );
        assert_eq!(
            entries[2].error_code.as_deref(),
            if fail_close { Some("io") } else { None }
        );
        assert!(!format!("{entries:?}").contains("SECRET"));
        call.finish(TraceOutcome::Completed);
    }
}

#[tokio::test]
async fn dropping_stream_aborts_watcher_and_releases_token() {
    let (session, sink, _) = fixture(None, None);
    let token = Arc::new(Token::default());
    let weak = Arc::downgrade(&token);
    let mut stream = session.call_cancellable(request(), token.clone()).unwrap();
    stream.next(&Token::default()).await.unwrap();
    drop(token);
    drop(stream);
    tokio::time::timeout(Duration::from_secs(1), async {
        while weak.upgrade().is_some() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    assert_terminal(&sink, "disposed", None);
}

#[tokio::test]
async fn request_trace_in_flight_custom_cancellation_preserves_native_error_and_attempt_observation()
 {
    let token = Arc::new(Token::default());
    let sink = Arc::new(Sink::default());
    let observed_errors = Arc::new(AtomicUsize::new(0));
    let closes = Arc::new(AtomicUsize::new(0));
    let failure = Error::Cancellation("synthetic auth cancelled".into());
    let call = Arc::new(CallTrace::start(
        sink.clone(),
        Arc::new(Clock),
        CallContext {
            agent_session_id: "agent".into(),
            correlation_id: "call".into(),
            provider_id: "provider".into(),
            model_id: "model".into(),
        },
        None,
    ));
    let mut stream = RequestTraceStream::new(
        Box::new(CustomCancellationStream {
            token: token.clone(),
            observed_errors: observed_errors.clone(),
            closes: closes.clone(),
            failure: failure.clone(),
            closed: false,
        }),
        call.clone(),
        "synthetic".into(),
        "request-1".into(),
    );
    let result = tokio::time::timeout(Duration::from_secs(1), stream.next(token.as_ref()))
        .await
        .expect("cooperative request cancellation read must settle promptly");
    assert_eq!(
        (result, observed_errors.load(Ordering::SeqCst)),
        (Err(failure), 1)
    );
    assert_eq!(closes.load(Ordering::SeqCst), 1);
    tokio::time::timeout(Duration::from_secs(1), stream.close())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(closes.load(Ordering::SeqCst), 1);
    let entries = sink.entries();
    assert_eq!(
        entries
            .iter()
            .map(|entry| entry.operation.as_str())
            .collect::<Vec<_>>(),
        ["call_started", "request_started", "request_finished"]
    );
    assert_eq!(entries[2].outcome.as_deref(), Some("cancelled"));
    assert_eq!(entries[2].error_code.as_deref(), Some("cancelled"));
    assert!(!format!("{entries:?}").contains("synthetic auth cancelled"));
    call.finish(TraceOutcome::Cancelled);
}

struct CustomCancellationSession {
    token: Arc<Token>,
    observed_errors: Arc<AtomicUsize>,
    closes: Arc<AtomicUsize>,
    failure: Error,
}
impl Session for CustomCancellationSession {
    fn begin_turn(&self) -> BoxFuture<'_, Result<(), Error>> {
        Box::pin(async { Ok(()) })
    }
    fn call(&self, _: Request) -> Result<Box<dyn EventStream>, Error> {
        panic!("diagnostic trace must be forwarded")
    }
    fn call_with_trace(
        &self,
        _: Request,
        _: Arc<CallTrace>,
    ) -> Result<Box<dyn EventStream>, Error> {
        Ok(Box::new(CustomCancellationStream {
            token: self.token.clone(),
            observed_errors: self.observed_errors.clone(),
            closes: self.closes.clone(),
            failure: self.failure.clone(),
            closed: false,
        }))
    }
    fn try_fall_back_to_http(&self) -> BoxFuture<'_, Result<bool, Error>> {
        Box::pin(async { Ok(false) })
    }
    fn close(&self) -> BoxFuture<'_, Result<(), Error>> {
        Box::pin(async { Ok(()) })
    }
}
struct CustomCancellationStream {
    token: Arc<Token>,
    observed_errors: Arc<AtomicUsize>,
    closes: Arc<AtomicUsize>,
    failure: Error,
    closed: bool,
}
impl EventStream for CustomCancellationStream {
    fn next<'a>(
        &'a mut self,
        cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<Option<Event>, Error>> {
        Box::pin(async move {
            // A credential operation cancels its caller, then finishes with its
            // own native cancellation exception. An outer cancellation observer
            // must not discard this already-issued operation before it settles.
            self.token.cancel();
            tokio::task::yield_now().await;
            assert!(
                cancellation.is_cancelled(),
                "the active read retains the caller's cancellation"
            );
            let result = Err(self.failure.clone());
            self.observed_errors.fetch_add(1, Ordering::SeqCst);
            result
        })
    }
    fn close(&mut self) -> BoxFuture<'_, Result<(), Error>> {
        Box::pin(async {
            if !self.closed {
                self.closed = true;
                self.closes.fetch_add(1, Ordering::SeqCst);
            }
            Ok(())
        })
    }
}
#[tokio::test]
async fn in_flight_custom_cancellation_preserves_native_error_and_attempt_observation() {
    for owning in [false, true] {
        let token = Arc::new(Token::default());
        let sink = Arc::new(Sink::default());
        let observed_errors = Arc::new(AtomicUsize::new(0));
        let closes = Arc::new(AtomicUsize::new(0));
        let failure = Error::Cancellation("synthetic auth cancelled".into());
        let session = DiagnosticSession::new(
            Arc::new(CustomCancellationSession {
                token: token.clone(),
                observed_errors: observed_errors.clone(),
                closes: closes.clone(),
                failure: failure.clone(),
            }),
            sink.clone(),
            Arc::new(Clock),
            "synthetic-agent".into(),
            "synthetic-provider".into(),
            None,
        );
        let mut stream = if owning {
            session.call_cancellable(request(), token.clone()).unwrap()
        } else {
            session.call(request()).unwrap()
        };
        let result = tokio::time::timeout(Duration::from_secs(1), stream.next(token.as_ref()))
            .await
            .expect("cooperative cancellation read must settle promptly");
        assert_eq!(
            (result, observed_errors.load(Ordering::SeqCst)),
            (Err(failure.clone()), 1)
        );
        assert_eq!(failure.to_string(), "synthetic auth cancelled");
        assert!(
            std::error::Error::source(&failure).is_none(),
            "native cancellation has no fabricated cause"
        );
        assert_eq!(closes.load(Ordering::SeqCst), 1);
        assert_terminal(&sink, "cancelled", Some("cancelled"));
        assert_eq!(
            sink.entries()
                .iter()
                .filter(|event| event.operation == "call_started")
                .count(),
            1
        );
        assert!(!format!("{:?}", sink.entries()).contains("synthetic auth cancelled"));
        tokio::time::timeout(Duration::from_secs(1), stream.close())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(closes.load(Ordering::SeqCst), 1);
        assert_terminal(&sink, "cancelled", Some("cancelled"));
    }
}
