//! User-session ownership over the retained storage lease and runtime registry.
use super::super::protocol::{
    self, HostedSession, HostedSessionFactory, RpcCode, RpcError, SessionEventStream,
};
use parrot_domain::{
    contracts::{
        async_ops::{BoxFuture, Cancellation, Clock, OperationError},
        events::EventSubscription,
        security::{RestrictableAccessSnapshot, SecurityProvider},
        storage::Committed,
    },
    protocol::messages::parrot as wire,
    values::{
        accounting::RetainedAgentBudget, context::ContextSize, identifiers::Identifier,
        lineage::AgentDirectoryLineage, project_workspace::ProjectWorkspace,
    },
};
use parrot_foundation::{paths::UserSessionResources, template_catalog::PromptTemplateCatalog};
use parrot_provider::routing::{
    aliases::{ModelRouter, ResolvedModelSelection},
    configuration::{CoordinatorError, Result as ConfigurationResult},
};
use parrot_runtime::{
    agent_turn::{
        engine::RuntimeBackend,
        lifecycle::{AgentSession, DrainBackend, IncomingActivity, Signal},
        services::{SelectionSource, TurnSkills},
    },
    children::{
        AgentIdentity, AgentRegistry, ChildDiagnostic, ChildDiagnostics, ChildResult,
        CompletionDelivery, HistoryPreparer, IdGenerator, PolicyLineage, PreparedScope,
        RegistryConfig, Scope, ScopeConfig, ScopeFactory, SelectionCell,
    },
    event_broker::EventBroker,
    interactions::{
        InteractionDiagnostics, InteractionError, PermissionBroker, PermissionTargetDescription,
        PermissionTargetDescriptionKind, QuestionBroker, TokioClock, map_pending_permission,
    },
    plan_mode::{
        ModeCompletionOutcome, ModeRegistry, ModeWorkflowFactory, RetainedModePolicyMaterializer,
        SessionMode, UserSessionModes,
    },
    prompt_context::{
        AgentSkills, PromptResult, SkillCatalog, SkillMetadata, SkillSnapshot, StandingInstruction,
    },
    tool_settlement::ToolSettlement,
};
use parrot_storage::{
    conversation::{AgentHistoryView, ConversationRepository, HistoryProjection},
    diagnostics::{DiagnosticEvent, DiagnosticSeverity, statistics::UserSessionStatistics},
    events::{Admission, ConversationPart, NeverCancelled},
    history_files::AgentHistoryFile,
    ownership::{
        OpenedSession, SessionInitialization, SessionModelSelection, SessionResourceLease,
        SessionStoreError, StoredUserSession, StoredUserSessionFactory,
    },
};
use parrot_tools::session::{WriteTarget, WriteTargetKind};
use std::{
    collections::HashMap,
    path::Path,
    sync::{Arc, Mutex, OnceLock, Weak},
    time::Duration,
};
use tokio::sync::{Mutex as AsyncMutex, oneshot};

pub type RuntimeSession<S> = AgentSession<RuntimeBackend<S, ToolSettlement<S>>>;
pub type ScopeBinder<S> = Arc<dyn Fn(&Arc<Scope<S>>) -> ChildResult<()> + Send + Sync>;
/// Observes saved events loaded during session initialization, keyed by user session.
pub type SessionEventLoadProgress = Arc<dyn Fn(&str, usize) + Send + Sync>;

pub struct AgentFactoryParts<S: RestrictableAccessSnapshot + 'static> {
    pub factory: Arc<dyn ScopeFactory<S>>,
    pub bind: ScopeBinder<S>,
}
pub trait AgentFactorySource<S: RestrictableAccessSnapshot + 'static>: Send + Sync {
    fn create(&self, services: Arc<UserServices<S>>) -> protocol::Result<AgentFactoryParts<S>>;
}

/// A user catalog is shared, while each agent gets the real turn-local AgentSkills.
pub trait UserSkills<S: RestrictableAccessSnapshot + 'static>: Send + Sync {
    fn refresh<'a>(
        &'a self,
        security: &'a S,
        cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, protocol::Result<SkillSnapshot>>;
    fn configure<'a>(
        &'a self,
        security: &'a S,
        path: &'a str,
        enabled: bool,
        cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, protocol::Result<SkillMetadata>>;
    fn new_agent(
        &self,
        selection: Arc<dyn SelectionSource<S>>,
    ) -> BoxFuture<'_, PromptResult<AgentSkillBindings>>;
}
pub struct AgentSkillBindings {
    pub skills: Arc<dyn TurnSkills>,
    pub prompt: Arc<dyn parrot_runtime::prompt_context::SystemPromptProvider>,
}

pub struct UserServices<S: RestrictableAccessSnapshot + 'static> {
    pub id: String,
    pub lease: Arc<SessionResourceLease>,
    pub resources: Arc<UserSessionResources<ProjectWorkspace>>,
    pub broker: Arc<EventBroker>,
    pub templates: Arc<PromptTemplateCatalog>,
    pub questions: Arc<QuestionBroker>,
    pub permissions: Arc<PermissionBroker<WriteTarget>>,
    pub skills: Arc<dyn UserSkills<S>>,
    pub lifetime: Arc<Signal>,
    pub standing: StandingInstruction,
    pub statistics: Arc<UserSessionStatistics>,
    pub root_identity: Arc<AgentIdentity>,
    pub modes: Arc<UserSessionModes<S>>,
    registry: OnceLock<Weak<AgentRegistry<S>>>,
}
impl<S: RestrictableAccessSnapshot + 'static> UserServices<S> {
    pub fn registry(&self) -> ChildResult<Arc<AgentRegistry<S>>> {
        self.registry.get().and_then(Weak::upgrade).ok_or_else(|| {
            parrot_runtime::children::ChildError::registry("the user session is shutting down")
        })
    }
    pub fn diagnostic(&self, event: &DiagnosticEvent) {
        self.lease.write_diagnostic(event);
    }
}

struct SessionChildDiagnostics {
    configured: Arc<dyn ChildDiagnostics>,
    lease: Weak<SessionResourceLease>,
}
impl ChildDiagnostics for SessionChildDiagnostics {
    fn record(&self, event: ChildDiagnostic) {
        if let Some(lease) = self.lease.upgrade() {
            let severity = if event.error_code.is_some() {
                DiagnosticSeverity::Error
            } else {
                DiagnosticSeverity::Information
            };
            let mut diagnostic = DiagnosticEvent::new("agent", event.event, severity);
            diagnostic.agent_session_id = Some(event.session_id.clone());
            diagnostic.correlation_id =
                (!event.correlation_id.is_empty()).then(|| event.correlation_id.clone());
            diagnostic.duration_milliseconds = event
                .duration_milliseconds
                .map(|duration| i64::try_from(duration).unwrap_or(i64::MAX));
            diagnostic.outcome = event.outcome.map(str::to_owned);
            diagnostic.error_code = event.error_code.map(str::to_owned);
            lease.write_diagnostic(&diagnostic);
        }
        self.configured.record(event);
    }
}

pub type WorkspaceResolver = Arc<dyn Fn(&Path) -> protocol::Result<ProjectWorkspace> + Send + Sync>;

/// Ordinary discovery for `UserSessionFactoryConfig::workspace`; candidates are not grants.
/// Discovery failures retain the protocol's invalid-workspace argument mapping.
pub fn ordinary_workspace_resolver() -> WorkspaceResolver {
    Arc::new(|directory| parrot_foundation::paths::discover_workspace(directory).map_err(invalid))
}

