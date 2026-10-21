use parrot_domain::contracts::async_ops::{BoxFuture, Cancellation};
use parrot_provider::{
    http_stream::{HttpError, HttpFailure},
    responses::ResponsesError,
    responses_websocket::WebSocketError,
    sessions::{
        Completion, Error, Event, EventStream, Provider, Request, Session,
        decorators::{
            RetrySleeper, RetryingProvider, SubscriptionUsage, UsageReporter,
            UsageReportingProvider, classify_failure,
        },
        retry::Failure,
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

#[derive(Default)]
struct Cancel {
    cancelled: AtomicBool,
    notify: tokio::sync::Notify,
}
impl Cancel {
    fn cancel(&self) {
        self.cancelled.store(true, Ordering::SeqCst);
        self.notify.notify_waiters();
    }
}
impl Cancellation for Cancel {
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
type Step = Result<Option<Event>, Error>;
#[derive(Default)]
struct State {
    attempts: Mutex<VecDeque<VecDeque<Step>>>,
    requests: Mutex<Vec<Request>>,
    closes: AtomicUsize,
    begins: AtomicUsize,
    session_closes: AtomicUsize,
    fallbacks: AtomicUsize,
    fallback: AtomicBool,
    opens: AtomicUsize,
    traces: Mutex<Vec<Arc<parrot_provider::sessions::diagnostics::CallTrace>>>,
    credentials: AtomicUsize,
    models: Mutex<Vec<parrot_provider::catalogs::Model>>,
    reporter: Mutex<Option<Arc<dyn UsageReporter>>>,
}
struct FakeProvider(Arc<State>);
struct FakeSession(Arc<State>);
struct FakeStream {
    state: Arc<State>,
    steps: VecDeque<Step>,
    closed: bool,
}
impl Provider for FakeProvider {
    fn id(&self) -> &str {
        "fake"
    }
    fn call(&self, request: Request) -> Result<Box<dyn EventStream>, Error> {
        FakeSession(self.0.clone()).call(request)
    }
    fn has_credential<'a>(&'a self, _: &'a dyn Cancellation) -> BoxFuture<'a, Result<bool, Error>> {
        self.0.credentials.fetch_add(1, Ordering::SeqCst);
        Box::pin(async { Ok(true) })
    }
    fn usage_reporter(&self) -> Option<Arc<dyn UsageReporter>> {
        self.0.reporter.lock().unwrap().clone()
    }
    fn seed_models(&self) -> Vec<parrot_provider::catalogs::Model> {
        self.0.models.lock().unwrap().clone()
    }
    fn list_models<'a>(
        &'a self,
        _: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<Vec<parrot_provider::catalogs::Model>, Error>> {
        Box::pin(async { Ok(vec![]) })
    }
    fn calculate_image_tokens(
        &self,
        _: &parrot_provider::catalogs::Model,
        _: &parrot_provider::sessions::Content,
    ) -> i64 {
        42
    }
    fn open_session(&self) -> Result<Arc<dyn Session>, Error> {
        self.0.opens.fetch_add(1, Ordering::SeqCst);
        Ok(Arc::new(FakeSession(self.0.clone())))
    }
}
impl Session for FakeSession {
    fn begin_turn(&self) -> BoxFuture<'_, Result<(), Error>> {
        Box::pin(async {
            self.0.begins.fetch_add(1, Ordering::SeqCst);
            Ok(())
        })
    }
    fn close(&self) -> BoxFuture<'_, Result<(), Error>> {
        Box::pin(async {
            self.0.session_closes.fetch_add(1, Ordering::SeqCst);
            Ok(())
        })
    }
    fn try_fall_back_to_http(&self) -> BoxFuture<'_, Result<bool, Error>> {
        Box::pin(async {
            self.0.fallbacks.fetch_add(1, Ordering::SeqCst);
            Ok(self.0.fallback.load(Ordering::SeqCst))
        })
    }
    fn call_with_trace(
        &self,
        request: Request,
        trace: Arc<parrot_provider::sessions::diagnostics::CallTrace>,
    ) -> Result<Box<dyn EventStream>, Error> {
        self.0.traces.lock().unwrap().push(trace);
        self.call(request)
    }
    fn call(&self, request: Request) -> Result<Box<dyn EventStream>, Error> {
        self.0.requests.lock().unwrap().push(request);
        let steps = self
            .0
            .attempts
            .lock()
            .unwrap()
            .pop_front()
            .unwrap_or_default();
        Ok(Box::new(FakeStream {
            state: self.0.clone(),
            steps,
            closed: false,
        }))
    }
}
impl EventStream for FakeStream {
    fn next<'a>(&'a mut self, _: &'a dyn Cancellation) -> BoxFuture<'a, Step> {
        Box::pin(async { self.steps.pop_front().unwrap_or(Ok(None)) })
    }
    fn close(&mut self) -> BoxFuture<'_, Result<(), Error>> {
        Box::pin(async {
            if !self.closed {
                self.closed = true;
                self.state.closes.fetch_add(1, Ordering::SeqCst);
            }
            Ok(())
        })
    }
}
fn setup(attempts: Vec<Vec<Step>>, retries: u32) -> (Arc<State>, Arc<dyn Session>) {
    let state = Arc::new(State::default());
    *state.attempts.lock().unwrap() = attempts.into_iter().map(VecDeque::from).collect();
    let session = RetryingProvider::new(Arc::new(FakeProvider(state.clone())), retries)
        .open_session()
        .unwrap();
    (state, session)
}
fn wire() -> Error {
    Error::Wire("dropped".into())
}
fn req() -> Request {
    Request {
        max_output_tokens: 8192,
        ..Request::default()
    }
}
fn retry(event: Option<Event>) -> Duration {
    match event.unwrap() {
        Event::Retry { retry_after, .. } => retry_after,
        event => panic!("not retry: {event:?}"),
    }
}

