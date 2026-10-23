//! The authoritative agent scope constructor. Policy and operating-system
//! capabilities are injected; this module never fabricates filesystem grants.
use super::{
    catalog::CatalogAdapter,
    tasks::ScopedTaskController,
    user::{AgentFactoryParts, AgentFactorySource, RuntimeSession, UserServices},
};
use crate::composition::{
    agents::{AgentSessionServices, ScopedAgentRuntime},
    context::{ContextControl, ContextSelection, ContextServices},
    processes::{ScopedProcessStatus, production_filename_generator},
    protocol,
    queues::ScopedQueueBackend,
};
use parrot_domain::{
    contracts::{
        async_ops::{
            BoxFuture, Cancellation, Clock, OperationError, OperationResult, OwnedWork,
            ScopeLifetime,
        },
        filesystem::{AuthorizedMutationService, AuthorizedReadService},
        runtime::SessionControl,
        security::RestrictableAccessSnapshot,
        status::ProcessStatusSource,
    },
    protocol::messages::parrot::{Delivery, Event, InputSource},
    values::{
        activity::AgentSessionActivity,
        config::ModelProfiles,
        context::{ContextSize, ContextSnapshot},
        identifiers::Identifier,
    },
};
use parrot_foundation::{
    config::{Configuration, runtime::AgentTaskForkHistoryMode},
    template_catalog::PromptTemplateCatalog,
};
use parrot_provider::{
    http_stream::HttpError,
    image_generation::{ImageGenerationRequest, ImageGenerationResult},
    routing::aliases::ModelRouter,
    sessions::{
        Error as ProviderError, ProviderSessions,
        diagnostics::{
            DiagnosticEvent as ProviderDiagnosticEvent,
            DiagnosticSeverity as ProviderDiagnosticSeverity, DiagnosticSink, LastRequestDumper,
            MonotonicDiagnosticClock,
        },
    },
};
use parrot_runtime::{
    active_work::{
        blockers::{
            ActiveWorkBlocker, ActiveWorkReminder, BlockerResult, ChildBlocker, ProcessBlocker,
            QueueBlocker, TaskBlocker,
        },
        completion::SessionCompletion,
        exit_reminders::{ExitReminders, ReminderError},
        runtime_tree::{RegistryRuntimeTreeSource, RuntimeTreeStatusProvider},
        status::{
            RuntimeStatus, StatusError, StatusObservation, StatusProvider, StatusQuery,
            StatusResult,
        },
    },
    agent_queues::{
        AgentQueues, QueueDiagnostic, QueueDiagnostics, QueueInventory, QueueSnapshotPublisher,
    },
    agent_turn::{
        activity::{LiveActivity, TurnDiagnostics},
        engine::{Publisher, RuntimeBackend, RuntimeConfig},
        lifecycle::{AgentSession, IncomingActivity, OperationHandle},
        provider_call::OutputWriter,
        services::{SelectionSource, SessionOutput, TurnError, TurnSelection, TurnStatus},
    },
    children::{
        AgentIdentity, ChildError, ChildResult, Operation, PreparedScope, Scope, ScopeFactory,
        ScopeParts, ServiceRegistration, SessionAdapter, SnapshotPublisher,
    },
    compaction::{
        CompactionPlanner, CompactionSettings, Compactor, ContextController, ContextError,
        ContextResult, ContextStatus,
    },
    interactions::{AgentAskIdentity, AgentAsks},
    plan_mode::{ModeResult, UserSessionModes},
    prompt_context::{
        AgentIdentityDescription, AgentPathEnvironment, CompositeSystemPromptProvider,
        SystemPromptProvider, TemplateSystemPromptProvider,
    },
    task_scheduler::{
        ForkHistoryMode, SchedulerConfig, TaskConfiguration, TaskDiagnostic, TaskDiagnostics,
        TaskSession,
    },
    tool_settlement::{
        SharedSecurity, ToolFactory, ToolInventory, ToolSettlement, ToolSettlementConfig,
    },
};
use parrot_storage::{
    artifacts::{compaction::CompactionGroupBlobStore, output_blobs::ToolOutputBlobStore},
    conversation::AgentHistoryView,
    diagnostics::{DiagnosticEvent, DiagnosticSeverity},
    events::NeverCancelled,
};
use parrot_tools::{
    agents::ResultTemplates,
    catalog::{
        self, AgentFactories, AgentServices, FileFactories, ProcessFactories, SessionBindings,
        SessionFactories, TaskQueueFactories, TaskQueueServices, WebFetchFactory,
    },
    files::WorkspacePaths,
    process::{
        ownership::{
            domain::CapturedProcessLauncher,
            execution::{Execution, ProcessLifetime},
            managed::{CompletionMessage, CompletionSink, ProcessDiagnostic, ProcessDiagnostics},
            monitor::{MonitorEvent, MonitorIdentity, MonitorReport, MonitorSink},
            owner::{ProcessAgent, ShellProcessOwner},
            protocol as process_protocol,
        },
        tools::{
            CommandRequest, ProcessResolver, ProcessToolServices, ProcessWorkspace, ResolvedProcess,
        },
    },
    session::{
        Compaction, Failure, FailureKind, ImageProvider, PermissionServices, SessionServices,
    },
    task_artifacts::TaskArtifactParserAdapter,
    tasks_queues::queues::{
        QueueDiagnostic as ToolQueueDiagnostic, QueueDiagnostics as ToolQueueDiagnostics,
    },
    web_fetch::WebFetcher,
};
use std::{
    collections::BTreeMap,
    fs::{File, OpenOptions},
    io::{self, Write},
    path::{Path, PathBuf},
    sync::{Arc, Mutex, OnceLock, Weak},
    time::Instant,
};

fn child(message: impl std::fmt::Display) -> ChildError {
    ChildError::failure("invalid_operation", message.to_string())
}
fn io_child(error: io::Error) -> ChildError {
    ChildError::failure("io", error.to_string())
}
fn failure(message: impl std::fmt::Display) -> Failure {
    Failure::new(FailureKind::InvalidOperation, message.to_string())
}
fn operation_failure(error: impl std::fmt::Display) -> OperationError<Failure> {
    OperationError::Failed(failure(error))
}
fn reminder_failure(
    error: ReminderError<parrot_runtime::event_broker::BrokerError>,
) -> OperationError<Failure> {
    match error {
        ReminderError::Publication(OperationError::Cancelled)
        | ReminderError::Template(OperationError::Cancelled) => OperationError::Cancelled,
        error => operation_failure(error),
    }
}

/// The selected workspace supplies its actual permission service. This constructor
/// does not supply or verify the separately open authorization backend.
pub type PermissionAuthorityFactory<S, P> =
    dyn Fn(Arc<UserServices<S>>, Arc<AgentIdentity>) -> protocol::Result<Arc<P>> + Send + Sync;

/// These are source constructor dependencies, not a prebuilt runtime or tool
/// factory. In particular the filesystem, permission and launcher values must
/// be the actual selected workspace's authorized services. The T36 production
/// supplier and T56 gzip/cookie implementation remain separately open.
pub struct AgentSourceConfig<S, F, L, P>
where
    S: RestrictableAccessSnapshot + 'static,
    L: CapturedProcessLauncher<Request = CommandRequest, Security = SharedSecurity<S>>,
    P: PermissionServices<Security = SharedSecurity<S>, Selection = ContextSelection<S>> + 'static,
{
    pub configuration: Arc<Configuration>,
    pub router: Arc<ModelRouter>,
    pub filesystem: F,
    pub launcher: Arc<L>,
    pub permission_authority: Arc<PermissionAuthorityFactory<S, P>>,
    pub web_fetcher: Arc<WebFetcher>,
    pub prompts: Vec<Arc<dyn SystemPromptProvider>>,
    pub clock: Arc<dyn Clock>,
    pub profiles: ModelProfiles,
}

