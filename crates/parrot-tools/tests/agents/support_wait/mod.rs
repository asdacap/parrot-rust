//! Test-only composition: real SQLite/JSONL, RuntimeBackend, AgentSession,
//! ToolSettlement and WaitService. This is not the production P08 adapter.
use std::{
    collections::{BTreeMap, VecDeque},
    future::Future,
    path::PathBuf,
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
    },
    time::{Duration, Instant, SystemTime},
};

use parrot_domain::{
    contracts::{
        async_ops::{BoxFuture, Cancellation, Cancelled, Clock, OperationError},
        filenames::FilenameGenerator,
        paths::{RootOrigin, SelectedSessionRoot},
        runtime::AgentRuntime,
        security::InvocationContext,
        tools::{ParallelSafety, Tool},
    },
    protocol::messages::parrot::{self as protocol, Event},
    values::{
        activity::AgentSessionActivity, config::ModelProfiles, tools::ConfiguredToolDefinition,
    },
};
use parrot_foundation::template_catalog::PromptTemplateCatalog;
use parrot_provider::{
    catalogs::Model,
    routing::{
        aliases::{ModelAliasCatalog, ModelRouter, ModelRouting, ResolvedModelSelection},
        registry::ProviderRegistry,
    },
    sessions::{self, Completion, Error, EventStream, Provider, Request, Session},
};
use parrot_runtime::{
    active_work::{
        blockers::{ActiveWorkBlocker, ActiveWorkReminder},
        exit_reminders::ExitReminders,
        wait::{WaitDuration, WaitService},
    },
    agent_turn::{
        engine::{RuntimeBackend, RuntimeConfig},
        lifecycle::{AgentSession, DrainState, Execution, IncomingActivity, Signal},
        provider_call::{OutputWriter, ProviderActivity},
        services::*,
    },
    compaction::{
        CompactionSettings, Compactor, ContextController, ContextResult, ContextSnapshot,
        ContextStatus,
    },
    event_broker::EventBroker,
    prompt_context::{PromptResult, SkillRendering, SystemPrompt},
    tool_settlement::{
        self, SharedSecurity, ToolDiagnostics, ToolFactory, ToolFailure, ToolHandle, ToolInventory,
        ToolMetadata, ToolOutput, ToolSettlement, ToolSettlementConfig,
    },
};
use parrot_storage::{
    artifacts::{compaction::CompactionGroupBlobStore, output_blobs::ToolOutputBlobStore},
    conversation::{AgentHistoryView, ConversationRepository},
    database::SessionDatabase,
    diagnostics::{DiagnosticEvent, FailureKind},
    events::{ConversationPart, EventRepository, NeverCancelled},
    history_files::AgentHistoryFile,
};
use parrot_tools::agents::{
    self, AgentSelection, AgentTool, AgentToolError, AgentToolKind, ModelSelector, NamedRequest,
    SendRequest, SendResult, SpawnRequest, SpawnResult, input::WaitInput, status::StatusSnapshot,
};
use tokio::sync::Semaphore;

use crate::Security;

static NEXT: AtomicU64 = AtomicU64::new(0);
fn id() -> String {
    format!("t58-wait-{}", NEXT.fetch_add(1, Ordering::SeqCst))
}
pub(super) async fn bounded<F: Future>(future: F) -> F::Output {
    tokio::time::timeout(Duration::from_secs(10), future)
        .await
        .expect("retained wait test did not settle")
}

pub(super) struct Gate {
    entered: Semaphore,
    release: Semaphore,
}
impl Gate {
    pub(super) fn new() -> Arc<Self> {
        Arc::new(Self {
            entered: Semaphore::new(0),
            release: Semaphore::new(0),
        })
    }
    async fn wait(&self, cancellation: &dyn Cancellation) -> Result<(), Error> {
        self.entered.add_permits(1);
        tokio::select! {
            permit = self.release.acquire() => { permit.unwrap().forget(); Ok(()) },
            _ = cancellation.cancelled() => Err(Error::Cancelled),
        }
    }
    pub(super) async fn entered(&self) {
        bounded(self.entered.acquire()).await.unwrap().forget();
    }
    pub(super) fn open(&self) {
        self.release.add_permits(1);
    }
}