pub type UserSkillsFactory<S> =
    Arc<dyn Fn(&ProjectWorkspace) -> protocol::Result<Arc<dyn UserSkills<S>>> + Send + Sync>;
/// The injected adapter must append and publish the SAME permission event, in order.
/// Its durable event implementation belongs to the interaction owner, not this facade.
pub type UserPermissionFactory = Arc<
    dyn Fn(
            Arc<SessionResourceLease>,
            Arc<EventBroker>,
            bool,
        ) -> protocol::Result<Arc<PermissionBroker<WriteTarget>>>
        + Send
        + Sync,
>;

pub struct DurablePermissionEvents {
    lease: Arc<SessionResourceLease>,
    broker: Arc<EventBroker>,
}
impl parrot_runtime::interactions::PermissionEvents<WriteTarget> for DurablePermissionEvents {
    fn append<'a>(
        &'a self,
        pending: &'a parrot_runtime::interactions::PendingPermission<WriteTarget>,
    ) -> BoxFuture<'a, Result<Committed<wire::Event>, InteractionError>> {
        Box::pin(async move {
            let mut event = wire::Event::new();
            event.id = Identifier::event_id().map_err(|error| {
                InteractionError::new(
                    parrot_runtime::interactions::InteractionErrorKind::Identifier,
                    error.to_string(),
                )
            })?;
            event.agent_session_id = pending.agent_session_id.clone();
            event.set_permission_pending(map_pending_permission(pending, |target| {
                PermissionTargetDescription {
                    path: target.path.to_string_lossy().into_owned(),
                    kind: match target.kind {
                        WriteTargetKind::File => PermissionTargetDescriptionKind::File,
                        WriteTargetKind::Directory => PermissionTargetDescriptionKind::Directory,
                    },
                }
            }));
            let stored = self
                .lease
                .events
                .append(event, None)
                .await
                .map_err(|error| {
                    InteractionError::new(
                        parrot_runtime::interactions::InteractionErrorKind::Permission,
                        error.to_string(),
                    )
                })?;
            Ok(Committed::after_commit(stored.stored.event))
        })
    }
    fn publish<'a>(
        &'a self,
        event: &'a Committed<wire::Event>,
    ) -> BoxFuture<'a, Result<(), InteractionError>> {
        Box::pin(async move {
            self.broker.publish_retained(event.value());
            Ok(())
        })
    }
}

pub fn durable_permission_factory(
    timeout: Option<Duration>,
    diagnostics: Arc<dyn InteractionDiagnostics>,
) -> UserPermissionFactory {
    Arc::new(move |lease, broker, interactive| {
        PermissionBroker::new(
            interactive,
            timeout,
            diagnostics.clone(),
            Arc::new(DurablePermissionEvents { lease, broker }),
        )
        .map(Arc::new)
        .map_err(invalid)
    })
}

pub struct UserSessionFactoryConfig<S: RestrictableAccessSnapshot + 'static> {
    pub agent_limits: parrot_foundation::config::runtime::AgentLimitsConfig,
    pub router: Arc<ModelRouter>,
    pub modes: Arc<ModeRegistry<S>>,
    pub workflows: Vec<(String, Arc<dyn ModeWorkflowFactory<S>>)>,
    pub materializer: Arc<dyn RetainedModePolicyMaterializer<S>>,
    pub source: Arc<dyn AgentFactorySource<S>>,
    pub profiles: Arc<dyn parrot_runtime::children::ProfileResolver<S>>,
    pub ids: Arc<dyn IdGenerator>,
    pub diagnostics: Arc<dyn ChildDiagnostics>,
    pub interaction_diagnostics: Arc<dyn InteractionDiagnostics>,
    pub templates: Arc<PromptTemplateCatalog>,
    pub workspace: WorkspaceResolver,
    pub launch_directory: std::path::PathBuf,
    pub skills: UserSkillsFactory<S>,
    pub permissions: UserPermissionFactory,
    pub user_input_timeout: Option<Duration>,
}

pub struct UserSessionFactory<S: RestrictableAccessSnapshot + 'static> {
    config: Arc<UserSessionFactoryConfig<S>>,
    question_clock: Arc<dyn Clock>,
    event_load_progress: Option<SessionEventLoadProgress>,
    pending: Mutex<HashMap<String, Weak<UserSessionCore<S>>>>,
}
impl<S: RestrictableAccessSnapshot + 'static> UserSessionFactory<S> {
    pub fn new(config: UserSessionFactoryConfig<S>) -> Self {
        Self::with_question_clock(config, Arc::new(TokioClock))
    }

    /// Uses the supplied clock for real session-owned question deadlines and pending projections.
    pub fn with_question_clock(config: UserSessionFactoryConfig<S>, clock: Arc<dyn Clock>) -> Self {
        Self {
            config: Arc::new(config),
            question_clock: clock,
            event_load_progress: None,
            pending: Mutex::new(HashMap::new()),
        }
    }

    pub fn with_event_load_progress(mut self, progress: Option<SessionEventLoadProgress>) -> Self {
        self.event_load_progress = progress;
        self
    }
}

struct Histories<S: RestrictableAccessSnapshot + 'static> {
    services: Arc<UserServices<S>>,
    repository: ConversationRepository,
}
impl<S: RestrictableAccessSnapshot + 'static> Histories<S> {
    async fn lineage(&self) -> ChildResult<AgentDirectoryLineage> {
        let records = self
            .services
            .lease
            .events
            .agent_lineage()
            .await
            .map_err(|error| child(error.to_string()))?;
        Ok(AgentDirectoryLineage::resolve(
            &records
                .into_iter()
                .map(|record| {
                    parrot_domain::values::lineage::AgentLineageRecord::new(
                        record.agent_session_id,
                        record.parent_agent_session_id,
                        record.name,
                    )
                })
                .collect::<Vec<_>>(),
        ))
    }

    async fn refresh_all(&self) -> ChildResult<()> {
        let lineage = self.lineage().await?;
        for directory in lineage.directories() {
            let path: Vec<_> = directory.name_path.iter().map(String::as_str).collect();
            let scratch = self
                .services
                .resources
                .agent_scratch(&path)
                .map_err(child_io)?;
            let file = AgentHistoryFile::new(
                self.services.lease.selected_root().clone(),
                scratch.history_path(),
                directory.session_ids.clone(),
            )
            .map_err(|error| child(error.to_string()))?;
            file.refresh(&self.repository, directory.session_ids.last().unwrap())
                .await
                .map_err(|error| child(error.to_string()))?;
        }
        for agent in self
            .repository
            .agent_history_session_ids()
            .await
            .map_err(|error| child(error.to_string()))?
        {
            if !lineage.contains(&agent) {
                let mut diagnostic = DiagnosticEvent::new(
                    "session",
                    "agent_history_unresolved",
                    DiagnosticSeverity::Warning,
                );
                diagnostic.agent_session_id = Some(agent);
                self.services.diagnostic(&diagnostic);
            }
        }
        Ok(())
    }
}
impl<S: RestrictableAccessSnapshot + 'static> HistoryPreparer for Histories<S> {
    fn prepare<'a>(
        &'a self,
        identity: &'a AgentIdentity,
    ) -> BoxFuture<'a, ChildResult<AgentHistoryView>> {
        Box::pin(async move {
            let path: Vec<_> = identity.name_path.iter().map(String::as_str).collect();
            let scratch = self
                .services
                .resources
                .agent_scratch(&path)
                .map_err(child_io)?;
            let lineage = self.lineage().await?;
            let occupants = lineage.occupants_of(&identity.name_path, &identity.session.session_id);
            let file = AgentHistoryFile::new(
                self.services.lease.selected_root().clone(),
                scratch.history_path(),
                occupants,
            )
            .map_err(|error| child(error.to_string()))?;
            self.repository
                .bind_agent_history(identity.session.session_id.clone(), Arc::new(file))
                .map_err(Into::into)
        })
    }
}