pub struct AuthoritativeAgentSource<S, F, L, P>
where
    S: RestrictableAccessSnapshot + 'static,
    L: CapturedProcessLauncher<Request = CommandRequest, Security = SharedSecurity<S>>,
    P: PermissionServices<Security = SharedSecurity<S>, Selection = ContextSelection<S>> + 'static,
{
    config: Arc<AgentSourceConfig<S, F, L, P>>,
}
impl<S, F, L, P> AuthoritativeAgentSource<S, F, L, P>
where
    S: RestrictableAccessSnapshot + 'static,
    L: CapturedProcessLauncher<Request = CommandRequest, Security = SharedSecurity<S>>,
    P: PermissionServices<Security = SharedSecurity<S>, Selection = ContextSelection<S>> + 'static,
{
    pub fn new(config: AgentSourceConfig<S, F, L, P>) -> Self {
        Self {
            config: Arc::new(config),
        }
    }
}

/// The two-phase cell breaks only constructor cycles. Admission binds the real
/// catalog exactly once; observing an unbound cell is an error, never an empty
/// or successful substitute catalog.
struct BoundCatalog<S: RestrictableAccessSnapshot + 'static>(OnceLock<Arc<CatalogAdapter<S>>>);
impl<S: RestrictableAccessSnapshot + 'static> BoundCatalog<S> {
    fn new() -> Self {
        Self(OnceLock::new())
    }
    fn bind(&self, catalog: CatalogAdapter<S>) -> ChildResult<()> {
        self.0
            .set(Arc::new(catalog))
            .map_err(|_| child("the scoped catalog is already bound"))
    }
}
impl<S: RestrictableAccessSnapshot + 'static> ToolFactory<S> for BoundCatalog<S> {
    fn materialize(&self) -> Result<ToolInventory<S>, TurnError> {
        self.0
            .get()
            .ok_or_else(|| TurnError::Service("the scoped catalog is not bound".into()))?
            .materialize()
    }
}

struct BoundWork(OnceLock<Arc<dyn ActiveWorkBlocker>>);
impl BoundWork {
    fn new() -> Self {
        Self(OnceLock::new())
    }
}
impl ActiveWorkBlocker for BoundWork {
    fn observe(&self) -> BoxFuture<'_, Result<Option<BlockerResult>, TurnError>> {
        Box::pin(async move {
            let actual = self
                .0
                .get()
                .ok_or_else(|| TurnError::Service("scope work observers are not bound".into()))?;
            actual.observe().await
        })
    }
}

/// The source output file is a stable append-only `output-arse.dat`, distinct
/// from the randomly named spill files. Closing actually flushes and closes it.
pub struct AgentOutput {
    path: PathBuf,
    file: Arc<Mutex<Option<File>>>,
}
impl AgentOutput {
    pub fn open(blobs: &Path) -> io::Result<Self> {
        std::fs::create_dir_all(blobs)?;
        let path = blobs.join("output-arse.dat");
        let file = OpenOptions::new().create(true).append(true).open(&path)?;
        Ok(Self {
            path,
            file: Arc::new(Mutex::new(Some(file))),
        })
    }
    pub fn path(&self) -> &Path {
        &self.path
    }
}
impl OutputWriter for AgentOutput {
    fn append<'a>(&'a self, text: &'a str) -> BoxFuture<'a, io::Result<()>> {
        let file = self.file.clone();
        let bytes = text.as_bytes().to_vec();
        Box::pin(async move {
            tokio::task::spawn_blocking(move || {
                let mut state = file
                    .lock()
                    .map_err(|_| io::Error::other("output file lock poisoned"))?;
                let file = state
                    .as_mut()
                    .ok_or_else(|| io::Error::other("the agent output file is closed"))?;
                file.write_all(&bytes)?;
                file.flush()
            })
            .await
            .map_err(io::Error::other)?
        })
    }
}
impl SessionOutput for AgentOutput {
    fn close(&self) -> BoxFuture<'_, io::Result<()>> {
        let file = self.file.clone();
        Box::pin(async move {
            tokio::task::spawn_blocking(move || {
                if let Some(mut file) = file
                    .lock()
                    .map_err(|_| io::Error::other("output file lock poisoned"))?
                    .take()
                {
                    file.flush()?;
                }
                Ok(())
            })
            .await
            .map_err(io::Error::other)?
        })
    }
}

struct ScopeLog<S: RestrictableAccessSnapshot + 'static> {
    user: Arc<UserServices<S>>,
    session_id: String,
    clock: Arc<dyn Clock>,
    turn: Mutex<Option<(String, Instant)>>,
}
impl<S: RestrictableAccessSnapshot + 'static> ScopeLog<S> {
    fn event(
        &self,
        component: &str,
        operation: &str,
        severity: DiagnosticSeverity,
        outcome: Option<&str>,
        code: Option<&str>,
    ) {
        let mut event = DiagnosticEvent::new(component, operation, severity);
        event.agent_session_id = Some(self.session_id.clone());
        event.outcome = outcome.map(str::to_owned);
        event.error_code = code.map(str::to_owned);
        self.user.diagnostic(&event);
    }
}
impl<S: RestrictableAccessSnapshot + 'static> DiagnosticSink for ScopeLog<S> {
    fn write(&self, event: ProviderDiagnosticEvent) {
        let bounded = |value: u64| i64::try_from(value).unwrap_or(i64::MAX);
        self.user.diagnostic(&DiagnosticEvent {
            category: event.category,
            operation: event.operation,
            severity: match event.severity {
                ProviderDiagnosticSeverity::Information => DiagnosticSeverity::Information,
                ProviderDiagnosticSeverity::Warning => DiagnosticSeverity::Warning,
                ProviderDiagnosticSeverity::Error => DiagnosticSeverity::Error,
            },
            user_session_id: event.user_session_id,
            agent_session_id: event
                .agent_session_id
                .or_else(|| Some(self.session_id.clone())),
            correlation_id: event.correlation_id,
            request_id: event.request_id,
            transport: event.transport,
            provider_id: event.provider_id,
            model_id: event.model_id,
            tool_name: event.tool_name,
            outcome: event.outcome,
            duration_milliseconds: event.duration_milliseconds.map(bounded),
            count: event.count,
            request_bytes: event.request_bytes.map(bounded),
            response_bytes: event.response_bytes.map(bounded),
            close_status: event.close_status,
            error_code: event.error_code,
        });
    }
}
impl<S: RestrictableAccessSnapshot + 'static> TurnDiagnostics for ScopeLog<S> {
    fn started(&self, turn_id: &str) {
        *self.turn.lock().expect("turn diagnostics poisoned") =
            Some((turn_id.to_owned(), self.clock.now()));
        let mut event = DiagnosticEvent::new("turn", "started", DiagnosticSeverity::Information);
        event.agent_session_id = Some(self.session_id.clone());
        event.correlation_id = Some(turn_id.to_owned());
        self.user.diagnostic(&event);
    }
    fn finished(&self, turn_id: &str, outcome: &str, code: Option<&str>) {
        let started = {
            let mut turn = self.turn.lock().expect("turn diagnostics poisoned");
            match turn.as_ref() {
                Some((active_id, started)) if active_id == turn_id => {
                    let started = *started;
                    *turn = None;
                    started
                }
                _ => return,
            }
        };
        let severity = if outcome == "failed" {
            DiagnosticSeverity::Error
        } else {
            DiagnosticSeverity::Information
        };
        let mut event = DiagnosticEvent::new("turn", "finished", severity);
        event.agent_session_id = Some(self.session_id.clone());
        event.correlation_id = Some(turn_id.to_owned());
        event.outcome = Some(outcome.to_owned());
        event.error_code = code.map(str::to_owned);
        event.duration_milliseconds = Some(
            self.clock
                .now()
                .saturating_duration_since(started)
                .as_millis()
                .min(i64::MAX as u128) as i64,
        );
        self.user.diagnostic(&event);
    }
}
impl<S: RestrictableAccessSnapshot + 'static> ProcessDiagnostics for ScopeLog<S> {
    fn write(&self, observed: ProcessDiagnostic) {
        self.event(
            "process",
            observed.operation,
            DiagnosticSeverity::Information,
            Some(observed.outcome),
            observed.error_kind.map(|_| "io"),
        );
    }
}
impl<S: RestrictableAccessSnapshot + 'static> QueueDiagnostics for ScopeLog<S> {
    fn record(&self, observed: QueueDiagnostic) {
        self.event(
            "queue",
            observed.event,
            DiagnosticSeverity::Information,
            Some(observed.outcome),
            None,
        );
    }
}
impl<S: RestrictableAccessSnapshot + 'static> ToolQueueDiagnostics for ScopeLog<S> {
    fn record(&self, observed: ToolQueueDiagnostic) {
        self.event(
            "queue",
            observed.event,
            DiagnosticSeverity::Information,
            observed.outcome,
            observed.error_code,
        );
    }
}
impl<S: RestrictableAccessSnapshot + 'static> TaskDiagnostics for ScopeLog<S> {
    fn record(&self, observed: TaskDiagnostic) {
        self.event(
            "agent_task",
            observed.operation,
            DiagnosticSeverity::Information,
            Some(observed.outcome),
            observed.error_code,
        );
    }
}
impl<S: RestrictableAccessSnapshot + 'static> parrot_runtime::tool_settlement::ToolDiagnostics
    for ScopeLog<S>
{
    fn write(&self, event: &DiagnosticEvent) {
        self.user.diagnostic(event);
    }
}