type ScriptStep = (Completion, Arc<Gate>);

/// The only scripted edge is the provider protocol. The runtime owns requests,
/// promotion, tool execution/settlement, state transitions and admission dedup.
#[derive(Clone, Default)]
pub(super) struct Scripted {
    pub(super) requests: Arc<Mutex<Vec<Request>>>,
    steps: Arc<Mutex<VecDeque<ScriptStep>>>,
}
impl Scripted {
    pub(super) fn step(&self, completion: Completion) -> Arc<Gate> {
        let gate = Gate::new();
        self.steps
            .lock()
            .unwrap()
            .push_back((completion, gate.clone()));
        gate
    }
    fn selection(&self) -> ResolvedModelSelection {
        let mut model = Model::new("model", "scripted");
        model.context_window = 100000;
        let registry = Arc::new(
            ProviderRegistry::new(
                vec![Arc::new(self.clone())],
                BTreeMap::from([("scripted".into(), vec![model])]),
            )
            .unwrap(),
        );
        let aliases = Arc::new(ModelAliasCatalog::new(registry.clone(), vec![]).unwrap());
        ModelRouter::new(
            registry,
            Arc::new(ModelRouting::new(aliases, "scripted/model")),
        )
        .resolve("scripted/model")
        .unwrap()
    }
    fn call_impl(&self, request: Request) -> Result<Box<dyn EventStream>, Error> {
        self.requests.lock().unwrap().push(request);
        let (completion, gate) = self
            .steps
            .lock()
            .unwrap()
            .pop_front()
            .expect("unexpected provider request");
        Ok(Box::new(Stream {
            completion: Some(completion),
            gate,
        }))
    }
}
impl Provider for Scripted {
    fn id(&self) -> &str {
        "scripted"
    }
    fn call(&self, request: Request) -> Result<Box<dyn EventStream>, Error> {
        self.call_impl(request)
    }
    fn open_session(&self) -> Result<Arc<dyn Session>, Error> {
        Ok(Arc::new(self.clone()))
    }
    fn has_credential<'a>(&'a self, _: &'a dyn Cancellation) -> BoxFuture<'a, Result<bool, Error>> {
        panic!("offline fixture never requests credentials")
    }
    fn seed_models(&self) -> Vec<Model> {
        vec![]
    }
    fn list_models<'a>(
        &'a self,
        _: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<Vec<Model>, Error>> {
        panic!("offline fixture never lists remote models")
    }
}
impl Session for Scripted {
    fn begin_turn(&self) -> BoxFuture<'_, Result<(), Error>> {
        Box::pin(async { Ok(()) })
    }
    fn call(&self, request: Request) -> Result<Box<dyn EventStream>, Error> {
        self.call_impl(request)
    }
    fn try_fall_back_to_http(&self) -> BoxFuture<'_, Result<bool, Error>> {
        Box::pin(async { Ok(false) })
    }
    fn close(&self) -> BoxFuture<'_, Result<(), Error>> {
        Box::pin(async { Ok(()) })
    }
}
struct Stream {
    completion: Option<Completion>,
    gate: Arc<Gate>,
}
impl EventStream for Stream {
    fn next<'a>(
        &'a mut self,
        cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<Option<sessions::Event>, Error>> {
        Box::pin(async move {
            let Some(completion) = self.completion.take() else {
                return Ok(None);
            };
            self.gate.wait(cancellation).await?;
            Ok(Some(sessions::Event::Completed(completion)))
        })
    }
    fn close(&mut self) -> BoxFuture<'_, Result<(), Error>> {
        Box::pin(async { Ok(()) })
    }
}

