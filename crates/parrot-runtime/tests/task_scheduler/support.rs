use super::{admission_publisher::AdmissionPublisher, fixture::*};
use parrot_domain::{
    contracts::{
        async_ops::{BoxFuture, Cancellation},
        filenames::FilenameGenerator,
        paths::{ResolvedRead, RootOrigin, SelectedSessionRoot},
        security::{AccessSnapshot, RestrictableAccessSnapshot},
    },
    protocol::messages::parrot::{self as protocol, Event},
    values::config::ModelProfiles,
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
use parrot_runtime::task_scheduler::*;
use parrot_runtime::{
    active_work::exit_reminders::ExitReminders,
    agent_turn::{
        engine::{RuntimeBackend, RuntimeConfig},
        lifecycle::{AgentSession, DrainState, Execution, IncomingActivity},
        provider_call::{OutputWriter, ProviderActivity},
        services::*,
    },
    children::*,
    compaction::*,
    event_broker::EventBroker,
    prompt_context::{PromptResult, SkillRendering, SystemPrompt},
};
use parrot_storage::{
    artifacts::compaction::CompactionGroupBlobStore,
    conversation::{AgentHistoryView, ConversationMessage, ConversationRepository, MessageRole},
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
        atomic::{AtomicU64, AtomicUsize, Ordering},
    },
    time::Duration,
};
use tokio::sync::Semaphore;