/// Every backlink is weak. The same retained session Arc is registered beside
/// the SessionAdapter clone, so context control never targets a temporary Arc.
struct ScopeLink<S: RestrictableAccessSnapshot + 'static> {
    scope: OnceLock<Weak<Scope<S>>>,
    session: OnceLock<Weak<RuntimeSession<S>>>,
    registry: Weak<parrot_runtime::children::AgentRegistry<S>>,
}
impl<S: RestrictableAccessSnapshot + 'static> ScopeLink<S> {
    fn scope(&self) -> ChildResult<Arc<Scope<S>>> {
        self.scope
            .get()
            .and_then(Weak::upgrade)
            .ok_or_else(|| child("the agent scope is not bound"))
    }
    fn session(&self) -> ChildResult<Arc<RuntimeSession<S>>> {
        self.session
            .get()
            .and_then(Weak::upgrade)
            .ok_or_else(|| child("the agent session has closed"))
    }
}
impl<S: RestrictableAccessSnapshot + 'static> ProcessResolver for ScopeLink<S> {
    fn resolve(&self, address: &str) -> io::Result<ResolvedProcess> {
        let scope = self.scope().map_err(io::Error::other)?;
        let registry = self
            .registry
            .upgrade()
            .ok_or_else(|| io::Error::other("the user session is shutting down"))?;
        let (target, name) = parrot_runtime::children::AgentResolver::new(&scope, &registry)
            .resolve_resource(address)
            .map_err(io::Error::other)?;
        Ok(ResolvedProcess {
            owner: target
                .get_service::<ShellProcessOwner>()
                .map_err(io::Error::other)?,
            name,
        })
    }
}
impl<S: RestrictableAccessSnapshot + 'static> CompletionSink for ScopeLink<S> {
    fn send<'a>(
        &'a self,
        message: &'a CompletionMessage,
        cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, io::Result<()>> {
        Box::pin(async move {
            let scope = self.scope().map_err(io::Error::other)?;
            if message.owner_agent_session_id != scope.identity().session.session_id {
                return Err(io::Error::other(
                    "process completion belongs to another agent",
                ));
            }
            let session = self.session().map_err(io::Error::other)?;
            session
                .send(
                    vec![parrot_storage::events::ConversationPart::text(
                        message.text.clone(),
                    )],
                    message.message_id.clone(),
                    message.delivery,
                    IncomingActivity {
                        name: message.process_name.clone(),
                        cause: Some(message.activity_description.clone()),
                        source: InputSource::INPUT_SOURCE_UNSPECIFIED,
                    },
                    Arc::new(NeverCancelled),
                )
                .await
                .map_err(io::Error::other)?;
            cancellation.check().map_err(io::Error::other)?;
            Ok(())
        })
    }
}

struct Workspace {
    root: String,
}
impl ProcessWorkspace for Workspace {
    fn root(&self) -> &str {
        &self.root
    }
    fn resolves_to_root(&self, target: &str) -> bool {
        let candidate = Path::new(target);
        let candidate = if candidate.is_absolute() {
            candidate.to_path_buf()
        } else {
            Path::new(&self.root).join(candidate)
        };
        // This is the source's redundant-cd diagnostic only, not authorization.
        std::fs::canonicalize(candidate)
            .ok()
            .zip(std::fs::canonicalize(&self.root).ok())
            .is_some_and(|(a, b)| a == b)
    }
}

struct EnvironmentLauncher<L> {
    inner: Arc<L>,
    environment: BTreeMap<String, String>,
}
impl<S, L> CapturedProcessLauncher for EnvironmentLauncher<L>
where
    S: RestrictableAccessSnapshot + 'static,
    L: CapturedProcessLauncher<Request = CommandRequest, Security = SharedSecurity<S>>,
{
    type Request = CommandRequest;
    type Security = SharedSecurity<S>;
    fn launch(
        &self,
        mut request: CommandRequest,
        security: Self::Security,
        lifetime: Arc<ProcessLifetime>,
    ) -> BoxFuture<'_, io::Result<Arc<dyn Execution>>> {
        let mut environment = self.environment.clone();
        environment.extend(request.environment);
        request.environment = environment.into_iter().collect();
        self.inner.launch(request, security, lifetime)
    }
}

struct ProcessPublisher {
    owner: Arc<ShellProcessOwner>,
    broker: Arc<parrot_runtime::event_broker::EventBroker>,
}
impl SnapshotPublisher for ProcessPublisher {
    fn capture(&self) -> ChildResult<Vec<Event>> {
        process_protocol::convert(&self.owner.capture_inventory()).map_err(io_child)
    }
    fn start(&self) -> ChildResult<Operation<()>> {
        let subscription = self.owner.subscribe_inventory();
        let broker = self.broker.clone();
        Ok(Operation::start(async move {
            process_protocol::publish_snapshots(subscription, move |batch| {
                broker.publish_inventory(&batch).map_err(io::Error::other)
            })
            .await
            .map_err(io_child)
        }))
    }
}
struct ProcessWork(Arc<ShellProcessOwner>);
impl OwnedWork for ProcessWork {
    type Error = ChildError;
    fn settle(&self) -> BoxFuture<'_, ChildResult<()>> {
        Box::pin(async move { self.0.settle().await.map_err(io_child) })
    }
}
impl ScopeLifetime for ProcessWork {
    type Error = ChildError;
    fn close(&self) -> BoxFuture<'_, ChildResult<()>> {
        Box::pin(async move { self.0.dispose().await.map_err(io_child) })
    }
}
struct ScopedMonitor<S: RestrictableAccessSnapshot + 'static> {
    link: Arc<ScopeLink<S>>,
    blobs: Arc<ToolOutputBlobStore>,
    templates: Arc<PromptTemplateCatalog>,
    log: Arc<ScopeLog<S>>,
}
impl<S: RestrictableAccessSnapshot + 'static> MonitorSink for ScopedMonitor<S> {
    fn render_event(&self, identity: &MonitorIdentity, lines: &str) -> io::Result<String> {
        ResultTemplates::render(
            self.templates.as_ref(),
            "monitor.event",
            &[
                ("name", &identity.name),
                ("description", &identity.description),
                ("lines", lines),
            ],
        )
        .map_err(io::Error::other)
    }
    fn persist_event<'a>(
        &'a self,
        text: String,
        lifetime: &'a dyn Cancellation,
    ) -> BoxFuture<'a, io::Result<String>> {
        Box::pin(async move {
            self.blobs
                .persist(&text, lifetime)
                .map_err(io::Error::other)
        })
    }
    fn send_event<'a>(
        &'a self,
        event: MonitorEvent,
        lifetime: &'a dyn Cancellation,
    ) -> BoxFuture<'a, io::Result<()>> {
        Box::pin(async move {
            lifetime.check().map_err(io::Error::other)?;
            let session = self.link.session().map_err(io::Error::other)?;
            session
                .send(
                    vec![parrot_storage::events::ConversationPart::text(event.text)],
                    Identifier::message_id()?,
                    Delivery::DELIVERY_STEER,
                    IncomingActivity {
                        name: event.activity_name,
                        cause: Some(event.activity_description),
                        source: InputSource::INPUT_SOURCE_MONITOR,
                    },
                    Arc::new(NeverCancelled),
                )
                .await
                .map_err(io::Error::other)?;
            Ok(())
        })
    }
    fn observe_failed(&self, _: &MonitorIdentity, _: &io::Error) {
        self.log.event(
            "monitor",
            "observe",
            DiagnosticSeverity::Warning,
            Some("failed"),
            Some("io"),
        );
    }
    fn render_completion(
        &self,
        identity: &MonitorIdentity,
        report: &MonitorReport,
    ) -> io::Result<String> {
        ResultTemplates::render(
            self.templates.as_ref(),
            "monitor.ended",
            &[
                ("name", &identity.name),
                ("description", &identity.description),
                ("events", &report.events.to_string()),
                ("reason", report.reason.as_str()),
                ("exit_code", &report.exit_code),
                ("error", &report.error),
                ("timeout_ms", &report.timeout_ms),
            ],
        )
        .map_err(io::Error::other)
    }
}

