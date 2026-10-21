use std::{
    collections::VecDeque,
    io,
    path::PathBuf,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering},
    },
    time::Duration,
};

use parrot_domain::{
    contracts::{
        async_ops::{BoxFuture, Cancellation, OperationError},
        events::EventPublisher,
        paths::{RootOrigin, SelectedSessionRoot},
        storage::Committed,
    },
    protocol::messages::parrot::{self as protocol, Event},
};
use parrot_provider::sessions::{self, Completion, Error, EventStream, Request, Session, ToolCall};
use parrot_runtime::{
    agent_turn::provider_call::{
        OutputWriter, ProviderActivity, ProviderCall, ProviderCallError, ProviderUsage,
    },
    event_broker::{BrokerError, EventBroker},
};
use parrot_storage::{
    conversation::{AgentHistoryView, ConversationRepository},
    database::SessionDatabase,
    events::{EventError, EventRepository, NeverCancelled},
    history_files::AgentHistoryFile,
};
use tokio::sync::{Notify, Semaphore};

static NEXT: AtomicU64 = AtomicU64::new(0);

struct Fixture {
    path: PathBuf,
    history: AgentHistoryView,
}
impl Fixture {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!(
            "parrot-t46-provider-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&path).unwrap();
        let root = SelectedSessionRoot::new(path.clone(), RootOrigin::Current).unwrap();
        let repository = ConversationRepository::new(EventRepository::new(
            SessionDatabase::open(root.clone()).unwrap(),
        ));
        let projection =
            AgentHistoryFile::new(root, path.join("history.jsonl"), vec!["agent".into()]).unwrap();
        let history = repository
            .bind_agent_history("agent".into(), Arc::new(projection))
            .unwrap();
        Self { path, history }
    }
    fn publisher(&self) -> Publisher {
        Publisher {
            events: self.history.repository().events().clone(),
            broker: EventBroker::new(),
            retained: Mutex::new(Vec::new()),
            transient: Mutex::new(Vec::new()),
            fail: AtomicBool::new(false),
            changed: Notify::new(),
        }
    }
    async fn persisted(&self) -> Vec<Event> {
        self.history.repository().events().replay().await.unwrap()
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

/// Inspect the actual SQLite repository before every retained publication.
/// The real broker still receives these values; persistence is never faked.
struct Publisher {
    events: EventRepository,
    broker: EventBroker,
    retained: Mutex<Vec<Event>>,
    transient: Mutex<Vec<Event>>,
    fail: AtomicBool,
    changed: Notify,
}
impl Publisher {
    fn retained(&self) -> Vec<Event> {
        self.retained.lock().unwrap().clone()
    }
    fn transient(&self) -> Vec<Event> {
        self.transient.lock().unwrap().clone()
    }
    async fn wait_for_retained(&self, count: usize) {
        loop {
            let changed = self.changed.notified();
            tokio::pin!(changed);
            changed.as_mut().enable();
            if self.retained.lock().unwrap().len() >= count {
                return;
            }
            changed.await;
        }
    }
}
impl EventPublisher for Publisher {
    type Event = Event;
    type Error = BrokerError;
    fn publish_committed<'a>(
        &'a self,
        event: &'a Committed<Event>,
        cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<(), OperationError<BrokerError>>> {
        Box::pin(async move {
            let persisted = self.events.replay().await.unwrap();
            assert_eq!(persisted.last(), Some(event.value()));
            if self.fail.load(Ordering::SeqCst) {
                return Err(OperationError::Failed(BrokerError::InvalidCapacity));
            }
            self.broker.publish_committed(event, cancellation).await?;
            self.retained.lock().unwrap().push(event.value().clone());
            self.changed.notify_waiters();
            Ok(())
        })
    }
    fn publish_transient<'a>(
        &'a self,
        event: Event,
        cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<(), OperationError<BrokerError>>> {
        Box::pin(async move {
            self.broker
                .publish_transient(event.clone(), cancellation)
                .await?;
            self.transient.lock().unwrap().push(event);
            Ok(())
        })
    }
}

#[derive(Default)]
struct Output {
    text: Mutex<String>,
    fail: AtomicBool,
}
impl Output {
    fn text(&self) -> String {
        self.text.lock().unwrap().clone()
    }
}
impl OutputWriter for Output {
    fn append<'a>(&'a self, text: &'a str) -> BoxFuture<'a, io::Result<()>> {
        Box::pin(async move {
            if self.fail.load(Ordering::SeqCst) {
                return Err(io::Error::other("output failed"));
            }
            self.text.lock().unwrap().push_str(text);
            Ok(())
        })
    }
}
#[derive(Default)]
struct Activity {
    begun: AtomicUsize,
    observed: Mutex<Vec<sessions::Event>>,
    finished: AtomicUsize,
}
impl ProviderActivity for Activity {
    fn begin_provider_request(&self) {
        self.begun.fetch_add(1, Ordering::SeqCst);
    }
    fn observe_provider_event(&self, event: &sessions::Event) {
        self.observed.lock().unwrap().push(event.clone());
    }
    fn finish_provider_request(&self) {
        self.finished.fetch_add(1, Ordering::SeqCst);
    }
}
#[derive(Default)]
struct Usage {
    observed: Vec<(Request, Completion)>,
    fail: bool,
}
impl ProviderUsage for Usage {
    fn observe_completion<'a>(
        &'a mut self,
        request: &'a Request,
        completed: &'a Completion,
    ) -> BoxFuture<'a, Result<(), EventError>> {
        Box::pin(async move {
            if self.fail {
                return Err(EventError::Invalid("usage failed".into()));
            }
            self.observed.push((request.clone(), completed.clone()));
            Ok(())
        })
    }
}