struct Main<S: RestrictableAccessSnapshot + 'static> {
    model: ResolvedModelSelection,
    mode: SessionMode<S>,
    scope: Arc<Scope<S>>,
    session: Arc<RuntimeSession<S>>,
}
type MutationJoin = Arc<AsyncMutex<Option<tokio::task::JoinHandle<()>>>>;
#[derive(Default)]
struct MutationState {
    closing: bool,
    failure: Option<RpcError>,
    tasks: Vec<MutationJoin>,
}
struct OwnedMutations {
    state: Mutex<MutationState>,
    runtime: tokio::runtime::Handle,
}
impl OwnedMutations {
    fn new(runtime: tokio::runtime::Handle) -> Self {
        Self {
            state: Mutex::new(MutationState::default()),
            runtime,
        }
    }
    fn spawn<F>(&self, work: impl FnOnce() -> F + Send + 'static) -> protocol::Result<()>
    where
        F: std::future::Future<Output = ()> + Send + 'static,
    {
        let mut state = self.state.lock().expect("user mutations poisoned");
        if state.closing {
            return Err(RpcError::new(
                RpcCode::Unavailable,
                "the user session is shutting down",
            ));
        }
        state.tasks.push(Arc::new(AsyncMutex::new(Some(
            self.runtime.spawn(async move { work().await }),
        ))));
        Ok(())
    }
    fn observe<T, F>(
        &self,
        work: impl FnOnce() -> F + Send + 'static,
    ) -> BoxFuture<'static, protocol::Result<T>>
    where
        T: Send + 'static,
        F: std::future::Future<Output = T> + Send + 'static,
    {
        let (send, receive) = oneshot::channel();
        let admitted = self.spawn(move || async move {
            let result = work().await;
            let _ = send.send(result);
        });
        Box::pin(async move {
            admitted?;
            receive.await.map_err(internal)
        })
    }
    fn synchronous<T>(
        &self,
        operation: impl FnOnce() -> protocol::Result<T>,
    ) -> protocol::Result<T> {
        let state = self.state.lock().expect("user mutations poisoned");
        if state.closing {
            return Err(RpcError::new(
                RpcCode::Unavailable,
                "the user session is shutting down",
            ));
        }
        operation()
    }
    fn stop(&self) {
        self.state.lock().expect("user mutations poisoned").closing = true;
    }
    async fn join(&self) -> protocol::Result<()> {
        let tasks = self
            .state
            .lock()
            .expect("user mutations poisoned")
            .tasks
            .clone();
        for task in tasks {
            let mut slot = task.lock().await;
            if let Some(task) = slot.as_mut() {
                if let Err(error) = task.await {
                    self.state
                        .lock()
                        .expect("user mutations poisoned")
                        .failure
                        .get_or_insert_with(|| internal(error));
                }
            }
            *slot = None;
        }
        self.state
            .lock()
            .expect("user mutations poisoned")
            .failure
            .clone()
            .map_or(Ok(()), Err)
    }
}
struct ObservationCancellation {
    observer: Arc<Signal>,
    lifetime: Arc<Signal>,
}
impl Cancellation for ObservationCancellation {
    fn is_cancelled(&self) -> bool {
        self.observer.is_cancelled() || self.lifetime.is_cancelled()
    }
    fn cancelled(&self) -> BoxFuture<'_, ()> {
        Box::pin(async move {
            tokio::select! { _ = self.observer.cancelled() => {}, _ = self.lifetime.cancelled() => {} }
        })
    }
}
struct CancelObservation(Arc<Signal>);
impl Drop for CancelObservation {
    fn drop(&mut self) {
        self.0.cancel();
    }
}

pub struct UserSessionCore<S: RestrictableAccessSnapshot + 'static> {
    pub services: Arc<UserServices<S>>,
    pub registry: Arc<AgentRegistry<S>>,
    modes: Arc<UserSessionModes<S>>,
    materializer: Arc<dyn RetainedModePolicyMaterializer<S>>,
    repository: ConversationRepository,
    main: Mutex<Main<S>>,
    update_gate: AsyncMutex<()>,
    mutations: OwnedMutations,
    close: AsyncMutex<Option<protocol::Result<()>>>,
}