/// Runtime tree observation upgrades authority only for the synchronous capture;
/// retaining a status provider must not keep its registry/scope cycle alive.
struct ScopedTree<S: RestrictableAccessSnapshot + 'static>(
    Weak<parrot_runtime::children::AgentRegistry<S>>,
);
impl<S: RestrictableAccessSnapshot + 'static>
    parrot_runtime::active_work::runtime_tree::RuntimeTreeSnapshotSource for ScopedTree<S>
{
    fn capture(
        &self,
    ) -> Result<parrot_runtime::active_work::runtime_tree::RuntimeTreeSnapshot, StatusError> {
        let registry = self
            .0
            .upgrade()
            .ok_or_else(|| StatusError::new("the user session is shutting down"))?;
        RegistryRuntimeTreeSource::new(
            registry,
            Arc::new(|scope| {
                scope
                    .get_service::<AgentSessionServices<S>>()
                    .map(|services| services.activity.capture().state)
                    .map_err(|error| StatusError::caused_by(error.to_string(), error))
            }),
        )
        .capture()
    }
}

struct TaskStatus {
    tasks: Arc<dyn parrot_runtime::active_work::blockers::TaskSnapshotSource>,
    templates: Arc<PromptTemplateCatalog>,
}
impl StatusProvider for TaskStatus {
    fn key(&self) -> &str {
        "runtime:agent-tasks"
    }
    fn observe<'a>(
        &'a self,
        query: &'a StatusQuery,
        cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, StatusResult<StatusObservation>> {
        Box::pin(async move {
            cancellation
                .check()
                .map_err(|_| OperationError::Cancelled)?;
            let tasks = self.tasks.snapshot().await;
            if tasks.is_empty() {
                return Ok(StatusObservation::default());
            }
            let nodes = tasks.iter().map(|task| serde_json::json!({"indent":"  ", "name":task.name, "description":task.description, "status":parrot_runtime::task_scheduler::status_name(task.state)})).collect::<Vec<_>>();
            let model = serde_json::json!({"section":"agent-tasks", "agents":[], "runs":[{"run_id":query.session_id,"display_name":if tasks.len()==1 { tasks[0].name.clone() } else { format!("{} tasks", tasks.len()) }, "owner_session_id":query.session_id,"revision":"","nodes":nodes}]});
            self.templates
                .render(
                    "status.runtime",
                    &model
                        .as_object()
                        .expect("status model is object")
                        .iter()
                        .map(|(key, value)| (key.clone(), value.clone()))
                        .collect::<Vec<_>>(),
                    cancellation,
                )
                .map(StatusObservation::available_text)
                .map_err(|error| OperationError::Failed(StatusError::new(error.to_string())))
        })
    }
}

struct RoutedSelection<S: RestrictableAccessSnapshot + 'static> {
    source: Arc<dyn SelectionSource<S>>,
    router: Arc<ModelRouter>,
}
impl<S: RestrictableAccessSnapshot + 'static> SelectionSource<S> for RoutedSelection<S> {
    fn capture(&self) -> Result<TurnSelection<S>, TurnError> {
        let mut selection = self.source.capture()?;
        selection.model = self
            .router
            .resolve(&selection.model.requested_selector.value)
            .map_err(|error| TurnError::Service(error.to_string()))?;
        Ok(selection)
    }
    fn update(&self, selection: TurnSelection<S>) -> Result<(), TurnError> {
        self.source.update(selection)
    }
}

pub struct ScopedTurnStatus<S: RestrictableAccessSnapshot + 'static> {
    pub runtime: Arc<RuntimeStatus>,
    identity: Arc<AgentIdentity>,
    modes: Arc<UserSessionModes<S>>,
    configuration: Arc<Configuration>,
}
impl<S: RestrictableAccessSnapshot + 'static> ScopedTurnStatus<S> {
    pub fn query(&self, selection: &TurnSelection<S>) -> StatusQuery {
        StatusQuery {
            session_id: self.identity.session.session_id.clone(),
            parent_session_id: self.identity.session.parent_session_id.clone(),
            parent_session_name: self.identity.parent_name.clone(),
            profile: selection.profile.id.clone(),
            requested_model: selection.model.requested_selector.value.clone(),
        }
    }
    pub fn profile_prompt(&self, selection: &TurnSelection<S>) -> ModeResult<String> {
        if self.modes.has_workflow(&selection.profile.id) {
            return self
                .modes
                .resolve_profile(&selection.profile.id)?
                .profile()
                .prompt();
        }
        Ok(self
            .configuration
            .runtime
            .profiles
            .get(&selection.profile.id)
            .map(|profile| profile.prompt.clone())
            .unwrap_or_default())
    }
}
impl<S: RestrictableAccessSnapshot + 'static> TurnStatus<S> for ScopedTurnStatus<S> {
    fn bind(&self, selection: &TurnSelection<S>) -> Arc<dyn ContextStatus> {
        let prompt = if self.modes.has_workflow(&selection.profile.id) {
            self.modes
                .resolve_profile(&selection.profile.id)
                .and_then(|mode| {
                    mode.prepare()?;
                    mode.profile().prompt()
                })
        } else {
            self.profile_prompt(selection)
        };
        Arc::new(ScopedModeStatus {
            runtime: self.runtime.clone(),
            query: self.query(selection),
            prompt,
        })
    }
}

struct ScopedModeStatus {
    runtime: Arc<RuntimeStatus>,
    query: StatusQuery,
    prompt: ModeResult<String>,
}
impl ScopedModeStatus {
    fn context(&self) -> ContextResult<Arc<dyn ContextStatus>> {
        let prompt = self
            .prompt
            .as_ref()
            .map_err(|error| ContextError::Operation(error.to_string()))?;
        Ok(self
            .runtime
            .bind_context(self.query.clone(), prompt.clone()))
    }
}
impl ContextStatus for ScopedModeStatus {
    fn observe<'a>(
        &'a self,
        context: &'a ContextSnapshot,
        cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, ContextResult<String>> {
        Box::pin(async move { self.context()?.observe(context, cancellation).await })
    }
    fn observe_fitted<'a>(
        &'a self,
        estimate: &'a (dyn Fn(&str) -> ContextResult<ContextSnapshot> + Send + Sync),
        cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, ContextResult<String>> {
        Box::pin(async move { self.context()?.observe_fitted(estimate, cancellation).await })
    }
}

struct ScopedImages<S: RestrictableAccessSnapshot + 'static>(std::marker::PhantomData<fn() -> S>);
impl<S: RestrictableAccessSnapshot + 'static> ImageProvider for ScopedImages<S> {
    type Selection = ContextSelection<S>;
    type Security = SharedSecurity<S>;
    fn generate<'a>(
        &'a self,
        selection: &'a Self::Selection,
        request: ImageGenerationRequest,
        cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, OperationResult<ImageGenerationResult, Failure>> {
        Box::pin(async move {
            selection
                .turn
                .model
                .canonical_model
                .provider
                .generate_image(request, cancellation)
                .await
                .map_err(|error| {
                    let kind = match &error {
                        ProviderError::Cancelled
                        | ProviderError::Cancellation(_)
                        | ProviderError::Http(
                            HttpError::Cancelled | HttpError::RequestCancelled(_),
                        ) => {
                            return OperationError::Cancelled;
                        }
                        ProviderError::Authentication(_) => FailureKind::Authentication,
                        ProviderError::Http(HttpError::Transport(_) | HttpError::Http(_)) => {
                            FailureKind::Network
                        }
                        _ => FailureKind::Provider,
                    };
                    OperationError::Failed(Failure::new(kind, error.to_string()))
                })
        })
    }
}