#[tokio::test(start_paused = true)]
async fn retry_is_lazy_and_delay_begins_only_on_following_pull() {
    let (state, session) = setup(
        vec![
            vec![Err(wire())],
            vec![Ok(Some(Event::TextDelta("ok".into())))],
        ],
        5,
    );
    let mut stream = session.call(req()).unwrap();
    assert_eq!(state.requests.lock().unwrap().len(), 0);
    let cancel = Cancel::default();
    let start = tokio::time::Instant::now();
    assert_eq!(
        retry(stream.next(&cancel).await.unwrap()),
        Duration::from_millis(200)
    );
    assert_eq!(start.elapsed(), Duration::ZERO);
    assert_eq!(state.requests.lock().unwrap().len(), 1);
    assert_eq!(state.closes.load(Ordering::SeqCst), 1);
    assert_eq!(
        stream.next(&cancel).await.unwrap(),
        Some(Event::TextDelta("ok".into()))
    );
    assert_eq!(start.elapsed(), Duration::from_millis(200));
    stream.close().await.unwrap();
    stream.close().await.unwrap();
    assert_eq!(state.closes.load(Ordering::SeqCst), 2);
    assert_eq!(state.opens.load(Ordering::SeqCst), 1);
}
#[tokio::test(start_paused = true)]
async fn cancellation_during_retry_delay_does_not_start_an_attempt() {
    let (state, session) = setup(vec![vec![Err(wire())]], 5);
    let mut stream = session.call(req()).unwrap();
    let cancel = Cancel::default();
    retry(stream.next(&cancel).await.unwrap());
    cancel.cancel();
    assert_eq!(stream.next(&cancel).await, Err(Error::Cancelled));
    assert_eq!(stream.next(&cancel).await, Ok(None));
    assert_eq!(state.requests.lock().unwrap().len(), 1);
}
#[tokio::test(start_paused = true)]
async fn every_visible_event_suppresses_retry() {
    let visible = vec![
        Event::TextDelta(String::new()),
        Event::Completed(Completion::default()),
        Event::ToolCallDelta(Default::default()),
        Event::ReasoningDelta {
            text: String::new(),
            kind: parrot_provider::sessions::ReasoningKind::Raw,
            part_id: String::new(),
            completed: false,
        },
    ];
    for event in visible {
        let (state, session) = setup(vec![vec![Ok(Some(event.clone())), Err(wire())]], 5);
        let mut stream = session.call(req()).unwrap();
        let cancel = Cancel::default();
        assert_eq!(stream.next(&cancel).await.unwrap(), Some(event));
        assert_eq!(stream.next(&cancel).await, Err(wire()));
        assert_eq!(state.requests.lock().unwrap().len(), 1);
        assert_eq!(state.closes.load(Ordering::SeqCst), 1);
    }
}
#[tokio::test(start_paused = true)]
async fn lifecycle_and_encrypted_metadata_do_not_block_retry() {
    let events = vec![
        Event::HttpRequestStarted,
        Event::HttpResponseHeadersReceived,
        Event::EncryptedReasoningReceived,
    ];
    let mut steps: Vec<Step> = events
        .iter()
        .cloned()
        .map(|event| Ok(Some(event)))
        .collect();
    steps.push(Err(wire()));
    let (state, session) = setup(vec![steps, vec![]], 5);
    let mut stream = session.call(req()).unwrap();
    let cancel = Cancel::default();
    for event in events {
        assert_eq!(stream.next(&cancel).await.unwrap(), Some(event));
    }
    retry(stream.next(&cancel).await.unwrap());
    assert_eq!(stream.next(&cancel).await.unwrap(), None);
    assert_eq!(state.closes.load(Ordering::SeqCst), 2);
}
#[tokio::test(start_paused = true)]
async fn context_reduction_changes_only_cloned_request() {
    let failure = Error::Http(HttpError::Response(HttpFailure { status: 400, error_type: "invalid_request_error".into(), error_code: "context_length_exceeded".into(), detail: "Requested token count exceeds the model's maximum context length of 10000 tokens. You requested a total of 14000 tokens: 6000 tokens from the input messages and 8000 tokens for the completion.".into(), response_body: "original".into() }));
    let (state, session) = setup(vec![vec![Err(failure)], vec![]], 5);
    let request = Request {
        max_output_tokens: 8000,
        ..req()
    };
    let mut stream = session.call(request.clone()).unwrap();
    let cancel = Cancel::default();
    assert_eq!(retry(stream.next(&cancel).await.unwrap()), Duration::ZERO);
    stream.next(&cancel).await.unwrap();
    let requests = state.requests.lock().unwrap();
    assert_eq!(requests[0].max_output_tokens, 8000);
    assert!(requests[1].max_output_tokens < 8000);
    assert_eq!(request.max_output_tokens, 8000);
}
#[tokio::test(start_paused = true)]
async fn socket_budget_exhaustion_falls_back_then_resets_stream_budget() {
    let socket = Error::Socket(WebSocketError::Transport("closed".into()));
    let mut attempts = vec![vec![Err(socket)]; 6];
    attempts.push(vec![Err(wire())]);
    attempts.push(vec![]);
    let (state, session) = setup(attempts, 5);
    state.fallback.store(true, Ordering::SeqCst);
    let mut stream = session.call(req()).unwrap();
    let cancel = Cancel::default();
    for _ in 0..6 {
        retry(stream.next(&cancel).await.unwrap());
    }
    assert_eq!(state.fallbacks.load(Ordering::SeqCst), 1);
    assert_eq!(
        retry(stream.next(&cancel).await.unwrap()),
        Duration::from_millis(200)
    );
    assert_eq!(stream.next(&cancel).await.unwrap(), None);
    assert_eq!(state.closes.load(Ordering::SeqCst), 8);
}
#[tokio::test(start_paused = true)]
async fn unsupported_fallback_reports_source_invalid_operation() {
    let original = Error::Socket(WebSocketError::Upgrade {
        status: 503,
        message: "upgrade".into(),
    });
    let (state, session) = setup(vec![vec![Err(original.clone())]; 6], 5);
    let mut stream = session.call(req()).unwrap();
    let cancel = Cancel::default();
    for _ in 0..5 {
        retry(stream.next(&cancel).await.unwrap());
    }
    assert_eq!(
        stream.next(&cancel).await,
        Err(Error::InvalidOperation(
            "A provider call requested HTTP fallback without supporting it.".into()
        ))
    );
    assert_eq!(state.fallbacks.load(Ordering::SeqCst), 1);
}
#[tokio::test(start_paused = true)]
async fn header_limit_and_session_lifecycle_delegation() {
    let failure = Error::Http(HttpError::HeaderTimeout(Duration::from_secs(60)));
    let (state, session) = setup(
        vec![vec![Err(failure.clone())], vec![Err(failure.clone())]],
        1,
    );
    session.begin_turn().await.unwrap();
    let mut stream = session.call(req()).unwrap();
    let cancel = Cancel::default();
    assert_eq!(
        retry(stream.next(&cancel).await.unwrap()),
        Duration::from_secs(2)
    );
    let exhausted = stream.next(&cancel).await.unwrap_err();
    assert_eq!(
        exhausted.to_string(),
        "Provider header timeout retry limit exceeded (1 retries, 2 attempts)."
    );
    assert_eq!(
        exhausted,
        Error::HeaderRetryLimit {
            maximum_retries: 1,
            attempts: 2,
            cause: Box::new(failure.clone())
        }
    );
    let cause = std::error::Error::source(&exhausted)
        .unwrap()
        .downcast_ref::<Error>()
        .unwrap();
    assert_eq!(cause, &failure);
    session.close().await.unwrap();
    assert_eq!(state.begins.load(Ordering::SeqCst), 1);
    assert_eq!(state.session_closes.load(Ordering::SeqCst), 1);
}
#[test]
fn typed_nested_errors_keep_provider_classification_fields() {
    let response = ResponsesError::ProviderResponse {
        error_type: "type".into(),
        error_code: "code".into(),
        detail: "detail".into(),
        response_body: "secret".into(),
    };
    let expected = Failure::Response {
        error_type: "type".into(),
        code: "code".into(),
        detail: "detail".into(),
    };
    assert_eq!(
        classify_failure(&Error::Response(response.clone())),
        expected
    );
    assert_eq!(
        classify_failure(&Error::Socket(WebSocketError::Response(response))),
        expected
    );
    assert_eq!(
        classify_failure(&Error::ChatProvider(
            parrot_provider::chat_completions::ProviderFailure {
                error_type: "type".into(),
                error_code: "code".into(),
                message: "detail".into(),
                response_body: "secret".into()
            }
        )),
        expected
    );
}
#[tokio::test(start_paused = true)]
async fn direct_provider_retry_does_not_open_a_session_and_forwards_capabilities() {
    let state = Arc::new(State::default());
    *state.attempts.lock().unwrap() =
        vec![VecDeque::from(vec![Err(wire())]), VecDeque::new()].into();
    let provider = RetryingProvider::new(Arc::new(FakeProvider(state.clone())), 5);
    let cancel = Cancel::default();
    assert!(provider.has_credential(&cancel).await.unwrap());
    assert!(provider.seed_models().is_empty());
    assert!(provider.list_models(&cancel).await.unwrap().is_empty());
    assert_eq!(
        provider.calculate_image_tokens(
            &parrot_provider::catalogs::Model::new("model", "fake"),
            &parrot_provider::sessions::Content::text("x")
        ),
        42
    );
    let mut stream = provider.call(req()).unwrap();
    retry(stream.next(&cancel).await.unwrap());
    assert_eq!(stream.next(&cancel).await.unwrap(), None);
    assert_eq!(state.opens.load(Ordering::SeqCst), 0);
    assert_eq!(state.requests.lock().unwrap().len(), 2);
}
#[tokio::test(start_paused = true)]
async fn direct_provider_fallback_reports_source_invalid_operation() {
    let state = Arc::new(State::default());
    *state.attempts.lock().unwrap() = vec![
        VecDeque::from(vec![Err(Error::Socket(
            WebSocketError::Transport("closed".into())
        ))]);
        6
    ]
    .into();
    let provider = RetryingProvider::new(Arc::new(FakeProvider(state.clone())), 5);
    let mut stream = provider.call(req()).unwrap();
    let cancel = Cancel::default();
    for _ in 0..5 {
        retry(stream.next(&cancel).await.unwrap());
    }
    assert_eq!(
        stream.next(&cancel).await,
        Err(Error::InvalidOperation(
            "A stateless provider cannot select HTTP fallback.".into()
        ))
    );
    assert_eq!(state.opens.load(Ordering::SeqCst), 0);
    assert_eq!(state.fallbacks.load(Ordering::SeqCst), 0);
}
#[tokio::test(start_paused = true)]
async fn retries_preserve_attached_call_trace_identity() {
    use parrot_provider::sessions::diagnostics::{
        CallContext, CallTrace, DiagnosticEvent, DiagnosticSink, MonotonicDiagnosticClock,
    };
    struct Sink;
    impl DiagnosticSink for Sink {
        fn write(&self, _: DiagnosticEvent) {}
    }
    let trace = Arc::new(CallTrace::start(
        Arc::new(Sink),
        Arc::new(MonotonicDiagnosticClock::default()),
        CallContext {
            agent_session_id: "agent".into(),
            correlation_id: "call".into(),
            provider_id: "fake".into(),
            model_id: "model".into(),
        },
        None,
    ));
    let (state, session) = setup(vec![vec![Err(wire())], vec![]], 5);
    let mut stream = session.call_with_trace(req(), trace.clone()).unwrap();
    let cancel = Cancel::default();
    retry(stream.next(&cancel).await.unwrap());
    stream.next(&cancel).await.unwrap();
    let traces = state.traces.lock().unwrap();
    assert_eq!(traces.len(), 2);
    assert!(traces.iter().all(|observed| Arc::ptr_eq(observed, &trace)));
}
#[tokio::test]
async fn cancelled_or_nonretryable_failures_retain_the_exact_original_error() {
    let errors = vec![
        Error::Cancellation("cancel detail".into()),
        Error::Provider("missing key".into()),
        Error::Http(HttpError::Response(HttpFailure {
            status: 401,
            error_type: "auth".into(),
            error_code: "bad_key".into(),
            detail: "detail".into(),
            response_body: "raw response bytes".into(),
        })),
    ];
    for original in errors {
        let (state, session) = setup(vec![vec![Err(original.clone())]], 5);
        let mut stream = session.call(req()).unwrap();
        assert_eq!(stream.next(&Cancel::default()).await, Err(original));
        assert_eq!(state.requests.lock().unwrap().len(), 1);
        assert_eq!(state.closes.load(Ordering::SeqCst), 1);
    }
}
#[tokio::test(start_paused = true)]
async fn default_sleeper_zero_completes_synchronously_but_prior_cancellation_wins() {
    use parrot_provider::sessions::decorators::TokioRetrySleeper;
    let cancellation = Cancel::default();
    let start = tokio::time::Instant::now();
    TokioRetrySleeper
        .sleep(Duration::ZERO, &cancellation)
        .await
        .unwrap();
    assert_eq!(start.elapsed(), Duration::ZERO);
    cancellation.cancel();
    assert_eq!(
        TokioRetrySleeper.sleep(Duration::ZERO, &cancellation).await,
        Err(Error::Cancelled)
    );
}
#[derive(Default)]
struct RecordingSleeper {
    delays: Mutex<Vec<Duration>>,
    wait_for_cancellation: bool,
}
impl RetrySleeper for RecordingSleeper {
    fn sleep<'a>(
        &'a self,
        delay: Duration,
        cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<(), Error>> {
        self.delays.lock().unwrap().push(delay);
        Box::pin(async move {
            if self.wait_for_cancellation {
                cancellation.cancelled().await;
                return Err(Error::Cancelled);
            }
            assert!(!cancellation.is_cancelled());
            Ok(())
        })
    }
}
fn provider_with_sleeper(
    attempts: Vec<Vec<Step>>,
    sleeper: Arc<dyn RetrySleeper>,
) -> (Arc<State>, RetryingProvider) {
    let state = Arc::new(State::default());
    *state.attempts.lock().unwrap() = attempts.into_iter().map(VecDeque::from).collect();
    let provider =
        RetryingProvider::with_sleeper(Arc::new(FakeProvider(state.clone())), 5, sleeper);
    (state, provider)
}
#[tokio::test(start_paused = true)]
async fn injected_sleeper_records_exact_requested_delays_only_after_retry_pull() {
    let sleeper = Arc::new(RecordingSleeper::default());
    let (state, provider) = provider_with_sleeper(
        vec![vec![Err(wire())], vec![Err(wire())], vec![]],
        sleeper.clone(),
    );
    let mut stream = provider.call(req()).unwrap();
    let cancellation = Cancel::default();
    let started = tokio::time::Instant::now();
    assert_eq!(
        retry(stream.next(&cancellation).await.unwrap()),
        Duration::from_millis(200)
    );
    assert!(sleeper.delays.lock().unwrap().is_empty());
    assert_eq!(state.requests.lock().unwrap().len(), 1);
    assert_eq!(
        retry(stream.next(&cancellation).await.unwrap()),
        Duration::from_millis(400)
    );
    assert_eq!(
        *sleeper.delays.lock().unwrap(),
        vec![Duration::from_millis(200)]
    );
    assert_eq!(stream.next(&cancellation).await.unwrap(), None);
    assert_eq!(
        *sleeper.delays.lock().unwrap(),
        vec![Duration::from_millis(200), Duration::from_millis(400)]
    );
    assert_eq!(started.elapsed(), Duration::ZERO);
    assert_eq!(state.opens.load(Ordering::SeqCst), 0);
}
#[tokio::test(start_paused = true)]
async fn session_retries_use_injected_sleeper_and_check_cancellation_before_timer_creation() {
    let sleeper = Arc::new(RecordingSleeper::default());
    let (state, provider) =
        provider_with_sleeper(vec![vec![Err(wire())], vec![Err(wire())]], sleeper.clone());
    let session = provider.open_session().unwrap();
    let mut stream = session.call(req()).unwrap();
    let cancellation = Cancel::default();
    retry(stream.next(&cancellation).await.unwrap());
    retry(stream.next(&cancellation).await.unwrap());
    assert_eq!(
        *sleeper.delays.lock().unwrap(),
        vec![Duration::from_millis(200)]
    );
    cancellation.cancel();
    assert_eq!(stream.next(&cancellation).await, Err(Error::Cancelled));
    assert_eq!(
        *sleeper.delays.lock().unwrap(),
        vec![Duration::from_millis(200)]
    );
    assert_eq!(state.requests.lock().unwrap().len(), 2);
}
#[tokio::test(start_paused = true)]
async fn zero_delay_retry_creates_no_timer_and_does_not_advance_tokio_time() {
    let failure = Error::Response(ResponsesError::ProviderResponse {
        error_type: "invalid_request_error".into(), error_code: "context_length_exceeded".into(),
        detail: "Requested token count exceeds the model's maximum context length of 10000 tokens. You requested a total of 14000 tokens: 6000 tokens from the input messages and 8000 tokens for the completion.".into(),
        response_body: String::new(),
    });
    let sleeper = Arc::new(RecordingSleeper::default());
    let (state, provider) =
        provider_with_sleeper(vec![vec![Err(failure)], vec![]], sleeper.clone());
    let mut stream = provider.open_session().unwrap().call(req()).unwrap();
    let cancellation = Cancel::default();
    let started = tokio::time::Instant::now();
    assert_eq!(
        retry(stream.next(&cancellation).await.unwrap()),
        Duration::ZERO
    );
    assert_eq!(stream.next(&cancellation).await.unwrap(), None);
    assert!(sleeper.delays.lock().unwrap().is_empty());
    assert_eq!(started.elapsed(), Duration::ZERO);
    assert_eq!(state.requests.lock().unwrap().len(), 2);
}
#[tokio::test(start_paused = true)]
async fn cancellation_during_injected_sleep_does_not_start_another_attempt() {
    let sleeper = Arc::new(RecordingSleeper {
        wait_for_cancellation: true,
        ..Default::default()
    });
    let (state, provider) = provider_with_sleeper(vec![vec![Err(wire())], vec![]], sleeper.clone());
    let mut stream = provider.call(req()).unwrap();
    let cancellation = Cancel::default();
    retry(stream.next(&cancellation).await.unwrap());
    let result = tokio::join!(stream.next(&cancellation), async {
        tokio::task::yield_now().await;
        assert_eq!(
            *sleeper.delays.lock().unwrap(),
            vec![Duration::from_millis(200)]
        );
        assert_eq!(state.requests.lock().unwrap().len(), 1);
        cancellation.cancel();
    })
    .0;
    assert_eq!(result, Err(Error::Cancelled));
    assert_eq!(state.requests.lock().unwrap().len(), 1);
    assert_eq!(stream.next(&cancellation).await.unwrap(), None);
}
struct Reporter(AtomicUsize);
impl UsageReporter for Reporter {
    fn usage<'a>(
        &'a self,
        _: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<SubscriptionUsage, Error>> {
        Box::pin(async {
            self.0.fetch_add(1, Ordering::SeqCst);
            Ok(SubscriptionUsage {
                plan_type: "quota".into(),
                ..Default::default()
            })
        })
    }
}
#[tokio::test]
async fn usage_capability_is_transparent_and_never_observes_completed_tokens() {
    let state = Arc::new(State::default());
    let reporter_impl = Arc::new(Reporter(AtomicUsize::new(0)));
    let reporter: Arc<dyn UsageReporter> = reporter_impl.clone();
    state
        .attempts
        .lock()
        .unwrap()
        .push_back(VecDeque::from(vec![Ok(Some(Event::Completed(
            Completion {
                input_tokens: 11,
                cached_input_tokens: 3,
                output_tokens: 9,
                ..Default::default()
            },
        )))]));
    let provider: Arc<dyn Provider> = Arc::new(UsageReportingProvider::new(
        Arc::new(FakeProvider(state.clone())),
        reporter.clone(),
    ));
    let retrying = RetryingProvider::new(provider, 5);
    assert_eq!(retrying.id(), "fake");
    assert!(Arc::ptr_eq(&retrying.usage_reporter().unwrap(), &reporter));
    let mut direct = retrying.call(req()).unwrap();
    assert!(matches!(
        direct.next(&Cancel::default()).await.unwrap(),
        Some(Event::Completed(Completion {
            input_tokens: 11,
            cached_input_tokens: 3,
            output_tokens: 9,
            ..
        }))
    ));
    assert_eq!(reporter_impl.0.load(Ordering::SeqCst), 0);
    direct.close().await.unwrap();
    let session = retrying.open_session().unwrap();
    assert_eq!(state.opens.load(Ordering::SeqCst), 1);
    assert_eq!(
        reporter.usage(&Cancel::default()).await.unwrap().plan_type,
        "quota"
    );
    session.close().await.unwrap();
}