impl<S: RestrictableAccessSnapshot + 'static> UserSessionCore<S> {
    async fn create(
        config: Arc<UserSessionFactoryConfig<S>>,
        question_clock: Arc<dyn Clock>,
        event_load_progress: Option<SessionEventLoadProgress>,
        lease: Arc<SessionResourceLease>,
        initialization: SessionInitialization,
        settled_on_error: &mut bool,
    ) -> protocol::Result<Arc<Self>> {
        let workspace = (config.workspace)(&config.launch_directory)?;
        let resources = Arc::new(
            UserSessionResources::new(
                lease.selected_root().clone(),
                initialization.id.clone(),
                workspace,
            )
            .map_err(internal)?,
        );
        let repository = ConversationRepository::new(lease.events.clone())
            .with_image_store(lease.images.store().clone())
            .map_err(internal)?;
        let requested = config
            .modes
            .resolve(&initialization.mode)
            .map_err(invalid)?;
        let (agent_id, mode_id) = repository
            .session_state(initialization.id.value().into(), requested.id().into())
            .await
            .map_err(internal)?;
        let identity = AgentIdentity::root(
            agent_id,
            initialization.root_agent_name.clone(),
            config.templates.clone(),
        );
        let modes = Arc::new(
            UserSessionModes::new(config.modes.clone(), config.workflows.clone())
                .map_err(invalid)?,
        );
        let names: Vec<_> = identity.name_path.iter().map(String::as_str).collect();
        modes.attach(resources.agent_scratch(&names).map_err(internal)?);
        let mode = modes.resolve(&mode_id).map_err(invalid)?;
        let model = config
            .router
            .resolve(&initialization.model.selector)
            .map_err(invalid)?;
        let broker = Arc::new(EventBroker::new());
        let lifetime = Arc::new(Signal::new());
        let questions = Arc::new(
            QuestionBroker::with_clock(
                config.user_input_timeout,
                question_clock,
                config.interaction_diagnostics.clone(),
            )
            .map_err(invalid)?,
        );
        std::fs::create_dir_all(resources.scratch_directory()).map_err(internal)?;
        let progress = event_load_progress.map(|observe| {
            let id = initialization.id.value().to_owned();
            Arc::new(move |count| observe(&id, count)) as parrot_storage::events::EventLoadProgress
        });
        let statistics = lease
            .events
            .get_runtime_statistics_with_progress(progress)
            .await
            .map_err(internal)?;
        *settled_on_error = false;
        let permissions = (config.permissions)(
            lease.clone(),
            broker.clone(),
            initialization.interactive_permissions,
        )?;
        let services = Arc::new(UserServices {
            id: initialization.id.value().into(),
            lease: lease.clone(),
            resources: resources.clone(),
            broker,
            templates: config.templates.clone(),
            questions,
            permissions,
            skills: (config.skills)(resources.workspace())?,
            lifetime: lifetime.clone(),
            standing: StandingInstruction::new(config.templates.clone()),
            statistics,
            root_identity: identity.clone(),
            modes: modes.clone(),
            registry: OnceLock::new(),
        });
        let mut cleanup_registry = None;
        let mut cleanup_scope = None;
        let initialized = async {
            let source = config.source.create(services.clone())?;
            let histories = Arc::new(Histories {
                services: services.clone(),
                repository: repository.clone(),
            });
            let bind = source.bind.clone();
            let registry = AgentRegistry::new(RegistryConfig {
                agent_limits: config.agent_limits,
                profiles: config.profiles.clone(),
                factory: source.factory.clone(),
                histories: histories.clone(),
                ids: config.ids.clone(),
                diagnostics: Arc::new(SessionChildDiagnostics {
                    configured: config.diagnostics.clone(),
                    lease: Arc::downgrade(&lease),
                }),
                retained: RetainedAgentBudget::new(1024),
                lifetime: lifetime.clone(),
                validate_child: bind.clone(),
            });
            cleanup_registry = Some(registry.clone());
            services
                .registry
                .set(Arc::downgrade(&registry))
                .map_err(|_| internal("user registry already bound"))?;
            histories.refresh_all().await.map_err(internal)?;
            let history = histories.prepare(&identity).await.map_err(internal)?;
            history.refresh().await.map_err(internal)?;
            let selection = modes
                .capture_turn_selection(&mode_id, model.clone(), config.materializer.as_ref())
                .map_err(invalid)?;
            let cell = Arc::new(SelectionCell::new(selection, PolicyLineage::root()));
            let parent = parrot_runtime::children::ParentScope::<S>::completion(
                identity.clone(),
                None,
                CompletionDelivery::Automatic,
                config.ids.clone(),
            );
            let parts = match source
                .factory
                .create(PreparedScope {
                    identity: identity.clone(),
                    history: history.clone(),
                    selection: cell.clone(),
                    lineage: PolicyLineage::root(),
                    parent_completion: parent,
                    lifetime: lifetime.clone(),
                })
                .await
            {
                Ok(parts) => parts,
                Err(error) => {
                    lifetime.cancel();
                    services.questions.close();
                    services.permissions.close().await;
                    let _ = registry.shutdown().join().await;
                    services.broker.complete();
                    return Err(internal(error));
                }
            };
            let scope = Scope::create(ScopeConfig {
                authority: registry.clone(),
                identity,
                history,
                selection: cell,
                parent: None,
                delivery: CompletionDelivery::Automatic,
                retention: None,
                parts,
            })
            .join()
            .await
            .map_err(internal)?;
            cleanup_scope = Some(scope.clone());
            bind(&scope)
                .and_then(|()| registry.register_root(scope.clone()))
                .map_err(internal)?;
            let session = scope.get_service::<RuntimeSession<S>>().map_err(internal)?;
            let core = Arc::new(Self {
                services: services.clone(),
                registry,
                modes,
                materializer: config.materializer.clone(),
                repository,
                main: Mutex::new(Main {
                    model,
                    mode,
                    scope,
                    session: session.clone(),
                }),
                update_gate: AsyncMutex::new(()),
                mutations: OwnedMutations::new(services.lease.runtime_handle()),
                close: AsyncMutex::new(None),
            });
            session.recover();
            Ok(core)
        }
        .await;
        if initialized.is_err() {
            let mut joined = true;
            lifetime.cancel();
            services.questions.close();
            services.permissions.close().await;
            if let Some(registry) = cleanup_registry {
                joined &= registry.shutdown().join().await.is_ok();
            }
            if let Some(scope) = cleanup_scope {
                joined &= scope.close().join().await.is_ok();
            }
            services.broker.complete();
            *settled_on_error &= joined;
        }
        initialized
    }
    fn owned_mutation<T, F>(
        self: &Arc<Self>,
        operation: impl FnOnce(Arc<Self>) -> F + Send + 'static,
    ) -> BoxFuture<'static, protocol::Result<T>>
    where
        T: Send + 'static,
        F: std::future::Future<Output = protocol::Result<T>> + Send + 'static,
    {
        let core = self.clone();
        let observer = self.mutations.observe(move || operation(core));
        Box::pin(async move { observer.await? })
    }
    fn observe_operation<'a, T, F>(
        self: &'a Arc<Self>,
        cancellation: &'a dyn Cancellation,
        operation: impl FnOnce(Arc<Self>, Arc<dyn Cancellation>) -> F + Send + 'static,
    ) -> BoxFuture<'a, protocol::Result<T>>
    where
        T: Send + 'static,
        F: std::future::Future<Output = protocol::Result<T>> + Send + 'static,
    {
        Box::pin(async move {
            if cancellation.is_cancelled() {
                return Err(RpcError::cancelled());
            }
            let signal = Arc::new(Signal::new());
            let linked: Arc<dyn Cancellation> = Arc::new(ObservationCancellation {
                observer: signal.clone(),
                lifetime: self.services.lifetime.clone(),
            });
            let _guard = CancelObservation(signal);
            let observer = self.owned_mutation(move |core| operation(core, linked));
            tokio::select! { biased; result = observer => result, _ = cancellation.cancelled() => Err(RpcError::cancelled()) }
        })
    }
    fn selected(
        &self,
    ) -> (
        ResolvedModelSelection,
        String,
        Arc<RuntimeSession<S>>,
        Arc<Scope<S>>,
    ) {
        let main = self.main.lock().expect("user main poisoned");
        (
            main.model.clone(),
            main.mode.profile().id().into(),
            main.session.clone(),
            main.scope.clone(),
        )
    }
    fn snapshot(&self) -> wire::UserSession {
        let (model, mode, _, _) = self.selected();
        wire::UserSession {
            id: self.services.id.clone(),
            model: model.requested_selector.value.clone(),
            mode,
            working_directory: self
                .services
                .resources
                .workspace()
                .launch_directory()
                .to_string_lossy()
                .into_owned(),
            ..wire::UserSession::new()
        }
    }
    fn publish_metadata(&self) -> protocol::Result<()> {
        let (model, mode, _, _) = self.selected();
        self.services
            .lease
            .update_selection_metadata(
                model.canonical_model.provider.id(),
                &model.canonical_model.selector(),
                &model.requested_selector.value,
                &mode,
            )
            .map_err(internal)
    }
    async fn update(
        &self,
        model: Option<ResolvedModelSelection>,
        mode: Option<String>,
    ) -> protocol::Result<()> {
        let _gate = self.update_gate.lock().await;
        let (previous, previous_mode, session, _) = self.selected();
        let next_mode = self
            .modes
            .resolve(mode.as_deref().unwrap_or(&previous_mode))
            .map_err(invalid)?;
        let mut selected = self
            .modes
            .capture_turn_selection(
                next_mode.profile().id(),
                model.clone().unwrap_or(previous),
                self.materializer.as_ref(),
            )
            .map_err(invalid)?;
        if previous_mode != next_mode.profile().id() {
            self.repository
                .update_mode(
                    self.services.id.clone(),
                    self.services.root_identity.session.session_id.clone(),
                    next_mode.profile().id().into(),
                )
                .await
                .map_err(internal)?;
        }
        {
            let mut main = self.main.lock().expect("user main poisoned");
            // A model-only preset can commit while the durable mode write awaits.
            // Only an explicitly supplied model replaces that newer choice.
            selected.model = model.unwrap_or_else(|| main.model.clone());
            session
                .backend()
                .update_selection(selected.clone())
                .map_err(internal)?;
            main.model = selected.model;
            main.mode = next_mode;
        }
        self.publish_metadata()
    }
    fn update_model(&self, model: ResolvedModelSelection) -> ConfigurationResult<()> {
        {
            let mut main = self.main.lock().expect("user main poisoned");
            let selection = self
                .modes
                .capture_turn_selection(
                    main.mode.profile().id(),
                    model.clone(),
                    self.materializer.as_ref(),
                )
                .map_err(|e| {
                    CoordinatorError::Provider(parrot_provider::sessions::Error::InvalidOperation(
                        e.to_string(),
                    ))
                })?;
            main.session
                .backend()
                .update_selection(selection)
                .map_err(|e| {
                    CoordinatorError::Provider(parrot_provider::sessions::Error::InvalidOperation(
                        e.to_string(),
                    ))
                })?;
            main.model = model;
        }
        self.publish_metadata().map_err(|e| {
            CoordinatorError::Provider(parrot_provider::sessions::Error::InvalidOperation(
                e.to_string(),
            ))
        })
    }
    async fn publish(&self, mut event: wire::Event) -> protocol::Result<()> {
        if event.id.is_empty() {
            event.id = Identifier::event_id().map_err(internal)?;
        }
        let committed = self
            .services
            .lease
            .events
            .append(event, None)
            .await
            .map_err(internal)?;
        self.services
            .broker
            .publish_retained(&committed.stored.event);
        Ok(())
    }
    async fn close_core(&self) -> protocol::Result<()> {
        let mut closed = self.close.lock().await;
        if let Some(result) = closed.as_ref() {
            return result.clone();
        }
        self.mutations.stop();
        self.services.lifetime.cancel();
        self.services.questions.close();
        self.services.permissions.close().await;
        let mut failure = None;
        let shutdown = self.registry.shutdown();
        if let Err(error) = shutdown.join().await {
            failure.get_or_insert_with(|| internal(error));
        }
        if let Err(error) = self.mutations.join().await {
            failure.get_or_insert(error);
        }
        self.services.broker.complete();
        if failure.is_none() {
            self.services.diagnostic(&DiagnosticEvent::new(
                "session",
                "producers_stopped",
                DiagnosticSeverity::Information,
            ));
            self.services.lease.confirm_producers_settled();
        }
        let result = failure.map_or(Ok(()), Err);
        *closed = Some(result.clone());
        result
    }
}