static NEXT: AtomicU64 = AtomicU64::new(0);
pub fn id() -> String {
    format!("t50-{:06}", NEXT.fetch_add(1, Ordering::SeqCst))
}
pub async fn bounded<F: std::future::Future>(future: F) -> F::Output {
    tokio::time::timeout(Duration::from_secs(10), future)
        .await
        .expect("T50 operation did not settle")
}
#[derive(Default)]
pub struct Trace {
    values: Mutex<Vec<String>>,
    changed: tokio::sync::Notify,
}
impl Trace {
    pub fn push(&self, value: impl Into<String>) {
        self.values.lock().unwrap().push(value.into());
        self.changed.notify_waiters();
    }
    pub fn values(&self) -> Vec<String> {
        self.values.lock().unwrap().clone()
    }
    pub async fn wait_for(&self, expected: &str) {
        bounded(async {
            loop {
                let changed = self.changed.notified();
                tokio::pin!(changed);
                changed.as_mut().enable();
                if self.values().iter().any(|value| value == expected) {
                    break;
                }
                changed.await;
            }
        })
        .await;
    }
}
pub struct Gate {
    entered: Semaphore,
    release: Semaphore,
}
impl Gate {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            entered: Semaphore::new(0),
            release: Semaphore::new(0),
        })
    }
    pub async fn wait(&self, cancellation: &dyn Cancellation) -> Result<(), Error> {
        self.entered.add_permits(1);
        tokio::select! { permit = self.release.acquire() => { permit.unwrap().forget(); Ok(()) }, _ = cancellation.cancelled() => Err(Error::Cancelled) }
    }
    pub async fn entered(&self) {
        bounded(self.entered.acquire()).await.unwrap().forget();
    }
    pub fn open(&self) {
        self.release.add_permits(1);
    }
}
/// Test-only immutable permissions. It never grants filesystem authority.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Security {
    pub permissions: u8,
    pub read_only: bool,
}
impl Security {
    pub fn new(permissions: u8) -> Arc<Self> {
        Arc::new(Self {
            permissions,
            read_only: false,
        })
    }
}
impl AccessSnapshot for Security {
    type Error = ();
    fn check_read(&self, _: &ResolvedRead) -> Result<(), ()> {
        panic!("test snapshot is not filesystem authority")
    }
    fn check_write(&self, _: &Path) -> Result<(), ()> {
        panic!("test snapshot is not filesystem authority")
    }
}
impl RestrictableAccessSnapshot for Security {
    fn restrict_with(&self, child: &Self) -> Self {
        Self {
            permissions: self.permissions & child.permissions,
            read_only: self.read_only || child.read_only,
        }
    }
}
pub fn profile(id: &str) -> TurnProfile {
    TurnProfile {
        id: id.into(),
        maximum_requests: 8,
        allowed_tools: None,
        disabled_tools: vec![],
    }
}
pub fn selection(
    provider: &Scripted,
    model: &str,
    mode: &str,
    permissions: u8,
) -> TurnSelection<Security> {
    TurnSelection {
        model: provider.selection(model),
        profile: profile(mode),
        security: Security::new(permissions),
    }
}
pub fn templates() -> Arc<PromptTemplateCatalog> {
    templates_with_content("")
}
pub fn templates_with_content(content: &str) -> Arc<PromptTemplateCatalog> {
    let env = [
        ("HOME", "/tmp/t48/home"),
        ("XDG_CONFIG_HOME", "/tmp/t48/config"),
        ("XDG_STATE_HOME", "/tmp/t48/state"),
        ("XDG_DATA_HOME", "/tmp/t48/data"),
        ("WORKDIR", "/tmp/t48/work"),
    ]
    .into_iter()
    .map(|(k, v)| (k.into(), v.into()))
    .collect();
    Arc::new(
        PromptTemplateCatalog::from_content(content, &env)
            .unwrap()
            .1,
    )
}
struct Names;
impl FilenameGenerator for Names {
    fn next_filename(&self) -> String {
        format!("{}-arse.dat", id())
    }
}
/// Every retained scope uses one real selected-root SQLite repository and its own JSONL projection.
pub struct Histories {
    pub path: PathBuf,
    pub repository: ConversationRepository,
}
impl Histories {
    pub fn new() -> Arc<Self> {
        let path = std::env::current_dir().unwrap().join(".tmp").join(format!(
            "children-{}-{}",
            std::process::id(),
            id()
        ));
        std::fs::create_dir_all(path.join("scratch/blobs")).unwrap();
        let database = SessionDatabase::open(
            SelectedSessionRoot::new(path.clone(), RootOrigin::Current).unwrap(),
        )
        .unwrap();
        Arc::new(Self {
            path,
            repository: ConversationRepository::new(EventRepository::new(database)),
        })
    }
    pub fn history(&self, identity: &AgentIdentity) -> AgentHistoryView {
        let root = self.repository.events().database().selected_root().clone();
        self.repository
            .bind_agent_history(
                identity.session.session_id.clone(),
                Arc::new(
                    AgentHistoryFile::new(
                        root,
                        self.path
                            .join(format!("{}.jsonl", identity.session.session_id)),
                        vec![identity.session.session_id.clone()],
                    )
                    .unwrap(),
                ),
            )
            .unwrap()
    }
    pub fn controller(&self, history: AgentHistoryView) -> ContextController {
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
        ContextController::new(compactor, history, blobs, templates)
    }
    pub async fn append(&self, history: &AgentHistoryView, text: &str) {
        let mut event = Event::new();
        event.id = id();
        event.agent_session_id = history.agent_session_id().into();
        history
            .append_conversation(event, ConversationMessage::text(MessageRole::User, text))
            .await
            .unwrap();
    }
}
impl HistoryPreparer for Histories {
    fn prepare<'a>(
        &'a self,
        identity: &'a AgentIdentity,
    ) -> BoxFuture<'a, ChildResult<AgentHistoryView>> {
        Box::pin(async move { Ok(self.history(identity)) })
    }
}
impl Drop for Histories {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}
#[derive(Default)]
pub struct Ids;
impl IdGenerator for Ids {
    fn agent_session(&self) -> ChildResult<String> {
        Ok(id())
    }
    fn message(&self) -> ChildResult<String> {
        Ok(id())
    }
}
pub struct Profiles;
impl ProfileResolver<Security> for Profiles {
    fn resolve_child(&self, name: &str) -> ChildResult<ChildProfile<Security>> {
        Ok(ChildProfile {
            profile: profile(name),
            security: Security::new(0xff),
            recursion_limit: if name == "once" { 1 } else { 8 },
        })
    }
}
#[derive(Default)]
pub struct Diagnostics(pub Mutex<Vec<ChildDiagnostic>>);
impl ChildDiagnostics for Diagnostics {
    fn record(&self, event: ChildDiagnostic) {
        self.0.lock().unwrap().push(event);
    }
}

