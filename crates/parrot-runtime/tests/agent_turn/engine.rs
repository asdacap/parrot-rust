//! T46 integration: the actual session drain, SQLite/JSONL history, context and provider pool.
use parrot_domain::{
    contracts::{
        async_ops::{BoxFuture, Cancellation, OperationError},
        events::EventPublisher,
        filenames::FilenameGenerator,
        paths::{ResolvedRead, RootOrigin, SelectedSessionRoot},
        security::AccessSnapshot,
        storage::Committed,
    },
    protocol::messages::parrot::{self as protocol, Event},
    values::config::ModelProfiles,
};
use parrot_foundation::template_catalog::PromptTemplateCatalog;
use parrot_provider::{
    catalogs::Model,
    routing::{
        aliases::{ModelAliasCatalog, ModelRouter, ModelRouting},
        registry::ProviderRegistry,
    },
    sessions::{self, Completion, Error, EventStream, Provider, Request, Session},
};
use parrot_runtime::{
    active_work::exit_reminders::ExitReminders,
    agent_turn::{
        engine::{Publisher as RuntimePublisher, RuntimeBackend, RuntimeConfig},
        lifecycle::{AgentSession, Execution, ExecutionStatus, IncomingActivity, Signal},
        provider_call::{OutputWriter, ProviderActivity},
        services::*,
    },
    compaction::*,
    event_broker::{BrokerError, EventBroker},
    prompt_context::{PromptResult, SkillRendering, SystemPrompt},
};
use parrot_storage::{
    artifacts::compaction::CompactionGroupBlobStore,
    conversation::{
        AgentConversation, AgentHistoryView, ConversationMessage, ConversationRepository,
        MessageRole,
    },
    database::SessionDatabase,
    events::{ConversationPart, EventRepository, NeverCancelled},
    history_files::AgentHistoryFile,
};
use std::{
    collections::{BTreeMap, VecDeque},
    io,
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering},
    },
    time::Duration,
};
use tokio::sync::Semaphore;