#[derive(Default)]
struct Token {
    cancelled: AtomicBool,
    changed: Notify,
}
impl Token {
    fn cancel(&self) {
        self.cancelled.store(true, Ordering::SeqCst);
        self.changed.notify_waiters();
    }
}
impl Cancellation for Token {
    fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::SeqCst)
    }
    fn cancelled(&self) -> BoxFuture<'_, ()> {
        Box::pin(async move {
            loop {
                let changed = self.changed.notified();
                tokio::pin!(changed);
                changed.as_mut().enable();
                if self.is_cancelled() {
                    return;
                }
                changed.await;
            }
        })
    }
}

enum Step {
    Event(sessions::Event),
    Fail(Error),
    Stall,
}
fn text(value: &str) -> Step {
    Step::Event(sessions::Event::TextDelta(value.into()))
}
fn reasoning(value: &str, kind: sessions::ReasoningKind, part_id: &str, completed: bool) -> Step {
    Step::Event(sessions::Event::ReasoningDelta {
        text: value.into(),
        kind,
        part_id: part_id.into(),
        completed,
    })
}
struct StreamState {
    stalled: Semaphore,
    resume: Semaphore,
    closing: Semaphore,
    close_release: Semaphore,
    gate_close: bool,
    closed: AtomicBool,
    close_count: AtomicUsize,
}
impl StreamState {
    fn new(gate_close: bool) -> Arc<Self> {
        Arc::new(Self {
            stalled: Semaphore::new(0),
            resume: Semaphore::new(0),
            closing: Semaphore::new(0),
            close_release: Semaphore::new(0),
            gate_close,
            closed: AtomicBool::new(false),
            close_count: AtomicUsize::new(0),
        })
    }
    async fn wait_for_stall(&self) {
        self.stalled.acquire().await.unwrap().forget();
    }
    async fn wait_for_close(&self) {
        self.closing.acquire().await.unwrap().forget();
    }
    fn assert_closed(&self) {
        assert!(self.closed.load(Ordering::SeqCst));
        assert_eq!(self.close_count.load(Ordering::SeqCst), 1);
    }
}
struct FakeStream {
    steps: VecDeque<Step>,
    state: Arc<StreamState>,
    close_error: Option<Error>,
}
impl EventStream for FakeStream {
    fn next<'a>(
        &'a mut self,
        _: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<Option<sessions::Event>, Error>> {
        Box::pin(async move {
            match self.steps.pop_front() {
                Some(Step::Event(event)) => Ok(Some(event)),
                Some(Step::Fail(error)) => Err(error),
                Some(Step::Stall) => {
                    self.state.stalled.add_permits(1);
                    self.state.resume.acquire().await.unwrap().forget();
                    Ok(None)
                }
                None => Ok(None),
            }
        })
    }
    fn close(&mut self) -> BoxFuture<'_, Result<(), Error>> {
        Box::pin(async move {
            self.state.close_count.fetch_add(1, Ordering::SeqCst);
            self.state.closing.add_permits(1);
            if self.state.gate_close {
                self.state.close_release.acquire().await.unwrap().forget();
            }
            self.state.closed.store(true, Ordering::SeqCst);
            self.close_error.take().map_or(Ok(()), Err)
        })
    }
}
struct FakeSession {
    stream: Mutex<Option<FakeStream>>,
    requests: Mutex<Vec<Request>>,
}
impl FakeSession {
    fn new(steps: Vec<Step>, state: Arc<StreamState>) -> Self {
        Self {
            stream: Mutex::new(Some(FakeStream {
                steps: steps.into(),
                state,
                close_error: None,
            })),
            requests: Mutex::new(Vec::new()),
        }
    }
}
impl Session for FakeSession {
    fn begin_turn(&self) -> BoxFuture<'_, Result<(), Error>> {
        panic!("the caller owns begin_turn")
    }
    fn call(&self, request: Request) -> Result<Box<dyn EventStream>, Error> {
        self.requests.lock().unwrap().push(request);
        Ok(Box::new(
            self.stream
                .lock()
                .unwrap()
                .take()
                .expect("one provider call"),
        ))
    }
    fn try_fall_back_to_http(&self) -> BoxFuture<'_, Result<bool, Error>> {
        panic!("not a stream responsibility")
    }
    fn close(&self) -> BoxFuture<'_, Result<(), Error>> {
        panic!("the caller owns session close")
    }
}
fn ids() -> impl Fn() -> String + Send + Sync {
    let counter = AtomicU64::new(0);
    move || format!("event-{}", counter.fetch_add(1, Ordering::SeqCst))
}
fn request() -> Request {
    Request {
        model: "model".into(),
        instructions: "real request".into(),
        max_output_tokens: 321,
        messages: vec![sessions::Message::user("question")],
        reasoning: Some(sessions::Reasoning {
            effort: "high".into(),
            summary: "auto".into(),
        }),
        ..Default::default()
    }
}
fn phases(publisher: &Publisher) -> Vec<(protocol::ProviderRequestPhase, u32)> {
    publisher
        .transient()
        .into_iter()
        .filter(|event| event.has_provider_request_phase_changed())
        .map(|event| {
            (
                event
                    .provider_request_phase_changed()
                    .phase
                    .enum_value()
                    .unwrap(),
                event.provider_request_phase_changed().attempt,
            )
        })
        .collect()
}