fn templates() -> Arc<PromptTemplateCatalog> {
    let env = [
        ("HOME", "/tmp/t58/home"),
        ("XDG_CONFIG_HOME", "/tmp/t58/config"),
        ("XDG_STATE_HOME", "/tmp/t58/state"),
        ("XDG_DATA_HOME", "/tmp/t58/data"),
        ("WORKDIR", "/tmp/t58/work"),
    ]
    .into_iter()
    .map(|(key, value)| (key.into(), value.into()))
    .collect();
    Arc::new(PromptTemplateCatalog::from_content("", &env).unwrap().1)
}
struct Names;
impl FilenameGenerator for Names {
    fn next_filename(&self) -> String {
        format!("{}.dat", id())
    }
}
struct TestClock;
impl Clock for TestClock {
    fn now(&self) -> Instant {
        Instant::now()
    }
    fn utc_now(&self) -> SystemTime {
        SystemTime::now()
    }
    fn sleep_until<'a>(
        &'a self,
        _: Instant,
        _: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<(), Cancelled>> {
        panic!("activity clock never sleeps")
    }
}
struct Diagnostics;
impl ToolDiagnostics for Diagnostics {
    fn write(&self, _: &DiagnosticEvent) {}
}
struct Prompt;
impl SystemPrompt for Prompt {
    fn renew_epoch(&mut self) {}
    fn sample_epoch<'a>(&'a mut self, _: &'a dyn Cancellation) -> BoxFuture<'a, PromptResult<()>> {
        Box::pin(async { Ok(()) })
    }
    fn build(&mut self, _: &ResolvedModelSelection) -> PromptResult<String> {
        Ok("offline retained-wait fixture".into())
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
struct Status;
impl ContextStatus for Status {
    fn observe<'a>(
        &'a self,
        _: &'a ContextSnapshot,
        _: &'a dyn Cancellation,
    ) -> BoxFuture<'a, ContextResult<String>> {
        Box::pin(async { Ok(String::new()) })
    }
    fn observe_fitted<'a>(
        &'a self,
        estimate: &'a (dyn Fn(&str) -> ContextResult<ContextSnapshot> + Send + Sync),
        _: &'a dyn Cancellation,
    ) -> BoxFuture<'a, ContextResult<String>> {
        Box::pin(async move {
            estimate("")?;
            Ok(String::new())
        })
    }
}
impl TurnStatus<Security> for Status {
    fn bind(&self, _: &TurnSelection<Security>) -> Arc<dyn ContextStatus> {
        Arc::new(Self)
    }
}
struct Output;
impl OutputWriter for Output {
    fn append<'a>(&'a self, _: &'a str) -> BoxFuture<'a, std::io::Result<()>> {
        Box::pin(async { Ok(()) })
    }
}
impl SessionOutput for Output {
    fn close(&self) -> BoxFuture<'_, std::io::Result<()>> {
        Box::pin(async { Ok(()) })
    }
}
struct Activity(Arc<AgentSessionActivity>);
impl ProviderActivity for Activity {
    fn begin_provider_request(&self) {}
    fn observe_provider_event(&self, _: &sessions::Event) {}
    fn finish_provider_request(&self) {}
}
impl SessionActivity for Activity {
    fn change_state(&self, state: DrainState) {
        self.0.change_state(state);
    }
    fn begin_execution(&self) -> i64 {
        self.0.begin_execution()
    }
    fn execution_duration(&self) -> Duration {
        Duration::ZERO
    }
    fn finish_execution(&self, execution: i64, outcome: &Execution) {
        self.0.finish_execution(execution, outcome.clone());
    }
    fn turn_started(&self, _: &str) {}
    fn turn_finished(&self, _: &str, _: &str, _: Option<&str>) {}
    fn assistant_message(&self, _: &str) {}
}
struct NoParent;
impl ParentCompletion for NoParent {
    fn deliver<'a>(&'a self, _: &'a Execution) -> BoxFuture<'a, Result<(), TurnError>> {
        Box::pin(async { Ok(()) })
    }
}
struct Selection(Mutex<TurnSelection<Security>>);
impl SelectionSource<Security> for Selection {
    fn capture(&self) -> Result<TurnSelection<Security>, TurnError> {
        Ok(self.0.lock().unwrap().clone())
    }
    fn update(&self, value: TurnSelection<Security>) -> Result<(), TurnError> {
        *self.0.lock().unwrap() = value;
        Ok(())
    }
}

pub(super) type Backend = RuntimeBackend<Security, ToolSettlement<Security>>;