pub struct CheckpointRequest {
    pub title: String,
    pub assistant_sequence: i64,
    pub call_id: String,
}
pub struct ReminderRequest {
    pub title: String,
    pub description: String,
    pub cancellation: Arc<dyn Cancellation>,
}
pub struct ClearReminderRequest {
    pub title: String,
    pub cancellation: Arc<dyn Cancellation>,
}
struct ScopedSessionServices<S: RestrictableAccessSnapshot + 'static> {
    link: Arc<ScopeLink<S>>,
    history: AgentHistoryView,
    reminders: Arc<ExitReminders<Publisher>>,
    status: Arc<ScopedTurnStatus<S>>,
}
impl<S: RestrictableAccessSnapshot + 'static> SessionServices for ScopedSessionServices<S> {
    type Security = SharedSecurity<S>;
    type Selection = ContextSelection<S>;
    fn checkpoint<'a>(
        &'a self,
        title: &'a str,
        assistant_sequence: i64,
        call_id: &'a str,
    ) -> BoxFuture<'a, Result<(), Failure>> {
        Box::pin(async move {
            self.history
                .record_checkpoint(title.to_owned(), assistant_sequence, call_id.to_owned())
                .await
                .map(|_| ())
                .map_err(failure)
        })
    }
    fn set_reminder<'a>(
        &'a self,
        title: &'a str,
        description: &'a str,
        cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, OperationResult<(), Failure>> {
        // This service is retained only by the tool catalog. The actual drain
        // retains and joins its invocation, including admitted reminder commits.
        Box::pin(async move {
            self.reminders
                .set(title, description, cancellation)
                .await
                .map_err(reminder_failure)
        })
    }
    fn clear_reminder<'a>(
        &'a self,
        title: &'a str,
        cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, OperationResult<bool, Failure>> {
        Box::pin(async move {
            self.reminders
                .clear(title, cancellation)
                .await
                .map_err(reminder_failure)
        })
    }
    fn reminder_titles(&self) -> BoxFuture<'_, Vec<String>> {
        Box::pin(self.reminders.titles())
    }
    fn runtime_status<'a>(
        &'a self,
        selection: &'a ContextSelection<S>,
        cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, OperationResult<String, Failure>> {
        Box::pin(async move {
            let context = selection.estimate_context(cancellation).await?;
            self.status
                .runtime
                .observe_runtime(&self.status.query(&selection.turn), context, cancellation)
                .await
                .map_err(|error| match error {
                    OperationError::Cancelled => OperationError::Cancelled,
                    OperationError::Failed(error) => operation_failure(error),
                })
        })
    }
    fn statistics_status<'a>(
        &'a self,
        selection: &'a ContextSelection<S>,
        cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, OperationResult<String, Failure>> {
        Box::pin(async move {
            let session = self.link.session().map_err(operation_failure)?;
            self.status
                .runtime
                .observe_statistics(
                    &self.status.query(&selection.turn),
                    session.backend().usage().capture(),
                    cancellation,
                )
                .await
                .map_err(|error| match error {
                    OperationError::Cancelled => OperationError::Cancelled,
                    OperationError::Failed(error) => operation_failure(error),
                })
        })
    }
    fn compact<'a>(
        &'a self,
        _: &'a ContextSelection<S>,
        _: Option<ContextSize>,
        _: &'a dyn Cancellation,
    ) -> BoxFuture<'a, OperationResult<Compaction, Failure>> {
        // The public retained service is ContextServices, which supplies the
        // invocation's borrowed route. Re-entering here would self-queue.
        Box::pin(async {
            Err(operation_failure(
                "compaction must use its retained ContextServices route",
            ))
        })
    }
    fn estimate_after_tool_result(
        &self,
        _: &ContextSelection<S>,
        _: &str,
        _: &str,
    ) -> Result<ContextSnapshot, Failure> {
        Err(failure(
            "tool-result estimation must use its retained ContextServices route",
        ))
    }
}
impl<S: RestrictableAccessSnapshot + 'static> SessionControl for ScopedSessionServices<S> {
    type CheckpointRequest = CheckpointRequest;
    type CheckpointResult = ();
    type ReminderRequest = ReminderRequest;
    type ReminderResult = OperationHandle<()>;
    type ClearReminderRequest = ClearReminderRequest;
    type ClearReminderResult = OperationHandle<bool>;
    type CompactRequest = (Option<ContextSize>, Arc<dyn Cancellation>);
    type CompactResult = Compaction;
    type Status = parrot_domain::values::activity::AgentSessionActivitySnapshot;
    type Error = Failure;
    fn checkpoint<'a>(
        &'a self,
        request: CheckpointRequest,
        cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, OperationResult<(), Failure>> {
        Box::pin(async move {
            cancellation
                .check()
                .map_err(|_| OperationError::Cancelled)?;
            let operation = self
                .link
                .session()
                .map_err(operation_failure)?
                .owned_mutation(
                    move |backend, _| {
                        Box::pin(async move {
                            backend
                                .history()
                                .record_checkpoint(
                                    request.title,
                                    request.assistant_sequence,
                                    request.call_id,
                                )
                                .await
                                .map(|_| ())
                                .map_err(|error| {
                                    parrot_runtime::agent_turn::lifecycle::LifecycleError::Failed(
                                        error.to_string(),
                                    )
                                })
                        })
                    },
                    Arc::new(NeverCancelled),
                )
                .map_err(operation_failure)?;
            operation.join().await.map_err(operation_failure)
        })
    }
    fn compact<'a>(
        &'a self,
        request: Self::CompactRequest,
        cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, OperationResult<Compaction, Failure>> {
        Box::pin(async move {
            cancellation
                .check()
                .map_err(|_| OperationError::Cancelled)?;
            self.link
                .session()
                .map_err(operation_failure)?
                .compact_context(request.0, request.1)
                .await
                .map(|result| Compaction {
                    reduced: result.reduced,
                    context: result.context,
                })
                .map_err(|error| {
                    if error.is_cancelled() {
                        OperationError::Cancelled
                    } else {
                        operation_failure(error)
                    }
                })
        })
    }
    fn set_reminder(&self, request: ReminderRequest) -> Result<OperationHandle<()>, Failure> {
        self.link
            .session()
            .map_err(failure)?
            .set_exit_reminder(request.title, request.description, request.cancellation)
            .map_err(failure)
    }
    fn clear_reminder(
        &self,
        request: ClearReminderRequest,
    ) -> Result<OperationHandle<bool>, Failure> {
        self.link
            .session()
            .map_err(failure)?
            .clear_exit_reminder(request.title, request.cancellation)
            .map_err(failure)
    }
    fn status(&self) -> Result<Self::Status, Failure> {
        self.link
            .scope()
            .map_err(failure)?
            .get_service::<AgentSessionServices<S>>()
            .map(|services| services.activity.capture())
            .map_err(failure)
    }
}

struct ScopeClock(Arc<dyn Clock>);
impl Clock for ScopeClock {
    fn now(&self) -> std::time::Instant {
        self.0.now()
    }
    fn utc_now(&self) -> std::time::SystemTime {
        self.0.utc_now()
    }
    fn sleep_until<'a>(
        &'a self,
        deadline: std::time::Instant,
        cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<(), parrot_domain::contracts::async_ops::Cancelled>> {
        self.0.sleep_until(deadline, cancellation)
    }
}