#[tokio::test]
async fn batches_fragments_by_source_stream_and_returns_the_whole_last_completion() {
    let fixture = Fixture::new();
    let publisher = fixture.publisher();
    let output = Output::default();
    let activity = Activity::default();
    let mut usage = Usage::default();
    let ids = ids();
    let state = StreamState::new(false);
    let completion = Completion {
        response_id: "last".into(),
        finish_reason: "tool_calls".into(),
        input_tokens: -2,
        cached_input_tokens: 9,
        output_tokens: 12,
        assistant_text: "authoritative".into(),
        tool_calls: vec![ToolCall {
            id: "call".into(),
            name: "read".into(),
            arguments_json: "{\"path\":\"x\"}".into(),
        }],
        provider_id: "provider".into(),
        provider_content: "opaque provider replay".into(),
    };
    let last = sessions::Event::Completed(completion.clone()).with_text("completion envelope");
    let session = FakeSession::new(
        vec![
            Step::Event(sessions::Event::HttpRequestStarted),
            Step::Event(sessions::Event::HttpResponseHeadersReceived),
            text("a"),
            text("b"),
            text("c"),
            reasoning("r1", sessions::ReasoningKind::Raw, "part", false),
            reasoning("r2", sessions::ReasoningKind::Raw, "part", false),
            reasoning("r3", sessions::ReasoningKind::Raw, "part", true),
            reasoning("s1", sessions::ReasoningKind::Summary, "part", false),
            reasoning("s2", sessions::ReasoningKind::Summary, "part", false),
            reasoning("s3", sessions::ReasoningKind::Summary, "other", true),
            Step::Event(sessions::Event::EncryptedReasoningReceived),
            Step::Event(
                sessions::Event::ToolCallDelta(ToolCall {
                    id: "call".into(),
                    name: "read".into(),
                    arguments_json: "{".into(),
                })
                .with_text("wire arguments"),
            ),
            Step::Event(sessions::Event::Completed(Completion {
                input_tokens: 1,
                ..Default::default()
            })),
            Step::Event(last.clone()),
        ],
        state.clone(),
    );
    let request = request();
    let actual = ProviderCall {
        history: &fixture.history,
        publisher: &publisher,
        event_id: &ids,
        output: &output,
        activity: &activity,
        usage: &mut usage,
    }
    .stream(&session, request.clone(), &NeverCancelled)
    .await
    .unwrap();
    assert_eq!(actual, last);
    assert_eq!(*session.requests.lock().unwrap(), vec![request.clone()]);
    assert_eq!(
        usage.observed,
        vec![
            (
                request.clone(),
                Completion {
                    input_tokens: 1,
                    ..Default::default()
                }
            ),
            (request, completion)
        ]
    );
    let retained = publisher.retained();
    assert_eq!(
        retained
            .iter()
            .filter(|event| event.has_text_chunk())
            .map(|event| event.text_chunk().fragment.as_str())
            .collect::<Vec<_>>(),
        ["a", "bc"]
    );
    assert_eq!(
        retained
            .iter()
            .filter(|event| event.has_reasoning_chunk())
            .map(|event| (
                event.reasoning_chunk().fragment.as_str(),
                event.reasoning_chunk().completed,
                event.reasoning_chunk().part_id.as_str()
            ))
            .collect::<Vec<_>>(),
        [
            ("r1", false, "part"),
            ("r2r3", true, "part"),
            ("s1", false, "part"),
            ("s2", false, "part"),
            ("s3", true, "other")
        ]
    );
    assert_eq!(
        retained
            .last()
            .unwrap()
            .tool_call_chunk()
            .arguments_fragment,
        "wire arguments"
    );
    assert_eq!(fixture.persisted().await, retained);
    assert_eq!(output.text(), "abc\n");
    assert_eq!(activity.begun.load(Ordering::SeqCst), 1);
    assert_eq!(activity.finished.load(Ordering::SeqCst), 1);
    assert_eq!(activity.observed.lock().unwrap().len(), 11);
    let usage_events: Vec<_> = publisher
        .transient()
        .into_iter()
        .filter(|event| event.has_provider_call_usage())
        .collect();
    assert_eq!(usage_events.len(), 2);
    assert_eq!(usage_events[1].provider_call_usage().input_tokens, 0);
    assert_eq!(usage_events[1].provider_call_usage().output_tokens, 12);
    assert_eq!(
        phases(&publisher),
        [
            (
                protocol::ProviderRequestPhase::PROVIDER_REQUEST_PHASE_REQUESTING,
                1
            ),
            (
                protocol::ProviderRequestPhase::PROVIDER_REQUEST_PHASE_HEADERS_RECEIVED,
                1
            ),
            (
                protocol::ProviderRequestPhase::PROVIDER_REQUEST_PHASE_IDLE,
                1
            ),
            (
                protocol::ProviderRequestPhase::PROVIDER_REQUEST_PHASE_IDLE,
                1
            )
        ]
    );
    state.assert_closed();
}