#[derive(Default)]
pub struct Tools {
    pub captures: Mutex<Vec<TurnSelection<Security>>>,
}
impl TurnTools<Security> for Tools {
    type Snapshot = Vec<sessions::Tool>;
    fn materialize(&self) -> Result<Self::Snapshot, TurnError> {
        Ok(vec![])
    }
    fn permitted(
        &self,
        snapshot: &Self::Snapshot,
        selection: &TurnSelection<Security>,
    ) -> Result<Self::Snapshot, TurnError> {
        self.captures.lock().unwrap().push(selection.clone());
        Ok(snapshot.clone())
    }
    fn definitions<'a>(&self, snapshot: &'a Self::Snapshot) -> &'a [sessions::Tool] {
        snapshot
    }
    fn enabled_after_interruption(&self, snapshot: &Self::Snapshot) -> Self::Snapshot {
        snapshot.clone()
    }
    fn reconcile<'a>(
        &'a self,
        _: TurnResources<'a>,
        _: &'a TurnSelection<Security>,
        _: &'a Self::Snapshot,
        _: &'a dyn SelectionSource<Security>,
        _: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<(), TurnError>> {
        Box::pin(async { Ok(()) })
    }
}
struct Prompt(String);
impl SystemPrompt for Prompt {
    fn renew_epoch(&mut self) {}
    fn sample_epoch<'a>(&'a mut self, _: &'a dyn Cancellation) -> BoxFuture<'a, PromptResult<()>> {
        Box::pin(async { Ok(()) })
    }
    fn build(&mut self, _: &ResolvedModelSelection) -> PromptResult<String> {
        Ok(format!("offline task fixture scope:{}", self.0))
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
#[derive(Default)]
pub struct Output {
    pub text: Mutex<String>,
    pub closes: AtomicUsize,
    pub gate: Mutex<Option<Arc<Gate>>>,
    trace: Option<(Arc<Trace>, String)>,
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
            let gate = self.gate.lock().unwrap().clone();
            if let Some(gate) = gate {
                gate.wait(&NeverCancelled).await.unwrap();
            }
            self.closes.fetch_add(1, Ordering::SeqCst);
            if let Some((trace, name)) = &self.trace {
                trace.push(format!("session:{name}"));
            }
            Ok(())
        })
    }
}
struct Activity;
impl ProviderActivity for Activity {
    fn begin_provider_request(&self) {}
    fn observe_provider_event(&self, _: &sessions::Event) {}
    fn finish_provider_request(&self) {}
}
impl SessionActivity for Activity {
    fn change_state(&self, _: DrainState) {}
    fn begin_execution(&self) -> i64 {
        1
    }
    fn execution_duration(&self) -> Duration {
        Duration::ZERO
    }
    fn finish_execution(&self, _: i64, _: &Execution) {}
    fn turn_started(&self, _: &str) {}
    fn turn_finished(&self, _: &str, _: &str, _: Option<&str>) {}
    fn assistant_message(&self, _: &str) {}
}
pub type Backend = RuntimeBackend<Security, Tools>;
pub type Adapter = SessionAdapter<Security, Backend>;
pub struct Built {
    pub adapter: Arc<Adapter>,
    pub history: AgentHistoryView,
    pub tools: Arc<Tools>,
    pub output: Arc<Output>,
    pub selection: Arc<dyn SelectionSource<Security>>,
    pub publisher: Arc<AdmissionPublisher>,
    pub scheduler: Arc<Scheduler>,
    pub task_session: Arc<RecordingSession>,
}
#[derive(Default)]
pub struct Factory {
    pub build_calls: AtomicUsize,
    pub built: Mutex<BTreeMap<String, Arc<Built>>>,
    pub trace: Arc<Trace>,
    pub construction: Mutex<Option<Arc<Gate>>>,
    pub plans: Mutex<BTreeMap<String, ScopePlan>>,
    pub histories: Mutex<Option<Arc<Histories>>>,
    pub registry: Mutex<std::sync::Weak<AgentRegistry<Security>>>,
    pub parser: Arc<Parser>,
    pub router: Arc<Router>,
    pub task_diagnostics: Arc<TaskLog>,
    pub configuration: Mutex<Option<TaskConfiguration>>,
}
#[derive(Default)]
pub struct ScopePlan {
    pub failure: Option<ChildError>,
    pub duplicate_service: bool,
    pub publishers: Vec<Arc<dyn SnapshotPublisher>>,
    pub work: Vec<Arc<dyn WorkOwner<Error = ChildError>>>,
    pub resources: Vec<Arc<dyn ScopeResource<Error = ChildError>>>,
    pub final_resources: Vec<Arc<dyn ScopeResource<Error = ChildError>>>,
}
pub struct Marker(pub String);
pub trait BorrowedService: Send + Sync {
    fn name(&self) -> &str;
}
impl BorrowedService for Marker {
    fn name(&self) -> &str {
        &self.0
    }
}
impl Factory {
    pub fn get(&self, name: &str) -> Arc<Built> {
        self.built.lock().unwrap().get(name).unwrap().clone()
    }
    pub async fn build(
        &self,
        prepared: PreparedScope<Security>,
    ) -> ChildResult<ScopeParts<Security>> {
        self.build_calls.fetch_add(1, Ordering::SeqCst);
        let gate = self.construction.lock().unwrap().take();
        if let Some(gate) = gate {
            gate.wait(&NeverCancelled).await.unwrap();
        }
        let name = prepared.identity.session.name.clone();
        let plan = self.plans.lock().unwrap().remove(&name).unwrap_or_default();
        if let Some(error) = plan.failure {
            return Err(error);
        }
        let history = prepared.history.clone();
        let broker = Arc::new(EventBroker::new());
        let publisher = Arc::new(AdmissionPublisher {
            broker: broker.clone(),
            gate: Mutex::new(None),
            reject: std::sync::atomic::AtomicBool::new(false),
            reject_progress: std::sync::atomic::AtomicBool::new(false),
        });
        let templates = prepared.identity.templates.clone();
        let erased: Arc<parrot_runtime::agent_turn::engine::Publisher> = publisher.clone();
        let reminders = Arc::new(
            ExitReminders::new(history.clone(), erased, templates.clone())
                .await
                .unwrap(),
        );
        let tools = Arc::new(Tools::default());
        let output = Arc::new(Output {
            trace: Some((self.trace.clone(), name.clone())),
            ..Output::default()
        });
        let histories = self.histories.lock().unwrap().clone().unwrap();
        let mut ancestor_ids = Vec::new();
        let mut parent = prepared.identity.session.parent_session_id.clone();
        while !parent.is_empty() {
            let ancestor = self
                .built
                .lock()
                .unwrap()
                .values()
                .find(|built| built.adapter.identity().session.session_id == parent)
                .unwrap()
                .clone();
            ancestor_ids.push(parent);
            parent = ancestor
                .adapter
                .identity()
                .session
                .parent_session_id
                .clone();
        }
        let statistics = history
            .repository()
            .events()
            .get_runtime_statistics()
            .await
            .unwrap();
        let ancestors = ancestor_ids
            .into_iter()
            .map(|id| {
                let handle = statistics.get_agent_statistics(&id);
                (id, handle)
            })
            .collect();
        let backend = RuntimeBackend::new(RuntimeConfig {
            agent_limits: parrot_foundation::config::runtime::AgentLimitsConfig {
                disable_max_turns: false,
                ..Default::default()
            },
            identity: prepared.identity.session.clone(),
            history: history.clone(),
            broker,
            publisher: publisher.clone(),
            selection: prepared.selection.clone(),
            tools: tools.clone(),
            prompt: Box::new(Prompt(name.clone())),
            skills: Arc::new(Skills),
            context: histories.controller(history.clone()),
            status: Arc::new(Status),
            completions: vec![],
            templates,
            reminders,
            output: output.clone(),
            activity: Arc::new(Activity),
            parent_completion: prepared.parent_completion,
            ancestors,
            event_id: Arc::new(id),
        })
        .await
        .unwrap();
        let session = AgentSession::new(backend, prepared.lifetime.clone());
        let task_session = Arc::new(RecordingSession::new(Arc::new(TaskSessionAdapter::new(
            prepared.identity.clone(),
            session.clone(),
        ))));
        let scheduler = TaskScheduler::new(SchedulerConfig {
            owner_identity: prepared.identity.clone(),
            registry: self.registry.lock().unwrap().clone(),
            owner_session: task_session.clone(),
            history: history.clone(),
            parser: self.parser.clone(),
            router: self.router.clone(),
            configuration: self
                .configuration
                .lock()
                .unwrap()
                .clone()
                .unwrap_or_else(|| {
                    let mut configuration =
                        TaskConfiguration::defaults(prepared.identity.templates.clone());
                    configuration.fork_history_mode = ForkHistoryMode::Empty;
                    configuration
                }),
            output_blobs: Arc::new(
                parrot_storage::artifacts::output_blobs::ToolOutputBlobStore::for_session(
                    histories.repository.events().database().selected_root(),
                    "scratch/blobs",
                    Arc::new(Names),
                )
                .unwrap(),
            ),
            publisher: publisher.clone(),
            diagnostics: self.task_diagnostics.clone(),
            lifetime: prepared.lifetime,
        })?;
        let adapter = Arc::new(SessionAdapter::new(
            prepared.identity,
            prepared.selection.clone(),
            session,
        ));
        self.built.lock().unwrap().insert(
            name.clone(),
            Arc::new(Built {
                adapter: adapter.clone(),
                history,
                tools,
                output,
                selection: prepared.selection,
                publisher,
                scheduler: scheduler.clone(),
                task_session: task_session.clone(),
            }),
        );
        self.trace.push(format!("built:{name}"));
        let marker = Arc::new(Marker(name));
        let borrowed: Arc<dyn BorrowedService> = marker.clone();
        let erased_task_session: Arc<dyn TaskSession> = task_session;
        let mut services = vec![
            ServiceRegistration::new(marker.clone()),
            ServiceRegistration::new(borrowed),
            ServiceRegistration::new(scheduler.clone()),
            ServiceRegistration::new(erased_task_session),
        ];
        if plan.duplicate_service {
            services.push(ServiceRegistration::new(marker));
        }
        Ok(ScopeParts {
            session: adapter,
            services,
            publishers: plan.publishers,
            work: {
                let mut work = plan.work;
                work.push(scheduler.work_owner());
                work
            },
            resources: plan.resources,
            final_resources: plan.final_resources,
        })
    }
}
impl ScopeFactory<Security> for Factory {
    fn create(
        &self,
        prepared: PreparedScope<Security>,
    ) -> BoxFuture<'_, ChildResult<ScopeParts<Security>>> {
        Box::pin(self.build(prepared))
    }
}