/// A caller-bound test bridge for precisely the Wait operation. Other operations
/// deliberately fail if used; no canned successful runtime results are supplied.
pub(super) struct WaitBridge {
    service: Mutex<Option<WaitService<Backend>>>,
    entered: Semaphore,
}
impl Default for WaitBridge {
    fn default() -> Self {
        Self {
            service: Mutex::new(None),
            entered: Semaphore::new(0),
        }
    }
}
impl WaitBridge {
    pub(super) async fn entered(&self) {
        bounded(self.entered.acquire()).await.unwrap().forget();
    }
    fn service(&self) -> WaitService<Backend> {
        let service = self.service.lock().unwrap();
        let service = service
            .as_ref()
            .expect("wait bridge must bind after session construction");
        WaitService {
            session: service.session.clone(),
            active_work: service.active_work.clone(),
        }
    }
}
impl AgentRuntime for WaitBridge {
    type Security = Security;
    type SpawnRequest = SpawnRequest;
    type SpawnResult = SpawnResult;
    type SendRequest = SendRequest;
    type SendResult = SendResult;
    type InterruptRequest = NamedRequest;
    type InterruptResult = ();
    type StatusRequest = NamedRequest;
    type StatusResult = StatusSnapshot;
    type WaitRequest = WaitInput;
    type WaitResult = String;
    type Error = AgentToolError;
    fn spawn<'a>(
        &'a self,
        _: SpawnRequest,
        _: Security,
        _: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<SpawnResult, OperationError<AgentToolError>>> {
        panic!("wait-only bridge")
    }
    fn send<'a>(
        &'a self,
        _: SendRequest,
        _: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<SendResult, OperationError<AgentToolError>>> {
        panic!("wait-only bridge")
    }
    fn interrupt<'a>(
        &'a self,
        _: NamedRequest,
        _: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<(), OperationError<AgentToolError>>> {
        panic!("wait-only bridge")
    }
    fn status(&self, _: NamedRequest) -> Result<StatusSnapshot, AgentToolError> {
        panic!("wait-only bridge")
    }
    fn wait<'a>(
        &'a self,
        request: WaitInput,
        cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<String, OperationError<AgentToolError>>> {
        Box::pin(async move {
            let service = self.service();
            let waiting = service.wait(
                WaitDuration::new(request.duration_ms).expect("validated WaitInput"),
                cancellation,
            );
            tokio::pin!(waiting);
            let mut first_poll = true;
            std::future::poll_fn(|cx| {
                let result = waiting.as_mut().poll(cx);
                // Signal only after polling the actual session registration, not
                // merely after entering the wrapper or publishing ToolStarted.
                if first_poll {
                    self.entered.add_permits(1);
                    first_poll = false;
                }
                result
            })
            .await
            .map_err(|error| {
                if error.is_cancelled() {
                    OperationError::Cancelled
                } else {
                    OperationError::Failed(AgentToolError::Service(error.to_string()))
                }
            })
        })
    }
}
struct Models;
impl ModelSelector for Models {
    fn resolve(&self, _: &str) -> Result<String, AgentToolError> {
        panic!("wait does not select a model")
    }
}
fn definition() -> ConfiguredToolDefinition {
    ConfiguredToolDefinition::new(
        "Wait for incoming activity",
        r#"{"type":"object","properties":{"duration_ms":{"type":"integer","minimum":10000,"maximum":4294967294}},"additionalProperties":false}"#,
    )
}
type WaitTool = AgentTool<WaitBridge, Models>;
fn wait_tool(bridge: Arc<WaitBridge>) -> WaitTool {
    AgentTool::new(
        AgentToolKind::Wait,
        definition(),
        bridge,
        Arc::new(Models),
        None,
        false,
    )
}