#[tokio::test]
async fn stalled_next_flushes_held_suffix_without_restarting_or_detaching_the_read() {
    let fixture = Fixture::new();
    let publisher = fixture.publisher();
    let output = Output::default();
    let activity = Activity::default();
    let mut usage = Usage::default();
    let ids = ids();
    let state = StreamState::new(false);
    let session = FakeSession::new(
        vec![text("opening"), text(" held"), Step::Stall],
        state.clone(),
    );
    let mut call = ProviderCall {
        history: &fixture.history,
        publisher: &publisher,
        event_id: &ids,
        output: &output,
        activity: &activity,
        usage: &mut usage,
    };
    let future = call.stream(&session, request(), &NeverCancelled);
    fn assert_send<T: Send>(_: &T) {}
    assert_send(&future);
    let (result, ()) = tokio::join!(future, async {
        state.wait_for_stall().await;
        tokio::time::timeout(Duration::from_secs(3), publisher.wait_for_retained(2))
            .await
            .unwrap();
        assert_eq!(output.text(), "opening held");
        assert!(!state.closed.load(Ordering::SeqCst));
        state.resume.add_permits(1);
    });
    assert_eq!(
        result.unwrap(),
        sessions::Event::Completed(Completion::default())
    );
    assert_eq!(output.text(), "opening held\n");
    assert_eq!(fixture.persisted().await.len(), 2);
    state.assert_closed();
}