static NEXT: AtomicU64 = AtomicU64::new(0);
fn id() -> String {
    format!("t46-engine-{}", NEXT.fetch_add(1, Ordering::SeqCst))
}
async fn bounded<F: std::future::Future>(future: F) -> F::Output {
    tokio::time::timeout(Duration::from_secs(10), future)
        .await
        .expect("real drain did not settle")
}
#[derive(Default)]
struct Trace(Mutex<Vec<String>>);
impl Trace {
    fn push(&self, value: impl Into<String>) {
        self.0.lock().unwrap().push(value.into());
    }
    fn values(&self) -> Vec<String> {
        self.0.lock().unwrap().clone()
    }
}
struct Gate {
    entered: Semaphore,
    release: Semaphore,
}
impl Gate {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            entered: Semaphore::new(0),
            release: Semaphore::new(0),
        })
    }
    async fn wait(&self, cancellation: &dyn Cancellation) -> Result<(), Error> {
        self.entered.add_permits(1);
        tokio::select! { permit = self.release.acquire() => { permit.unwrap().forget(); Ok(()) }, _ = cancellation.cancelled() => Err(Error::Cancelled) }
    }
    async fn entered(&self) {
        bounded(self.entered.acquire()).await.unwrap().forget();
    }
    fn open(&self) {
        self.release.add_permits(1);
    }
}
struct Names;
impl FilenameGenerator for Names {
    fn next_filename(&self) -> String {
        format!("{}.dat", id())
    }
}
fn templates() -> Arc<PromptTemplateCatalog> {
    let env = [
        ("HOME", "/tmp/t46/home"),
        ("XDG_CONFIG_HOME", "/tmp/t46/config"),
        ("XDG_STATE_HOME", "/tmp/t46/state"),
        ("XDG_DATA_HOME", "/tmp/t46/data"),
        ("WORKDIR", "/tmp/t46/work"),
    ]
    .into_iter()
    .map(|(k, v)| (k.into(), v.into()))
    .collect();
    Arc::new(PromptTemplateCatalog::from_content("", &env).unwrap().1)
}
struct Fixture {
    path: PathBuf,
    repository: ConversationRepository,
    history: AgentHistoryView,
}
impl Fixture {
    fn new() -> Self {
        let path = std::env::current_dir().unwrap().join(".tmp").join(format!(
            "engine-{}-{}",
            std::process::id(),
            id()
        ));
        std::fs::create_dir_all(path.join("scratch/blobs")).unwrap();
        let repository = Self::open(&path);
        let history = Self::history_for(&path, &repository);
        Self {
            path,
            repository,
            history,
        }
    }
    fn open(path: &Path) -> ConversationRepository {
        ConversationRepository::new(EventRepository::new(
            SessionDatabase::open(
                SelectedSessionRoot::new(path.into(), RootOrigin::Current).unwrap(),
            )
            .unwrap(),
        ))
    }
    fn history_for(path: &Path, repository: &ConversationRepository) -> AgentHistoryView {
        let root = repository.events().database().selected_root().clone();
        repository
            .bind_agent_history(
                "agent".into(),
                Arc::new(
                    AgentHistoryFile::new(root, path.join("agent.jsonl"), vec!["agent".into()])
                        .unwrap(),
                ),
            )
            .unwrap()
    }
    fn controller(&self) -> ContextController {
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
        let blobs = CompactionGroupBlobStore::for_session(
            self.repository.events().database().selected_root(),
            "scratch",
            Arc::new(Names),
        )
        .unwrap();
        ContextController::new(compactor, self.history.clone(), blobs, templates)
    }
    async fn seed(&self) {
        for index in 0..6 {
            for role in [MessageRole::User, MessageRole::Assistant] {
                let mut event = Event::new();
                event.id = id();
                event.agent_session_id = "agent".into();
                self.history
                    .append_conversation(
                        event,
                        ConversationMessage::text(
                            role,
                            format!("seed-{index}-{}", "x ".repeat(200)),
                        ),
                    )
                    .await
                    .unwrap();
            }
        }
    }
    async fn events(&self) -> Vec<Event> {
        self.repository.events().replay().await.unwrap()
    }
    async fn conversation(&self) -> AgentConversation {
        AgentConversation::new(self.history.clone()).await.unwrap()
    }
    fn jsonl(&self) -> String {
        std::fs::read_to_string(self.path.join("agent.jsonl")).unwrap()
    }
    async fn runtime(
        &self,
        provider: Scripted,
        tools: Arc<Tools>,
        callbacks: Vec<Arc<dyn TurnCompletion<Security>>>,
        lifetime: Arc<dyn Cancellation>,
        terminal_failure: usize,
    ) -> Runtime {
        self.runtime_with_overrides(
            provider,
            tools,
            callbacks,
            lifetime,
            terminal_failure,
            RuntimeOverrides::default(),
        )
        .await
    }
    async fn runtime_with_overrides(
        &self,
        provider: Scripted,
        tools: Arc<Tools>,
        callbacks: Vec<Arc<dyn TurnCompletion<Security>>>,
        lifetime: Arc<dyn Cancellation>,
        terminal_failure: usize,
        overrides: RuntimeOverrides,
    ) -> Runtime {
        let broker = Arc::new(EventBroker::new());
        let trace = tools.trace.clone();
        let publisher = Arc::new(Publisher {
            history: self.history.clone(),
            broker: broker.clone(),
            terminal_failure,
            terminals: AtomicUsize::new(0),
            terminal_observed: AtomicBool::new(false),
            reminder_gate: Mutex::new(None),
            cancel_retry: AtomicBool::new(false),
            trace: trace.clone(),
        });
        let erased: Arc<RuntimePublisher> = publisher.clone();
        let templates = templates();
        let reminders = Arc::new(
            ExitReminders::new(self.history.clone(), erased.clone(), templates.clone())
                .await
                .unwrap(),
        );
        let trace = tools.trace.clone();
        let output = Arc::new(Output::default());
        let activity = Arc::new(Activity::new(trace.clone()));
        let backend = RuntimeBackend::new(RuntimeConfig {
            identity: SessionIdentity {
                session_id: "agent".into(),
                name: "test".into(),
                parent_session_id: String::new(),
                depth: 1,
            },
            history: self.history.clone(),
            broker,
            publisher: erased,
            selection: Arc::new(Selection(Mutex::new(TurnSelection {
                model: provider.selection(),
                profile: TurnProfile {
                    id: "mode".into(),
                    maximum_requests: 8,
                    allowed_tools: None,
                    disabled_tools: vec![],
                },
                security: Arc::new(Security),
            }))),
            tools: tools.clone(),
            prompt: overrides.prompt.unwrap_or_else(|| {
                Box::new(Prompt {
                    trace: trace.clone(),
                    sampled: false,
                    cancel_sample: tools.prompt_cancelled.load(Ordering::SeqCst),
                })
            }),
            skills: Arc::new(Skills(trace.clone())),
            context: self.controller(),
            status: overrides
                .status
                .unwrap_or_else(|| Arc::new(Status(trace.clone()))),
            completions: callbacks,
            templates,
            reminders,
            output: output.clone(),
            activity: activity.clone(),
            parent_completion: Arc::new(Parent(trace)),
            ancestors: vec![],
            event_id: Arc::new(id),
        })
        .await
        .unwrap();
        Runtime {
            session: AgentSession::new(backend, lifetime),
            publisher,
            output,
            activity,
        }
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}
#[derive(Default)]
struct RuntimeOverrides {
    prompt: Option<Box<dyn SystemPrompt>>,
    status: Option<Arc<dyn TurnStatus<Security>>>,
}
type Backend = RuntimeBackend<Security, Tools>;
struct Runtime {
    session: AgentSession<Backend>,
    publisher: Arc<Publisher>,
    output: Arc<Output>,
    activity: Arc<Activity>,
}
impl Runtime {
    async fn send(
        &self,
        text: &str,
        delivery: protocol::Delivery,
    ) -> parrot_runtime::agent_turn::lifecycle::SendResult {
        bounded(self.session.send(
            vec![ConversationPart::text(text)],
            id(),
            delivery,
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
    async fn close(&self) {
        bounded(self.session.close()).await.unwrap();
    }
}
struct Publisher {
    history: AgentHistoryView,
    broker: Arc<EventBroker>,
    terminal_failure: usize,
    terminals: AtomicUsize,
    terminal_observed: AtomicBool,
    reminder_gate: Mutex<Option<Arc<Gate>>>,
    cancel_retry: AtomicBool,
    trace: Arc<Trace>,
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
            let stored = self.history.repository().events().replay().await.unwrap();
            assert!(
                stored.iter().any(|stored| stored == event.value()),
                "publication preceded SQLite commit"
            );
            if event.value().has_retry_notice() && self.cancel_retry.load(Ordering::SeqCst) {
                return Err(OperationError::Cancelled);
            }
            if event.value().has_turn_ended() {
                self.terminals.fetch_add(1, Ordering::SeqCst);
                let conversation = self.history.conversation().await.unwrap();
                assert_eq!(
                    conversation.last().unwrap().message.role,
                    MessageRole::Assistant
                );
                self.terminal_observed.store(true, Ordering::SeqCst);
                match self.terminal_failure {
                    1 => return Err(OperationError::Cancelled),
                    2 => return Err(OperationError::Failed(BrokerError::InvalidCapacity)),
                    _ => {}
                }
            }
            if event.value().has_agent_notice()
                && event.value().agent_notice().kind == "exit_reminder_changed"
            {
                let gate = self.reminder_gate.lock().unwrap().clone();
                if let Some(gate) = gate {
                    gate.wait(cancellation)
                        .await
                        .map_err(|_| OperationError::Cancelled)?;
                }
            }
            self.broker.publish_committed(event, cancellation).await?;
            if event.value().has_agent_finished() || event.value().has_agent_failed() {
                self.trace.push("execution-terminal-published");
            }
            Ok(())
        })
    }
    fn publish_transient<'a>(
        &'a self,
        event: Event,
        cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<(), OperationError<BrokerError>>> {
        self.broker.publish_transient(event, cancellation)
    }
}
struct Security;
impl AccessSnapshot for Security {
    type Error = ();
    fn check_read(&self, _: &ResolvedRead) -> Result<(), ()> {
        panic!("no real filesystem authority")
    }
    fn check_write(&self, _: &Path) -> Result<(), ()> {
        panic!("no real filesystem authority")
    }
}
struct Selection(Mutex<TurnSelection<Security>>);
impl SelectionSource<Security> for Selection {
    fn capture(&self) -> Result<TurnSelection<Security>, TurnError> {
        Ok(self.0.lock().unwrap().clone())
    }
    fn update(&self, selection: TurnSelection<Security>) -> Result<(), TurnError> {
        *self.0.lock().unwrap() = selection;
        Ok(())
    }
}
#[derive(Default)]
struct Tools {
    trace: Arc<Trace>,
    inline_prepare: AtomicBool,
    inline_compact: AtomicBool,
    prompt_cancelled: AtomicBool,
    reconciliations: AtomicUsize,
    definitions: Vec<sessions::Tool>,
    materializations: AtomicUsize,
    reconcile_gate: Mutex<Option<Arc<Gate>>>,
}
impl TurnTools<Security> for Tools {
    type Snapshot = Vec<sessions::Tool>;
    fn materialize(&self) -> Result<Self::Snapshot, TurnError> {
        self.materializations.fetch_add(1, Ordering::SeqCst);
        Ok(self.definitions.clone())
    }
    fn permitted(
        &self,
        snapshot: &Self::Snapshot,
        selection: &TurnSelection<Security>,
    ) -> Result<Self::Snapshot, TurnError> {
        Ok(snapshot
            .iter()
            .filter(|tool| selection.profile.is_tool_permitted(&tool.name))
            .cloned()
            .collect())
    }
    fn definitions<'a>(&self, snapshot: &'a Self::Snapshot) -> &'a [sessions::Tool] {
        snapshot
    }
    fn enabled_after_interruption(&self, snapshot: &Self::Snapshot) -> Self::Snapshot {
        snapshot.clone()
    }
    fn reconcile<'a>(
        &'a self,
        resources: TurnResources<'a>,
        selection: &'a TurnSelection<Security>,
        _: &'a Self::Snapshot,
        _: &'a dyn SelectionSource<Security>,
        cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<(), TurnError>> {
        Box::pin(async move {
            self.reconciliations.fetch_add(1, Ordering::SeqCst);
            let gate = self.reconcile_gate.lock().unwrap().take();
            if let Some(gate) = gate {
                gate.wait(cancellation).await.map_err(TurnError::Provider)?;
            }
            for assistant in resources
                .history
                .conversation()
                .await?
                .into_iter()
                .filter(|item| !item.message.tool_calls.is_empty())
            {
                for call in assistant.message.tool_calls {
                    if resources
                        .history
                        .tool_terminals_of_calls(vec![call.id.clone()])
                        .await?
                        .is_empty()
                    {
                        let mut event = Event::new();
                        event.id = id();
                        event.agent_session_id = "agent".into();
                        if let Some(item) = resources
                            .history
                            .append_tool_settlement(
                                event,
                                assistant.sequence,
                                parrot_storage::events::ToolExecutionTerminal {
                                    tool_call_id: call.id,
                                    tool_name: call.name,
                                    status: parrot_storage::events::ToolExecutionStatus::Finished,
                                    result_parts: vec![ConversationPart::text("fake tool result")],
                                    message: "fake tool result".into(),
                                },
                            )
                            .await?
                        {
                            let message = resources.conversation.materialize(item).await?;
                            resources.conversation.add(message);
                        }
                    }
                }
            }
            if resources
                .conversation
                .messages()
                .iter()
                .any(|message| !message.tool_calls.is_empty())
                && self.inline_compact.swap(false, Ordering::SeqCst)
            {
                let result = resources
                    .context
                    .compact(
                        EpochInput {
                            selection: &selection.model,
                            tools: &[],
                            conversation: resources.conversation,
                            prompt: resources.prompt,
                            sessions: resources.sessions,
                            status: resources.status,
                            events: resources.events,
                        },
                        None,
                        cancellation,
                    )
                    .await?;
                assert!(result.reduced);
                self.trace.push("inline-compacted");
            }
            if self.inline_prepare.swap(false, Ordering::SeqCst) {
                let epoch = resources
                    .context
                    .prepare_epoch(
                        EpochInput {
                            selection: &selection.model,
                            tools: &[],
                            conversation: resources.conversation,
                            prompt: resources.prompt,
                            sessions: resources.sessions,
                            status: resources.status,
                            events: resources.events,
                        },
                        cancellation,
                    )
                    .await?;
                self.trace.push(format!("inline:{}", epoch.instructions));
            }
            Ok(())
        })
    }
}
struct Prompt {
    trace: Arc<Trace>,
    sampled: bool,
    cancel_sample: bool,
}
impl SystemPrompt for Prompt {
    fn renew_epoch(&mut self) {
        self.sampled = false;
    }
    fn sample_epoch<'a>(
        &'a mut self,
        cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, PromptResult<()>> {
        Box::pin(async move {
            self.trace.push("epoch-start");
            tokio::task::yield_now().await;
            if self.cancel_sample {
                return Err(parrot_runtime::prompt_context::PromptError::Cancelled);
            }
            cancellation
                .check()
                .map_err(|_| parrot_runtime::prompt_context::PromptError::Cancelled)?;
            self.sampled = true;
            self.trace.push("epoch-done");
            Ok(())
        })
    }
    fn build(
        &mut self,
        _: &parrot_provider::routing::aliases::ResolvedModelSelection,
    ) -> PromptResult<String> {
        assert!(self.sampled, "prompt build raced awaited epoch");
        Ok("sampled epoch instructions".into())
    }
}
struct Skills(Arc<Trace>);
impl TurnSkills for Skills {
    fn begin_turn<'a>(&'a self, _: &'a dyn Cancellation) -> BoxFuture<'a, PromptResult<()>> {
        Box::pin(async move {
            self.0.push("skills-begin");
            Ok(())
        })
    }
    fn select<'a>(
        &'a self,
        _: &'a [ConversationPart],
        _: &'a dyn Cancellation,
    ) -> BoxFuture<'a, PromptResult<()>> {
        Box::pin(async move {
            self.0.push("skills-select");
            Ok(())
        })
    }
    fn render<'a>(
        &'a self,
        _: &'a dyn Cancellation,
    ) -> BoxFuture<'a, PromptResult<SkillRendering>> {
        Box::pin(async move {
            self.0.push("skills-render");
            Ok(SkillRendering {
                content: String::new(),
                loaded_paths: vec![],
            })
        })
    }
    fn end_turn(&self) -> BoxFuture<'_, ()> {
        Box::pin(async move {
            self.0.push("skills-end");
        })
    }
}
struct Status(Arc<Trace>);
impl ContextStatus for Status {
    fn observe<'a>(
        &'a self,
        _: &'a ContextSnapshot,
        _: &'a dyn Cancellation,
    ) -> BoxFuture<'a, ContextResult<String>> {
        Box::pin(async move {
            self.0.push("status");
            Ok("actual status".into())
        })
    }
    fn observe_fitted<'a>(
        &'a self,
        estimate: &'a (dyn Fn(&str) -> ContextResult<ContextSnapshot> + Send + Sync),
        _: &'a dyn Cancellation,
    ) -> BoxFuture<'a, ContextResult<String>> {
        Box::pin(async move {
            estimate("actual status")?;
            self.0.push("status-fitted");
            Ok("actual status".into())
        })
    }
}
impl TurnStatus<Security> for Status {
    fn bind(&self, _: &TurnSelection<Security>) -> Arc<dyn ContextStatus> {
        Arc::new(Status(self.0.clone()))
    }
}
#[derive(Default)]
struct Output {
    text: Mutex<String>,
    closes: AtomicUsize,
    close_gate: Mutex<Option<Arc<Gate>>>,
}
impl OutputWriter for Output {
    fn append<'a>(&'a self, text: &'a str) -> BoxFuture<'a, io::Result<()>> {
        Box::pin(async move {
            self.text.lock().unwrap().push_str(text);
            Ok(())
        })
    }
}
impl SessionOutput for Output {
    fn close(&self) -> BoxFuture<'_, io::Result<()>> {
        Box::pin(async move {
            let gate = self.close_gate.lock().unwrap().clone();
            if let Some(gate) = gate {
                gate.wait(&NeverCancelled).await.unwrap();
            }
            self.closes.fetch_add(1, Ordering::SeqCst);
            Ok(())
        })
    }
}
#[path = "engine/activity_fixture.rs"]
mod activity_fixture;
#[path = "engine/activity_timing.rs"]
mod activity_timing;
use activity_fixture::Activity;
struct Parent(Arc<Trace>);
impl ParentCompletion for Parent {
    fn deliver<'a>(&'a self, execution: &'a Execution) -> BoxFuture<'a, Result<(), TurnError>> {
        Box::pin(async move {
            self.0
                .push(format!("parent-delivered:{:?}", execution.status));
            Ok(())
        })
    }
}
#[path = "engine/a003_cases.rs"]
mod a003_cases;
#[path = "engine/budget_selection.rs"]
mod budget_selection;
#[path = "engine/context_cancellation.rs"]
mod context_cancellation;
#[path = "engine/crash_recovery.rs"]
mod crash_recovery;
#[path = "engine/external_compaction.rs"]
mod external_compaction;
#[path = "engine/inline_context.rs"]
mod inline_context;
#[path = "engine/input_history.rs"]
mod input_history;
#[path = "engine/late_mutation.rs"]
mod late_mutation;
#[path = "engine/live_observation.rs"]
mod live_observation;
#[path = "engine/regressions.rs"]
mod regressions;
#[path = "engine/repository_fault.rs"]
mod repository_fault;
#[path = "engine/request_budget_edges.rs"]
mod request_budget_edges;
#[path = "engine/restart_retry.rs"]
mod restart_retry;
#[path = "engine/skill_placement.rs"]
mod skill_placement;
#[path = "engine/source_callback_reset.rs"]
mod source_callback_reset;
struct Step {
    completion: Result<Completion, Error>,
    gate: Option<Arc<Gate>>,
    close_gate: Option<Arc<Gate>>,
    events: VecDeque<sessions::Event>,
}
impl Step {
    fn complete(text: &str) -> Self {
        Self {
            completion: Ok(Completion {
                assistant_text: text.into(),
                finish_reason: "stop".into(),
                input_tokens: 17,
                output_tokens: 9,
                ..Completion::default()
            }),
            gate: None,
            close_gate: None,
            events: VecDeque::new(),
        }
    }
}
#[derive(Clone, Default)]
struct Scripted {
    steps: Arc<Mutex<VecDeque<Step>>>,
    requests: Arc<Mutex<Vec<Request>>>,
    stream_closes: Arc<AtomicUsize>,
    session_closes: Arc<AtomicUsize>,
    session_close_gate: Arc<Mutex<Option<Arc<Gate>>>>,
}
impl Scripted {
    fn new(steps: Vec<Step>) -> Self {
        Self {
            steps: Arc::new(Mutex::new(steps.into())),
            ..Self::default()
        }
    }
    fn call_impl(&self, request: Request) -> Result<Box<dyn EventStream>, Error> {
        self.requests.lock().unwrap().push(request);
        let step = self
            .steps
            .lock()
            .unwrap()
            .pop_front()
            .expect("unexpected real provider request");
        Ok(Box::new(Stream {
            step: Some(step),
            emitted: false,
            closes: self.stream_closes.clone(),
        }))
    }
    fn selection(&self) -> parrot_provider::routing::aliases::ResolvedModelSelection {
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
        panic!("no credentials")
    }
    fn seed_models(&self) -> Vec<Model> {
        vec![]
    }
    fn list_models<'a>(
        &'a self,
        _: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<Vec<Model>, Error>> {
        panic!("no network")
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
        Box::pin(async move {
            let gate = self.session_close_gate.lock().unwrap().clone();
            if let Some(gate) = gate {
                gate.wait(&NeverCancelled).await?;
            }
            self.session_closes.fetch_add(1, Ordering::SeqCst);
            Ok(())
        })
    }
}
struct Stream {
    step: Option<Step>,
    emitted: bool,
    closes: Arc<AtomicUsize>,
}
impl EventStream for Stream {
    fn next<'a>(
        &'a mut self,
        cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<Option<sessions::Event>, Error>> {
        Box::pin(async move {
            if self.emitted {
                return Ok(None);
            }
            let Some(step) = self.step.as_mut() else {
                return Ok(None);
            };
            if let Some(gate) = step.gate.take() {
                gate.wait(cancellation).await?;
            }
            if let Some(event) = step.events.pop_front() {
                return Ok(Some(event));
            }
            cancellation.check().map_err(|_| Error::Cancelled)?;
            let completed = step.completion.clone()?;
            self.emitted = true;
            Ok(Some(sessions::Event::Completed(completed)))
        })
    }
    fn close(&mut self) -> BoxFuture<'_, Result<(), Error>> {
        Box::pin(async move {
            if let Some(gate) = self.step.as_mut().and_then(|step| step.close_gate.take()) {
                gate.wait(&NeverCancelled).await?;
            }
            self.closes.fetch_add(1, Ordering::SeqCst);
            Ok(())
        })
    }
}