/// Only the associated-type conversion is test-local; ordinary Tool execution,
/// Started/Finished events, terminal commit and conversation projection use the
/// actual runtime ToolSettlement implementation.
struct RetainedTool {
    metadata: ToolMetadata,
    tool: WaitTool,
}
impl Tool for RetainedTool {
    type Definition = ToolMetadata;
    type Invocation = tool_settlement::ToolInvocation;
    type Selection = TurnSelection<Security>;
    type Security = SharedSecurity<Security>;
    type Output = ToolOutput;
    type Error = ToolFailure;
    fn definition(&self) -> &ToolMetadata {
        &self.metadata
    }
    fn parallel_safety(&self, invocation: &Self::Invocation) -> ParallelSafety {
        self.tool.parallel_safety(&agents::ToolInvocation {
            arguments_json: invocation.arguments_json.clone(),
            assistant_sequence: invocation.assistant_sequence,
            call_id: invocation.call_id.clone(),
        })
    }
    fn enabled_after_interruption(&self) -> bool {
        self.tool.enabled_after_interruption()
    }
    fn invoke<'a>(
        &'a self,
        invocation: Self::Invocation,
        context: InvocationContext<'a, Self::Security, Self::Selection>,
    ) -> BoxFuture<'a, Result<ToolOutput, OperationError<ToolFailure>>> {
        Box::pin(async move {
            self.tool
                .invoke(
                    agents::ToolInvocation {
                        arguments_json: invocation.arguments_json,
                        assistant_sequence: invocation.assistant_sequence,
                        call_id: invocation.call_id,
                    },
                    InvocationContext::new(
                        context.security().snapshot().clone(),
                        AgentSelection {
                            requested_model: context
                                .selection()
                                .model
                                .requested_selector
                                .value
                                .clone(),
                        },
                        context.cancellation(),
                    ),
                )
                .await
                .map(ToolOutput::text)
                .map_err(|error| match error {
                    OperationError::Cancelled => OperationError::Cancelled,
                    OperationError::Failed(error) => OperationError::Failed(ToolFailure::new(
                        error.to_string(),
                        FailureKind::Unexpected,
                    )),
                })
        })
    }
}
struct Factory(Arc<RetainedTool>);
impl ToolFactory<Security> for Factory {
    fn materialize(&self) -> Result<ToolInventory<Security>, TurnError> {
        Ok(ToolInventory {
            tools: vec![ToolHandle::Ordinary(self.0.clone())],
            supported: vec![true],
            definitions: vec![definition()],
        })
    }
}