#[tokio::test]
async fn cancellation_keeps_held_fragments_and_awaits_actual_stream_close() {
    let fixture = Fixture::new();
    let publisher = fixture.publisher();
    let output = Output::default();
    let activity = Activity::default();
    let mut usage = Usage::default();
    let ids = ids();
    let state = StreamState::new(true);
    let session = FakeSession::new(
        vec![text("opening"), text(" held"), Step::Stall],
        state.clone(),
    );
    let token = Token::default();
    let returned = AtomicBool::new(false);
    let mut call = ProviderCall {
        history: &fixture.history,
        publisher: &publisher,
        event_id: &ids,
        output: &output,
        activity: &activity,
        usage: &mut usage,
    };
    let (result, ()) = tokio::join!(
        async {
            let result = call.stream(&session, request(), &token).await;
            returned.store(true, Ordering::SeqCst);
            result
        },
        async {
            state.wait_for_stall().await;
            token.cancel();
            state.wait_for_close().await;
            assert_eq!(publisher.retained().len(), 2);
            assert_eq!(output.text(), "opening held");
            assert!(!returned.load(Ordering::SeqCst));
            assert_eq!(activity.finished.load(Ordering::SeqCst), 0);
            state.close_release.add_permits(1);
        }
    );
    assert!(matches!(
        result,
        Err(ProviderCallError::Provider(Error::Cancelled))
    ));
    assert_eq!(output.text(), "opening held\n");
    assert_eq!(fixture.persisted().await, publisher.retained());
    assert_eq!(activity.finished.load(Ordering::SeqCst), 1);
    state.assert_closed();
}

#[tokio::test]
async fn stream_failure_keeps_suffix_and_original_provider_body_until_close_finishes() {
    let fixture = Fixture::new();
    let publisher = fixture.publisher();
    let output = Output::default();
    let activity = Activity::default();
    let mut usage = Usage::default();
    let ids = ids();
    let state = StreamState::new(true);
    let error = Error::Response(
        parrot_provider::responses::ResponsesError::ProviderResponse {
            error_type: "failure".into(),
            error_code: "code".into(),
            detail: "detail".into(),
            response_body: "raw body remains on actual provider error".into(),
        },
    );
    let session = FakeSession::new(
        vec![text("a"), text("b"), Step::Fail(error.clone())],
        state.clone(),
    );
    let mut call = ProviderCall {
        history: &fixture.history,
        publisher: &publisher,
        event_id: &ids,
        output: &output,
        activity: &activity,
        usage: &mut usage,
    };
    let (result, ()) = tokio::join!(call.stream(&session, request(), &NeverCancelled), async {
        state.wait_for_close().await;
        assert_eq!(publisher.retained().len(), 2);
        assert_eq!(output.text(), "ab");
        state.close_release.add_permits(1);
    });
    let Err(ProviderCallError::Provider(actual)) = result else {
        panic!("provider failure expected");
    };
    assert_eq!(actual, error);
    assert_eq!(
        actual.response_body(),
        "raw body remains on actual provider error"
    );
    assert_eq!(fixture.persisted().await, publisher.retained());
    assert_eq!(output.text(), "ab\n");
    state.assert_closed();
}