/// The scheduler and retained tools belong to the session. Their forwarding
/// edge back to that same session must therefore be weak, not a second owner.
struct ScopedTaskSession<S: RestrictableAccessSnapshot + 'static> {
    identity: Arc<AgentIdentity>,
    link: Arc<ScopeLink<S>>,
}
impl<S: RestrictableAccessSnapshot + 'static> TaskSession for ScopedTaskSession<S> {
    fn identity(&self) -> &Arc<AgentIdentity> {
        &self.identity
    }
    fn enqueue(
        &self,
        parts: Vec<parrot_storage::events::ConversationPart>,
        message_id: String,
        cancellation: Arc<dyn Cancellation>,
    ) -> ChildResult<Operation<parrot_runtime::agent_turn::lifecycle::Execution>> {
        let execution =
            self.link
                .session()?
                .enqueue_execution(parts, message_id, None, None, cancellation)?;
        Ok(Operation::start(async move {
            execution.join().await.map_err(Into::into)
        }))
    }
    fn record(
        &self,
        parts: Vec<parrot_storage::events::ConversationPart>,
        message_id: String,
        delivery: Delivery,
    ) -> BoxFuture<'_, ChildResult<()>> {
        Box::pin(async move {
            self.link
                .session()?
                .record_handle(parts, message_id, delivery, Arc::new(NeverCancelled))?
                .join()
                .await?;
            Ok(())
        })
    }
    fn send(
        &self,
        parts: Vec<parrot_storage::events::ConversationPart>,
        message_id: String,
        delivery: Delivery,
        activity: IncomingActivity,
    ) -> BoxFuture<'_, ChildResult<()>> {
        Box::pin(async move {
            self.link
                .session()?
                .send_handle(
                    parts,
                    message_id,
                    delivery,
                    activity,
                    Arc::new(NeverCancelled),
                )?
                .join()
                .await?;
            Ok(())
        })
    }
}

type CatalogBinder<S> = dyn Fn(&Arc<Scope<S>>) -> ChildResult<()> + Send + Sync;
struct ScopeBindings<S: RestrictableAccessSnapshot + 'static> {
    link: Arc<ScopeLink<S>>,
    agents: Arc<ScopedAgentRuntime<S>>,
    queues: Arc<AgentQueues<S>>,
    child_work: Arc<BoundWork>,
    catalog: Box<CatalogBinder<S>>,
}
fn bind_scope<S: RestrictableAccessSnapshot + 'static>(scope: &Arc<Scope<S>>) -> ChildResult<()> {
    let bindings = scope.get_service::<ScopeBindings<S>>()?;
    bindings
        .link
        .scope
        .set(Arc::downgrade(scope))
        .map_err(|_| child("scope services are already bound"))?;
    bindings.agents.bind(scope)?;
    bindings.queues.bind_scope(scope)?;
    bindings.queues.initialize().map_err(ChildError::from)?;
    bindings
        .child_work
        .0
        .set(Arc::new(ChildBlocker {
            children: scope.children().clone(),
            owner_session_id: scope.identity().session.session_id.clone(),
        }))
        .map_err(|_| child("child work observer is already bound"))?;
    (bindings.catalog)(scope)
}

struct ScopedFactory<S, F, L, P>
where
    S: RestrictableAccessSnapshot + 'static,
    L: CapturedProcessLauncher<Request = CommandRequest, Security = SharedSecurity<S>>,
    P: PermissionServices<Security = SharedSecurity<S>, Selection = ContextSelection<S>> + 'static,
{
    config: Arc<AgentSourceConfig<S, F, L, P>>,
    user: Arc<UserServices<S>>,
}
impl<S, F, L, P> AgentFactorySource<S> for AuthoritativeAgentSource<S, F, L, P>
where
    S: RestrictableAccessSnapshot + 'static,
    F: AuthorizedReadService<Security = SharedSecurity<S>>
        + AuthorizedMutationService<Security = SharedSecurity<S>>
        + Clone
        + 'static,
    L: CapturedProcessLauncher<Request = CommandRequest, Security = SharedSecurity<S>> + 'static,
    P: PermissionServices<Security = SharedSecurity<S>, Selection = ContextSelection<S>> + 'static,
{
    fn create(&self, user: Arc<UserServices<S>>) -> protocol::Result<AgentFactoryParts<S>> {
        Ok(AgentFactoryParts {
            factory: Arc::new(ScopedFactory {
                config: self.config.clone(),
                user,
            }),
            bind: Arc::new(bind_scope::<S>),
        })
    }
}