fn internal(error: impl std::fmt::Display) -> RpcError {
    RpcError::new(RpcCode::Internal, error.to_string())
}
fn invalid(error: impl std::fmt::Display) -> RpcError {
    RpcError::new(RpcCode::InvalidArgument, error.to_string())
}
fn status_error(
    error: OperationError<parrot_runtime::active_work::status::StatusError>,
) -> RpcError {
    match error {
        OperationError::Cancelled => RpcError::cancelled(),
        OperationError::Failed(error) => internal(error),
    }
}
fn interaction_stopped(error: RpcError) -> InteractionError {
    InteractionError::new(
        parrot_runtime::interactions::InteractionErrorKind::Disposed,
        error.to_string(),
    )
}
fn child(message: impl Into<String>) -> parrot_runtime::children::ChildError {
    parrot_runtime::children::ChildError::registry(message)
}
fn child_io(error: std::io::Error) -> parrot_runtime::children::ChildError {
    child(error.to_string())
}

impl<S: RestrictableAccessSnapshot + 'static> UserSessionCore<S> {
    async fn send(
        self: &Arc<Self>,
        mut parts: Vec<ConversationPart>,
        mut message_id: String,
        delivery: wire::Delivery,
        caller: Arc<dyn Cancellation>,
    ) -> protocol::Result<Admission> {
        let (session, mode, selection) = {
            let main = self.main.lock().expect("user main poisoned");
            (
                main.session.clone(),
                main.mode.clone(),
                main.scope.selection().capture().map_err(internal)?,
            )
        };
        mode.prepare().map_err(invalid)?;
        if delivery == wire::Delivery::DELIVERY_STEER {
            return session
                .send(
                    parts,
                    message_id,
                    delivery,
                    IncomingActivity::default(),
                    caller,
                )
                .await
                .map(|sent| sent.admission)
                .map_err(lifecycle);
        }
        let (send, receive) = oneshot::channel();
        let admitted = Arc::new(Mutex::new(Some(send)));
        let callback = admitted.clone();
        let mut callback = Some(Arc::new(move |admission: &Admission| {
            if let Some(send) = callback.lock().expect("admission result poisoned").take() {
                let _ = send.send(Ok(admission.clone()));
            }
        })
            as parrot_runtime::agent_turn::lifecycle::AdmissionCallback);
        let core = self.clone();
        self.mutations.spawn(move || async move {
            tokio::task::yield_now().await;
            let outcome: protocol::Result<()> = async {
                loop {
                    session
                        .send_and_wait(
                            parts,
                            message_id,
                            Some(selection.clone()),
                            callback.take(),
                            core.services.lifetime.clone(),
                        )
                        .await
                        .map_err(lifecycle)?;
                    match mode.complete().map_err(invalid)? {
                        ModeCompletionOutcome::None => return Ok(()),
                        ModeCompletionOutcome::Completed(mut complete) => {
                            complete.agent_session_id =
                                core.services.root_identity.session.session_id.clone();
                            complete.message_id = Identifier::message_id().map_err(internal)?;
                            let mut event = wire::Event::new();
                            event.agent_session_id = complete.agent_session_id.clone();
                            event.set_plan_completed(complete);
                            core.publish(event).await?;
                            return Ok(());
                        }
                        ModeCompletionOutcome::Repair(diagnostic) => {
                            let mut event = wire::Event::new();
                            event.agent_session_id =
                                core.services.root_identity.session.session_id.clone();
                            event.set_agent_notice(wire::AgentNotice {
                                kind: "plan_validation_repair".into(),
                                notice: format!(
                                    "Retrying after plan validation failure: {diagnostic}"
                                ),
                                ..wire::AgentNotice::new()
                            });
                            core.publish(event).await?;
                            mode.prepare().map_err(invalid)?;
                            parts = vec![ConversationPart::Text(diagnostic)];
                            message_id = Identifier::message_id().map_err(internal)?;
                        }
                    }
                }
            }
            .await;
            if let Err(error) = outcome {
                if let Some(send) = admitted.lock().expect("admission result poisoned").take() {
                    let _ = send.send(Err(error));
                } else {
                    core.diagnostic_failure("queued_prompt_failed");
                }
            }
            let mut event = wire::Event::new();
            event.agent_session_id = core.services.root_identity.session.session_id.clone();
            event.set_mode_turn_completed(wire::ModeTurnCompleted::new());
            if core.publish(event).await.is_err() {
                core.diagnostic_failure("queued_prompt_failed");
            }
        })?;
        tokio::select! {
            biased;
            result = receive => result.map_err(internal)?,
            _ = caller.cancelled() => Err(RpcError::cancelled()),
        }
    }
    fn diagnostic_failure(&self, name: &str) {
        let mut event = DiagnosticEvent::new("session", name, DiagnosticSeverity::Error);
        event.agent_session_id = Some(self.services.root_identity.session.session_id.clone());
        event.error_code = Some("unexpected".into());
        self.services.diagnostic(&event);
    }
    async fn set_goal(
        &self,
        goal: Option<String>,
        cancellation: Arc<dyn Cancellation>,
    ) -> protocol::Result<()> {
        let (_, _, session, _) = self.selected();
        match goal {
            Some(goal) => {
                let reminder = self
                    .services
                    .templates
                    .render(
                        "goal.root-reminder",
                        &[("goal".into(), goal.into())],
                        &NeverCancelled,
                    )
                    .map_err(internal)?;
                session
                    .set_exit_reminder("goal".into(), reminder.clone(), cancellation.clone())
                    .map_err(lifecycle)?
                    .join()
                    .await
                    .map_err(lifecycle)?;
                let notice = self
                    .services
                    .templates
                    .render(
                        "goal.root-reminder-notice",
                        &[("reminder".into(), reminder.into())],
                        &NeverCancelled,
                    )
                    .map_err(internal)?;
                session
                    .send(
                        vec![ConversationPart::Text(notice)],
                        Identifier::message_id().map_err(internal)?,
                        wire::Delivery::DELIVERY_STEER,
                        IncomingActivity::default(),
                        cancellation,
                    )
                    .await
                    .map_err(lifecycle)?;
            }
            None => {
                session
                    .clear_exit_reminder("goal".into(), cancellation)
                    .map_err(lifecycle)?
                    .join()
                    .await
                    .map_err(lifecycle)?;
            }
        }
        Ok(())
    }
}

