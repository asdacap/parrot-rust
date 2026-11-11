use super::*;
use parrot_runtime::{
    active_work::exit_reminders::ExitReminders,
    agent_turn::{
        activity::{LiveActivity, TurnDiagnostics},
        engine::{RuntimeBackend, RuntimeConfig},
        lifecycle::{AgentSession, Execution, IncomingActivity},
        provider_call::OutputWriter,
    },
};
use std::io;

/// Opens a file-backed recovery store under `directory`, initializing it when absent.
pub(super) async fn test_agent_state_store(
    directory: &std::path::Path,
    owner: &str,
) -> std::sync::Arc<dyn parrot_domain::contracts::agent_state::AgentStateStore> {
    use parrot_domain::contracts::agent_state::AgentStateStore;
    use parrot_domain::values::agent_state::{
        AgentDescriptor, AgentStateSnapshot, CompletionDeliverySnapshot, SelectionSnapshot,
    };
    use parrot_foundation::paths::AgentScratchDirectory;
    std::fs::create_dir_all(directory).unwrap();
    AgentScratchDirectory::new(directory, directory).unwrap();
    let store = std::sync::Arc::new(
        parrot_storage::agent_state::FileAgentStateStore::open(directory, owner.into())
            .await
            .unwrap(),
    );
    if store.load().await.unwrap().is_none() {
        store
            .initialize(AgentStateSnapshot::new(
                owner.into(),
                AgentDescriptor {
                    parent_session_id: None,
                    name: "test".into(),
                    selection: SelectionSnapshot {
                        profile_id: "mode".into(),
                        model_selector: "scripted/model".into(),
                    },
                    local_scope: String::new(),
                    completion_delivery: CompletionDeliverySnapshot::Automatic,
                },
            ))
            .await
            .unwrap();
    }
    store
}

struct Skills;
impl TurnSkills for Skills {
    fn begin_turn<'a>(&'a self, _: &'a dyn Cancellation) -> BoxFuture<'a, PromptResult<()>> {
        Box::pin(async { Ok(()) })
    }
    fn get_skills<'a>(
        &'a self,
        references: &'a [parrot_core::SkillReference],
        _: &'a dyn Cancellation,
    ) -> BoxFuture<'a, PromptResult<Vec<Option<parrot_core::Skill>>>> {
        Box::pin(async move { Ok(references.iter().map(|_| None).collect()) })
    }
    fn read_skill_content<'a>(
        &'a self,
        _: &'a parrot_core::Skill,
        _: &'a dyn Cancellation,
    ) -> BoxFuture<'a, PromptResult<parrot_core::SkillContent>> {
        Box::pin(async { panic!("empty skill store has no content") })
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
        elapsed_ms: Option<i64>,
        _: &'a str,
    ) -> parrot_domain::contracts::async_ops::BoxFuture<
        'a,
        Result<(), parrot_runtime::agent_turn::services::TurnError>,
    > {
        self.deliver(execution, elapsed_ms)
    }

    fn deliver<'a>(
        &'a self,
        _: &'a Execution,
        _: Option<i64>,
    ) -> BoxFuture<'a, Result<(), TurnError>> {
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
) -> (AgentSession<RuntimeBackend<Security>>, Arc<Factory>) {
    runtime_with_publisher(
        fixture,
        provider,
        handles,
        Arc::new(Publisher::new(fixture.history.clone())),
    )
    .await
}
pub(super) async fn runtime_with_publisher(
    fixture: &Fixture,
    provider: &Scripted,
    handles: Vec<ToolHandle<Security>>,
    publisher: Arc<Publisher>,
) -> (AgentSession<RuntimeBackend<Security>>, Arc<Factory>) {
    let broker = Arc::new(EventBroker::new());
    let publisher: Arc<parrot_runtime::agent_turn::engine::Publisher> = publisher;
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
    let tool_execution = ToolExecutionConfig {
        path_variables: Default::default(),

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
    };
    let tools = factory.clone();
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
        tool_execution,
        prompt: Box::new(Prompt),
        skills: Arc::new(Skills),
        context: fixture.controller(),
        status: Arc::new(Status),
        completion_sources: vec![],
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
    let store = test_agent_state_store(&fixture.path.join("agent-state"), "agent").await;
    (
        AgentSession::with_state(backend, Arc::new(Signal::new()), store),
        factory,
    )
}
pub(super) async fn send(
    session: &AgentSession<RuntimeBackend<Security>>,
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