// Exact argument matrices from pinned RetryingProviderTests.cs. Supplemental
// tests above remain separate; these assertions track the canonical methods.
fn canonical_request() -> Request {
    Request {
        model: "m".into(),
        ..Default::default()
    }
}
fn canonical_completed(input: i32, output: i32, text: &str) -> Event {
    Event::Completed(Completion {
        finish_reason: "stop".into(),
        input_tokens: input,
        output_tokens: output,
        assistant_text: text.into(),
        ..Default::default()
    })
}
fn canonical_http(status: u16, kind: &str, code: &str, detail: &str) -> Error {
    Error::Http(HttpError::Response(HttpFailure {
        status,
        error_type: kind.into(),
        error_code: code.into(),
        detail: detail.into(),
        response_body: String::new(),
    }))
}
fn canonical_response(kind: &str, code: &str, detail: &str) -> Error {
    Error::Response(ResponsesError::ProviderResponse {
        error_type: kind.into(),
        error_code: code.into(),
        detail: detail.into(),
        response_body: String::new(),
    })
}
fn canonical_visible() -> Vec<Event> {
    vec![
        Event::TextDelta(String::new()),
        Event::ReasoningDelta {
            text: String::new(),
            kind: parrot_provider::sessions::ReasoningKind::Raw,
            part_id: String::new(),
            completed: false,
        },
        Event::ToolCallDelta(Default::default()),
        Event::Completed(Default::default()),
    ]
}
fn canonical_fixture(
    attempts: Vec<Vec<Step>>,
    headers: u32,
) -> (Arc<State>, RetryingProvider, Arc<RecordingSleeper>) {
    let sleeper = Arc::new(RecordingSleeper::default());
    let (state, _) = provider_with_sleeper(attempts, sleeper.clone());
    let provider = RetryingProvider::with_sleeper(
        Arc::new(FakeProvider(state.clone())),
        headers,
        sleeper.clone(),
    );
    (state, provider, sleeper)
}
fn canonical_stream(
    provider: &dyn Provider,
    session: &Arc<dyn Session>,
    use_session: bool,
    request: Request,
) -> Box<dyn EventStream> {
    if use_session {
        session.call(request).unwrap()
    } else {
        provider.call(request).unwrap()
    }
}
async fn canonical_drain(
    mut stream: Box<dyn EventStream>,
    cancellation: &dyn Cancellation,
) -> (Vec<Event>, Result<(), Error>) {
    let mut events = vec![];
    let result = loop {
        match stream.next(cancellation).await {
            Ok(Some(event)) => events.push(event),
            Ok(None) => break Ok(()),
            Err(error) => break Err(error),
        }
    };
    stream.close().await.unwrap();
    (events, result)
}
fn canonical_calls(state: &State) -> usize {
    state.requests.lock().unwrap().len()
}
fn canonical_retry_count(events: &[Event]) -> usize {
    events
        .iter()
        .filter(|event| matches!(event, Event::Retry { .. }))
        .count()
}
fn canonical_kind(event: &Event) -> &'static str {
    match event.base_event() {
        Event::HttpRequestStarted => "HttpRequestStarted",
        Event::HttpResponseHeadersReceived => "HttpResponseHeadersReceived",
        Event::EncryptedReasoningReceived => "EncryptedReasoningReceived",
        Event::TextDelta(_) => "TextDelta",
        Event::ReasoningDelta { .. } => "ReasoningDelta",
        Event::ToolCallDelta(_) => "ToolCallDelta",
        Event::Retry { .. } => "Retry",
        Event::Completed(_) => "Completed",
        _ => unreachable!(),
    }
}
#[tokio::test]
async fn canonical_idle_timeout_after_visible_output_is_not_retried() {
    for use_session in [false, true] {
        for event in canonical_visible().into_iter().take(3) {
            let failure =
                Error::Wire("provider: idle timeout waiting for HTTP/SSE response bytes".into());
            let (state, provider, _) =
                canonical_fixture(vec![vec![Ok(Some(event.clone())), Err(failure.clone())]], 5);
            let session = provider.open_session().unwrap();
            let (events, result) = canonical_drain(
                canonical_stream(&provider, &session, use_session, canonical_request()),
                &Cancel::default(),
            )
            .await;
            assert_eq!(result, Err(failure));
            assert_eq!(canonical_calls(&state), 1);
            assert_eq!(events, vec![event]);
            session.close().await.unwrap();
        }
    }
}
#[tokio::test]
async fn canonical_encrypted_reasoning_metadata_preserves_retry_eligibility() {
    for use_session in [false, true] {
        let failure =
            Error::Wire("provider: idle timeout waiting for HTTP/SSE response bytes".into());
        let (state, provider, _) = canonical_fixture(
            vec![
                vec![Ok(Some(Event::EncryptedReasoningReceived)), Err(failure)],
                vec![
                    Ok(Some(Event::EncryptedReasoningReceived)),
                    Ok(Some(canonical_completed(1, 1, "answer"))),
                ],
            ],
            5,
        );
        let session = provider.open_session().unwrap();
        let (events, result) = canonical_drain(
            canonical_stream(&provider, &session, use_session, canonical_request()),
            &Cancel::default(),
        )
        .await;
        result.unwrap();
        assert_eq!(canonical_calls(&state), 2);
        assert_eq!(
            events
                .iter()
                .map(canonical_kind)
                .collect::<Vec<_>>()
                .join(","),
            "EncryptedReasoningReceived,Retry,EncryptedReasoningReceived,Completed"
        );
        assert!(!events[0].is_visible_output());
        assert!(!events[2].is_visible_output());
        session.close().await.unwrap();
    }
}
#[tokio::test]
async fn canonical_idle_timeout_before_output_retries_with_bounded_exhaustion() {
    for use_session in [false, true] {
        for exhausted in [false, true] {
            let failure =
                Error::Wire("provider: idle timeout waiting for HTTP/SSE response bytes".into());
            let mut attempts = vec![
                vec![
                    Ok(Some(Event::HttpRequestStarted)),
                    Ok(Some(Event::HttpResponseHeadersReceived)),
                    Err(failure.clone())
                ];
                if exhausted { 6 } else { 1 }
            ];
            if !exhausted {
                attempts.push(vec![Ok(Some(canonical_completed(1, 1, "answer")))]);
            }
            let (state, provider, _) = canonical_fixture(attempts, 5);
            let session = provider.open_session().unwrap();
            let (events, result) = canonical_drain(
                canonical_stream(&provider, &session, use_session, canonical_request()),
                &Cancel::default(),
            )
            .await;
            if exhausted {
                assert_eq!(result, Err(failure));
            } else {
                result.unwrap();
                assert_eq!(canonical_kind(events.last().unwrap()), "Completed");
            }
            assert_eq!(canonical_calls(&state), if exhausted { 6 } else { 2 });
            assert_eq!(
                canonical_retry_count(&events),
                if exhausted { 5 } else { 1 }
            );
            session.close().await.unwrap();
        }
    }
}
#[tokio::test]
async fn canonical_header_timeout_retry_can_succeed_or_be_cancelled() {
    for use_session in [false, true] {
        for cancel_retry in [false, true] {
            let (state, provider, sleeper) = canonical_fixture(
                vec![
                    vec![Err(Error::HeaderTimeout(
                        "Exception of type 'Parrot.Llm.Wire.HeaderTimeoutException' was thrown."
                            .into(),
                    ))],
                    vec![Ok(Some(canonical_completed(0, 0, "")))],
                ],
                1,
            );
            let session = provider.open_session().unwrap();
            let cancellation = Cancel::default();
            let mut stream =
                canonical_stream(&provider, &session, use_session, canonical_request());
            assert_eq!(
                canonical_kind(&stream.next(&cancellation).await.unwrap().unwrap()),
                "Retry"
            );
            if cancel_retry {
                cancellation.cancel();
                assert_eq!(stream.next(&cancellation).await, Err(Error::Cancelled));
                assert_eq!(canonical_calls(&state), 1);
                assert!(sleeper.delays.lock().unwrap().is_empty());
            } else {
                assert_eq!(
                    canonical_kind(&stream.next(&cancellation).await.unwrap().unwrap()),
                    "Completed"
                );
                assert_eq!(stream.next(&cancellation).await.unwrap(), None);
                assert_eq!(canonical_calls(&state), 2);
            }
            stream.close().await.unwrap();
            session.close().await.unwrap();
        }
    }
}
#[tokio::test]
async fn canonical_http_lifecycle_events_do_not_prevent_retry() {
    for use_session in [false, true] {
        for received_headers in [false, true] {
            let mut first = vec![Ok(Some(Event::HttpRequestStarted))];
            if received_headers {
                first.push(Ok(Some(Event::HttpResponseHeadersReceived)));
            }
            first.push(Err(Error::HeaderTimeout(
                "Exception of type 'Parrot.Llm.Wire.HeaderTimeoutException' was thrown.".into(),
            )));
            let (state, provider, _) = canonical_fixture(
                vec![
                    first,
                    vec![
                        Ok(Some(Event::HttpRequestStarted)),
                        Ok(Some(Event::HttpResponseHeadersReceived)),
                        Ok(Some(canonical_completed(1, 1, "answer"))),
                    ],
                ],
                1,
            );
            let session = provider.open_session().unwrap();
            let (events, result) = canonical_drain(
                canonical_stream(&provider, &session, use_session, canonical_request()),
                &Cancel::default(),
            )
            .await;
            result.unwrap();
            assert_eq!(
                events
                    .iter()
                    .map(canonical_kind)
                    .collect::<Vec<_>>()
                    .join(","),
                if received_headers {
                    "HttpRequestStarted,HttpResponseHeadersReceived,Retry,HttpRequestStarted,HttpResponseHeadersReceived,Completed"
                } else {
                    "HttpRequestStarted,Retry,HttpRequestStarted,HttpResponseHeadersReceived,Completed"
                }
            );
            assert_eq!(canonical_calls(&state), 2);
            assert_eq!(events.last().unwrap(), &canonical_completed(1, 1, "answer"));
            session.close().await.unwrap();
        }
    }
}
#[tokio::test]
async fn canonical_header_timeout_retries_stop_at_the_configured_limit() {
    for use_session in [false, true] {
        for maximum_retries in [0, 1, 5] {
            let timeout = Error::HeaderTimeout("private-sentinel".into());
            let (state, provider, _) = canonical_fixture(
                vec![vec![Err(timeout.clone())]; maximum_retries as usize + 1],
                maximum_retries,
            );
            let session = provider.open_session().unwrap();
            let (events, result) = canonical_drain(
                canonical_stream(&provider, &session, use_session, canonical_request()),
                &Cancel::default(),
            )
            .await;
            assert_eq!(canonical_calls(&state), maximum_retries as usize + 1);
            assert_eq!(events.len(), maximum_retries as usize);
            assert_eq!(
                events
                    .iter()
                    .map(|event| match event {
                        Event::Retry { attempt, .. } => *attempt,
                        _ => panic!("unexpected event"),
                    })
                    .collect::<Vec<_>>(),
                (1..=maximum_retries as i32).collect::<Vec<_>>()
            );
            let exhausted = result.unwrap_err();
            assert_eq!(
                exhausted.to_string(),
                format!(
                    "Provider header timeout retry limit exceeded ({maximum_retries} retries, {} attempts).",
                    maximum_retries + 1
                )
            );
            assert_eq!(
                std::error::Error::source(&exhausted)
                    .unwrap()
                    .downcast_ref::<Error>(),
                Some(&timeout)
            );
            session.close().await.unwrap();
        }
    }
}
#[tokio::test]
async fn canonical_retry_events_preserve_output_without_duplicate_observation() {
    for observer_fails in [false, true] {
        let completed = canonical_completed(1, 2, "private-sentinel");
        let (state, provider, _) = canonical_fixture(
            vec![
                vec![Err(Error::Io("private-sentinel".into()))],
                vec![Ok(Some(completed.clone()))],
            ],
            5,
        );
        let session = provider.open_session().unwrap();
        let observed = Arc::new(Mutex::new(vec![]));
        let capture = observed.clone();
        let stream = session
            .call_with_retry_observation(
                canonical_request(),
                Arc::new(move |attempt, delay| {
                    capture.lock().unwrap().push((attempt, delay));
                    assert!(!observer_fails, "private-sentinel");
                }),
            )
            .unwrap();
        let (events, result) = canonical_drain(stream, &Cancel::default()).await;
        result.unwrap();
        assert_eq!(canonical_calls(&state), 2);
        assert_eq!(&events[1..], &[completed]);
        assert!(observed.lock().unwrap().is_empty());
        assert_eq!(
            events[0],
            Event::Retry {
                attempt: 1,
                retry_after: Duration::from_millis(200),
                reason: "Provider connection or protocol failure. Retrying.".into()
            }
        );
        session.close().await.unwrap();
    }
}
#[tokio::test]
async fn canonical_transient_failures_emit_safe_retry_events_before_waiting() {
    for use_session in [false, true] {
        for (kind, expected_ms) in [
            ("headers", 2000),
            ("http429", 200),
            ("http500", 200),
            ("http503", 2000),
            ("upgrade429", 200),
            ("upgrade502", 200),
            ("websocket", 200),
            ("network", 200),
        ] {
            let failure = match kind {
                "headers" => Error::HeaderTimeout("private-sentinel".into()),
                "http429" => canonical_http(429, "", "", "private-sentinel"),
                "http500" => canonical_http(500, "", "", "private-sentinel"),
                "http503" => canonical_http(503, "", "", "private-sentinel"),
                "upgrade429" => Error::Socket(WebSocketError::Upgrade {
                    status: 429,
                    message: "private-sentinel".into(),
                }),
                "upgrade502" => Error::Socket(WebSocketError::Upgrade {
                    status: 502,
                    message: "private-sentinel".into(),
                }),
                "websocket" => Error::Socket(WebSocketError::Transport("private-sentinel".into())),
                _ => Error::Io("private-sentinel".into()),
            };
            let (state, provider, sleeper) = canonical_fixture(vec![vec![Err(failure)]], 5);
            let session = provider.open_session().unwrap();
            let mut stream =
                canonical_stream(&provider, &session, use_session, canonical_request());
            let event = stream.next(&Cancel::default()).await.unwrap().unwrap();
            assert_eq!(canonical_calls(&state), 1);
            match event {
                Event::Retry {
                    attempt,
                    retry_after,
                    reason,
                } => {
                    assert_eq!(attempt, 1);
                    assert_eq!(retry_after, Duration::from_millis(expected_ms));
                    assert!(!reason.trim().is_empty());
                    assert!(!reason.contains("private-sentinel"));
                }
                _ => panic!("not retry: {kind}"),
            }
            assert!(sleeper.delays.lock().unwrap().is_empty());
            stream.close().await.unwrap();
            session.close().await.unwrap();
        }
    }
}
#[tokio::test]
async fn canonical_visible_retry_is_not_also_reported_to_the_silent_observer() {
    let (state, provider, _) = canonical_fixture(
        vec![vec![Err(canonical_http(
            503,
            "",
            "",
            "Service Unavailable",
        ))]],
        5,
    );
    let session = provider.open_session().unwrap();
    let observations = Arc::new(AtomicUsize::new(0));
    let capture = observations.clone();
    let mut stream = session
        .call_with_retry_observation(
            canonical_request(),
            Arc::new(move |_, _| {
                capture.fetch_add(1, Ordering::SeqCst);
            }),
        )
        .unwrap();
    assert_eq!(
        canonical_kind(&stream.next(&Cancel::default()).await.unwrap().unwrap()),
        "Retry"
    );
    assert_eq!(observations.load(Ordering::SeqCst), 0);
    assert_eq!(canonical_calls(&state), 1);
    stream.close().await.unwrap();
    session.close().await.unwrap();
}
#[tokio::test]
async fn canonical_context_budget_recovery_is_bounded_and_preserves_the_request() {
    const DETAIL: &str = "litellm.BadRequestError: OpenAIException - Requested token count exceeds the model's maximum context length of 180000 tokens. You requested a total of 180514 tokens: 147746 tokens from the input messages and 32768 tokens for the completion. Please reduce your prompt.";
    const SECOND: &str = "Requested token count exceeds the model's maximum context length of 180000 tokens. You requested a total of 180001 tokens: 155135 tokens from the input messages and 24866 tokens for the completion.";
    for use_session in [false, true] {
        for fails_again in [false, true] {
            for stream_error in [false, true] {
                let failure = if stream_error {
                    canonical_response("null", "400", DETAIL)
                } else {
                    canonical_http(
                        400,
                        "null",
                        "400",
                        &format!("{DETAIL} Fallback failed: {DETAIL}"),
                    )
                };
                let second = if stream_error {
                    canonical_response("null", "400", SECOND)
                } else {
                    canonical_http(400, "null", "400", SECOND)
                };
                let (state, provider, _) = canonical_fixture(
                    vec![
                        vec![Err(failure)],
                        if fails_again {
                            vec![Err(second.clone())]
                        } else {
                            vec![Ok(Some(canonical_completed(147746, 1, "reply")))]
                        },
                    ],
                    5,
                );
                let session = provider.open_session().unwrap();
                let request = Request {
                    max_output_tokens: 32768,
                    instructions: "preserve".into(),
                    include_router_metadata: true,
                    ..canonical_request()
                };
                let (events, result) = canonical_drain(
                    canonical_stream(&provider, &session, use_session, request.clone()),
                    &Cancel::default(),
                )
                .await;
                if fails_again {
                    assert_eq!(result, Err(second));
                    assert_eq!(events.len(), 1);
                } else {
                    result.unwrap();
                    assert_eq!(canonical_kind(events.last().unwrap()), "Completed");
                }
                assert_eq!(canonical_calls(&state), 2);
                assert_eq!(
                    events[0],
                    Event::Retry {
                        attempt: 1,
                        retry_after: Duration::ZERO,
                        reason:
                            "Context limit exceeded. Retrying with a reduced output token budget."
                                .into()
                    }
                );
                assert_eq!(
                    *state.requests.lock().unwrap(),
                    vec![
                        request.clone(),
                        Request {
                            max_output_tokens: 24866,
                            ..request.clone()
                        }
                    ]
                );
                assert_eq!(request.max_output_tokens, 32768);
                session.close().await.unwrap();
            }
        }
    }
}
#[tokio::test]
async fn canonical_context_errors_without_output_capacity_are_not_retried() {
    for use_session in [false, true] {
        for (input, total) in [("180001", "212769"), ("171429", "204197")] {
            let failure = canonical_http(
                400,
                "null",
                "400",
                &format!(
                    "Requested token count exceeds the model's maximum context length of 180000 tokens. You requested a total of {total} tokens: {input} tokens from the input messages and 32768 tokens for the completion."
                ),
            );
            let (state, provider, _) = canonical_fixture(vec![vec![Err(failure.clone())]], 5);
            let session = provider.open_session().unwrap();
            let (_, result) = canonical_drain(
                canonical_stream(
                    &provider,
                    &session,
                    use_session,
                    Request {
                        max_output_tokens: 32768,
                        ..canonical_request()
                    },
                ),
                &Cancel::default(),
            )
            .await;
            assert_eq!(result, Err(failure));
            assert_eq!(canonical_calls(&state), 1);
            session.close().await.unwrap();
        }
    }
}
#[tokio::test]
async fn canonical_context_errors_after_visible_output_are_not_retried() {
    for use_session in [false, true] {
        for event in canonical_visible() {
            let failure = canonical_http(
                400,
                "null",
                "400",
                "Requested token count exceeds the model's maximum context length of 180000 tokens. You requested a total of 180514 tokens: 147746 tokens from the input messages and 32768 tokens for the completion.",
            );
            let (state, provider, _) =
                canonical_fixture(vec![vec![Ok(Some(event)), Err(failure.clone())]], 5);
            let session = provider.open_session().unwrap();
            let (_, result) = canonical_drain(
                canonical_stream(
                    &provider,
                    &session,
                    use_session,
                    Request {
                        max_output_tokens: 32768,
                        ..canonical_request()
                    },
                ),
                &Cancel::default(),
            )
            .await;
            assert_eq!(result, Err(failure));
            assert_eq!(canonical_calls(&state), 1);
            session.close().await.unwrap();
        }
    }
}
#[tokio::test]
async fn inner_invalid_operation_retries_in_direct_and_session_calls() {
    for use_session in [false, true] {
        let failure = Error::InvalidOperation("private-sentinel".into());
        assert_eq!(classify_failure(&failure), Failure::Protocol);
        let completed = canonical_completed(1, 1, "reply");
        let (state, provider, sleeper) = canonical_fixture(
            vec![vec![Err(failure)], vec![Ok(Some(completed.clone()))]],
            5,
        );
        let session = provider.open_session().unwrap();
        let (events, result) = canonical_drain(
            canonical_stream(&provider, &session, use_session, canonical_request()),
            &Cancel::default(),
        )
        .await;
        result.unwrap();
        assert_eq!(
            events,
            vec![
                Event::Retry {
                    attempt: 1,
                    retry_after: Duration::from_millis(200),
                    reason: "Provider connection or protocol failure. Retrying.".into(),
                },
                completed
            ]
        );
        assert_eq!(canonical_calls(&state), 2);
        assert_eq!(
            *sleeper.delays.lock().unwrap(),
            vec![Duration::from_millis(200)]
        );
        session.close().await.unwrap();
    }
}
#[tokio::test]
async fn canonical_a_drop_before_any_output_reconnects() {
    let (state, provider, _) = canonical_fixture(
        vec![
            vec![Err(Error::Io("dropped".into()))],
            vec![
                Ok(Some(Event::TextDelta("hi".into()))),
                Ok(Some(canonical_completed(1, 1, "hi"))),
            ],
        ],
        5,
    );
    let (events, result) = canonical_drain(
        provider.call(canonical_request()).unwrap(),
        &Cancel::default(),
    )
    .await;
    result.unwrap();
    assert_eq!(canonical_calls(&state), 2);
    assert_eq!(
        events
            .iter()
            .filter_map(|event| if let Event::TextDelta(text) = event {
                Some(text.as_str())
            } else {
                None
            })
            .collect::<String>(),
        "hi"
    );
    assert_eq!(canonical_kind(events.last().unwrap()), "Completed");
}
#[tokio::test]
async fn canonical_a_drop_after_output_is_not_retried() {
    let failure = Error::Io("mid-stream".into());
    let (state, provider, _) = canonical_fixture(
        vec![vec![
            Ok(Some(Event::TextDelta("partial".into()))),
            Err(failure.clone()),
        ]],
        5,
    );
    let (events, result) = canonical_drain(
        provider.call(canonical_request()).unwrap(),
        &Cancel::default(),
    )
    .await;
    assert_eq!(result, Err(failure));
    assert_eq!(canonical_calls(&state), 1);
    assert_eq!(events, vec![Event::TextDelta("partial".into())]);
}
#[tokio::test]
async fn canonical_a_permanent_failure_is_not_retried() {
    for error_type in [
        "usage_limit_reached",
        "invalid_request_error",
        "request_too_large",
    ] {
        let failure = if error_type == "request_too_large" {
            Error::Http(HttpError::RequestTooLarge(1))
        } else {
            canonical_http(400, error_type, "", "no")
        };
        let (state, provider, _) = canonical_fixture(vec![vec![Err(failure.clone())]], 5);
        let (_, result) = canonical_drain(
            provider.call(canonical_request()).unwrap(),
            &Cancel::default(),
        )
        .await;
        assert_eq!(result, Err(failure));
        assert_eq!(canonical_calls(&state), 1);
    }
}
#[tokio::test]
async fn canonical_service_unavailable_uses_the_visible_overload_retry() {
    for error_code in ["", "insufficient_quota"] {
        let (state, provider, _) = canonical_fixture(
            vec![vec![Err(canonical_http(
                503,
                "",
                error_code,
                "Service Unavailable",
            ))]],
            5,
        );
        let mut stream = provider.call(canonical_request()).unwrap();
        assert_eq!(
            stream.next(&Cancel::default()).await.unwrap(),
            Some(Event::Retry {
                attempt: 1,
                retry_after: Duration::from_secs(2),
                reason: "Provider servers are overloaded. Retrying (attempt 1/5).".into()
            })
        );
        assert_eq!(canonical_calls(&state), 1);
        stream.close().await.unwrap();
    }
}
#[tokio::test]
async fn canonical_credential_availability_is_delegated_without_retry() {
    let (state, provider, _) = canonical_fixture(vec![], 5);
    assert!(provider.has_credential(&Cancel::default()).await.unwrap());
    assert_eq!(state.credentials.load(Ordering::SeqCst), 1);
    assert_eq!(canonical_calls(&state), 0);
}
#[tokio::test]
async fn canonical_a_terminal_stream_error_surfaces() {
    let failure = canonical_response("insufficient_quota", "", "spent");
    let (state, provider, _) = canonical_fixture(vec![vec![Err(failure.clone())]], 5);
    let (_, result) = canonical_drain(
        provider.call(canonical_request()).unwrap(),
        &Cancel::default(),
    )
    .await;
    assert_eq!(result, Err(failure));
    assert_eq!(canonical_calls(&state), 1);
}
#[tokio::test]
async fn canonical_a_structured_stream_context_error_is_not_retried() {
    let failure = canonical_response("", "context_window_exceeded", "too long");
    let (state, provider, _) = canonical_fixture(vec![vec![Err(failure.clone())]], 5);
    let (_, result) = canonical_drain(
        provider.call(canonical_request()).unwrap(),
        &Cancel::default(),
    )
    .await;
    assert_eq!(result, Err(failure));
    assert_eq!(canonical_calls(&state), 1);
}
struct CanonicalUsageReporter {
    calls: AtomicUsize,
    cancellation: AtomicUsize,
}
impl UsageReporter for CanonicalUsageReporter {
    fn usage<'a>(
        &'a self,
        cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<SubscriptionUsage, Error>> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.cancellation.store(
            cancellation as *const dyn Cancellation as *const () as usize,
            Ordering::SeqCst,
        );
        Box::pin(async {
            Ok(SubscriptionUsage {
                plan_type: "subscription".into(),
                ..Default::default()
            })
        })
    }
}
#[tokio::test]
async fn canonical_seed_models_and_optional_usage_reporting_are_forwarded() {
    for reports_usage in [false, true] {
        let reporter = Arc::new(CanonicalUsageReporter {
            calls: AtomicUsize::new(0),
            cancellation: AtomicUsize::new(0),
        });
        let (state, provider, _) = canonical_fixture(vec![], 5);
        let seed = vec![parrot_provider::catalogs::Model::new("seed", "scripted")];
        *state.models.lock().unwrap() = seed.clone();
        if reports_usage {
            *state.reporter.lock().unwrap() = Some(reporter.clone());
        }
        assert_eq!(provider.seed_models(), seed);
        let cancellation = Cancel::default();
        if reports_usage {
            let forwarded = provider.usage_reporter().unwrap();
            let expected: Arc<dyn UsageReporter> = reporter.clone();
            assert!(Arc::ptr_eq(&forwarded, &expected));
            assert_eq!(
                forwarded.usage(&cancellation).await.unwrap().plan_type,
                "subscription"
            );
            assert_eq!(reporter.calls.load(Ordering::SeqCst), 1);
            assert_eq!(
                reporter.cancellation.load(Ordering::SeqCst),
                &cancellation as *const Cancel as usize
            );
        } else {
            assert!(provider.usage_reporter().is_none());
            assert_eq!(reporter.calls.load(Ordering::SeqCst), 0);
        }
    }
}
#[tokio::test]
async fn canonical_stateless_sessions_report_unsupported_fallback_without_consuming_a_call() {
    for retry in [false, true] {
        let (state, wrapped, _) =
            canonical_fixture(vec![vec![Ok(Some(canonical_completed(1, 1, "reply")))]], 5);
        let provider: Arc<dyn Provider> = if retry {
            Arc::new(wrapped)
        } else {
            Arc::new(FakeProvider(state.clone()))
        };
        let session = provider.open_session().unwrap();
        assert!(!session.try_fall_back_to_http().await.unwrap());
        assert_eq!(canonical_calls(&state), 0);
        let (events, result) = canonical_drain(
            session.call(canonical_request()).unwrap(),
            &Cancel::default(),
        )
        .await;
        result.unwrap();
        assert_eq!(events.len(), 1);
        assert_eq!(canonical_kind(&events[0]), "Completed");
        assert_eq!(canonical_calls(&state), 1);
        session.close().await.unwrap();
    }
}
#[tokio::test]
async fn canonical_exhausted_websocket_retries_preserve_the_unsupported_session_fallback_error() {
    let (state, provider, _) = canonical_fixture(
        vec![
            vec![Err(Error::Socket(WebSocketError::Transport(
                "dropped".into()
            )))];
            6
        ],
        5,
    );
    let session = provider.open_session().unwrap();
    let (_, result) = canonical_drain(
        session.call(canonical_request()).unwrap(),
        &Cancel::default(),
    )
    .await;
    assert_eq!(
        result,
        Err(Error::InvalidOperation(
            "A provider call requested HTTP fallback without supporting it.".into()
        ))
    );
    assert_eq!(canonical_calls(&state), 6);
    session.close().await.unwrap();
}
