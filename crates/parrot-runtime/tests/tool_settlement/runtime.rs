use super::*;
use parrot_runtime::{
    active_work::exit_reminders::ExitReminders,
    agent_turn::{
        activity::{LiveActivity, TurnDiagnostics},
        engine::{RuntimeBackend, RuntimeConfig},
        lifecycle::{AgentSession, Execution, ExecutionStatus, IncomingActivity},
        provider_call::OutputWriter,
    },
    prompt_context::SkillRendering,
};
use std::io;

struct CompactTool {
    metadata: ToolMetadata,
    calls: AtomicUsize,
}
impl CompactTool {
    fn new() -> Self {
        Self {
            metadata: ToolMetadata {
                name: "compact_context".into(),
                aliases: vec![],
            },
            calls: AtomicUsize::new(0),
        }
    }
}
impl BorrowedTool<Security> for CompactTool {
    fn definition(&self) -> &ToolMetadata {
        &self.metadata
    }
    fn enabled_after_interruption(&self) -> bool {
        false
    }
    fn invoke<'a>(
        &'a self,
        _: ToolInvocation,
        context: BorrowedToolContext<'a, Security>,
        cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<ToolOutput, OperationError<ToolFailure>>> {
        Box::pin(async move {
            self.calls.fetch_add(1, Ordering::SeqCst);
            let resources = context.resources;
            let result = resources
                .context
                .compact(
                    EpochInput {
                        selection: &context.selection.model,
                        tools: context.definitions,
                        conversation: resources.conversation,
                        prompt: resources.prompt,
                        sessions: resources.sessions,
                        status: resources.status,
                        events: resources.events,
                    },
                    None,
                    cancellation,
                )
                .await
                .map_err(|error| {
                    if matches!(
                        error,
                        ContextError::Cancelled
                            | ContextError::Compaction(
                                CompactionError::Cancelled
                                    | CompactionError::Provider(Error::Cancelled)
                            )
                    ) {
                        OperationError::Cancelled
                    } else {
                        OperationError::Failed(ToolFailure::new(
                            error.to_string(),
                            FailureKind::InvalidOperation,
                        ))
                    }
                })?;
            assert!(result.reduced);
            Ok(ToolOutput::text("compacted inline"))
        })
    }
}
struct Skills;
impl TurnSkills for Skills {
    fn begin_turn<'a>(&'a self, _: &'a dyn Cancellation) -> BoxFuture<'a, PromptResult<()>> {
        Box::pin(async { Ok(()) })
    }
    fn select<'a>(
        &'a self,
        _: &'a [ConversationPart],
        _: &'a dyn Cancellation,
    ) -> BoxFuture<'a, PromptResult<()>> {
        Box::pin(async { Ok(()) })
    }
    fn render<'a>(
        &'a self,
        _: &'a dyn Cancellation,
    ) -> BoxFuture<'a, PromptResult<SkillRendering>> {
        Box::pin(async {
            Ok(SkillRendering {
                content: String::new(),
                loaded_paths: vec![],
            })
        })
    }
    fn end_turn(&self) -> BoxFuture<'_, ()> {
        Box::pin(async {})
    }
}
struct Output;
impl OutputWriter for Output {
    fn append<'a>(&'a self, _: &'a str) -> BoxFuture<'a, io::Result<()>> {
        Box::pin(async { Ok(()) })
    }
}
impl SessionOutput for Output {
    fn close(&self) -> BoxFuture<'_, io::Result<()>> {
        Box::pin(async { Ok(()) })
    }
}
struct Parent;
impl ParentCompletion for Parent {
    fn deliver_correlated<'a>(
        &'a self,
        execution: &'a parrot_runtime::agent_turn::lifecycle::Execution,
        _: &'a str,
    ) -> parrot_domain::contracts::async_ops::BoxFuture<
        'a,
        Result<(), parrot_runtime::agent_turn::services::TurnError>,
    > {
        self.deliver(execution)
    }

    fn deliver<'a>(&'a self, _: &'a Execution) -> BoxFuture<'a, Result<(), TurnError>> {
        Box::pin(async { Ok(()) })
    }
}
struct TurnLog;
impl TurnDiagnostics for TurnLog {
    fn started(&self, _: &str) {}
    fn finished(&self, _: &str, _: &str, _: Option<&str>) {}
}
pub(super) async fn runtime(
    fixture: &Fixture,
    provider: &Scripted,
    handles: Vec<ToolHandle<Security>>,
) -> (
    AgentSession<RuntimeBackend<Security, ToolSettlement<Security>>>,
    Arc<Factory>,
) {
    let broker = Arc::new(EventBroker::new());
    let publisher: Arc<parrot_runtime::agent_turn::engine::Publisher> =
        Arc::new(Publisher::new(fixture.history.clone()));
    let templates = templates();
    let reminders = Arc::new(
        ExitReminders::new(
            fixture.history.clone(),
            publisher.clone(),
            templates.clone(),
        )
        .await
        .unwrap(),
    );
    let factory = Arc::new(Factory {
        tools: handles,
        creates: AtomicUsize::new(0),
    });
    let tracker = Arc::new(AgentSessionActivity::new(Arc::new(TestClock)));
    let tools = Arc::new(
        ToolSettlement::new(ToolSettlementConfig {
            factory: factory.clone(),
            templates: templates.clone(),
            output_blobs: Arc::new(
                ToolOutputBlobStore::for_session(
                    fixture.repository.events().database().selected_root(),
                    "scratch",
                    Arc::new(Names),
                )
                .unwrap(),
            ),
            publisher: publisher.clone(),
            activity: tracker.clone(),
            diagnostics: Arc::new(Diagnostics::default()),
            image_bytes_per_cycle: 1024 * 1024,
        })
        .unwrap(),
    );
    let backend = RuntimeBackend::new(RuntimeConfig {
        agent_limits: parrot_foundation::config::runtime::AgentLimitsConfig {
            disable_max_turns: false,
            ..Default::default()
        },
        identity: SessionIdentity {
            session_id: "agent".into(),
            name: "settlement composition".into(),
            parent_session_id: String::new(),
            depth: 1,
        },
        history: fixture.history.clone(),
        broker,
        publisher,
        selection: Arc::new(Selection(Mutex::new(selection(provider)))),
        tools,
        prompt: Box::new(Prompt),
        skills: Arc::new(Skills),
        context: fixture.controller(),
        status: Arc::new(Status),
        completions: vec![],
        templates,
        reminders,
        output: Arc::new(Output),
        activity: Arc::new(LiveActivity::new(tracker, Arc::new(TurnLog))),
        parent_completion: Arc::new(Parent),
        ancestors: vec![],
        event_id: Arc::new(id),
    })
    .await
    .unwrap();
    (AgentSession::new(backend, Arc::new(Signal::new())), factory)
}
pub(super) async fn send(
    session: &AgentSession<RuntimeBackend<Security, ToolSettlement<Security>>>,
) -> parrot_runtime::agent_turn::lifecycle::SendResult {
    bounded(session.send(
        vec![ConversationPart::text("current question")],
        id(),
        protocol::Delivery::DELIVERY_STEER,
        IncomingActivity {
            name: "test".into(),
            cause: None,
            source: protocol::InputSource::default(),
        },
        Arc::new(NeverCancelled),
    ))
    .await
    .unwrap()
}
fn compact_step() -> Step {
    let mut step = Step::complete("invoke actual compact");
    step.completion.as_mut().unwrap().tool_calls = vec![call("inline-compact", "compact_context")];
    step
}