#[derive(Clone, Default)]
pub struct Scripted {
    pub requests: Arc<Mutex<Vec<Request>>>,
    pub gates: Arc<Mutex<VecDeque<Arc<Gate>>>>,
    pub failures: Arc<Mutex<VecDeque<Error>>>,
    pub closes: Arc<AtomicUsize>,
    pub replies: Arc<Mutex<BTreeMap<String, VecDeque<Reply>>>>,
    pub changed: Arc<tokio::sync::Notify>,
}
impl Scripted {
    pub fn selection(&self, model: &str) -> ResolvedModelSelection {
        let mut first = Model::new("first", "scripted");
        first.context_window = 100000;
        let mut second = Model::new("second", "scripted");
        second.context_window = 100000;
        let registry = Arc::new(
            ProviderRegistry::new(
                vec![Arc::new(self.clone())],
                BTreeMap::from([("scripted".into(), vec![first, second])]),
            )
            .unwrap(),
        );
        let aliases = Arc::new(ModelAliasCatalog::new(registry.clone(), vec![]).unwrap());
        ModelRouter::new(
            registry,
            Arc::new(ModelRouting::new(aliases, "scripted/first")),
        )
        .resolve(&format!("scripted/{model}"))
        .unwrap()
    }
    fn call_impl(&self, request: Request) -> Result<Box<dyn EventStream>, Error> {
        let name = request
            .instructions
            .strip_prefix("offline task fixture scope:")
            .expect("real fixture prompt")
            .to_owned();
        let reply = self
            .replies
            .lock()
            .unwrap()
            .get_mut(&name)
            .and_then(VecDeque::pop_front)
            .unwrap_or_else(|| Reply::text("answer"));
        self.requests.lock().unwrap().push(request);
        self.changed.notify_waiters();
        Ok(Box::new(Stream {
            gate: reply
                .gate
                .or_else(|| self.gates.lock().unwrap().pop_front()),
            failure: self.failures.lock().unwrap().pop_front(),
            completed: false,
            text: reply.text,
            close_gate: reply.close_gate,
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
            self.closes.fetch_add(1, Ordering::SeqCst);
            Ok(())
        })
    }
}
struct Stream {
    gate: Option<Arc<Gate>>,
    failure: Option<Error>,
    completed: bool,
    text: String,
    close_gate: Option<Arc<Gate>>,
}
impl EventStream for Stream {
    fn next<'a>(
        &'a mut self,
        cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<Option<sessions::Event>, Error>> {
        Box::pin(async move {
            if self.completed {
                return Ok(None);
            }
            if let Some(gate) = self.gate.take() {
                gate.wait(cancellation).await?;
            }
            self.completed = true;
            if let Some(error) = self.failure.take() {
                return Err(error);
            }
            Ok(Some(sessions::Event::Completed(Completion {
                assistant_text: self.text.clone(),
                finish_reason: "stop".into(),
                input_tokens: 3,
                output_tokens: 2,
                ..Completion::default()
            })))
        })
    }
    fn close(&mut self) -> BoxFuture<'_, Result<(), Error>> {
        Box::pin(async move {
            if let Some(gate) = self.close_gate.take() {
                gate.wait(&NeverCancelled).await?;
            }
            Ok(())
        })
    }
}
pub async fn send(
    session: &dyn ScopedSession<Security>,
    text: &str,
) -> parrot_runtime::agent_turn::lifecycle::SendResult {
    bounded(
        session
            .send(
                vec![ConversationPart::text(text)],
                id(),
                protocol::Delivery::DELIVERY_STEER,
                IncomingActivity {
                    name: "test".into(),
                    cause: None,
                    source: protocol::InputSource::default(),
                },
                Arc::new(NeverCancelled),
            )
            .unwrap()
            .wait(&NeverCancelled),
    )
    .await
    .unwrap()
}