pub(super) struct Harness {
    pub(super) session: AgentSession<Backend>,
    pub(super) bridge: Arc<WaitBridge>,
    pub(super) provider: Scripted,
    pub(super) history: AgentHistoryView,
    pub(super) activity: Arc<AgentSessionActivity>,
    directory: PathBuf,
}
impl Harness {
    pub(super) async fn new(blockers: Vec<Arc<dyn ActiveWorkBlocker>>) -> Self {
        let directory = std::env::current_dir().unwrap().join(".tmp").join(format!(
            "retained-wait-{}-{}",
            std::process::id(),
            id()
        ));
        std::fs::create_dir_all(directory.join("scratch")).unwrap();
        let root = SelectedSessionRoot::new(directory.clone(), RootOrigin::Current).unwrap();
        let repository = ConversationRepository::new(EventRepository::new(
            SessionDatabase::open(root.clone()).unwrap(),
        ));
        let session_id = id();
        let history = repository
            .bind_agent_history(
                session_id.clone(),
                Arc::new(
                    AgentHistoryFile::new(
                        root.clone(),
                        directory.join("history.jsonl"),
                        vec![session_id.clone()],
                    )
                    .unwrap(),
                ),
            )
            .unwrap();
        let templates = templates();
        let compactor = Compactor::new(
            CompactionSettings {
                trigger_percent: 95,
                target_percent: 30,
                maximum_input_tokens: 60000,
                summary_output_tokens: 1024,
                summary_instructions: String::new(),
                summary_prefix: String::new(),
            },
            ModelProfiles::new(&BTreeMap::new()),
            templates.clone(),
        )
        .unwrap();
        let context = ContextController::new(
            compactor,
            history.clone(),
            CompactionGroupBlobStore::for_session(&root, "scratch", Arc::new(Names)).unwrap(),
            templates.clone(),
        );
        let broker = Arc::new(EventBroker::new());
        let publisher: Arc<parrot_runtime::agent_turn::engine::Publisher> = broker.clone();
        let reminders = Arc::new(
            ExitReminders::new(history.clone(), publisher, templates.clone())
                .await
                .unwrap(),
        );
        let activity = Arc::new(AgentSessionActivity::new(Arc::new(TestClock)));
        let bridge = Arc::new(WaitBridge::default());
        let tools = Arc::new(
            ToolSettlement::new(ToolSettlementConfig {
                factory: Arc::new(Factory(Arc::new(RetainedTool {
                    metadata: ToolMetadata {
                        name: "wait".into(),
                        aliases: vec![],
                    },
                    tool: wait_tool(bridge.clone()),
                }))),
                templates: templates.clone(),
                output_blobs: Arc::new(
                    ToolOutputBlobStore::for_session(&root, "scratch", Arc::new(Names)).unwrap(),
                ),
                publisher: broker.clone(),
                activity: activity.clone(),
                diagnostics: Arc::new(Diagnostics),
                image_bytes_per_cycle: 0,
            })
            .unwrap(),
        );
        let provider = Scripted::default();
        let backend = RuntimeBackend::new(RuntimeConfig {
            agent_limits: parrot_foundation::config::runtime::AgentLimitsConfig {
                disable_max_turns: false,
                ..Default::default()
            },
            identity: SessionIdentity {
                session_id,
                name: "main".into(),
                parent_session_id: String::new(),
                depth: 0,
            },
            history: history.clone(),
            broker: broker.clone(),
            publisher: broker,
            selection: Arc::new(Selection(Mutex::new(TurnSelection {
                model: provider.selection(),
                profile: TurnProfile {
                    id: "wait-test".into(),
                    maximum_requests: 8,
                    allowed_tools: None,
                    disabled_tools: vec![],
                },
                security: Arc::new(Security(42)),
            }))),
            tools,
            prompt: Box::new(Prompt),
            skills: Arc::new(Skills),
            context,
            status: Arc::new(Status),
            completions: vec![],
            templates: templates.clone(),
            reminders,
            output: Arc::new(Output),
            activity: Arc::new(Activity(activity.clone())),
            parent_completion: Arc::new(NoParent),
            ancestors: vec![],
            event_id: Arc::new(id),
        })
        .await
        .unwrap();
        let session = AgentSession::new(backend, Arc::new(Signal::new()));
        *bridge.service.lock().unwrap() = Some(WaitService {
            session: session.clone(),
            active_work: Arc::new(ActiveWorkReminder {
                blockers,
                templates,
            }),
        });
        Self {
            session,
            bridge,
            provider,
            history,
            activity,
            directory,
        }
    }
    pub(super) async fn invoke_wait(
        &self,
        arguments: &str,
        cancellation: &dyn Cancellation,
    ) -> Result<String, OperationError<AgentToolError>> {
        wait_tool(self.bridge.clone())
            .invoke(crate::invocation(arguments), crate::context(cancellation))
            .await
    }
    pub(super) async fn send(
        &self,
        text: &str,
        message: &str,
        activity: IncomingActivity,
    ) -> parrot_runtime::agent_turn::lifecycle::SendResult {
        bounded(self.session.send(
            vec![ConversationPart::text(text)],
            message.into(),
            protocol::Delivery::DELIVERY_STEER,
            activity,
            Arc::new(NeverCancelled),
        ))
        .await
        .unwrap()
    }
    pub(super) async fn events(&self) -> Vec<Event> {
        self.history.repository().events().replay().await.unwrap()
    }
    pub(super) async fn close(&self) {
        bounded(self.session.close()).await.unwrap();
        self.bridge.service.lock().unwrap().take();
    }
}
impl Drop for Harness {
    fn drop(&mut self) {
        self.bridge.service.lock().unwrap().take();
        let _ = std::fs::remove_dir_all(&self.directory);
    }
}

pub(super) fn wait_completion() -> Completion {
    Completion {
        finish_reason: "tool_calls".into(),
        tool_calls: vec![sessions::ToolCall {
            id: "wait-call".into(),
            name: "wait".into(),
            arguments_json: "{}".into(),
        }],
        ..Completion::default()
    }
}
pub(super) fn done_completion() -> Completion {
    Completion {
        finish_reason: "stop".into(),
        assistant_text: "done".into(),
        ..Completion::default()
    }
}