#[tokio::test]
async fn actual_runtime_retains_tool_factory_and_inline_actual_t45_state_reaches_next_provider() {
    let fixture = Fixture::new();
    fixture.seed().await;
    let compact = Arc::new(CompactTool::new());
    let provider = Scripted::new(vec![
        compact_step(),
        Step::complete("inline durable summary"),
        Step::complete("answer using inline summary"),
        Step::complete("next turn answer"),
    ]);
    let (session, factory) = runtime(
        &fixture,
        &provider,
        vec![ToolHandle::Borrowed(compact.clone())],
    )
    .await;
    let sent = send(&session).await;
    assert_eq!(
        bounded(sent.execution.join()).await.unwrap().status,
        ExecutionStatus::Succeeded
    );
    let next = send(&session).await;
    assert_eq!(
        bounded(next.execution.join()).await.unwrap().status,
        ExecutionStatus::Succeeded
    );
    assert_eq!(compact.calls.load(Ordering::SeqCst), 1);
    assert_eq!(factory.creates.load(Ordering::SeqCst), 1);
    let requests = provider.requests.lock().unwrap().clone();
    assert_eq!(requests.len(), 4);
    assert!(
        requests[2]
            .messages
            .iter()
            .any(|message| message.content().contains("inline durable summary"))
    );
    assert!(
        !requests[2]
            .messages
            .iter()
            .any(|message| message.content().starts_with("seed-0-"))
    );
    let events = fixture.events().await;
    assert_eq!(
        events
            .iter()
            .filter(|event| event.has_compaction_finished())
            .count(),
        1
    );
    assert_eq!(
        events
            .iter()
            .filter(|event| event.has_turn_started())
            .count(),
        2
    );
    assert_eq!(
        events.iter().filter(|event| event.has_turn_ended()).count(),
        2
    );
    assert_eq!(
        events
            .iter()
            .filter(|event| event.has_tool_finished())
            .count(),
        1
    );
    assert_eq!(
        events
            .iter()
            .filter(|event| event.has_tool_execution_started())
            .count(),
        1
    );
    assert!(
        fixture
            .history
            .compaction_history()
            .await
            .unwrap()
            .is_some()
    );
    assert_eq!(
        fixture
            .jsonl()
            .lines()
            .map(|line| serde_json::from_str::<serde_json::Value>(line).unwrap())
            .filter(|entry| entry["type"] == "compaction")
            .count(),
        1
    );
    bounded(session.close()).await.unwrap();
}