struct StoredCore<S: RestrictableAccessSnapshot + 'static>(Arc<UserSessionCore<S>>);
impl<S: RestrictableAccessSnapshot + 'static> StoredUserSession for StoredCore<S> {
    fn model(&self) -> SessionModelSelection {
        let (model, _, _, _) = self.0.selected();
        SessionModelSelection {
            provider_id: model.canonical_model.provider.id().into(),
            canonical_model: model.canonical_model.selector(),
            selector: model.requested_selector.value.clone(),
        }
    }
    fn mode(&self) -> String {
        self.0.selected().1
    }
    fn dispose(&mut self) -> BoxFuture<'_, Result<(), SessionStoreError>> {
        Box::pin(async move {
            self.0
                .close_core()
                .await
                .map_err(SessionStoreError::failure)?;
            self.0.services.lease.close().await
        })
    }
}
impl<S: RestrictableAccessSnapshot + 'static> StoredUserSessionFactory for UserSessionFactory<S> {
    fn create(
        &self,
        lease: Arc<SessionResourceLease>,
        initialization: SessionInitialization,
    ) -> BoxFuture<'_, Result<Box<dyn StoredUserSession>, SessionStoreError>> {
        let config = self.config.clone();
        let question_clock = self.question_clock.clone();
        let event_load_progress = self.event_load_progress.clone();
        Box::pin(async move {
            let mut settled_on_error = true;
            let core = match UserSessionCore::create(
                config,
                question_clock,
                event_load_progress,
                lease.clone(),
                initialization,
                &mut settled_on_error,
            )
            .await
            {
                Ok(core) => core,
                Err(error) => {
                    if settled_on_error {
                        lease.confirm_producers_settled();
                    }
                    return Err(SessionStoreError::failure(error));
                }
            };
            let duplicate = {
                let mut pending = self.pending.lock().expect("pending sessions poisoned");
                pending.retain(|_, core| core.strong_count() > 0);
                if pending
                    .get(&core.services.id)
                    .and_then(Weak::upgrade)
                    .is_some()
                {
                    true
                } else {
                    pending.insert(core.services.id.clone(), Arc::downgrade(&core));
                    false
                }
            };
            if duplicate {
                let _ = core.close_core().await;
                return Err(SessionStoreError::failure(RpcError::new(
                    RpcCode::AlreadyExists,
                    "the user session is already created",
                )));
            }
            Ok(Box::new(StoredCore(core)) as Box<dyn StoredUserSession>)
        })
    }
}