impl<S, F, L, P> ScopeFactory<S> for ScopedFactory<S, F, L, P>
where
    S: RestrictableAccessSnapshot + 'static,
    F: AuthorizedReadService<Security = SharedSecurity<S>>
        + AuthorizedMutationService<Security = SharedSecurity<S>>
        + Clone
        + 'static,
    L: CapturedProcessLauncher<Request = CommandRequest, Security = SharedSecurity<S>> + 'static,
    P: PermissionServices<Security = SharedSecurity<S>, Selection = ContextSelection<S>> + 'static,
{
    fn create(&self, prepared: PreparedScope<S>) -> BoxFuture<'_, ChildResult<ScopeParts<S>>> {
        Box::pin(async move {
            let registry = self.user.registry()?;
            let identity = prepared.identity.clone();
            let scratch = self
                .user
                .resources
                .agent_scratch(
                    &identity
                        .name_path
                        .iter()
                        .map(String::as_str)
                        .collect::<Vec<_>>(),
                )
                .map_err(io_child)?;
            let environment = AgentPathEnvironment::from_resources(&self.user.resources, &scratch)
                .map_err(child)?;
            let root = self.user.lease.selected_root();
            let agent_relative = scratch.root().strip_prefix(root.path()).map_err(child)?;
            let filenames = production_filename_generator();
            let blobs = Arc::new(
                ToolOutputBlobStore::for_session(
                    root,
                    agent_relative.join("blobs"),
                    filenames.clone(),
                )
                .map_err(child)?,
            );
            let compaction_blobs =
                CompactionGroupBlobStore::for_session(root, agent_relative, filenames)
                    .map_err(child)?;
            let output = Arc::new(AgentOutput::open(&scratch.blob_directory()).map_err(io_child)?);
            let log = Arc::new(ScopeLog {
                user: self.user.clone(),
                session_id: identity.session.session_id.clone(),
                clock: self.config.clock.clone(),
                turn: Mutex::new(None),
            });
            let link = Arc::new(ScopeLink {
                scope: OnceLock::new(),
                session: OnceLock::new(),
                registry: Arc::downgrade(&registry),
            });
            let processes = Arc::new(
                ShellProcessOwner::new(
                    &identity.session.session_id,
                    Some(prepared.lifetime.clone()),
                    link.clone(),
                    log.clone(),
                )
                .map_err(io_child)?,
            );
            let process_status: Arc<dyn ProcessStatusSource> =
                Arc::new(ScopedProcessStatus::new(processes.clone()));
            let inventory = QueueInventory::new(
                parrot_storage::queue_files::Owner {
                    session_id: identity.session.session_id.clone(),
                    name: identity.session.name.clone(),
                    parent_session_id: identity.session.parent_session_id.clone(),
                    parent_name: identity.parent_name.clone(),
                },
                format!(
                    "queue-inventory-{}",
                    Identifier::event_id().map_err(io_child)?
                ),
            )
            .map_err(child)?;
            let queues = AgentQueues::new(
                identity.clone(),
                &self.user.resources,
                inventory,
                log.clone(),
            )
            .map_err(ChildError::from)?;
            let agents = Arc::new(ScopedAgentRuntime::new(
                Arc::downgrade(&registry),
                self.config.router.clone(),
                self.user.questions.clone(),
                self.user.templates.clone(),
            ));
            let asks = Arc::new(
                self.user
                    .asks
                    .register(
                        AgentAskIdentity {
                            session_id: identity.session.session_id.clone(),
                            name: identity.session.name.clone(),
                        },
                        agents.clone(),
                        agents.clone(),
                    )
                    .map_err(child)?,
            );
            let mut session: Option<Arc<RuntimeSession<S>>> = None;
            let mut controller: Option<Arc<ScopedTaskController<S, ContextSelection<S>>>> = None;
            let constructed = async {
                let permission =
                    (self.config.permission_authority)(self.user.clone(), identity.clone())
                        .map_err(child)?;
                let task_session: Arc<dyn TaskSession> = Arc::new(ScopedTaskSession {
                    identity: identity.clone(),
                    link: link.clone(),
                });
                let task_config = &self.config.configuration.runtime.agent_tasks;
                let tasks = ScopedTaskController::new(
                    SchedulerConfig {
                        owner_identity: identity.clone(),
                        registry: Arc::downgrade(&registry),
                        owner_session: task_session.clone(),
                        history: prepared.history.clone(),
                        parser: Arc::new(TaskArtifactParserAdapter),
                        router: self.config.router.clone(),
                        configuration: TaskConfiguration {
                            maximum_attempts: usize::try_from(task_config.maximum_attempts)
                                .map_err(child)?,
                            maximum_response_repairs: usize::try_from(
                                task_config.maximum_response_repairs,
                            )
                            .map_err(child)?,
                            fork_history_mode: match task_config.fork_history_mode {
                                AgentTaskForkHistoryMode::Dependency => ForkHistoryMode::Dependency,
                                AgentTaskForkHistoryMode::Parent => ForkHistoryMode::Parent,
                                AgentTaskForkHistoryMode::Empty => ForkHistoryMode::Empty,
                            },
                            templates: self.user.templates.clone(),
                        },
                        output_blobs: blobs.clone(),
                        publisher: self.user.broker.clone(),
                        diagnostics: log.clone(),
                        lifetime: prepared.lifetime.clone(),
                    },
                    |selection: &ContextSelection<S>| selection.turn.clone(),
                )?;
                controller = Some(tasks.clone());
                let child_work = Arc::new(BoundWork::new());
                let active_work = Arc::new(ActiveWorkReminder {
                    blockers: vec![
                        child_work.clone(),
                        Arc::new(ProcessBlocker {
                            processes: process_status.clone(),
                        }),
                        Arc::new(TaskBlocker {
                            tasks: tasks.scheduler().clone(),
                            templates: self.user.templates.clone(),
                        }),
                        Arc::new(QueueBlocker {
                            queues: queues.clone(),
                            templates: self.user.templates.clone(),
                        }),
                    ],
                    templates: self.user.templates.clone(),
                });
                let runtime_status = Arc::new(
                    RuntimeStatus::new(
                        self.user.templates.clone(),
                        self.config.clock.clone(),
                        vec![
                            Arc::new(RuntimeTreeStatusProvider {
                                source: Arc::new(ScopedTree(Arc::downgrade(&registry))),
                                templates: self.user.templates.clone(),
                            }),
                            Arc::new(TaskStatus {
                                tasks: tasks.scheduler().clone(),
                                templates: self.user.templates.clone(),
                            }),
                        ],
                    )
                    .map_err(child)?,
                );
                let status = Arc::new(ScopedTurnStatus {
                    runtime: runtime_status.clone(),
                    identity: identity.clone(),
                    modes: if identity.session.parent_session_id.is_empty() {
                        self.user.modes.clone()
                    } else {
                        Arc::new(self.user.modes.for_agent(scratch.clone()).map_err(child)?)
                    },
                    configuration: self.config.configuration.clone(),
                });
                let publisher: Arc<Publisher> = self.user.broker.clone();
                let reminders = Arc::new(
                    ExitReminders::new(
                        prepared.history.clone(),
                        publisher.clone(),
                        self.user.templates.clone(),
                    )
                    .await
                    .map_err(child)?,
                );
                let ancillary = Arc::new(ScopedSessionServices {
                    link: link.clone(),
                    history: prepared.history.clone(),
                    reminders: reminders.clone(),
                    status: status.clone(),
                });
                let context_control = Arc::new(ContextControl::new(ancillary.clone()));
                let context_services = Arc::new(ContextServices::<S, _, _>::new(
                    ancillary,
                    context_control.clone(),
                ));
                let activity = Arc::new(AgentSessionActivity::new(self.config.clock.clone()));
                let bound_catalog = Arc::new(BoundCatalog::new());
                let settlement = Arc::new(
                    ToolSettlement::new(ToolSettlementConfig {
                        factory: bound_catalog.clone(),
                        templates: self.user.templates.clone(),
                        output_blobs: blobs.clone(),
                        publisher: publisher.clone(),
                        activity: activity.clone(),
                        diagnostics: log.clone(),
                        image_bytes_per_cycle: self
                            .config
                            .configuration
                            .runtime
                            .request_limits
                            .image_bytes_per_tool_cycle,
                    })
                    .map_err(child)?,
                );
                let compact = &self.config.configuration.runtime.compaction;
                let settings = CompactionSettings {
                    trigger_percent: compact.trigger_percent,
                    target_percent: compact.target_percent,
                    maximum_input_tokens: compact.maximum_input_tokens,
                    summary_output_tokens: compact.summary_output_tokens,
                    summary_instructions: String::new(),
                    summary_prefix: String::new(),
                };
                let planner = Arc::new(CompactionPlanner::new(
                    settings.clone(),
                    self.config.profiles.clone(),
                ));
                let compactor = Compactor::new(
                    settings,
                    self.config.profiles.clone(),
                    self.user.templates.clone(),
                )
                .map_err(child)?;
                let skills = self
                    .user
                    .skills
                    .new_agent(prepared.selection.clone())
                    .await
                    .map_err(child)?;
                let mut prompts = self.config.prompts.clone();
                prompts.push(Arc::new(
                    environment.prompt_provider(self.user.templates.clone()),
                ));
                prompts.push(Arc::new(TemplateSystemPromptProvider::new(
                    "runtime:system-context:92-agent-scratch",
                    "context.agent-scratch",
                    vec![(
                        "path".into(),
                        scratch.scratch_path().to_string_lossy().into_owned().into(),
                    )],
                    self.user.templates.clone(),
                )));
                prompts.push(Arc::new(TemplateSystemPromptProvider::new(
                    "runtime:system-context:93-agent-history",
                    "context.agent-history",
                    vec![(
                        "path".into(),
                        scratch.history_path().to_string_lossy().into_owned().into(),
                    )],
                    self.user.templates.clone(),
                )));
                prompts.push(Arc::new(self.user.standing.clone()));
                prompts.push(skills.prompt);
                let prompt = CompositeSystemPromptProvider::new(
                    "runtime:agent-session-system-prompt",
                    prompts,
                )
                .map_err(child)?
                .materialize(&AgentIdentityDescription {
                    session_id: identity.session.session_id.clone(),
                    parent_session_id: identity.session.parent_session_id.clone(),
                    parent_session_name: identity.parent_name.clone(),
                    agent_name: identity.session.name.clone(),
                    depth: identity.session.depth,
                    scope_text: identity
                        .prompt_scope
                        .format(identity.session.depth)
                        .map_err(child)?,
                })
                .map_err(child)?;
                let completion = Arc::new(SessionCompletion {
                    asks: Some(asks.clone()),
                    active_work: active_work.clone(),
                    reminders: reminders.clone(),
                    history: prepared.history.clone(),
                    broker: self.user.broker.clone(),
                    publisher: publisher.clone(),
                });
                let mut ancestors = Vec::new();
                let mut parent = registry.snapshot_scopes().into_iter().find(|scope| {
                    scope.identity().session.session_id == identity.session.parent_session_id
                });
                while let Some(scope) = parent {
                    ancestors.push((
                        scope.identity().session.session_id.clone(),
                        self.user
                            .statistics
                            .get_agent_statistics(&scope.identity().session.session_id),
                    ));
                    parent = scope.parent_scope().parent();
                }
                let provider_sessions = ProviderSessions::with_diagnostics(
                    log.clone(),
                    Arc::new(MonotonicDiagnosticClock::default()),
                    identity.session.session_id.clone(),
                    Some(Arc::new(LastRequestDumper::new(
                        scratch.root().join("last_request.json"),
                        log.clone(),
                    ))),
                );
                let backend = RuntimeBackend::with_sessions(
                    RuntimeConfig {
                        agent_limits: self.config.configuration.runtime.agent_limits,
                        identity: identity.session.clone(),
                        history: prepared.history.clone(),
                        broker: self.user.broker.clone(),
                        publisher,
                        selection: Arc::new(RoutedSelection {
                            source: prepared.selection.clone(),
                            router: self.config.router.clone(),
                        }),
                        tools: settlement,
                        prompt,
                        skills: skills.skills,
                        context: ContextController::new(
                            compactor,
                            prepared.history.clone(),
                            compaction_blobs,
                            self.user.templates.clone(),
                        ),
                        status: status.clone(),
                        completions: vec![completion],
                        templates: self.user.templates.clone(),
                        reminders,
                        output: output.clone(),
                        activity: Arc::new(LiveActivity::new(activity.clone(), log.clone())),
                        parent_completion: prepared.parent_completion.clone(),
                        ancestors,
                        event_id: Arc::new(|| {
                            Identifier::event_id().expect("event identity entropy unavailable")
                        }),
                    },
                    provider_sessions,
                )
                .await
                .map_err(child)?;
                let actual_session =
                    Arc::new(AgentSession::new(backend, prepared.lifetime.clone()));
                session = Some(actual_session.clone());
                link.session
                    .set(Arc::downgrade(&actual_session))
                    .map_err(|_| child("agent session already bound"))?;
                context_control
                    .bind_session(Arc::downgrade(&actual_session))
                    .map_err(child)?;
                let process_services = Arc::new(ProcessToolServices {
                    owner: processes.clone(),
                    agent: ProcessAgent {
                        session_id: identity.session.session_id.clone(),
                        name: identity.session.name.clone(),
                        parent_session_id: identity.session.parent_session_id.clone(),
                        parent_name: identity.parent_name.clone(),
                        depth: identity.session.depth,
                    },
                    launcher: Arc::new(EnvironmentLauncher {
                        inner: self.config.launcher.clone(),
                        environment: environment.materialize().clone(),
                    }),
                    resolver: link.clone(),
                    workspace: Arc::new(Workspace {
                        root: self
                            .user
                            .resources
                            .workspace()
                            .launch_directory()
                            .to_string_lossy()
                            .into_owned(),
                    }),
                    read_only_prefixes: self
                        .config
                        .configuration
                        .runtime
                        .read_only_exec_command_prefixes
                        .iter()
                        .cloned()
                        .collect(),
                    templates: self.user.templates.clone(),
                    monitor_sink: Arc::new(ScopedMonitor {
                        link: link.clone(),
                        blobs: blobs.clone(),
                        templates: self.user.templates.clone(),
                        log: log.clone(),
                    }),
                });
                let config = self.config.clone();
                let user = self.user.clone();
                let catalog_cells = bound_catalog.clone();
                let catalog_agents = agents.clone();
                let catalog_tasks = tasks.clone();
                let catalog_context = context_services.clone();
                let catalog_environment = environment.clone();
                let catalog_planner = planner.clone();
                let catalog_log = log.clone();
                let binding = Arc::new(ScopeBindings {
                    link: link.clone(),
                    agents: agents.clone(),
                    queues: queues.clone(),
                    child_work,
                    catalog: Box::new(move |scope: &Arc<Scope<S>>| {
                        let definitions = catalog::configured_definitions(&config.configuration);
                        let files = FileFactories::new(
                            config.filesystem.clone(),
                            WorkspacePaths::new(catalog_environment.materialize()),
                            Arc::new(
                                parrot_storage::artifacts::ImageArtifactRepository::new(
                                    user.lease.database.clone(),
                                    user.lease.images.store().clone(),
                                )
                                .map_err(child)?,
                            ),
                            config
                                .configuration
                                .runtime
                                .request_limits
                                .image_bytes_per_read,
                            &definitions,
                        )
                        .map_err(child)?;
                        let process = ProcessFactories::new(process_services.clone(), &definitions)
                            .map_err(child)?;
                        let agents = AgentFactories::new(
                            AgentServices {
                                runtime: catalog_agents.clone(),
                                models: config.router.clone(),
                                questions: catalog_agents.clone(),
                                communication: catalog_agents.clone(),
                                agent_depth: scope.identity().session.depth,
                                templates: user.templates.clone(),
                                allow_parent: config.configuration.runtime.agent_send_to_parent,
                            },
                            &definitions,
                        )
                        .map_err(child)?;
                        let services = SessionFactories::new(
                            SessionBindings {
                                control: catalog_context.clone(),
                                images: Arc::new(ScopedImages::<S>(std::marker::PhantomData)),
                                permission: permission.clone(),
                                filesystem: Arc::new(config.filesystem.clone()),
                                templates: user.templates.clone(),
                                agent_depth: scope.identity().session.depth,
                            },
                            &definitions,
                        )
                        .map_err(child)?;
                        let tasks = TaskQueueFactories::new(
                            TaskQueueServices {
                                tasks: catalog_tasks.clone(),
                                filesystem: Arc::new(config.filesystem.clone()),
                                templates: user.templates.clone(),
                                queues: Arc::new(ScopedQueueBackend::new(
                                    scope.clone(),
                                    user.registry()?,
                                )?),
                                clock: Arc::new(ScopeClock(config.clock.clone())),
                                diagnostics: catalog_log.clone(),
                            },
                            &definitions,
                        )
                        .map_err(child)?;
                        let web = WebFetchFactory::new(config.web_fetcher.clone(), &definitions)
                            .map_err(child)?;
                        catalog_cells.bind(
                            CatalogAdapter::new(
                                catalog::Catalog::new(process, agents, files, services, tasks, web),
                                catalog_planner.clone(),
                                user.templates.clone(),
                            )
                            .map_err(child)?,
                        )
                    }),
                });
                Ok(ScopeParts {
                    agent_state: prepared.agent_state.clone(),
                    session: Arc::new(SessionAdapter::new(
                        identity.clone(),
                        prepared.selection.clone(),
                        actual_session.as_ref().clone(),
                    )),
                    services: vec![
                        ServiceRegistration::new(actual_session.clone()),
                        ServiceRegistration::new(Arc::new(AgentSessionServices {
                            session: actual_session,
                            activity,
                            output_path: output.path().to_string_lossy().into_owned(),
                            active_work,
                        })),
                        ServiceRegistration::new::<dyn TaskSession>(task_session),
                        ServiceRegistration::new(tasks.scheduler().clone()),
                        ServiceRegistration::new(tasks.clone()),
                        ServiceRegistration::new(queues.clone()),
                        ServiceRegistration::new(processes.clone()),
                        ServiceRegistration::new::<dyn ProcessStatusSource>(process_status),
                        ServiceRegistration::new(agents.clone()),
                        ServiceRegistration::new(asks.clone()),
                        ServiceRegistration::new(runtime_status),
                        ServiceRegistration::new(status),
                        ServiceRegistration::new(context_control),
                        ServiceRegistration::new(context_services),
                        ServiceRegistration::new(binding),
                    ],
                    publishers: vec![
                        Arc::new(QueueSnapshotPublisher::new(
                            queues.clone(),
                            self.user.broker.clone(),
                            self.user.root_identity.session.session_id.clone(),
                        )),
                        Arc::new(ProcessPublisher {
                            owner: processes.clone(),
                            broker: self.user.broker.clone(),
                        }),
                    ],
                    work: vec![
                        tasks.scheduler().work_owner(),
                        Arc::new(ProcessWork(processes.clone())),
                    ],
                    resources: vec![
                        Arc::new(ProcessWork(processes.clone())),
                        Arc::new(AsksClose(asks.clone())),
                    ],
                    final_resources: vec![queues.clone()],
                })
            }
            .await;
            match constructed {
                Ok(parts) => Ok(parts),
                Err(mut failure) => {
                    // Collection stages retain their first failure; later source
                    // finally stages still run and replace earlier failures.
                    let task_failure = if let Some(tasks) = controller {
                        tasks.scheduler().settle().join().await.err()
                    } else {
                        None
                    };
                    let process_failure = processes.settle().await.err().map(io_child);
                    if let Some(error) = task_failure.or(process_failure) {
                        failure = error;
                    }
                    if let Some(session) = session {
                        if let Err(error) = session.close().await {
                            failure = error.into();
                        }
                    } else if let Err(error) = output.close().await {
                        failure = io_child(error);
                    }
                    if let Err(error) = processes.dispose().await {
                        failure = io_child(error);
                    }
                    asks.close();
                    if let Err(error) = queues.dispose() {
                        failure = error.into();
                    }
                    Err(failure)
                }
            }
        })
    }
}

struct AsksClose(Arc<AgentAsks>);
impl ScopeLifetime for AsksClose {
    type Error = ChildError;
    fn close(&self) -> BoxFuture<'_, ChildResult<()>> {
        self.0.close();
        Box::pin(async { Ok(()) })
    }
}