#[tokio::test]
async fn retry_flushes_output_marks_attempt_and_empty_data_does_not_end_header_phase() {
    let fixture = Fixture::new();
    let publisher = fixture.publisher();
    let output = Output::default();
    let activity = Activity::default();
    let mut usage = Usage::default();
    let ids = ids();
    let state = StreamState::new(false);
    let session = FakeSession::new(
        vec![
            Step::Event(sessions::Event::HttpRequestStarted),
            Step::Event(sessions::Event::HttpResponseHeadersReceived),
            text(""),
            Step::Event(sessions::Event::ToolCallDelta(ToolCall::default())),
            text("a"),
            text("b"),
            Step::Event(sessions::Event::Retry {
                attempt: 7,
                retry_after: Duration::from_millis(1234),
                reason: "try again".into(),
            }),
            Step::Event(sessions::Event::HttpRequestStarted),
            Step::Event(sessions::Event::HttpResponseHeadersReceived),
            Step::Event(sessions::Event::EncryptedReasoningReceived),
            text("c"),
        ],
        state.clone(),
    );
    let result = ProviderCall {
        history: &fixture.history,
        publisher: &publisher,
        event_id: &ids,
        output: &output,
        activity: &activity,
        usage: &mut usage,
    }
    .stream(&session, request(), &NeverCancelled)
    .await
    .unwrap();
    assert_eq!(result, sessions::Event::Completed(Completion::default()));
    assert!(usage.observed.is_empty());
    assert_eq!(output.text(), "ab\n[retry]\nc\n");
    let retained = publisher.retained();
    let retry = retained
        .iter()
        .find(|event| event.has_retry_notice())
        .unwrap()
        .retry_notice();
    assert_eq!(
        (retry.attempt, retry.retry_after_ms, retry.reason.as_str()),
        (7, 1234, "try again")
    );
    assert_eq!(fixture.persisted().await, retained);
    assert_eq!(
        phases(&publisher),
        [
            (
                protocol::ProviderRequestPhase::PROVIDER_REQUEST_PHASE_REQUESTING,
                1
            ),
            (
                protocol::ProviderRequestPhase::PROVIDER_REQUEST_PHASE_HEADERS_RECEIVED,
                1
            ),
            (
                protocol::ProviderRequestPhase::PROVIDER_REQUEST_PHASE_IDLE,
                1
            ),
            (
                protocol::ProviderRequestPhase::PROVIDER_REQUEST_PHASE_IDLE,
                1
            ),
            (
                protocol::ProviderRequestPhase::PROVIDER_REQUEST_PHASE_REQUESTING,
                2
            ),
            (
                protocol::ProviderRequestPhase::PROVIDER_REQUEST_PHASE_HEADERS_RECEIVED,
                2
            ),
            (
                protocol::ProviderRequestPhase::PROVIDER_REQUEST_PHASE_IDLE,
                2
            ),
            (
                protocol::ProviderRequestPhase::PROVIDER_REQUEST_PHASE_IDLE,
                2
            )
        ]
    );
    state.assert_closed();
}

#[tokio::test]
async fn retry_after_text_without_more_text_does_not_add_an_extra_final_newline() {
    let fixture = Fixture::new();
    let publisher = fixture.publisher();
    let output = Output::default();
    let activity = Activity::default();
    let mut usage = Usage::default();
    let ids = ids();
    let state = StreamState::new(false);
    let session = FakeSession::new(
        vec![
            text("a"),
            Step::Event(sessions::Event::Retry {
                attempt: 1,
                retry_after: Duration::ZERO,
                reason: "retry".into(),
            }),
        ],
        state.clone(),
    );
    ProviderCall {
        history: &fixture.history,
        publisher: &publisher,
        event_id: &ids,
        output: &output,
        activity: &activity,
        usage: &mut usage,
    }
    .stream(&session, request(), &NeverCancelled)
    .await
    .unwrap();
    assert_eq!(output.text(), "a\n[retry]\n");
    state.assert_closed();
}