struct HostedCloseJoin {
    receipt: Option<oneshot::Receiver<Result<(), tokio::task::JoinError>>>,
    result: Option<protocol::Result<()>>,
}
impl HostedCloseJoin {
    async fn join(&mut self) -> protocol::Result<()> {
        if let Some(result) = &self.result {
            return result.clone();
        }
        let result = self
            .receipt
            .as_mut()
            .expect("admitted close receipt")
            .await
            .map_err(internal)
            .and_then(|joined| joined.map_err(internal));
        self.receipt = None;
        self.result = Some(result.clone());
        result
    }
}
struct HostedClose {
    result: tokio::sync::watch::Receiver<Option<protocol::Result<()>>>,
    task: Arc<AsyncMutex<HostedCloseJoin>>,
}
struct HostedCore<S: RestrictableAccessSnapshot + 'static> {
    core: Arc<UserSessionCore<S>>,
    opened: Mutex<Option<OpenedSession>>,
    closing: OnceLock<HostedClose>,
}
impl<S: RestrictableAccessSnapshot + 'static> HostedSessionFactory for UserSessionFactory<S> {
    fn bind_opened(
        &self,
        mut opened: OpenedSession,
    ) -> BoxFuture<'_, protocol::Result<Arc<dyn HostedSession>>> {
        let id = opened.initialization.id.value().to_owned();
        let pending = {
            let mut pending = self.pending.lock().expect("pending sessions poisoned");
            match pending.get(&id).and_then(Weak::upgrade) {
                Some(core) if Arc::ptr_eq(&core.services.lease, &opened.resources) => {
                    pending.remove(&id);
                    Some(core)
                }
                _ => None,
            }
        };
        Box::pin(async move {
            match pending {
                Some(core) => Ok(Arc::new(HostedCore {
                    core,
                    opened: Mutex::new(Some(opened)),
                    closing: OnceLock::new(),
                }) as Arc<dyn HostedSession>),
                _ => {
                    let _ = opened.close().await;
                    Err(RpcError::new(
                        RpcCode::FailedPrecondition,
                        "opened session does not match its created capability",
                    ))
                }
            }
        })
    }
    fn list_modes(&self) -> Vec<String> {
        self.config.modes.list().to_vec()
    }
    fn resolve_mode(&self, mode: &str) -> protocol::Result<()> {
        self.config.modes.resolve(mode).map(|_| ()).map_err(invalid)
    }
    fn resolve_workspace(&self, directory: &Path) -> protocol::Result<ProjectWorkspace> {
        (self.config.workspace)(directory)
    }
}
impl<S: RestrictableAccessSnapshot + 'static> HostedSession for HostedCore<S> {
    fn snapshot(&self) -> wire::UserSession {
        self.core.snapshot()
    }
    fn resolve_mode(&self, mode: &str) -> protocol::Result<String> {
        self.core
            .modes
            .resolve(mode)
            .map(|mode| mode.profile().id().into())
            .map_err(invalid)
    }
    fn update(
        &self,
        model: Option<ResolvedModelSelection>,
        mode: Option<String>,
    ) -> BoxFuture<'_, protocol::Result<()>> {
        self.core
            .owned_mutation(move |core| async move { core.update(model, mode).await })
    }
    fn update_selection(&self, model: ResolvedModelSelection) -> ConfigurationResult<()> {
        self.core
            .mutations
            .synchronous(|| self.core.update_model(model).map_err(internal))
            .map_err(|error| {
                CoordinatorError::Provider(parrot_provider::sessions::Error::InvalidOperation(
                    error.to_string(),
                ))
            })
    }
    fn persist_image<'a>(
        &'a self,
        upload: protocol::ImageUpload,
        cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, protocol::Result<parrot_storage::artifacts::ImageArtifactMetadata>> {
        self.core
            .observe_operation(cancellation, move |core, cancellation| async move {
                core.services
                    .lease
                    .images
                    .persist(
                        std::io::Cursor::new(upload.content),
                        &upload.upload_id,
                        &upload.display_name,
                        "upload",
                        Some(&upload.media_type),
                        Some(upload.byte_length),
                        cancellation.as_ref(),
                    )
                    .await
                    .map_err(RpcError::from)
            })
    }
    fn resolve_image<'a>(
        &'a self,
        id: &'a str,
    ) -> BoxFuture<'a, protocol::Result<Option<parrot_storage::artifacts::ImageArtifactMetadata>>>
    {
        let id = id.to_owned();
        self.core.owned_mutation(move |core| async move {
            core.services
                .lease
                .images
                .resolve(&id, &NeverCancelled)
                .await
                .map_err(RpcError::from)
        })
    }
    fn pending_questions(
        &self,
    ) -> protocol::Result<Vec<parrot_runtime::interactions::PendingQuestion>> {
        self.core
            .mutations
            .synchronous(|| Ok(self.core.services.questions.pending()))
    }
    fn reply_question(
        &self,
        id: &str,
        reply: parrot_runtime::interactions::QuestionReply,
    ) -> Result<(), InteractionError> {
        self.core
            .mutations
            .synchronous(|| Ok(self.core.services.questions.reply(id, reply)))
            .map_err(interaction_stopped)?
    }
    fn reject_question(&self, id: &str) -> Result<(), InteractionError> {
        self.core
            .mutations
            .synchronous(|| Ok(self.core.services.questions.reject(id)))
            .map_err(interaction_stopped)?
    }
    fn pending_permissions(&self) -> BoxFuture<'_, protocol::Result<Vec<wire::PendingPermission>>> {
        self.core.owned_mutation(|core| async move {
            Ok(core
                .services
                .permissions
                .pending()
                .await
                .iter()
                .map(|pending| {
                    map_pending_permission(pending, |target| PermissionTargetDescription {
                        path: target.path.to_string_lossy().into_owned(),
                        kind: match target.kind {
                            WriteTargetKind::File => PermissionTargetDescriptionKind::File,
                            WriteTargetKind::Directory => {
                                PermissionTargetDescriptionKind::Directory
                            }
                        },
                    })
                })
                .collect())
        })
    }
    fn reply_permission<'a>(
        &'a self,
        id: &'a str,
        choice: &'a str,
        reason: &'a str,
    ) -> BoxFuture<'a, Result<(), InteractionError>> {
        let (id, choice, reason) = (id.to_owned(), choice.to_owned(), reason.to_owned());
        let core = self.core.clone();
        let observer = self.core.mutations.observe(move || async move {
            core.services.permissions.reply(&id, &choice, &reason).await
        });
        Box::pin(async move {
            observer.await.map_err(|error| {
                InteractionError::new(
                    parrot_runtime::interactions::InteractionErrorKind::Disposed,
                    error.to_string(),
                )
            })?
        })
    }
    fn refresh_skills<'a>(
        &'a self,
        cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, protocol::Result<SkillSnapshot>> {
        self.core
            .observe_operation(cancellation, |core, cancellation| async move {
                let security = core
                    .selected()
                    .3
                    .selection()
                    .capture()
                    .map_err(internal)?
                    .security;
                core.services
                    .skills
                    .refresh(&security, cancellation.as_ref())
                    .await
            })
    }
    fn configure_skill<'a>(
        &'a self,
        path: &'a str,
        enabled: bool,
        cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, protocol::Result<SkillMetadata>> {
        let path = path.to_owned();
        self.core
            .observe_operation(cancellation, move |core, cancellation| async move {
                let security = core
                    .selected()
                    .3
                    .selection()
                    .capture()
                    .map_err(internal)?
                    .security;
                core.services
                    .skills
                    .configure(&security, &path, enabled, cancellation.as_ref())
                    .await
            })
    }
    fn send(
        &self,
        parts: Vec<ConversationPart>,
        id: String,
        delivery: wire::Delivery,
        cancellation: Arc<dyn Cancellation>,
    ) -> BoxFuture<'_, protocol::Result<Admission>> {
        self.core.owned_mutation(move |core| async move {
            core.send(parts, id, delivery, cancellation).await
        })
    }
    fn interrupt(
        &self,
        cancellation: Arc<dyn Cancellation>,
    ) -> BoxFuture<'_, protocol::Result<()>> {
        self.core.owned_mutation(move |core| async move {
            core.selected()
                .2
                .interrupt(cancellation.as_ref())
                .await
                .map_err(lifecycle)
        })
    }
    fn compact(
        &self,
        target: Option<ContextSize>,
        cancellation: Arc<dyn Cancellation>,
    ) -> BoxFuture<'_, protocol::Result<()>> {
        self.core.owned_mutation(move |core| async move {
            core.selected()
                .2
                .compact_context(target, cancellation)
                .await
                .map(|_| ())
                .map_err(internal)
        })
    }
    fn set_goal(
        &self,
        goal: Option<String>,
        cancellation: Arc<dyn Cancellation>,
    ) -> BoxFuture<'_, protocol::Result<()>> {
        self.core
            .owned_mutation(move |core| async move { core.set_goal(goal, cancellation).await })
    }
    fn set_standing_instruction(&self, instruction: String) -> protocol::Result<()> {
        self.core.mutations.synchronous(|| {
            self.core.services.standing.update(instruction);
            Ok(())
        })
    }
    fn observe_status<'a>(
        &'a self,
        cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, protocol::Result<String>> {
        self.core
            .observe_operation(cancellation, |core, cancellation| async move {
                let (_, _, session, scope) = core.selected();
                let observed = session
                    .backend()
                    .observe_context(cancellation.as_ref())
                    .await
                    .map_err(|error| {
                        if error.is_cancelled() {
                            RpcError::cancelled()
                        } else {
                            internal(error)
                        }
                    })?;
                let status = scope
                    .get_service::<super::scope::ScopedTurnStatus<S>>()
                    .map_err(internal)?;
                let query = status.query(&observed.selection);
                let profile = status
                    .profile_prompt(&observed.selection)
                    .map_err(internal)?;
                let full = status
                    .runtime
                    .observe_with_context(&query, &profile, observed.context, cancellation.as_ref())
                    .await
                    .map_err(status_error)?;
                let statistics = session.backend().usage().capture();
                let statistics = status
                    .runtime
                    .observe_statistics(&query, statistics, cancellation.as_ref())
                    .await
                    .map_err(status_error)?;
                Ok([full, statistics]
                    .into_iter()
                    .filter(|text| !text.trim().is_empty())
                    .collect::<Vec<_>>()
                    .join("\n\n"))
            })
    }
    fn listen(
        &self,
        cursor: Option<String>,
        cancellation: Arc<dyn Cancellation>,
    ) -> protocol::Result<Box<dyn SessionEventStream>> {
        let pending = self.core.owned_mutation(move |core| async move {
            let replay_core = core.clone();
            let initial_core = core.clone();
            let subscription = core
                .services
                .broker
                .listen(
                    cursor.as_deref(),
                    move || async move {
                        replay_core
                            .services
                            .lease
                            .events
                            .replay()
                            .await
                            .map_err(internal)
                    },
                    move || async move {
                        let mut initial = Vec::new();
                        for scope in initial_core.registry.snapshot_scopes() {
                            initial.extend(scope.capture_snapshot_events().map_err(internal)?);
                        }
                        let root = initial_core.services.statistics.get_agent_statistics(
                            &initial_core.services.root_identity.session.session_id,
                        );
                        let mut event = wire::Event::new();
                        event.set_session_usage_snapshot(
                            initial_core.services.statistics.capture_usage(&root),
                        );
                        initial.push(event);
                        Ok::<_, RpcError>(initial)
                    },
                )
                .await?;
            Ok(Arc::new(subscription) as Arc<UserSubscription>)
        });
        Ok(Box::new(UserStream {
            pending: Some(pending),
            subscription: None,
            cancellation,
        }))
    }
    fn record_opened(&self) -> protocol::Result<()> {
        self.core.mutations.synchronous(|| {
            self.opened
                .lock()
                .expect("opened session poisoned")
                .as_ref()
                .ok_or_else(|| {
                    RpcError::new(RpcCode::Unavailable, "the user session is shutting down")
                })?
                .record_opened()
                .map_err(internal)
        })
    }
    fn write_diagnostic(&self, event: &DiagnosticEvent) {
        let _ = self.core.mutations.synchronous(|| {
            self.core.services.diagnostic(event);
            Ok(())
        });
    }
    fn close(&self) -> BoxFuture<'_, protocol::Result<()>> {
        let close = self.closing.get_or_init(|| {
            self.core.mutations.stop();
            let opened = self.opened.lock().expect("opened session poisoned").take();
            let (send, receive) = tokio::sync::watch::channel(None);
            let failed = send.clone();
            let task = match self.core.services.lease.spawn_close(move || async move {
                let result = match opened {
                    Some(mut opened) => opened.close().await.map_err(internal),
                    None => Ok(()),
                };
                let _ = send.send(Some(result));
            }) {
                Ok(receipt) => HostedCloseJoin {
                    receipt: Some(receipt),
                    result: None,
                },
                Err(error) => {
                    let error = internal(error);
                    let _ = failed.send(Some(Err(error.clone())));
                    HostedCloseJoin {
                        receipt: None,
                        result: Some(Err(error)),
                    }
                }
            };
            HostedClose {
                result: receive,
                task: Arc::new(AsyncMutex::new(task)),
            }
        });
        let mut completed = close.result.clone();
        let task = close.task.clone();
        Box::pin(async move {
            let result = loop {
                if let Some(result) = completed.borrow_and_update().clone() {
                    break result;
                }
                if let Err(error) = completed.changed().await {
                    break Err(internal(error));
                }
            };
            task.lock().await.join().await?;
            result
        })
    }
}