#[tokio::test]
async fn terminal_commit_precedes_cancellable_publication_without_second_interrupted_terminal() {
    for failure in [1, 2] {
        let fixture = Fixture::new();
        let provider = Scripted::new(vec![Step::complete("normal durable answer")]);
        let runtime = fixture
            .runtime(
                provider.clone(),
                Arc::new(Tools::default()),
                vec![],
                Arc::new(Signal::new()),
                failure,
            )
            .await;
        let sent = runtime
            .send("question", protocol::Delivery::DELIVERY_STEER)
            .await;
        let execution = bounded(sent.execution.join()).await.unwrap();
        assert_ne!(execution.status, ExecutionStatus::Succeeded);
        let events = fixture.events().await;
        let ended: Vec<_> = events
            .iter()
            .filter(|event| event.has_turn_ended())
            .collect();
        assert_eq!(ended.len(), 1);
        assert_eq!(ended[0].turn_ended().finish_reason, "stop");
        assert_eq!(runtime.publisher.terminals.load(Ordering::SeqCst), 1);
        assert!(runtime.publisher.terminal_observed.load(Ordering::SeqCst));
        let conversation = fixture.conversation().await;
        let assistants: Vec<_> = conversation
            .messages()
            .into_iter()
            .filter(|message| message.role == MessageRole::Assistant)
            .collect();
        assert_eq!(assistants.len(), 1);
        assert_eq!(assistants[0].content(), "normal durable answer");
        assert!(!fixture.jsonl().contains("interrupted"));
        runtime.close().await;
    }
}
#[tokio::test]
async fn epoch_status_and_skill_input_order_is_awaited_in_actual_turn() {
    let fixture = Fixture::new();
    let tools = Arc::new(Tools::default());
    let trace = tools.trace.clone();
    let provider = Scripted::new(vec![Step::complete("answer")]);
    let runtime = fixture
        .runtime(provider.clone(), tools, vec![], Arc::new(Signal::new()), 0)
        .await;
    let sent = runtime
        .send("question", protocol::Delivery::DELIVERY_STEER)
        .await;
    assert_eq!(
        bounded(sent.execution.join()).await.unwrap().status,
        ExecutionStatus::Succeeded
    );
    let trace = trace.values();
    let position = |name: &str| trace.iter().position(|entry| entry == name).unwrap();
    assert!(position("epoch-start") < position("epoch-done"));
    assert!(position("epoch-done") < position("status-fitted"));
    assert!(position("status-fitted") < position("skills-begin"));
    assert!(position("skills-begin") < position("skills-select"));
    assert!(position("skills-select") < position("skills-render"));
    let requests = provider.requests.lock().unwrap().clone();
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].instructions, "sampled epoch instructions");
    drop(requests);
    assert_eq!(
        fixture
            .events()
            .await
            .iter()
            .filter(|event| event.has_input_promoted())
            .count(),
        1
    );
    runtime.close().await;
}
#[tokio::test]
async fn borrowed_tool_context_prepare_runs_inline_and_is_used_by_the_next_provider() {
    let fixture = Fixture::new();
    let tools = Arc::new(Tools::default());
    tools.inline_prepare.store(true, Ordering::SeqCst);
    let trace = tools.trace.clone();
    let provider = Scripted::new(vec![Step::complete("inline result")]);
    let runtime = fixture
        .runtime(provider.clone(), tools, vec![], Arc::new(Signal::new()), 0)
        .await;
    let sent = runtime
        .send("question", protocol::Delivery::DELIVERY_STEER)
        .await;
    assert_eq!(
        bounded(sent.execution.join()).await.unwrap().status,
        ExecutionStatus::Succeeded
    );
    assert!(
        trace
            .values()
            .iter()
            .any(|entry| entry == "inline:sampled epoch instructions")
    );
    assert_eq!(
        provider.requests.lock().unwrap()[0].instructions,
        "sampled epoch instructions"
    );
    assert_eq!(
        fixture
            .events()
            .await
            .iter()
            .filter(|event| event.has_turn_started())
            .count(),
        1
    );
    runtime.close().await;
}