#[tokio::test]
async fn storage_publication_output_and_usage_failures_all_close_the_real_stream() {
    for boundary in ["storage", "publication", "output", "usage"] {
        let fixture = Fixture::new();
        let publisher = fixture.publisher();
        let output = Output::default();
        let activity = Activity::default();
        let mut usage = Usage {
            fail: boundary == "usage",
            ..Default::default()
        };
        let ids = || "duplicate".into();
        let state = StreamState::new(false);
        if boundary == "publication" {
            publisher.fail.store(true, Ordering::SeqCst);
        }
        if boundary == "output" {
            output.fail.store(true, Ordering::SeqCst);
        }
        let steps = if boundary == "usage" {
            vec![Step::Event(sessions::Event::Completed(
                Completion::default(),
            ))]
        } else {
            vec![
                text("a"),
                reasoning("r", sessions::ReasoningKind::Raw, "part", false),
            ]
        };
        let session = FakeSession::new(steps, state.clone());
        let result = ProviderCall {
            history: &fixture.history,
            publisher: &publisher,
            event_id: &ids,
            output: &output,
            activity: &activity,
            usage: &mut usage,
        }
        .stream(&session, request(), &NeverCancelled)
        .await;
        match boundary {
            "storage" | "usage" => assert!(matches!(result, Err(ProviderCallError::Storage(_)))),
            "publication" => assert!(matches!(result, Err(ProviderCallError::Publication(_)))),
            "output" => assert!(matches!(result, Err(ProviderCallError::Output(_)))),
            _ => unreachable!(),
        }
        let persisted = fixture.persisted().await;
        assert_eq!(
            persisted.len(),
            usize::from(matches!(boundary, "storage" | "publication"))
        );
        assert_eq!(
            publisher.retained().len(),
            usize::from(boundary == "storage")
        );
        assert_eq!(activity.finished.load(Ordering::SeqCst), 1);
        state.assert_closed();
    }
}

#[tokio::test]
async fn close_failure_is_awaited_and_does_not_skip_idle_activity_or_newline() {
    let fixture = Fixture::new();
    let publisher = fixture.publisher();
    let output = Output::default();
    let activity = Activity::default();
    let mut usage = Usage::default();
    let ids = ids();
    let state = StreamState::new(false);
    let session = FakeSession::new(vec![text("a")], state.clone());
    session.stream.lock().unwrap().as_mut().unwrap().close_error =
        Some(Error::Io("close failed".into()));
    let result = ProviderCall {
        history: &fixture.history,
        publisher: &publisher,
        event_id: &ids,
        output: &output,
        activity: &activity,
        usage: &mut usage,
    }
    .stream(&session, request(), &NeverCancelled)
    .await;
    assert!(
        matches!(result, Err(ProviderCallError::Provider(Error::Io(message))) if message == "close failed")
    );
    assert_eq!(output.text(), "a\n");
    assert_eq!(activity.finished.load(Ordering::SeqCst), 1);
    assert_eq!(
        phases(&publisher),
        [(
            protocol::ProviderRequestPhase::PROVIDER_REQUEST_PHASE_IDLE,
            0
        )]
    );
    state.assert_closed();
}

#[tokio::test]
async fn compaction_retry_helper_commits_before_publication_without_stream_dependencies() {
    use parrot_runtime::agent_turn::provider_call::emit_retry;

    let fixture = Fixture::new();
    let publisher = fixture.publisher();
    let ids = ids();
    let event = sessions::Event::Retry {
        attempt: 3,
        retry_after: Duration::from_millis(275),
        reason: "original reason".into(),
    }
    .with_text("actual retry text");
    emit_retry(&fixture.history, &publisher, &ids, &event, &NeverCancelled)
        .await
        .unwrap();
    let retained = publisher.retained();
    assert_eq!(fixture.persisted().await, retained);
    let retry = retained[0].retry_notice();
    assert_eq!(
        (retry.attempt, retry.retry_after_ms, retry.reason.as_str()),
        (3, 275, "actual retry text")
    );
    assert!(publisher.transient().is_empty());

    let token = Token::default();
    token.cancel();
    let result = emit_retry(&fixture.history, &publisher, &ids, &event, &token).await;
    assert!(matches!(
        result,
        Err(ProviderCallError::Publication(OperationError::Cancelled))
    ));
    assert_eq!(fixture.persisted().await.len(), 2);
    assert_eq!(publisher.retained().len(), 1);
}