pub struct CatalogUserSkills<
    R: parrot_domain::contracts::filesystem::AuthorizedReadService + ?Sized,
> {
    pub catalog: Arc<SkillCatalog<R>>,
    pub reader: Arc<R>,
    pub templates: Arc<PromptTemplateCatalog>,
    pub working_directory: String,
}
struct SkillSecurity<S: RestrictableAccessSnapshot + Clone + 'static>(Arc<dyn SelectionSource<S>>);
impl<S: RestrictableAccessSnapshot + Clone + 'static> SecurityProvider for SkillSecurity<S> {
    type Snapshot = S;
    fn capture(&self) -> S {
        self.0
            .capture()
            .expect("validated scoped selection remains capturable")
            .security
            .as_ref()
            .clone()
    }
}
impl<S, R> UserSkills<S> for CatalogUserSkills<R>
where
    S: RestrictableAccessSnapshot + Clone + 'static,
    R: parrot_domain::contracts::filesystem::AuthorizedReadService<Security = S> + ?Sized + 'static,
{
    fn refresh<'a>(
        &'a self,
        security: &'a S,
        cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, protocol::Result<SkillSnapshot>> {
        Box::pin(async move {
            self.catalog
                .refresh(security, cancellation)
                .await
                .map(|snapshot| snapshot.as_ref().clone())
                .map_err(skill_error)
        })
    }
    fn configure<'a>(
        &'a self,
        security: &'a S,
        path: &'a str,
        enabled: bool,
        cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, protocol::Result<SkillMetadata>> {
        Box::pin(async move {
            self.catalog
                .configure(path, enabled, security, cancellation)
                .await
                .map_err(skill_error)
        })
    }
    fn new_agent(
        &self,
        selection: Arc<dyn SelectionSource<S>>,
    ) -> BoxFuture<'_, PromptResult<AgentSkillBindings>> {
        Box::pin(async move {
            let skills = Arc::new(AsyncMutex::new(AgentSkills::new(
                self.catalog.clone(),
                self.reader.clone(),
                Arc::new(SkillSecurity(selection)),
                self.templates.clone(),
                self.working_directory.clone(),
            )?));
            let prompt = Arc::new(
                parrot_runtime::prompt_context::AgentSkillPromptProvider::new(skills.clone()).await,
            );
            Ok(AgentSkillBindings { skills, prompt })
        })
    }
}
fn skill_error(
    error: OperationError<parrot_domain::contracts::filesystem::FileReadError>,
) -> RpcError {
    use parrot_domain::contracts::filesystem::FileReadErrorKind;
    match error {
        OperationError::Cancelled => RpcError::cancelled(),
        OperationError::Failed(error) => RpcError::new(
            match error.kind {
                FileReadErrorKind::Denied => RpcCode::PermissionDenied,
                FileReadErrorKind::NotFound => RpcCode::NotFound,
                FileReadErrorKind::InvalidPath | FileReadErrorKind::InvalidOperation => {
                    RpcCode::InvalidArgument
                }
                _ => RpcCode::Internal,
            },
            error.to_string(),
        ),
    }
}
fn lifecycle(error: parrot_runtime::agent_turn::lifecycle::LifecycleError) -> RpcError {
    if error.is_cancelled() {
        RpcError::cancelled()
    } else {
        internal(error)
    }
}

#[cfg(test)]
#[path = "../../../tests/composition/factory/user.rs"]
mod tests;

type UserSubscription = dyn EventSubscription<
        Event = wire::Event,
        Error = parrot_runtime::event_broker::ListenError<RpcError>,
    >;
struct UserStream {
    pending: Option<BoxFuture<'static, protocol::Result<Arc<UserSubscription>>>>,
    subscription: Option<Arc<UserSubscription>>,
    cancellation: Arc<dyn Cancellation>,
}
impl SessionEventStream for UserStream {
    fn next<'a>(
        &'a mut self,
        cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, protocol::Result<Option<wire::Event>>> {
        Box::pin(async move {
            if let Some(pending) = self.pending.as_mut() {
                let subscription = tokio::select! {
                    biased;
                    result = pending => result?,
                    _ = cancellation.cancelled() => return Err(RpcError::cancelled()),
                    _ = self.cancellation.cancelled() => return Err(RpcError::cancelled()),
                };
                self.pending = None;
                self.subscription = Some(subscription);
            }
            let subscription = self
                .subscription
                .as_ref()
                .expect("initialized user subscription");
            tokio::select! {
                biased;
                result = subscription.next(cancellation) => result.map_err(|e| match e { OperationError::Cancelled => RpcError::cancelled(), OperationError::Failed(e) => internal(e) }),
                _ = self.cancellation.cancelled() => Err(RpcError::cancelled()),
            }
        })
    }
}