#[tokio::test]
async fn actual_runtime_interrupt_joins_inline_actual_t45_without_self_queue_or_second_drain() {
    let fixture = Fixture::new();
    fixture.seed().await;
    let compact = Arc::new(CompactTool::new());
    let gate = Gate::new();
    let mut summary = Step::complete("not committed");
    summary.gate = Some(gate.clone());
    let provider = Scripted::new(vec![compact_step(), summary]);
    let (session, _) = runtime(&fixture, &provider, vec![ToolHandle::Borrowed(compact)]).await;
    let sent = send(&session).await;
    gate.entered().await;
    bounded(session.interrupt(&NeverCancelled)).await.unwrap();
    assert_eq!(
        bounded(sent.execution.join()).await.unwrap().status,
        ExecutionStatus::Canceled
    );
    assert!(
        fixture
            .history
            .compaction_history()
            .await
            .unwrap()
            .is_none()
    );
    assert_eq!(provider.requests.lock().unwrap().len(), 2);
    assert_eq!(provider.closes.load(Ordering::SeqCst), 2);
    let events = fixture.events().await;
    assert_eq!(
        events
            .iter()
            .filter(|event| event.has_turn_started())
            .count(),
        1
    );
    assert_eq!(
        events.iter().filter(|event| event.has_turn_ended()).count(),
        1
    );
    assert_eq!(
        events
            .iter()
            .filter(|event| event.has_tool_cancelled())
            .count(),
        1
    );
    bounded(session.close()).await.unwrap();
}

#[path = "live_observation.rs"]
mod live_observation;
