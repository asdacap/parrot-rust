//! Bounded authoritative constructor tests, not a T36 security/backend or whole-host proof.
//! The filesystem, permission supplier and launcher deny all external operations;
//! scopes, storage, publication, drain, scheduler, queues and process owners are real.
use std::{
    collections::BTreeMap,
    io,
    marker::PhantomData,
    net::IpAddr,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::{Duration, Instant, SystemTime},
};

use parrot_domain::{
    contracts::{
        async_ops::{BoxFuture, Cancellation, Cancelled, Clock, OperationError, OperationResult},
        events::EventSubscription,
        filesystem::{
            AuthorizedMutationService, AuthorizedReadService, BoundRead, FileMetadata,
            FileReadError, FileReadErrorKind, LinkPolicy, MutationRead, MutationRequest,
            MutationTarget, ReadRequest,
        },
        paths::ResolvedRead,
        security::{AccessSnapshot, InvocationContext, RestrictableAccessSnapshot},
        tools::Tool,
    },
    values::{
        config::{ModelProfiles, PathPlatform, SkillConfiguration},
        identifiers::Identifier,
        project_workspace::ProjectWorkspace,
    },
};
use parrot_foundation::{
    config::Configuration, paths::AgentScratchDirectory, template_catalog::PromptTemplateCatalog,
};
use parrot_host::composition::{
    agents::AgentSessionServices,
    context::ContextSelection,
    factory::{
        scope::{AgentOutput, AgentSourceConfig, AuthoritativeAgentSource},
        tasks::ScopedTaskController,
        user::{
            AgentFactoryParts, AgentFactorySource, AgentSkillBindings, CatalogUserSkills,
            RuntimeSession, UserServices, UserSessionFactory, UserSessionFactoryConfig, UserSkills,
            durable_permission_factory,
        },
    },
    protocol::{self, HostedSession, HostedSessionFactory, RpcCode, RpcError},
    queues::ScopedQueueBackend,
};
use parrot_provider::{
    catalogs::Model,
    routing::{
        aliases::{
            ModelAliasCatalog, ModelAliasDefinition, ModelRouter, ModelRouting,
            ResolvedModelSelection,
        },
        registry::ProviderRegistry,
    },
    sessions::{
        Completion, Content, Error, Event as ProviderEvent, EventStream, Provider, Request, Session,
    },
};
use parrot_runtime::{
    agent_queues::AgentQueues,
    agent_turn::{
        lifecycle::DrainBackend,
        provider_call::OutputWriter,
        services::{SelectionSource, SessionOutput, TurnSelection},
    },
    children::{
        AgentIdentity, ChildDiagnostic, ChildDiagnostics, ChildError, ChildProfile, ChildResult,
        IdGenerator, PreparedScope, ProfileResolver, Scope, ScopeFactory, ScopeParts,
    },
    interactions::{BrokerDiagnostic, InteractionDiagnostics},
    plan_mode::{
        MaterializedModeProfile, ModeRegistry, ModeResult, RetainedModePolicyMaterializer,
    },
    prompt_context::{
        AgentIdentityDescription, PromptResult, SkillCatalog, SkillDiscoveryContext,
        StaticSystemPromptProvider, SystemPrompt, SystemPromptProvider,
    },
    task_scheduler::{TaskScheduler, TaskSession},
    tool_settlement::SharedSecurity,
};
use parrot_storage::{
    diagnostics::FileDiagnosticLog,
    events::{ConversationPart, NeverCancelled},
    ownership::{
        OwnershipRoots, ProcessIdentity, ProcessIdentityStatus, RuntimeIdentity, SessionAdmissions,
        SessionModelRouter, SessionModelSelection, SessionStore, SessionStoreError,
        SessionWorkspaceResolver,
    },
};
use parrot_tools::{
    catalog,
    process::{
        ownership::{
            domain::CapturedProcessLauncher,
            execution::{Execution, ProcessLifetime},
            owner::ShellProcessOwner,
        },
        tools::CommandRequest,
    },
    session::{
        Failure, FailureKind, PermissionReply, PermissionServices, WriteTarget, WriteTargetKind,
    },
    task_artifacts::TaskArtifactParserAdapter,
    tasks_queues::{
        ToolInvocation,
        queues::{QueueDiagnostic, QueueDiagnostics, QueueTool, QueueToolKind},
    },
    web_fetch::{
        FetchError, GzipDecoder, Options, PublicAddressPolicy, Resolver, ResponseBody, Transport,
        TransportSession, WebFetcher,
    },
};
use serde_json::{Value, json};

#[derive(Clone)]
struct Security;
impl AccessSnapshot for Security {
    type Error = FileReadError;
    fn check_read(&self, _: &ResolvedRead) -> Result<(), Self::Error> {
        Err(denied())
    }
    fn check_write(&self, _: &Path) -> Result<(), Self::Error> {
        Err(denied())
    }
}
impl RestrictableAccessSnapshot for Security {
    fn restrict_with(&self, _: &Self) -> Self {
        self.clone()
    }
}
fn denied() -> FileReadError {
    FileReadError::new(
        FileReadErrorKind::Denied,
        "synthetic fixture authorizes no filesystem operations",
    )
}
#[derive(Clone)]
struct Files<S>(PhantomData<S>);
impl<S: AccessSnapshot> AuthorizedReadService for Files<S> {
    type Security = S;
    fn inspect<'a>(
        &'a self,
        _: &'a Path,
        _: LinkPolicy,
        _: &'a S,
        _: &'a dyn Cancellation,
    ) -> BoxFuture<'a, OperationResult<FileMetadata, FileReadError>> {
        Box::pin(async { Err(OperationError::Failed(denied())) })
    }
    fn list_directory<'a>(
        &'a self,
        _: &'a Path,
        _: LinkPolicy,
        _: &'a S,
        _: &'a dyn Cancellation,
    ) -> BoxFuture<'a, OperationResult<Vec<FileMetadata>, FileReadError>> {
        Box::pin(async { Err(OperationError::Failed(denied())) })
    }
    fn read<'a>(
        &'a self,
        _: &'a ReadRequest,
        _: &'a S,
        _: &'a dyn Cancellation,
    ) -> BoxFuture<'a, OperationResult<BoundRead, FileReadError>> {
        Box::pin(async { Err(OperationError::Failed(denied())) })
    }
}
impl<S: AccessSnapshot> AuthorizedMutationService for Files<S> {
    type Security = S;
    fn inspect_for_mutation<'a>(
        &'a self,
        _: &'a MutationRequest,
        _: &'a S,
        _: &'a dyn Cancellation,
    ) -> BoxFuture<'a, OperationResult<MutationTarget, FileReadError>> {
        Box::pin(async { Err(OperationError::Failed(denied())) })
    }
    fn read_for_mutation<'a>(
        &'a self,
        _: &'a MutationRequest,
        _: &'a S,
        _: &'a dyn Cancellation,
    ) -> BoxFuture<'a, OperationResult<MutationRead, FileReadError>> {
        Box::pin(async { Err(OperationError::Failed(denied())) })
    }
    fn write_for_mutation<'a>(
        &'a self,
        _: &'a MutationRequest,
        _: &'a [u8],
        _: &'a S,
        _: &'a dyn Cancellation,
    ) -> BoxFuture<'a, OperationResult<MutationTarget, FileReadError>> {
        Box::pin(async { Err(OperationError::Failed(denied())) })
    }
}
struct TurnBarrier {
    entered: tokio::sync::Notify,
    release: tokio::sync::Semaphore,
}
impl Default for TurnBarrier {
    fn default() -> Self {
        Self {
            entered: tokio::sync::Notify::new(),
            release: tokio::sync::Semaphore::new(0),
        }
    }
}
type TurnBarriers = Arc<Mutex<BTreeMap<String, Arc<TurnBarrier>>>>;

struct LocalProvider {
    requests: Arc<Mutex<Vec<Request>>>,
    scripts: integrated_adapters::Scripts,
    turn_barriers: TurnBarriers,
}
impl LocalProvider {
    fn models() -> Vec<Model> {
        ["model", "model-next"]
            .into_iter()
            .map(|id| {
                let mut model = Model::new(id, "local");
                model.context_window = 128_000;
                model.max_output_tokens = 4_096;
                model.capabilities.tools = true;
                model
            })
            .collect()
    }
}
impl Provider for LocalProvider {
    fn id(&self) -> &str {
        "local"
    }
    fn call(&self, request: Request) -> Result<Box<dyn EventStream>, Error> {
        LocalSession(
            self.requests.clone(),
            self.scripts.clone(),
            self.turn_barriers.clone(),
        )
        .call(request)
    }
    fn has_credential<'a>(&'a self, _: &'a dyn Cancellation) -> BoxFuture<'a, Result<bool, Error>> {
        Box::pin(async { Ok(false) })
    }
    fn seed_models(&self) -> Vec<Model> {
        Self::models()
    }
    fn list_models<'a>(
        &'a self,
        _: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<Vec<Model>, Error>> {
        Box::pin(async { Ok(Self::models()) })
    }
    fn calculate_image_tokens(&self, _: &Model, _: &Content) -> i64 {
        0
    }
    fn open_session(&self) -> Result<Arc<dyn Session>, Error> {
        Ok(Arc::new(LocalSession(
            self.requests.clone(),
            self.scripts.clone(),
            self.turn_barriers.clone(),
        )))
    }
}
struct LocalSession(
    Arc<Mutex<Vec<Request>>>,
    integrated_adapters::Scripts,
    TurnBarriers,
);
impl Session for LocalSession {
    fn begin_turn(&self) -> BoxFuture<'_, Result<(), Error>> {
        Box::pin(async { Ok(()) })
    }
    fn call(&self, request: Request) -> Result<Box<dyn EventStream>, Error> {
        let scripted = integrated_adapters::scripted_stream(&self.1, &request);
        let barrier = {
            let mut barriers = self.2.lock().unwrap();
            let marker = barriers
                .keys()
                .find(|marker| {
                    request
                        .messages
                        .iter()
                        .any(|message| message.content().contains(marker.as_str()))
                })
                .cloned();
            marker.and_then(|marker| barriers.remove(&marker))
        };
        self.0.lock().unwrap().push(request);
        if let Some(stream) = scripted {
            return Ok(stream);
        }
        Ok(Box::new(LocalStream {
            event: Some(ProviderEvent::Completed(Completion {
                response_id: "local-response".into(),
                finish_reason: "stop".into(),
                input_tokens: 100,
                output_tokens: 5,
                assistant_text: if barrier.is_some() {
                    "private-response-sentinel".into()
                } else {
                    "deterministic reply".into()
                },
                provider_id: "local".into(),
                ..Default::default()
            })),
            barrier,
        }))
    }
    fn try_fall_back_to_http(&self) -> BoxFuture<'_, Result<bool, Error>> {
        Box::pin(async { Ok(false) })
    }
    fn close(&self) -> BoxFuture<'_, Result<(), Error>> {
        Box::pin(async { Ok(()) })
    }
}
struct LocalStream {
    event: Option<ProviderEvent>,
    barrier: Option<Arc<TurnBarrier>>,
}
impl EventStream for LocalStream {
    fn next<'a>(
        &'a mut self,
        cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<Option<ProviderEvent>, Error>> {
        Box::pin(async move {
            if let Some(barrier) = self.barrier.take() {
                barrier.entered.notify_one();
                tokio::select! {
                    permit = barrier.release.acquire() => permit.unwrap().forget(),
                    _ = cancellation.cancelled() => return Err(Error::Cancelled),
                }
            }
            cancellation.check().map_err(|_| Error::Cancelled)?;
            Ok(self.event.take())
        })
    }
    fn close(&mut self) -> BoxFuture<'_, Result<(), Error>> {
        self.event = None;
        Box::pin(async { Ok(()) })
    }
}
#[derive(Clone)]
struct RoutingPrompt(Arc<Mutex<Vec<ResolvedModelSelection>>>);
impl SystemPromptProvider for RoutingPrompt {
    fn key(&self) -> &str {
        "fixture:live-routing"
    }
    fn materialize(&self, _: &AgentIdentityDescription) -> PromptResult<Box<dyn SystemPrompt>> {
        Ok(Box::new(self.clone()))
    }
}
impl SystemPrompt for RoutingPrompt {
    fn renew_epoch(&mut self) {}
    fn build(&mut self, selection: &ResolvedModelSelection) -> PromptResult<String> {
        self.0.lock().unwrap().push(selection.clone());
        Ok(format!(
            "routing selector={} canonical={} revision={}",
            selection.requested_selector.value,
            selection.canonical_model.selector(),
            selection.routing_snapshot.revision
        ))
    }
}
#[derive(Default)]
struct StatusBuildBarrier {
    entered: tokio::sync::Notify,
    release: tokio::sync::Notify,
    cancelled: std::sync::atomic::AtomicBool,
    finished: std::sync::atomic::AtomicBool,
}
#[derive(Clone)]
struct BlockingStatusPrompt(Arc<StatusBuildBarrier>);
impl SystemPromptProvider for BlockingStatusPrompt {
    fn key(&self) -> &str {
        "fixture:status-live-build"
    }
    fn materialize(&self, _: &AgentIdentityDescription) -> PromptResult<Box<dyn SystemPrompt>> {
        Ok(Box::new(self.clone()))
    }
}
impl SystemPrompt for BlockingStatusPrompt {
    fn renew_epoch(&mut self) {}
    fn build(&mut self, _: &ResolvedModelSelection) -> PromptResult<String> {
        Ok("actual status live-build barrier".into())
    }
    fn build_live<'a>(
        &'a mut self,
        selection: &'a ResolvedModelSelection,
        cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, PromptResult<String>> {
        Box::pin(async move {
            self.0.entered.notify_one();
            // Actual live work must finish cleanup even after its observer leaves.
            self.0.release.notified().await;
            self.0.cancelled.store(
                cancellation.is_cancelled(),
                std::sync::atomic::Ordering::SeqCst,
            );
            self.0
                .finished
                .store(true, std::sync::atomic::Ordering::SeqCst);
            cancellation
                .check()
                .map_err(|_| parrot_runtime::prompt_context::PromptError::Cancelled)?;
            self.build(selection)
        })
    }
}
struct NoExternal;
impl CapturedProcessLauncher for NoExternal {
    type Request = CommandRequest;
    type Security = SharedSecurity<Security>;
    fn launch(
        &self,
        _: CommandRequest,
        _: Self::Security,
        _: Arc<ProcessLifetime>,
    ) -> BoxFuture<'_, io::Result<Arc<dyn Execution>>> {
        Box::pin(async {
            Err(io::Error::other(
                "synthetic fixture does not launch processes",
            ))
        })
    }
}
impl Resolver for NoExternal {
    fn resolve<'a>(&'a self, _: &'a str) -> BoxFuture<'a, io::Result<Vec<IpAddr>>> {
        Box::pin(async { Err(io::Error::other("synthetic fixture does not resolve hosts")) })
    }
}
impl Transport for NoExternal {
    fn session(&self) -> Result<Box<dyn TransportSession + '_>, FetchError> {
        Err(FetchError::Io(io::Error::other(
            "synthetic fixture does not fetch",
        )))
    }
}
impl GzipDecoder for NoExternal {
    fn decode(&self, _: Box<dyn ResponseBody>) -> Result<Box<dyn ResponseBody>, FetchError> {
        Err(FetchError::Io(io::Error::other(
            "synthetic fixture does not decode",
        )))
    }
}
impl PermissionServices for NoExternal {
    type Security = SharedSecurity<Security>;
    type Selection = ContextSelection<Security>;
    fn resolve_target(&self, requested: &str) -> Result<WriteTarget, Failure> {
        Ok(WriteTarget {
            path: requested.into(),
            kind: WriteTargetKind::File,
        })
    }
    fn allows_write(&self, _: &Self::Security, _: &WriteTarget) -> bool {
        false
    }
    fn is_read_only(&self, _: &Self::Security) -> bool {
        true
    }
    fn request<'a>(
        &'a self,
        _: &'a str,
        _: Vec<WriteTarget>,
        _: &'a dyn Cancellation,
    ) -> BoxFuture<'a, OperationResult<PermissionReply, Failure>> {
        Box::pin(async {
            Err(OperationError::Failed(Failure::new(
                FailureKind::InvalidOperation,
                "synthetic permission supplier never grants access",
            )))
        })
    }
}
#[derive(Default)]
struct FixtureClock {
    on_utc: Mutex<Option<Box<dyn FnOnce() + Send>>>,
    frozen: Mutex<Option<Instant>>,
}
impl Clock for FixtureClock {
    fn now(&self) -> Instant {
        self.frozen.lock().unwrap().unwrap_or_else(Instant::now)
    }
    fn utc_now(&self) -> SystemTime {
        let hook = self.on_utc.lock().unwrap().take();
        if let Some(hook) = hook {
            hook();
        }
        SystemTime::now()
    }
    fn sleep_until<'a>(
        &'a self,
        deadline: Instant,
        cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<(), Cancelled>> {
        Box::pin(async move {
            tokio::select! {
                _ = tokio::time::sleep_until(deadline.into()) => Ok(()),
                _ = cancellation.cancelled() => Err(Cancelled),
            }
        })
    }
}
struct Diagnostics;
impl ChildDiagnostics for Diagnostics {
    fn record(&self, _: ChildDiagnostic) {}
}
impl InteractionDiagnostics for Diagnostics {
    fn write(&self, _: BrokerDiagnostic) {}
}
impl QueueDiagnostics for Diagnostics {
    fn record(&self, _: QueueDiagnostic) {}
}
struct Policies;
impl RetainedModePolicyMaterializer<Security> for Policies {
    fn for_agent(
        &self,
        raw: &Arc<Security>,
        _: &AgentScratchDirectory,
    ) -> ModeResult<Arc<Security>> {
        Ok(raw.clone())
    }
}
impl ProfileResolver<Security> for Policies {
    fn resolve_child(&self, _: &str) -> ChildResult<ChildProfile<Security>> {
        Err(ChildError::registry(
            "this constructor fixture does not admit child profiles",
        ))
    }
}
impl IdGenerator for Policies {
    fn agent_session(&self) -> ChildResult<String> {
        Identifier::agent_session().map_err(|e| ChildError::registry(e.to_string()))
    }
    fn message(&self) -> ChildResult<String> {
        Identifier::message_id().map_err(|e| ChildError::registry(e.to_string()))
    }
}

#[derive(Default)]
struct Observations {
    user: Option<Arc<UserServices<Security>>>,
    identity: Option<Arc<AgentIdentity>>,
    selection: Option<Arc<dyn SelectionSource<Security>>>,
    bound: Option<Arc<Scope<Security>>>,
    constructors: usize,
    construction: Option<(String, Arc<TurnBarrier>)>,
}
struct ObservedSource {
    actual: Arc<dyn AgentFactorySource<Security>>,
    observations: Arc<Mutex<Observations>>,
}
struct ObservedFactory {
    actual: Arc<dyn ScopeFactory<Security>>,
    observations: Arc<Mutex<Observations>>,
}
impl ScopeFactory<Security> for ObservedFactory {
    fn create(
        &self,
        prepared: PreparedScope<Security>,
    ) -> BoxFuture<'_, ChildResult<ScopeParts<Security>>> {
        let construction = {
            let mut observed = self.observations.lock().unwrap();
            if prepared.identity.session.depth == 0 {
                observed.identity = Some(prepared.identity.clone());
                observed.selection = Some(prepared.selection.clone());
            }
            observed.constructors += 1;
            if observed
                .construction
                .as_ref()
                .is_some_and(|(name, _)| name == &prepared.identity.session.name)
            {
                observed.construction.take().map(|(_, gate)| gate)
            } else {
                None
            }
        };
        Box::pin(async move {
            let parts = self.actual.create(prepared).await?;
            if let Some(construction) = construction {
                construction.entered.notify_one();
                construction.release.acquire().await.unwrap().forget();
            }
            Ok(parts)
        })
    }
}
impl AgentFactorySource<Security> for ObservedSource {
    fn create(
        &self,
        user: Arc<UserServices<Security>>,
    ) -> protocol::Result<AgentFactoryParts<Security>> {
        self.observations.lock().unwrap().user = Some(user.clone());
        let actual = self.actual.create(user.clone())?;
        let observations = self.observations.clone();
        Ok(AgentFactoryParts {
            factory: Arc::new(ObservedFactory {
                actual: actual.factory,
                observations: observations.clone(),
            }),
            bind: Arc::new(move |scope| {
                if scope.identity().session.depth == 0 {
                    assert!(
                        user.registry()?.snapshot_roots().is_empty(),
                        "post-bind precedes root registration"
                    );
                } else {
                    let parent = scope.parent_scope().parent().expect("child parent scope");
                    assert_eq!(
                        scope.identity().session.depth,
                        parent.identity().session.depth + 1
                    );
                    assert_eq!(
                        scope.identity().session.parent_session_id,
                        parent.identity().session.session_id
                    );
                }
                (actual.bind)(scope)?;
                if scope.identity().session.depth == 0 {
                    observations.lock().unwrap().bound = Some(scope.clone());
                }
                Ok(())
            }),
        })
    }
}

impl SessionModelRouter for Policies {
    fn resolve(&self, selector: &str) -> Result<SessionModelSelection, SessionStoreError> {
        Ok(SessionModelSelection {
            provider_id: "local".into(),
            canonical_model: "local/model".into(),
            selector: selector.into(),
        })
    }
}
impl SessionWorkspaceResolver for Policies {
    fn resolve(&self, launch_directory: &Path) -> Result<PathBuf, SessionStoreError> {
        Ok(launch_directory.into())
    }
}

#[derive(Default)]
struct RefreshBarrier {
    entered: tokio::sync::Notify,
    release: tokio::sync::Notify,
    cancelled: std::sync::atomic::AtomicBool,
    finished: std::sync::atomic::AtomicBool,
    panic_after_release: std::sync::atomic::AtomicBool,
}
struct ObservedSkills {
    actual: CatalogUserSkills<Files<Security>>,
    barrier: Option<Arc<RefreshBarrier>>,
}
impl UserSkills<Security> for ObservedSkills {
    fn refresh<'a>(
        &'a self,
        security: &'a Security,
        cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, protocol::Result<parrot_runtime::prompt_context::SkillSnapshot>> {
        Box::pin(async move {
            let snapshot = self.actual.refresh(security, cancellation).await?;
            if let Some(barrier) = &self.barrier {
                let nested_barrier = barrier.clone();
                let cleanup = tokio::spawn(async move {
                    nested_barrier.entered.notify_one();
                    nested_barrier.release.notified().await;
                    assert!(
                        !nested_barrier
                            .panic_after_release
                            .load(std::sync::atomic::Ordering::SeqCst),
                        "synthetic actual refresh cleanup panic"
                    );
                    nested_barrier
                        .finished
                        .store(true, std::sync::atomic::Ordering::SeqCst);
                });
                cleanup.await.unwrap();
                barrier.cancelled.store(
                    cancellation.is_cancelled(),
                    std::sync::atomic::Ordering::SeqCst,
                );
            }
            Ok(snapshot)
        })
    }
    fn configure<'a>(
        &'a self,
        security: &'a Security,
        path: &'a str,
        enabled: bool,
        cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, protocol::Result<parrot_runtime::prompt_context::SkillMetadata>> {
        self.actual.configure(security, path, enabled, cancellation)
    }
    fn new_agent(
        &self,
        selection: Arc<dyn SelectionSource<Security>>,
    ) -> BoxFuture<'_, parrot_runtime::prompt_context::PromptResult<AgentSkillBindings>> {
        self.actual.new_agent(selection)
    }
}

struct Fixture {
    root: PathBuf,
    store: SessionStore,
    factory: Arc<UserSessionFactory<Security>>,
    observations: Arc<Mutex<Observations>>,
    configuration: Arc<Configuration>,
    routing: Arc<ModelRouting>,
    router: Arc<ModelRouter>,
    registry: Arc<ProviderRegistry>,
    requests: Arc<Mutex<Vec<Request>>>,
    prompt_selections: Arc<Mutex<Vec<ResolvedModelSelection>>>,
    clock: Arc<FixtureClock>,
    scripts: integrated_adapters::Scripts,
    turn_barriers: TurnBarriers,
}
impl Fixture {
    async fn new(reject_permission: bool, invalid_prompt: bool) -> Self {
        Self::with_refresh_barrier(reject_permission, invalid_prompt, None).await
    }
    async fn with_refresh_barrier(
        reject_permission: bool,
        invalid_prompt: bool,
        refresh_barrier: Option<Arc<RefreshBarrier>>,
    ) -> Self {
        Self::with_barriers(reject_permission, invalid_prompt, refresh_barrier, None).await
    }
    async fn with_barriers(
        reject_permission: bool,
        invalid_prompt: bool,
        refresh_barrier: Option<Arc<RefreshBarrier>>,
        status_barrier: Option<Arc<StatusBuildBarrier>>,
    ) -> Self {
        Self::with_profiles(
            reject_permission,
            invalid_prompt,
            refresh_barrier,
            status_barrier,
            Arc::new(Policies),
        )
        .await
    }
    async fn with_profiles(
        reject_permission: bool,
        invalid_prompt: bool,
        refresh_barrier: Option<Arc<RefreshBarrier>>,
        status_barrier: Option<Arc<StatusBuildBarrier>>,
        profiles: Arc<dyn ProfileResolver<Security>>,
    ) -> Self {
        Self::with_config(
            reject_permission,
            invalid_prompt,
            refresh_barrier,
            status_barrier,
            profiles,
            |_| {},
        )
        .await
    }
    async fn with_config(
        reject_permission: bool,
        invalid_prompt: bool,
        refresh_barrier: Option<Arc<RefreshBarrier>>,
        status_barrier: Option<Arc<StatusBuildBarrier>>,
        profiles: Arc<dyn ProfileResolver<Security>>,
        configure: impl FnOnce(&mut UserSessionFactoryConfig<Security>),
    ) -> Self {
        let root =
            std::env::temp_dir().join(format!("t62-scope-{}", Identifier::event_id().unwrap()));
        let workspace = root.join("workspace");
        std::fs::create_dir_all(&workspace).unwrap();
        let runtime = RuntimeIdentity::new(
            "host".into(),
            "boot".into(),
            123,
            "start".into(),
            "runtime".into(),
            Arc::new(|_| ProcessIdentity {
                status: ProcessIdentityStatus::Found,
                process_start_token: Some("start".into()),
            }),
        )
        .with_proven_comparable_process_start_tokens();
        let admissions = Arc::new(SessionAdmissions::new(
            OwnershipRoots::new(root.join("new"), root.join("old")).unwrap(),
            runtime,
            true,
        ));
        let global = Arc::new(
            FileDiagnosticLog::open_global(root.join("logs"), "scope-test", |_| {}).unwrap(),
        );
        let requests = Arc::new(Mutex::new(Vec::new()));
        let scripts = integrated_adapters::Scripts::default();
        let turn_barriers = TurnBarriers::default();
        let registry = Arc::new(
            ProviderRegistry::new(
                vec![Arc::new(LocalProvider {
                    requests: requests.clone(),
                    scripts: scripts.clone(),
                    turn_barriers: turn_barriers.clone(),
                })],
                BTreeMap::from([("local".into(), LocalProvider::models())]),
            )
            .unwrap(),
        );
        let aliases = Arc::new(
            ModelAliasCatalog::new(
                registry.clone(),
                vec![ModelAliasDefinition::new(
                    "live",
                    "local/model",
                    "synthetic live routing",
                )],
            )
            .unwrap(),
        );
        let routing = Arc::new(ModelRouting::new(aliases, "local/model"));
        let router = Arc::new(ModelRouter::new(registry.clone(), routing.clone()));
        let (configuration, templates) = PromptTemplateCatalog::from_content(
            "",
            &BTreeMap::from([("HOME".into(), root.to_string_lossy().into_owned())]),
        )
        .unwrap();
        let configuration = Arc::new(configuration);
        let templates = Arc::new(templates);
        let profile = MaterializedModeProfile::new(
            "build".into(),
            &configuration.runtime.profiles["build"],
            &[],
            Arc::new(Security),
        )
        .unwrap();
        let modes = Arc::new(ModeRegistry::new(vec![profile], "build").unwrap());
        let observations = Arc::new(Mutex::new(Observations::default()));
        let prompt_selections = Arc::new(Mutex::new(Vec::new()));
        let mut prompts: Vec<Arc<dyn SystemPromptProvider>> = if invalid_prompt {
            vec![Arc::new(StaticSystemPromptProvider::new(
                "invalid", "unused",
            ))]
        } else {
            vec![Arc::new(RoutingPrompt(prompt_selections.clone()))]
        };
        if let Some(barrier) = status_barrier {
            prompts.push(Arc::new(BlockingStatusPrompt(barrier)));
        }
        let clock = Arc::new(FixtureClock::default());
        let actual = AuthoritativeAgentSource::new(AgentSourceConfig {
            configuration: configuration.clone(),
            router: router.clone(),
            filesystem: Files::<SharedSecurity<Security>>(PhantomData),
            launcher: Arc::new(NoExternal),
            permission_authority: Arc::new(move |_, _| {
                if reject_permission {
                    Err(RpcError::new(
                        RpcCode::Internal,
                        "synthetic permission supplier failure",
                    ))
                } else {
                    Ok(Arc::new(NoExternal))
                }
            }),
            web_fetcher: Arc::new(WebFetcher::new(
                Arc::new(PublicAddressPolicy),
                Arc::new(NoExternal),
                Arc::new(NoExternal),
                Arc::new(NoExternal),
                Options::default(),
            )),
            prompts,
            clock: clock.clone(),
            profiles: ModelProfiles::new(&BTreeMap::new()),
        });
        let skill_templates = templates.clone();
        let mut config = UserSessionFactoryConfig {
            agent_limits: parrot_foundation::config::runtime::AgentLimitsConfig {
                disable_recursion_limit: false,
                ..Default::default()
            },
            router: router.clone(),
            modes,
            workflows: Vec::new(),
            materializer: Arc::new(Policies),
            source: Arc::new(ObservedSource {
                actual: Arc::new(actual),
                observations: observations.clone(),
            }),
            profiles,
            ids: Arc::new(Policies),
            diagnostics: Arc::new(Diagnostics),
            interaction_diagnostics: Arc::new(Diagnostics),
            templates,
            launch_directory: workspace.clone(),
            workspace: Arc::new(move |path| {
                Ok(ProjectWorkspace::from_resolved(
                    path.into(),
                    path.into(),
                    None,
                    None,
                ))
            }),
            skills: Arc::new(move |workspace| {
                let reader = Arc::new(Files::<Security>(PhantomData));
                Ok(Arc::new(ObservedSkills {
                    actual: CatalogUserSkills {
                        catalog: Arc::new(SkillCatalog::new(
                            reader.clone(),
                            Vec::new(),
                            SkillDiscoveryContext {
                                working_directory: workspace.launch_directory().into(),
                                platform: PathPlatform::Unix,
                            },
                            Arc::new(|| (SkillConfiguration::default(), 0)),
                        )),
                        reader,
                        templates: skill_templates.clone(),
                        working_directory: workspace
                            .launch_directory()
                            .to_string_lossy()
                            .into_owned(),
                    },
                    barrier: refresh_barrier.clone(),
                }))
            }),
            permissions: durable_permission_factory(None, Arc::new(Diagnostics)),
            user_input_timeout: None,
        };
        configure(&mut config);
        let factory = Arc::new(UserSessionFactory::new(config));
        let store = SessionStore::new(
            admissions,
            workspace,
            "build".into(),
            factory.clone(),
            Arc::new(Policies),
            Arc::new(Policies),
            global,
        );
        Self {
            root,
            store,
            factory,
            observations,
            configuration,
            routing,
            router,
            registry,
            requests,
            prompt_selections,
            clock,
            scripts,
            turn_barriers,
        }
    }
    async fn open(&self) -> protocol::Result<Arc<dyn HostedSession>> {
        self.open_with_selector("local/model").await
    }
    async fn open_with_selector(&self, selector: &str) -> protocol::Result<Arc<dyn HostedSession>> {
        let opened = self
            .store
            .create_fresh(
                SessionModelRouter::resolve(&Policies, selector).unwrap(),
                "build",
                true,
            )
            .await
            .map_err(|error| RpcError::new(RpcCode::Internal, error.to_string()))?;
        self.factory.bind_opened(opened).await
    }
    fn user(&self) -> Arc<UserServices<Security>> {
        self.observations.lock().unwrap().user.clone().unwrap()
    }
    fn root(&self) -> Arc<Scope<Security>> {
        self.observations.lock().unwrap().bound.clone().unwrap()
    }
}

#[tokio::test]
async fn actual_factory_binds_same_identity_selection_scheduler_and_task_owner_before_registration()
{
    let fixture = Fixture::new(false, false).await;
    let hosted = fixture.open().await.unwrap();
    let root = fixture.root();
    let user = fixture.user();
    {
        let observed = fixture.observations.lock().unwrap();
        assert_eq!(observed.constructors, 1);
        assert!(Arc::ptr_eq(
            root.identity(),
            observed.identity.as_ref().unwrap()
        ));
        let selection: Arc<dyn SelectionSource<Security>> = root.selection().clone();
        assert!(Arc::ptr_eq(
            &selection,
            observed.selection.as_ref().unwrap()
        ));
        assert!(Arc::ptr_eq(&selection, root.session().selection()));
        assert!(Arc::ptr_eq(root.identity(), root.session().identity()));
        assert!(Arc::ptr_eq(root.identity(), &user.root_identity));
    }
    let registry = user.registry().unwrap();
    assert!(registry.contains(&root));
    assert_eq!(registry.snapshot_roots().len(), 1);
    let session = root.get_service::<RuntimeSession<Security>>().unwrap();
    let services = root
        .get_service::<AgentSessionServices<Security>>()
        .unwrap();
    assert!(Arc::ptr_eq(&session, &services.session));
    let tasks = root
        .get_service::<ScopedTaskController<Security, ContextSelection<Security>>>()
        .unwrap();
    let scheduler = root
        .get_service::<TaskScheduler<Security, TaskArtifactParserAdapter>>()
        .unwrap();
    assert!(Arc::ptr_eq(tasks.scheduler(), &scheduler));
    let owner = root.get_service::<dyn TaskSession>().unwrap();
    assert!(Arc::ptr_eq(owner.identity(), root.identity()));
    assert!(Arc::ptr_eq(
        &owner,
        &root.get_service::<dyn TaskSession>().unwrap()
    ));
    owner
        .record(
            vec![ConversationPart::text("same task owner record")],
            "task-owner-record".into(),
            parrot_domain::protocol::messages::parrot::Delivery::DELIVERY_STEER,
        )
        .await
        .unwrap();
    assert!(user.lease.events.replay().await.unwrap().iter().any(
        |event| event.has_input_admitted()
            && event.input_admitted().message_id == "task-owner-record"
            && event.agent_session_id == root.identity().session.session_id
    ));
    assert!(root.get_service::<ShellProcessOwner>().is_ok());
    assert!(services.active_work.build().await.unwrap().is_none());
    tokio::time::timeout(Duration::from_secs(10), hosted.close())
        .await
        .expect("actual hosted shutdown timed out")
        .unwrap();
    assert!(!registry.is_accepting());
    assert!(session.close().await.is_ok());
    assert!(
        user.lease.is_cancelled(),
        "actual HostedCore closed the storage lease"
    );
}

#[tokio::test]
async fn actual_scoped_queue_tool_publishes_inventory_and_blocks_completion_until_drained() {
    let fixture = Fixture::new(false, false).await;
    let hosted = fixture.open().await.unwrap();
    let root = fixture.root();
    let user = fixture.user();
    let registry = user.registry().unwrap();
    let queues = root.get_service::<AgentQueues<Security>>().unwrap();
    let services = root
        .get_service::<AgentSessionServices<Security>>()
        .unwrap();
    let definitions = catalog::configured_definitions(&fixture.configuration);
    let subscription = user
        .broker
        .listen(
            None,
            || async { Ok::<_, RpcError>(Vec::new()) },
            || async { Ok::<_, RpcError>(Vec::new()) },
        )
        .await
        .unwrap();
    for (kind, input) in [
        (QueueToolKind::Create, json!({"name":"actual-work"})),
        (
            QueueToolKind::Push,
            json!({"name":"actual-work", "items":["actual payload"]}),
        ),
    ] {
        let tool = QueueTool::new(
            kind,
            &definitions,
            Arc::new(ScopedQueueBackend::new(root.clone(), registry.clone()).unwrap()),
            Arc::new(Files::<Security>(PhantomData)),
            Arc::new(FixtureClock::default()),
            Arc::new(Diagnostics),
        )
        .unwrap();
        let output = tool
            .invoke(
                ToolInvocation {
                    call_id: "queue-tool-call".into(),
                    arguments_json: input.to_string(),
                    assistant_sequence: 1,
                },
                InvocationContext::new(Security, (), &NeverCancelled),
            )
            .await
            .unwrap();
        let value: Value = serde_json::from_str(&output.text).unwrap();
        assert_eq!(value["name"], "actual-work");
    }
    assert!(Arc::ptr_eq(
        &queues,
        &root.get_service::<AgentQueues<Security>>().unwrap()
    ));
    assert!(
        services
            .active_work
            .build()
            .await
            .unwrap()
            .unwrap()
            .contains("actual-work")
    );
    let tool = QueueTool::new(
        QueueToolKind::Take,
        &definitions,
        Arc::new(ScopedQueueBackend::new(root.clone(), registry.clone()).unwrap()),
        Arc::new(Files::<Security>(PhantomData)),
        Arc::new(FixtureClock::default()),
        Arc::new(Diagnostics),
    )
    .unwrap();
    let output = tool
        .invoke(
            ToolInvocation {
                call_id: "queue-take-call".into(),
                arguments_json: json!({"name":"actual-work", "yield_after_ms":0}).to_string(),
                assistant_sequence: 1,
            },
            InvocationContext::new(Security, (), &NeverCancelled),
        )
        .await
        .unwrap();
    let value: Value = serde_json::from_str(&output.text).unwrap();
    assert_eq!(value["items"], json!(["actual payload"]));
    assert!(services.active_work.build().await.unwrap().is_none());
    tokio::time::timeout(Duration::from_secs(10), hosted.close())
        .await
        .expect("actual hosted shutdown timed out")
        .unwrap();
    let mut inventories = 0;
    while let Some(event) = subscription.next(&NeverCancelled).await.unwrap() {
        if event.has_tool_snapshot() && event.tool_snapshot().kind == "queue" {
            inventories += 1;
        }
    }
    assert!(
        inventories > 0,
        "actual queue publisher ran and broker completed after scope close"
    );
    user.lease.close().await.unwrap();
}

#[tokio::test]
async fn actual_constructor_rejections_do_not_register_or_keep_owned_producers_alive() {
    for (reject_permission, invalid_prompt, expected) in [
        (true, false, "synthetic permission supplier failure"),
        (
            false,
            true,
            "A system prompt provider key must be namespaced",
        ),
    ] {
        let fixture = Fixture::new(reject_permission, invalid_prompt).await;
        let result = fixture.open().await;
        assert!(result.is_err());
        assert!(result.err().unwrap().to_string().contains(expected));
        let user = fixture.user();
        assert!(fixture.observations.lock().unwrap().bound.is_none());
        assert!(user.lifetime.is_cancelled());
        assert!(user.permissions.pending().await.is_empty());
        if let Ok(registry) = user.registry() {
            assert!(!registry.is_accepting());
            assert!(registry.snapshot_scopes().is_empty());
        }
        let stream = user
            .broker
            .listen(
                None,
                || async { Ok::<_, RpcError>(Vec::new()) },
                || async { Ok::<_, RpcError>(Vec::new()) },
            )
            .await
            .unwrap();
        assert!(stream.next(&NeverCancelled).await.unwrap().is_none());
        assert!(
            user.lease.is_cancelled(),
            "actual SessionStore closed rejected construction resources"
        );
    }
}

#[tokio::test]
async fn opaque_actual_constructor_failure_keeps_resources_and_activation_fenced_without_attestation()
 {
    use parrot_domain::values::identifiers::UserSessionId;
    use parrot_storage::ownership::ClaimDisposition;

    let (release, barrier) = tokio::sync::oneshot::channel();
    let (started, entered) = tokio::sync::oneshot::channel();
    let gate = Arc::new(Mutex::new(Some((started, barrier))));
    let admitted = Arc::new(Mutex::new(None));
    let captured = admitted.clone();
    let fixture = Fixture::with_config(
        false,
        false,
        None,
        None,
        Arc::new(Policies),
        move |config| {
            config.permissions = Arc::new(move |lease, _, _| {
                let (started, barrier) = gate.lock().unwrap().take().unwrap();
                let retained = lease.clone();
                let producer = lease.runtime_handle().spawn(async move {
                    let _ = started.send(());
                    let _ = barrier.await;
                    retained.events.replay().await
                });
                *captured.lock().unwrap() = Some((lease, producer));
                Err(RpcError::new(
                    RpcCode::Internal,
                    "synthetic opaque supplier failure after admitting a producer",
                ))
            });
        },
    )
    .await;
    assert!(bounded(fixture.open()).await.is_err());
    bounded(entered).await.unwrap();
    let (lease, producer) = admitted.lock().unwrap().take().unwrap();
    assert!(!producer.is_finished());
    assert!(
        lease.is_cancelled(),
        "request-close flag does not attest settlement"
    );
    let first = bounded(lease.close()).await.unwrap_err().to_string();
    let repeated = bounded(lease.close()).await.unwrap_err().to_string();
    assert_eq!(
        first, repeated,
        "unknown construction keeps a stable failure, not success"
    );
    bounded(lease.events.replay()).await.unwrap();
    let id = UserSessionId::parse(
        lease
            .selected_root()
            .path()
            .file_name()
            .unwrap()
            .to_str()
            .unwrap(),
    )
    .unwrap();
    assert!(matches!(
        bounded(fixture.store.resume(&id, true, false)).await,
        Err(SessionStoreError::Admission(ClaimDisposition::Live))
    ));
    release.send(()).unwrap();
    bounded(producer).await.unwrap().unwrap();
}

#[tokio::test]
async fn actual_output_appends_to_stable_file_and_close_rejects_later_writes() {
    let root: PathBuf =
        std::env::temp_dir().join(format!("t62-output-{}", Identifier::event_id().unwrap()));
    let output = AgentOutput::open(&root).unwrap();
    output.append("first").await.unwrap();
    output.append(" second").await.unwrap();
    assert_eq!(output.path(), root.join("output-arse.dat"));
    assert_eq!(
        std::fs::read_to_string(output.path()).unwrap(),
        "first second"
    );
    output.close().await.unwrap();
    output.close().await.unwrap();
    assert!(output.append(" forbidden").await.is_err());
    let reopened = AgentOutput::open(&root).unwrap();
    reopened.append(" third").await.unwrap();
    reopened.close().await.unwrap();
    assert_eq!(
        std::fs::read_to_string(reopened.path()).unwrap(),
        "first second third"
    );
}

struct NeverGrant;
impl parrot_runtime::interactions::TargetApproval<WriteTarget> for NeverGrant {
    fn validate(
        &self,
        _: &[WriteTarget],
    ) -> Result<(), parrot_runtime::interactions::InteractionError> {
        Ok(())
    }
    fn approve(
        &self,
        _: &[WriteTarget],
    ) -> Result<(), parrot_runtime::interactions::InteractionError> {
        panic!("this constructor fixture never grants filesystem access")
    }
}
fn permission_request(
    user: &UserServices<Security>,
    reason: &str,
) -> parrot_runtime::interactions::PermissionRequest<WriteTarget> {
    parrot_runtime::interactions::PermissionRequest {
        agent_session_id: user.root_identity.session.session_id.clone(),
        reason: reason.into(),
        targets: vec![WriteTarget {
            path: "/synthetic/denied-file".into(),
            kind: WriteTargetKind::File,
        }],
        approval: Arc::new(NeverGrant),
    }
}
async fn bounded<T>(future: impl std::future::Future<Output = T>) -> T {
    tokio::time::timeout(Duration::from_secs(10), future)
        .await
        .expect("actual fixture operation timed out")
}

async fn assert_live_activation(fixture: &Fixture, user: &UserServices<Security>) {
    use parrot_domain::values::identifiers::UserSessionId;
    use parrot_storage::ownership::ClaimDisposition;

    let id = UserSessionId::parse(&user.id).unwrap();
    assert!(matches!(
        bounded(fixture.store.resume(&id, true, false)).await,
        Err(SessionStoreError::Admission(ClaimDisposition::Live))
    ));
}

fn assert_database_closed(user: &UserServices<Security>) {
    use parrot_domain::contracts::storage::AdmissionError;
    use parrot_storage::database::DatabaseError;

    assert!(matches!(
        user.lease.database.admit(
            "actual-resource-closed-after-join".to_owned(),
            &NeverCancelled,
            |_| Ok(())
        ),
        Err(AdmissionError::Rejected(DatabaseError::Closed))
    ));
}

// Current-thread execution makes the second request's started signal deterministic:
// the sender does not yield again until the actual append holds the permission gate.
#[tokio::test]
async fn dropped_hosted_permission_reply_observer_is_joined_before_actual_broker_and_lease_close() {
    use parrot_runtime::interactions::{InteractionErrorKind, PermissionDecision};
    use std::future::{Future, poll_fn};
    use std::task::Poll;
    use tokio::sync::oneshot;

    let fixture = Fixture::new(false, false).await;
    let hosted = bounded(fixture.open()).await.unwrap();
    let user = fixture.user();
    let registry = user.registry().unwrap();
    let stream = user
        .broker
        .listen(
            None,
            || async { Ok::<_, RpcError>(Vec::new()) },
            || async { Ok::<_, RpcError>(Vec::new()) },
        )
        .await
        .unwrap();
    let first_user = user.clone();
    let first = tokio::spawn(async move {
        first_user
            .permissions
            .request(
                permission_request(&first_user, "first actual request"),
                &NeverCancelled,
            )
            .await
    });
    let first_event = bounded(async {
        loop {
            let event = stream.next(&NeverCancelled).await.unwrap().unwrap();
            if event.has_permission_pending() {
                break event;
            }
        }
    })
    .await;
    let first_id = first_event.permission_pending().id.clone();
    assert_eq!(
        user.lease
            .events
            .replay()
            .await
            .unwrap()
            .iter()
            .filter(|event| event.has_permission_pending())
            .collect::<Vec<_>>(),
        vec![&first_event]
    );

    let (release, blocked) = std::sync::mpsc::channel();
    let (entered, database_entered) = oneshot::channel();
    let database_barrier = user
        .lease
        .database
        .admit(
            "hosted-permission-gate-barrier".to_owned(),
            &NeverCancelled,
            move |_| {
                entered.send(()).unwrap();
                blocked.recv().unwrap();
                Ok(())
            },
        )
        .unwrap();
    bounded(database_entered).await.unwrap();
    let (started, second_started) = oneshot::channel();
    let second_user = user.clone();
    let second = tokio::spawn(async move {
        started.send(()).unwrap();
        second_user
            .permissions
            .request(
                permission_request(&second_user, "second actual request"),
                &NeverCancelled,
            )
            .await
    });
    bounded(second_started).await.unwrap();

    // Calling the public facade admits its owned mutation immediately. Polling
    // confirms the observer waits; dropping it must not cancel that producer.
    {
        let reply = hosted.reply_permission(&first_id, "reject", "");
        tokio::pin!(reply);
        assert!(poll_fn(|context| Poll::Ready(reply.as_mut().poll(context).is_pending())).await);
        tokio::task::yield_now().await;
    }
    let replay = user.lease.events.replay();
    tokio::pin!(replay);
    assert!(poll_fn(|context| Poll::Ready(replay.as_mut().poll(context).is_pending())).await);
    let closing = hosted.close();
    tokio::pin!(closing);
    assert!(poll_fn(|context| Poll::Ready(closing.as_mut().poll(context).is_pending())).await);
    bounded(user.lifetime.cancelled()).await;
    let live_database = user
        .lease
        .database
        .admit(
            "actual-resource-live-before-join".to_owned(),
            &NeverCancelled,
            |_| Ok(()),
        )
        .unwrap();
    assert_live_activation(&fixture, &user).await;
    assert!(poll_fn(|context| Poll::Ready(closing.as_mut().poll(context).is_pending())).await);
    release.send(()).unwrap();
    bounded(database_barrier.into_completion()).await.unwrap();
    bounded(live_database.into_completion()).await.unwrap();
    let persisted = bounded(replay).await.unwrap();
    bounded(closing).await.unwrap();
    let first_reply = bounded(first).await.unwrap().unwrap();
    assert_eq!(
        first_reply.decision,
        PermissionDecision::Reject,
        "the retained public Hosted reply ran before pending-close"
    );
    let second_reply = bounded(second).await.unwrap();
    assert!(
        matches!(second_reply, Err(OperationError::Failed(error)) if error.kind == InteractionErrorKind::Permission && error.message == "permission session closed")
    );
    assert!(user.permissions.pending().await.is_empty());
    assert!(!registry.is_accepting());
    assert!(user.lease.is_cancelled());
    assert_database_closed(&user);
    let mut published = vec![first_event];
    bounded(async {
        while let Some(event) = stream.next(&NeverCancelled).await.unwrap() {
            if event.has_permission_pending() {
                published.push(event);
            }
        }
    })
    .await;
    let persisted: Vec<_> = persisted
        .into_iter()
        .filter(|event| event.has_permission_pending())
        .collect();
    assert_eq!(persisted.len(), 2);
    assert_eq!(
        published, persisted,
        "real durable append and real publication used identical events before broker completion"
    );
}

fn image_upload() -> protocol::ImageUpload {
    let content = include_bytes!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../parrot-storage/tests/artifacts/fixtures/pixel.png"
    ))
    .to_vec();
    protocol::ImageUpload {
        byte_length: content.len() as i64,
        content,
        upload_id: "actual-host-upload".into(),
        display_name: "pixel.png".into(),
        media_type: "image/png".into(),
    }
}

#[tokio::test]
async fn actual_hosted_images_questions_pending_reads_and_listen_share_the_close_fence() {
    let fixture = Fixture::new(false, false).await;
    let hosted = fixture.open().await.unwrap();
    let user = fixture.user();
    let image = bounded(hosted.persist_image(image_upload(), &NeverCancelled))
        .await
        .unwrap();
    assert_eq!((image.width, image.height, image.byte_length), (1, 1, 70));
    assert_eq!(
        bounded(hosted.resolve_image(&image.artifact_id))
            .await
            .unwrap(),
        Some(image.clone())
    );
    assert!(hosted.pending_questions().unwrap().is_empty());
    assert!(hosted.pending_permissions().await.unwrap().is_empty());
    let mut stream = hosted.listen(None, Arc::new(NeverCancelled)).unwrap();
    assert!(
        bounded(stream.next(&NeverCancelled))
            .await
            .unwrap()
            .is_some()
    );
    bounded(hosted.close()).await.unwrap();
    assert_eq!(
        hosted
            .persist_image(image_upload(), &NeverCancelled)
            .await
            .unwrap_err()
            .code,
        RpcCode::Unavailable
    );
    assert_eq!(
        hosted
            .resolve_image(&image.artifact_id)
            .await
            .unwrap_err()
            .code,
        RpcCode::Unavailable
    );
    assert_eq!(
        hosted.pending_questions().unwrap_err().code,
        RpcCode::Unavailable
    );
    assert_eq!(
        hosted.pending_permissions().await.unwrap_err().code,
        RpcCode::Unavailable
    );
    assert_eq!(
        hosted.reject_question("after-close").unwrap_err().kind,
        parrot_runtime::interactions::InteractionErrorKind::Disposed
    );
    let mut late = hosted.listen(None, Arc::new(NeverCancelled)).unwrap();
    assert_eq!(
        late.next(&NeverCancelled).await.unwrap_err().code,
        RpcCode::Unavailable
    );
    bounded(async { while stream.next(&NeverCancelled).await.unwrap().is_some() {} }).await;
    assert!(user.lease.is_cancelled());
}

#[tokio::test]
async fn actual_hosted_selection_updates_preserve_open_metadata_and_record_opened_is_explicit() {
    let fixture = Fixture::new(false, false).await;
    let hosted = fixture.open().await.unwrap();
    let user = fixture.user();
    let path = user.lease.selected_root().path().join("meta.json");
    let mut metadata: Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    metadata["LastOpenedAt"] = json!("retained-opened");
    metadata["CreatedAt"] = json!("retained-created");
    std::fs::write(&path, serde_json::to_vec(&metadata).unwrap()).unwrap();
    bounded(hosted.update(
        Some(fixture.router.resolve("local/model-next").unwrap()),
        Some("build".into()),
    ))
    .await
    .unwrap();
    let root = fixture.root();
    let snapshot = root.agent_state().load().await.unwrap().unwrap();
    assert_eq!(
        snapshot.descriptor.selection.model_selector,
        "local/model-next"
    );
    assert_eq!(
        snapshot.descriptor.selection.profile_id,
        root.selection().current().unwrap().profile.id
    );
    let updated: Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    metadata["Model"] = json!("local/model-next");
    metadata["Selector"] = json!("local/model-next");
    assert_eq!(
        updated, metadata,
        "selection-only publication preserves all existing open metadata"
    );
    hosted.record_opened().unwrap();
    let recorded: Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    assert_ne!(recorded["LastOpenedAt"], metadata["LastOpenedAt"]);
    metadata["LastOpenedAt"] = recorded["LastOpenedAt"].clone();
    assert_eq!(
        recorded, metadata,
        "only explicit successful-open publication changes last-opened"
    );
    bounded(hosted.close()).await.unwrap();
    assert_eq!(
        hosted.record_opened().unwrap_err().code,
        RpcCode::Unavailable
    );
}

#[tokio::test]
async fn actual_refresh_cancellation_returns_before_owned_cleanup_and_close_joins_it() {
    use parrot_runtime::agent_turn::lifecycle::Signal;
    use std::future::{Future, poll_fn};
    use std::sync::atomic::Ordering;
    use std::task::Poll;

    let barrier = Arc::new(RefreshBarrier::default());
    let fixture = Fixture::with_refresh_barrier(false, false, Some(barrier.clone())).await;
    let hosted = fixture.open().await.unwrap();
    let user = fixture.user();
    let cancellation = Signal::new();
    let observer = hosted.refresh_skills(&cancellation);
    tokio::pin!(observer);
    assert!(poll_fn(|context| Poll::Ready(observer.as_mut().poll(context).is_pending())).await);
    bounded(barrier.entered.notified()).await;
    cancellation.cancel();
    assert_eq!(
        bounded(observer).await.unwrap_err().code,
        RpcCode::Cancelled
    );
    let closing = hosted.close();
    tokio::pin!(closing);
    assert!(poll_fn(|context| Poll::Ready(closing.as_mut().poll(context).is_pending())).await);
    bounded(user.lifetime.cancelled()).await;
    assert!(!barrier.finished.load(Ordering::SeqCst));
    bounded(user.lease.events.replay()).await.unwrap();
    assert_live_activation(&fixture, &user).await;
    assert!(poll_fn(|context| Poll::Ready(closing.as_mut().poll(context).is_pending())).await);
    barrier.release.notify_one();
    bounded(closing).await.unwrap();
    assert!(barrier.finished.load(Ordering::SeqCst));
    assert!(barrier.cancelled.load(Ordering::SeqCst));
    assert_database_closed(&user);
}

#[tokio::test]
async fn actual_refresh_producer_panic_keeps_repeated_hosted_close_error_and_activation_fenced() {
    use parrot_domain::values::identifiers::UserSessionId;
    use parrot_storage::ownership::ClaimDisposition;
    use std::future::{Future, poll_fn};
    use std::sync::atomic::Ordering;
    use std::task::Poll;

    let barrier = Arc::new(RefreshBarrier::default());
    barrier.panic_after_release.store(true, Ordering::SeqCst);
    let fixture = Fixture::with_refresh_barrier(false, false, Some(barrier.clone())).await;
    let hosted = fixture.open().await.unwrap();
    let user = fixture.user();
    {
        let observer = hosted.refresh_skills(&NeverCancelled);
        tokio::pin!(observer);
        assert!(poll_fn(|context| Poll::Ready(observer.as_mut().poll(context).is_pending())).await);
        bounded(barrier.entered.notified()).await;
    }
    let closing = hosted.close();
    tokio::pin!(closing);
    assert!(poll_fn(|context| Poll::Ready(closing.as_mut().poll(context).is_pending())).await);
    bounded(user.lifetime.cancelled()).await;
    bounded(user.lease.events.replay()).await.unwrap();
    assert_live_activation(&fixture, &user).await;
    assert!(poll_fn(|context| Poll::Ready(closing.as_mut().poll(context).is_pending())).await);
    barrier.release.notify_one();
    let failure = bounded(closing).await.unwrap_err();
    assert_eq!(bounded(hosted.close()).await.unwrap_err(), failure);
    assert!(bounded(user.lease.close()).await.is_err());
    bounded(user.lease.events.replay()).await.unwrap();
    let id = UserSessionId::parse(&user.id).unwrap();
    assert!(matches!(
        bounded(fixture.store.resume(&id, true, false)).await,
        Err(SessionStoreError::Admission(ClaimDisposition::Live))
    ));
}

#[test]
fn actual_registered_hosted_close_survives_close_observer_and_caller_runtime_drop() {
    use std::future::{Future, poll_fn};
    use std::sync::atomic::Ordering;
    use std::task::Poll;

    let caller = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let barrier = Arc::new(RefreshBarrier::default());
    let fixture = caller.block_on(Fixture::with_refresh_barrier(
        false,
        false,
        Some(barrier.clone()),
    ));
    let hosted = caller.block_on(fixture.open()).unwrap();
    let user = fixture.user();
    caller.block_on(async {
        let observer = hosted.refresh_skills(&NeverCancelled);
        tokio::pin!(observer);
        assert!(poll_fn(|context| Poll::Ready(observer.as_mut().poll(context).is_pending())).await);
        bounded(barrier.entered.notified()).await;
        let closing = hosted.close();
        tokio::pin!(closing);
        assert!(poll_fn(|context| Poll::Ready(closing.as_mut().poll(context).is_pending())).await);
        bounded(user.lifetime.cancelled()).await;
        bounded(user.lease.events.replay()).await.unwrap();
        assert_live_activation(&fixture, &user).await;
        assert!(poll_fn(|context| Poll::Ready(closing.as_mut().poll(context).is_pending())).await);
    });
    drop(caller);
    assert!(!barrier.finished.load(Ordering::SeqCst));
    let replacement = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    replacement.block_on(async {
        let closing = hosted.close();
        tokio::pin!(closing);
        assert!(poll_fn(|context| Poll::Ready(closing.as_mut().poll(context).is_pending())).await);
        bounded(user.lease.events.replay()).await.unwrap();
        assert_live_activation(&fixture, &user).await;
        assert!(poll_fn(|context| Poll::Ready(closing.as_mut().poll(context).is_pending())).await);
        barrier.release.notify_one();
        bounded(closing).await.unwrap();
        bounded(hosted.close()).await.unwrap();
    });
    assert!(barrier.finished.load(Ordering::SeqCst));
    assert!(user.lease.is_cancelled());
    assert_database_closed(&user);
}

#[test]
fn actual_refresh_nested_cleanup_survives_caller_runtime_drop_and_blocks_hosted_close() {
    use std::future::{Future, poll_fn};
    use std::sync::atomic::Ordering;
    use std::task::Poll;

    let caller = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let barrier = Arc::new(RefreshBarrier::default());
    let fixture = caller.block_on(Fixture::with_refresh_barrier(
        false,
        false,
        Some(barrier.clone()),
    ));
    let hosted = caller.block_on(fixture.open()).unwrap();
    let user = fixture.user();
    caller.block_on(async {
        let observer = hosted.refresh_skills(&NeverCancelled);
        tokio::pin!(observer);
        assert!(poll_fn(|context| Poll::Ready(observer.as_mut().poll(context).is_pending())).await);
        bounded(barrier.entered.notified()).await;
    });
    drop(caller);
    let replacement = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    replacement.block_on(async {
        let closing = hosted.close();
        tokio::pin!(closing);
        assert!(poll_fn(|context| Poll::Ready(closing.as_mut().poll(context).is_pending())).await);
        bounded(user.lifetime.cancelled()).await;
        assert!(!barrier.finished.load(Ordering::SeqCst));
        bounded(user.lease.events.replay()).await.unwrap();
        assert_live_activation(&fixture, &user).await;
        assert!(poll_fn(|context| Poll::Ready(closing.as_mut().poll(context).is_pending())).await);
        assert_eq!(
            hosted.pending_questions().unwrap_err().code,
            RpcCode::Unavailable
        );
        assert_eq!(
            hosted.pending_permissions().await.unwrap_err().code,
            RpcCode::Unavailable
        );
        barrier.release.notify_one();
        bounded(closing).await.unwrap();
        assert!(barrier.finished.load(Ordering::SeqCst));
        assert!(
            barrier.cancelled.load(Ordering::SeqCst),
            "dropping the borrowed observer signals its owned producer"
        );
        assert!(user.lease.is_cancelled());
        assert_database_closed(&user);
    });
}

#[tokio::test]
async fn actual_scheduler_owner_guard_accepts_registered_task_session_and_blocks_failed_work() {
    use parrot_domain::values::agent_tasks::{ExecutionStatus, Task, TaskPayload};
    use parrot_storage::conversation::HistoryForkBoundary;

    let fixture = Fixture::new(false, false).await;
    let hosted = bounded(fixture.open()).await.unwrap();
    let root = fixture.root();
    let scheduler = root
        .get_service::<TaskScheduler<Security, TaskArtifactParserAdapter>>()
        .unwrap();
    let services = root
        .get_service::<AgentSessionServices<Security>>()
        .unwrap();
    let task = Task {
        name: "actual-task".into(),
        dependencies: Vec::new(),
        description: "registered owner admission".into(),
        payload: TaskPayload::Instruction("do not invoke a provider".into()),
        acceptance_criteria: "profile supplier rejects this fixture".into(),
        model: None,
        hidden: false,
        state: ExecutionStatus::Pending,
        result: None,
        failure: None,
    };
    bounded(
        scheduler
            .set_tasks(
                vec![task],
                root.selection().capture().unwrap(),
                HistoryForkBoundary::AfterCompletedHistory,
            )
            .unwrap()
            .join(),
    )
    .await
    .unwrap();
    let failed = bounded(async {
        loop {
            let task = scheduler.snapshot().await.pop().unwrap();
            if task.state == ExecutionStatus::Failed {
                break task;
            }
            tokio::task::yield_now().await;
        }
    })
    .await;
    // TaskScheduler::owner checks BOTH registered Arc identities before its
    // real child-profile resolver is reached. This failure is deliberately
    // downstream of that guard; it is not a successful task execution claim.
    assert!(
        failed
            .failure
            .as_deref()
            .unwrap()
            .contains("this constructor fixture does not admit child profiles")
    );
    let work = services.active_work.build().await.unwrap().unwrap();
    assert!(work.contains("actual-task [failed]"));
    let mut canceled = failed;
    canceled.state = ExecutionStatus::Canceled;
    bounded(
        scheduler
            .set_tasks(
                vec![canceled],
                root.selection().capture().unwrap(),
                HistoryForkBoundary::AfterCompletedHistory,
            )
            .unwrap()
            .join(),
    )
    .await
    .unwrap();
    assert!(services.active_work.build().await.unwrap().is_none());
    bounded(hosted.close()).await.unwrap();
}

fn assert_same_policy(actual: &TurnSelection<Security>, expected: &TurnSelection<Security>) {
    assert!(Arc::ptr_eq(&actual.security, &expected.security));
    assert_eq!(actual.profile.id, expected.profile.id);
    assert_eq!(
        actual.profile.maximum_requests,
        expected.profile.maximum_requests
    );
    assert_eq!(actual.profile.allowed_tools, expected.profile.allowed_tools);
    assert_eq!(
        actual.profile.disabled_tools,
        expected.profile.disabled_tools
    );
}

#[tokio::test]
async fn actual_drain_and_external_observer_share_live_routing_and_same_prepared_source() {
    let fixture = Fixture::new(false, false).await;
    let hosted = bounded(fixture.open_with_selector("live")).await.unwrap();
    let root = fixture.root();
    let session = root.get_service::<RuntimeSession<Security>>().unwrap();
    let prepared = fixture
        .observations
        .lock()
        .unwrap()
        .selection
        .clone()
        .unwrap();
    assert!(Arc::ptr_eq(&prepared, root.session().selection()));
    let original = prepared.capture().unwrap();
    assert_eq!(original.model.requested_selector.value, "live");
    assert_eq!(original.model.canonical_model.selector(), "local/model");
    assert_eq!(original.model.routing_snapshot.revision, 0);

    let mut updated = original.clone();
    updated.security = Arc::new(Security);
    updated.profile.maximum_requests = 3;
    updated.profile.allowed_tools = Some(vec!["queue_info".into()]);
    session.backend().update_selection(updated.clone()).unwrap();
    assert_same_policy(&prepared.capture().unwrap(), &updated);
    assert!(!Arc::ptr_eq(&original.security, &updated.security));
    let mut alias = ModelAliasDefinition::new("live", "local/model-next", "replacement");
    alias.context_limit = Some(parrot_domain::values::context::ContextSize::parse("64k").unwrap());
    let replacement = fixture.routing.prepare("local/model", vec![alias]).unwrap();
    assert_eq!(replacement.revision, 1);
    fixture.routing.publish(replacement.clone());
    assert_eq!(fixture.router.routing_revision(), 1);
    // The original source is deliberately still revision zero: only the actual
    // constructor's RoutedSelection must re-resolve it for the real drain.
    assert_eq!(
        prepared.capture().unwrap().model.routing_snapshot.revision,
        0
    );
    let current = session.backend().current_selection().unwrap();
    let observed = bounded(session.backend().observe_context(&NeverCancelled))
        .await
        .unwrap();
    for selection in [&current, &observed.selection] {
        assert_eq!(selection.model.requested_selector.value, "live");
        assert_eq!(
            selection.model.canonical_model.selector(),
            "local/model-next"
        );
        assert_eq!(selection.model.routing_snapshot.revision, 1);
        assert!(Arc::ptr_eq(&selection.model.routing_snapshot, &replacement));
        assert_same_policy(selection, &updated);
    }
    assert_eq!(observed.context.context_limit, 128_000);
    assert_eq!(observed.context.trigger_tokens, Some(64_000));
    assert!(observed.context.has_context_limit_override);
    assert!(observed.context.estimated_tokens > 0);
    let advertised = session
        .backend()
        .advertised_tool_definitions(&observed.selection)
        .unwrap();
    assert_eq!(
        advertised
            .iter()
            .map(|tool| tool.name.as_str())
            .collect::<Vec<_>>(),
        vec!["queue_info"]
    );
    assert!(
        fixture.requests.lock().unwrap().is_empty(),
        "external observation does not call a provider"
    );
    let status = root
        .get_service::<parrot_host::composition::factory::scope::ScopedTurnStatus<Security>>()
        .unwrap();
    let runtime_status = root
        .get_service::<parrot_runtime::active_work::status::RuntimeStatus>()
        .unwrap();
    assert!(Arc::ptr_eq(&status.runtime, &runtime_status));
    assert_same_policy(&prepared.capture().unwrap(), &updated);
    assert_eq!(
        prepared.capture().unwrap().model.canonical_model.selector(),
        "local/model"
    );
    assert_eq!(
        prepared.capture().unwrap().model.routing_snapshot.revision,
        0,
        "observer must not write its freshly routed model back into the source"
    );
    let output = bounded(session.send_and_wait(
        vec![ConversationPart::text("route this actual drain")],
        "live-routing-drain".into(),
        None,
        None,
        Arc::new(NeverCancelled),
    ))
    .await
    .unwrap();
    assert_eq!(output, "deterministic reply");
    {
        let requests = fixture.requests.lock().unwrap();
        assert_eq!(requests.len(), 1);
        assert_eq!(requests[0].model, "model-next");
        assert_eq!(requests[0].tools, advertised);
        assert!(
            requests[0]
                .instructions
                .contains("routing selector=live canonical=local/model-next revision=1")
        );
        assert_eq!(
            requests[0]
                .tools
                .iter()
                .map(|tool| tool.name.as_str())
                .collect::<Vec<_>>(),
            vec!["queue_info"]
        );
    }
    {
        let selections = fixture.prompt_selections.lock().unwrap();
        let selected = selections.last().unwrap();
        assert_eq!(selected.requested_selector.value, "live");
        assert_eq!(selected.canonical_model.selector(), "local/model-next");
        assert!(Arc::ptr_eq(&selected.routing_snapshot, &replacement));
    }
    assert_same_policy(&prepared.capture().unwrap(), &updated);
    assert_same_policy(&session.backend().current_selection().unwrap(), &updated);
    assert_eq!(
        prepared.capture().unwrap().model.routing_snapshot.revision,
        0,
        "drain must not write its freshly routed model back into the source"
    );
    let same_source: Arc<dyn SelectionSource<Security>> = root.selection().clone();
    assert!(Arc::ptr_eq(&prepared, &same_source));
    assert!(Arc::ptr_eq(&prepared, root.session().selection()));
    bounded(hosted.close()).await.unwrap();
}

#[tokio::test]
async fn actual_hosted_status_uses_live_context_then_full_status_then_later_statistics() {
    use parrot_domain::values::accounting::{AgentUsageIncrement, AgentUsageKey, AgentUsageTotals};
    use parrot_host::composition::factory::scope::ScopedTurnStatus;
    use parrot_runtime::active_work::status::RuntimeStatus;

    let fixture = Fixture::new(false, false).await;
    let hosted = bounded(fixture.open_with_selector("live")).await.unwrap();
    let root = fixture.root();
    let session = root.get_service::<RuntimeSession<Security>>().unwrap();
    let prepared = fixture
        .observations
        .lock()
        .unwrap()
        .selection
        .clone()
        .unwrap();
    let original = prepared.capture().unwrap();
    let mut alias = ModelAliasDefinition::new("live", "local/model-next", "status replacement");
    alias.context_limit = Some(parrot_domain::values::context::ContextSize::parse("64k").unwrap());
    let replacement = fixture.routing.prepare("local/model", vec![alias]).unwrap();
    fixture.routing.publish(replacement.clone());
    let current = session.backend().current_selection().unwrap();
    assert_same_policy(&current, &original);
    assert!(Arc::ptr_eq(&current.model.routing_snapshot, &replacement));
    assert_eq!(current.model.routing_snapshot.revision, 1);
    let status = root.get_service::<ScopedTurnStatus<Security>>().unwrap();
    assert!(Arc::ptr_eq(
        &status.runtime,
        &root.get_service::<RuntimeStatus>().unwrap()
    ));
    let query = status.query(&current);
    let profile = status.profile_prompt(&current).unwrap();
    assert!(!profile.is_empty());
    let user = fixture.user();
    let statistics = user
        .statistics
        .get_agent_statistics(&root.identity().session.session_id);
    let before = session.backend().usage().capture();
    assert_eq!(before, statistics.capture());
    let before_text = bounded(
        status
            .runtime
            .observe_statistics(&query, before, &NeverCancelled),
    )
    .await
    .unwrap();
    fixture.prompt_selections.lock().unwrap().clear();
    let selections = fixture.prompt_selections.clone();
    let hook_statistics = statistics.clone();
    // Only the actual generated-time provider calls utc_now during full status.
    // Mutating the real usage handle here distinguishes a later statistics capture.
    *fixture.clock.on_utc.lock().unwrap() = Some(Box::new(move || {
        let selections = selections.lock().unwrap();
        let observed = selections.last().expect("context precedes full status");
        assert_eq!(observed.requested_selector.value, "live");
        assert_eq!(observed.canonical_model.selector(), "local/model-next");
        assert!(Arc::ptr_eq(&observed.routing_snapshot, &replacement));
        hook_statistics
            .add_own(&AgentUsageIncrement {
                key: AgentUsageKey::from_request("status-clock", "after-full", None),
                totals: AgentUsageTotals {
                    input_tokens: 971,
                    output_tokens: 37,
                    tool_calls: 5,
                    ..AgentUsageTotals::default()
                },
                context_size: None,
                context_limit: None,
            })
            .unwrap();
    }));
    let rendered = bounded(hosted.observe_status(&NeverCancelled))
        .await
        .unwrap();
    assert!(fixture.clock.on_utc.lock().unwrap().is_none());
    assert!(rendered.contains(&profile));
    assert!(rendered.contains("Active profile: build\nModel: live"));
    assert!(rendered.contains("Generated at: "));
    assert!(rendered.contains(" / 128k tokens); auto-compact: >64000 tokens."));
    let after = session.backend().usage().capture();
    assert_eq!(after, statistics.capture());
    assert_eq!(after.own.totals.input_tokens, 971);
    let after_text = bounded(
        status
            .runtime
            .observe_statistics(&query, after, &NeverCancelled),
    )
    .await
    .unwrap();
    assert_ne!(before_text, after_text);
    assert!(rendered.ends_with(&format!("\n\n{after_text}")));
    assert!(!rendered.ends_with(&format!("\n\n{before_text}")));
    assert!(fixture.requests.lock().unwrap().is_empty());
    assert_same_policy(&prepared.capture().unwrap(), &original);
    assert_eq!(
        prepared.capture().unwrap().model.routing_snapshot.revision,
        0
    );
    assert_eq!(
        prepared.capture().unwrap().model.canonical_model.selector(),
        "local/model"
    );
    bounded(hosted.close()).await.unwrap();
}

#[test]
fn actual_hosted_status_live_build_survives_observer_runtime_drop_and_blocks_close() {
    use std::future::{Future, poll_fn};
    use std::sync::atomic::Ordering;
    use std::task::Poll;

    let caller = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let barrier = Arc::new(StatusBuildBarrier::default());
    let fixture = caller.block_on(Fixture::with_barriers(
        false,
        false,
        None,
        Some(barrier.clone()),
    ));
    let hosted = caller.block_on(fixture.open()).unwrap();
    let user = fixture.user();
    caller.block_on(async {
        let observer = hosted.observe_status(&NeverCancelled);
        tokio::pin!(observer);
        assert!(poll_fn(|context| Poll::Ready(observer.as_mut().poll(context).is_pending())).await);
        bounded(barrier.entered.notified()).await;
    });
    drop(caller);
    let replacement = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    replacement.block_on(async {
        let closing = hosted.close();
        tokio::pin!(closing);
        assert!(poll_fn(|context| Poll::Ready(closing.as_mut().poll(context).is_pending())).await);
        bounded(user.lifetime.cancelled()).await;
        assert!(!barrier.finished.load(Ordering::SeqCst));
        bounded(user.lease.events.replay()).await.unwrap();
        assert_live_activation(&fixture, &user).await;
        assert!(
            poll_fn(|context| Poll::Ready(closing.as_mut().poll(context).is_pending())).await,
            "actual status live-build cleanup still fences close after caller-runtime loss"
        );
        barrier.release.notify_one();
        bounded(closing).await.unwrap();
        assert!(barrier.finished.load(Ordering::SeqCst));
        assert!(barrier.cancelled.load(Ordering::SeqCst));
        bounded(hosted.close()).await.unwrap();
        assert_database_closed(&user);
        let stream = user
            .broker
            .listen(
                None,
                || async { Ok::<_, RpcError>(Vec::new()) },
                || async { Ok::<_, RpcError>(Vec::new()) },
            )
            .await
            .unwrap();
        assert!(
            bounded(stream.next(&NeverCancelled))
                .await
                .unwrap()
                .is_none()
        );
    });
    assert!(fixture.requests.lock().unwrap().is_empty());
}

#[tokio::test]
async fn actual_drain_and_external_observer_reject_removed_alias_without_stale_fallback() {
    let fixture = Fixture::new(false, false).await;
    let hosted = bounded(fixture.open_with_selector("live")).await.unwrap();
    let root = fixture.root();
    let session = root.get_service::<RuntimeSession<Security>>().unwrap();
    let previous = bounded(session.backend().observe_context(&NeverCancelled))
        .await
        .unwrap();
    assert_eq!(previous.selection.model.requested_selector.value, "live");
    assert_eq!(previous.selection.model.routing_snapshot.revision, 0);
    fixture
        .routing
        .publish(fixture.routing.prepare("local/model", Vec::new()).unwrap());
    let routing_error = fixture.router.resolve("live").unwrap_err().to_string();
    assert!(
        session
            .backend()
            .current_selection()
            .err()
            .unwrap()
            .to_string()
            .contains(&routing_error)
    );
    let observer_error = match bounded(session.backend().observe_context(&NeverCancelled)).await {
        Err(error) => error,
        Ok(_) => panic!("removed live alias must not return the previous successful observation"),
    };
    assert!(observer_error.to_string().contains(&routing_error));
    let status_error = bounded(hosted.observe_status(&NeverCancelled))
        .await
        .unwrap_err();
    assert!(status_error.to_string().contains(&routing_error));
    assert_eq!(
        root.selection()
            .capture()
            .unwrap()
            .model
            .canonical_model
            .selector(),
        "local/model"
    );
    assert!(
        bounded(session.send_and_wait(
            vec![ConversationPart::text("must not use stale routing")],
            "missing-live-routing".into(),
            None,
            None,
            Arc::new(NeverCancelled)
        ))
        .await
        .is_err()
    );
    assert!(fixture.requests.lock().unwrap().is_empty());
    bounded(hosted.close()).await.unwrap();
}

struct RoutingDiagnosticProfiles;
impl ProfileResolver<Security> for RoutingDiagnosticProfiles {
    fn resolve_child(&self, id: &str) -> ChildResult<ChildProfile<Security>> {
        Ok(ChildProfile {
            profile: parrot_runtime::agent_turn::services::TurnProfile {
                id: id.into(),
                maximum_requests: 4,
                allowed_tools: None,
                disabled_tools: Vec::new(),
            },
            security: Arc::new(Security),
            recursion_limit: 4,
        })
    }
}

fn turn_diagnostic_field<'a>(line: &'a str, name: &str) -> Option<&'a str> {
    line.split_whitespace().find_map(|field| {
        field
            .strip_prefix(name)?
            .strip_prefix("=\"")?
            .strip_suffix('"')
    })
}

#[cfg(target_os = "linux")]
fn session_log_handle_count(path: &Path) -> usize {
    std::fs::read_dir("/proc/self/fd")
        .unwrap()
        .filter(|entry| {
            let entry = entry.as_ref().unwrap();
            match std::fs::read_link(entry.path()) {
                Ok(target) => target == path,
                // Other test threads and the directory iterator can close their
                // own descriptors between enumeration and read_link.
                Err(error) if error.kind() == io::ErrorKind::NotFound => false,
                Err(error) => panic!("cannot inspect own descriptor: {error}"),
            }
        })
        .count()
}

// SessionRoutingDiagnosticsTests.Shared_provider_routes_root_and_child_turns_to_owning_session_and_resume_appends,
// both interrupt rows: real factory/store, root/root/child drains and session files.
async fn shared_provider_routes_actual_turn_diagnostics(interrupt: bool) {
    use parrot_runtime::children::{CompletionDelivery, HistorySource, LaunchRequest};
    use parrot_storage::conversation::{HistoryForkBoundary, HistoryForkSelection};

    let fixture = Fixture::with_profiles(
        false,
        false,
        None,
        None,
        Arc::new(RoutingDiagnosticProfiles),
    )
    .await;
    let frozen = Instant::now();
    *fixture.clock.frozen.lock().unwrap() = Some(frozen);
    let first = bounded(fixture.open()).await.unwrap();
    let first_user = fixture.user();
    let first_root = fixture.root();
    let second = bounded(fixture.open()).await.unwrap();
    let second_user = fixture.user();
    let second_root = fixture.root();
    let child = bounded(
        first_root
            .spawner()
            .spawn(LaunchRequest {
                parent: first_root.session().clone(),
                requested_name: "routing-child".into(),
                requested_profile: "diagnostic-child".into(),
                model: fixture.router.resolve("local/model").unwrap(),
                requested_scope: String::new(),
                delivery: CompletionDelivery::RetainedOnly,
                boundary: HistoryForkBoundary::AfterCompletedHistory,
                fork: HistoryForkSelection::parse("empty"),
                history_source: HistorySource::Parent,
            })
            .unwrap()
            .join(),
    )
    .await
    .unwrap();
    assert!(first_user.registry().unwrap().contains(&child));
    assert!(Arc::ptr_eq(
        &first_root,
        &child.parent_scope().parent().unwrap()
    ));
    let first_id = first.snapshot().id;
    let second_id = second.snapshot().id;
    let first_path = first_user.lease.selected_root().path().join("session.log");
    let second_path = second_user.lease.selected_root().path().join("session.log");
    assert_ne!(first_path, second_path);
    let scopes = [&first_root, &second_root, &child];
    let markers = [
        "private-root-prompt-sentinel https://example.invalid/?token=private-url-sentinel",
        "private-other-prompt-sentinel",
        "private-child-prompt-sentinel",
    ];
    let mut executions = Vec::new();
    let mut sessions = Vec::new();
    let mut barriers = Vec::new();
    for (scope, marker) in scopes.iter().zip(markers) {
        let barrier = Arc::new(TurnBarrier::default());
        fixture
            .turn_barriers
            .lock()
            .unwrap()
            .insert(marker.into(), barrier.clone());
        let session = scope.get_service::<RuntimeSession<Security>>().unwrap();
        let sent = bounded(session.send_text_message(marker.into(), Arc::new(NeverCancelled)))
            .await
            .unwrap();
        bounded(barrier.entered.notified()).await;
        executions.push(sent.execution);
        sessions.push(session);
        barriers.push(barrier);
    }
    assert_eq!(fixture.requests.lock().unwrap().len(), 3);
    *fixture.clock.frozen.lock().unwrap() = Some(frozen + Duration::from_millis(137));
    if interrupt {
        let handles: Vec<_> = sessions
            .iter()
            .map(|session| session.interrupt_handle().unwrap())
            .collect();
        for handle in handles {
            bounded(handle.join()).await.unwrap();
        }
    } else {
        barriers.pop().unwrap().release.add_permits(1);
        bounded(executions.pop().unwrap().join()).await.unwrap();
        bounded(async {
            while child.session().is_active() {
                tokio::task::yield_now().await;
            }
        })
        .await;
        for barrier in barriers {
            barrier.release.add_permits(1);
        }
    }
    for execution in executions {
        bounded(execution.join()).await.unwrap();
    }
    assert_eq!(fixture.requests.lock().unwrap().len(), 3);
    let first_events = first_user.lease.events.replay().await.unwrap();
    let second_events = second_user.lease.events.replay().await.unwrap();
    let mut turn_ids = Vec::new();
    for (scope, events) in [
        (&first_root, &first_events),
        (&second_root, &second_events),
        (&child, &first_events),
    ] {
        let agent = &scope.identity().session.session_id;
        let starts: Vec<_> = events
            .iter()
            .filter(|event| event.agent_session_id == *agent && event.has_turn_started())
            .collect();
        assert_eq!(starts.len(), 1, "actual durable TurnStarted for {agent}");
        assert_eq!(
            events
                .iter()
                .filter(|event| event.agent_session_id == *agent && event.has_turn_ended())
                .count(),
            1,
            "actual durable TurnEnded for {agent}"
        );
        turn_ids.push(starts[0].id.clone());
    }
    #[cfg(target_os = "linux")]
    for path in [&first_path, &second_path] {
        let count = session_log_handle_count(path);
        assert!(
            count >= 1,
            "live session must own its actual log descriptor"
        );
        eprintln!("session log descriptors before close: {count}");
    }
    bounded(first.close()).await.unwrap();
    bounded(second.close()).await.unwrap();
    #[cfg(target_os = "linux")]
    for path in [&first_path, &second_path] {
        let count = session_log_handle_count(path);
        assert_eq!(count, 0, "actual close must release its log descriptors");
        eprintln!("session log descriptors after close: {count}");
    }
    assert_database_closed(&first_user);
    assert_database_closed(&second_user);
    let first_log = std::fs::read_to_string(&first_path).unwrap();
    let second_log = std::fs::read_to_string(&second_path).unwrap();
    eprintln!(
        "actual turn diagnostics interrupt={interrupt}: first={} second={}\nFIRST\n{first_log}SECOND\n{second_log}",
        first_path.display(),
        second_path.display()
    );
    for (log, session, agents, excluded_session, excluded_agents) in [
        (
            &first_log,
            &first_id,
            vec![&first_root, &child],
            &second_id,
            vec![&second_root],
        ),
        (
            &second_log,
            &second_id,
            vec![&second_root],
            &first_id,
            vec![&first_root, &child],
        ),
    ] {
        assert!(log.contains(&format!("session=\"{session}\"")));
        assert!(!log.contains(excluded_session));
        let producers_stopped = log
            .lines()
            .position(|line| turn_diagnostic_field(line, "event") == Some("producers_stopped"))
            .expect("actual close must stop the session's producers");
        for agent in agents {
            let agent_id = agent.identity().session.session_id.as_str();
            assert!(log.contains(&format!("agent=\"{agent_id}\"")));
            let agent_closed = log
                .lines()
                .position(|line| {
                    turn_diagnostic_field(line, "category") == Some("agent")
                        && turn_diagnostic_field(line, "event") == Some("closed")
                        && turn_diagnostic_field(line, "agent") == Some(agent_id)
                })
                .expect("actual close must close each owning root/child agent");
            assert!(
                agent_closed < producers_stopped,
                "agent {agent_id} must close before its session's producers stop"
            );
        }
        for agent in excluded_agents {
            assert!(!log.contains(&agent.identity().session.session_id));
        }
        assert!(!log.contains("private-"));
        assert!(!log.contains("https://example.invalid"));
    }
    for ((scope, turn_id), log) in
        scopes
            .iter()
            .zip(turn_ids)
            .zip([&first_log, &second_log, &first_log])
    {
        let agent = &scope.identity().session.session_id;
        let lines: Vec<_> = log
            .lines()
            .filter(|line| {
                turn_diagnostic_field(line, "category") == Some("turn")
                    && turn_diagnostic_field(line, "agent") == Some(agent.as_str())
            })
            .collect();
        let starts: Vec<_> = lines
            .iter()
            .filter(|line| turn_diagnostic_field(line, "event") == Some("started"))
            .collect();
        let finishes: Vec<_> = lines
            .iter()
            .filter(|line| turn_diagnostic_field(line, "event") == Some("finished"))
            .collect();
        assert_eq!(starts.len(), 1, "exactly one started for {agent}");
        assert_eq!(finishes.len(), 1, "exactly one finished for {agent}");
        assert_eq!(
            turn_diagnostic_field(finishes[0], "outcome"),
            Some(if interrupt { "cancelled" } else { "completed" })
        );
        let duration = turn_diagnostic_field(finishes[0], "duration_ms")
            .expect("actual finished turn diagnostic must include duration_ms");
        assert!(duration.parse::<i64>().unwrap() >= 0);
        assert_eq!(
            duration, "137",
            "duration must use the injected elapsed clock"
        );
        assert_eq!(
            turn_diagnostic_field(starts[0], "correlation"),
            Some(turn_id.as_str())
        );
        assert_eq!(
            turn_diagnostic_field(finishes[0], "correlation"),
            Some(turn_id.as_str())
        );
    }
    assert!(first_log.contains("category=\"session\" event=\"closed\""));
    let resume_id = parrot_domain::values::identifiers::UserSessionId::parse(&first_id).unwrap();
    let reopened = bounded(fixture.store.resume(&resume_id, true, false))
        .await
        .unwrap();
    let resumed = bounded(fixture.factory.bind_opened(reopened))
        .await
        .unwrap();
    assert_eq!(resumed.snapshot().id, first_id);
    let appended = std::fs::read_to_string(&first_path).unwrap();
    assert!(appended.starts_with(&first_log));
    assert!(appended.contains("category=\"session\" event=\"resume\""));
    #[cfg(target_os = "linux")]
    {
        let count = session_log_handle_count(&first_path);
        assert!(
            count >= 1,
            "resumed session must own its actual log descriptor"
        );
        eprintln!("session log descriptors before resumed close: {count}");
    }
    bounded(resumed.close()).await.unwrap();
    #[cfg(target_os = "linux")]
    {
        let count = session_log_handle_count(&first_path);
        assert_eq!(count, 0, "resumed close must release its log descriptors");
        eprintln!("session log descriptors after resumed close: {count}");
    }

    let mut directories = vec![fixture.root.clone()];
    let mut log_paths = Vec::new();
    while let Some(directory) = directories.pop() {
        for entry in std::fs::read_dir(directory).unwrap() {
            let entry = entry.unwrap();
            if entry.file_type().unwrap().is_dir() {
                directories.push(entry.path());
            } else if entry.path().extension().is_some_and(|ext| ext == "log") {
                log_paths.push(entry.path());
            }
        }
    }
    assert!(log_paths.contains(&first_path));
    assert!(log_paths.contains(&second_path));
    assert!(log_paths.contains(&fixture.root.join("logs/parrot-scope-test.log")));
    log_paths.sort();
    for path in log_paths {
        let log = std::fs::read_to_string(&path).unwrap();
        assert!(!log.contains("private-"), "private payload in {path:?}");
        assert!(
            !log.contains("https://example.invalid"),
            "private URL in {path:?}"
        );
        if path != first_path && path != second_path {
            assert!(!log.contains("category=\"turn\""), "turn in {path:?}");
            assert!(
                !log.contains("category=\"provider\""),
                "provider in {path:?}"
            );
            assert!(
                !log.contains(&format!(
                    "agent=\"{}\"",
                    child.identity().session.session_id
                )),
                "child routing identity in {path:?}"
            );
        }
        eprintln!(
            "all-state log privacy interrupt={interrupt}: {}",
            path.display()
        );
    }

    // A successful ordinary POSIX open is not an exclusive/share-mode proof.
    // On Windows this requests the source test's FileShare.None semantics.
    let mut options = std::fs::OpenOptions::new();
    options.write(true);
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        options.share_mode(0);
    }
    let _writable_after_close = options.open(&first_path).unwrap();
}

#[tokio::test]
async fn actual_shared_provider_turn_diagnostics_complete() {
    shared_provider_routes_actual_turn_diagnostics(false).await;
}

#[tokio::test]
async fn actual_shared_provider_turn_diagnostics_interrupt() {
    shared_provider_routes_actual_turn_diagnostics(true).await;
}

mod ordinary_workspace_binding {
    use super::*;
    use parrot_host::composition::factory::user::ordinary_workspace_resolver;

    async fn fixture(configure: impl FnOnce(&mut UserSessionFactoryConfig<Security>)) -> Fixture {
        Fixture::with_config(false, false, None, None, Arc::new(Policies), |config| {
            config.workspace = ordinary_workspace_resolver();
            configure(config);
        })
        .await
    }

    #[tokio::test]
    async fn actual_constructor_rejects_invalid_workspace_arguments_before_scope_construction() {
        for case in ["empty", "blank", "relative", "missing", "file"] {
            let mut launch = PathBuf::new();
            let fixture = fixture(|config| {
                config.launch_directory = match case {
                    "empty" => PathBuf::new(),
                    "blank" => PathBuf::from("  "),
                    "relative" => PathBuf::from("relative/workspace"),
                    "missing" => config.launch_directory.join("missing"),
                    "file" => {
                        let file = config.launch_directory.join("not-a-directory");
                        std::fs::write(&file, "ordinary file").unwrap();
                        file
                    }
                    _ => unreachable!(),
                };
                launch = config.launch_directory.clone();
            })
            .await;
            let resolution_error = fixture.factory.resolve_workspace(&launch).unwrap_err();
            assert_eq!(resolution_error.code, RpcCode::InvalidArgument, "{case}");
            let error = match bounded(fixture.open()).await {
                Err(error) => error,
                Ok(hosted) => {
                    bounded(hosted.close()).await.unwrap();
                    panic!("{case}: invalid workspace was accepted");
                }
            };
            assert_eq!(
                error.code,
                RpcCode::Internal,
                "{case}: fixture store wrapper"
            );
            assert_eq!(error.message, resolution_error.message, "{case}");
            assert_eq!(
                resolution_error.message,
                if matches!(case, "empty" | "blank" | "relative") {
                    "the project launch directory must be absolute and nonblank"
                } else {
                    "project launch directory does not exist"
                },
                "{case}"
            );
            assert_eq!(fixture.observations.lock().unwrap().constructors, 0);
        }
    }

    #[tokio::test]
    async fn actual_constructor_and_resume_use_real_linked_git_candidate_discovery() {
        let fixture = fixture(|config| {
            let workspace = &config.launch_directory;
            let root = workspace.parent().unwrap();
            let git_directory = root.join("repository/.git/worktrees/topic");
            std::fs::create_dir_all(&git_directory).unwrap();
            std::fs::write(
                workspace.join(".git"),
                "gitdir: ../repository/.git/worktrees/topic\n",
            )
            .unwrap();
            std::fs::write(git_directory.join("commondir"), "../..\n").unwrap();
            std::fs::write(
                git_directory.join("gitdir"),
                workspace.join(".git").to_str().unwrap(),
            )
            .unwrap();
            config.launch_directory = workspace.join("nested");
            std::fs::create_dir_all(&config.launch_directory).unwrap();
        })
        .await;
        let hosted = bounded(fixture.open()).await.unwrap();
        let workspace = fixture.root.join("workspace");
        let launch = workspace.join("nested");
        let repository = fixture.root.join("repository");
        let user = fixture.user();
        let discovered = user.resources.workspace();
        assert_eq!(discovered.launch_directory(), launch);
        assert_eq!(discovered.physical_identity(), launch);
        assert_eq!(discovered.repository_root(), Some(workspace.as_path()));
        assert!(discovered.is_git_repository());
        assert_eq!(discovered.writable_roots(), [repository, launch.clone()]);
        let resolved = fixture.factory.resolve_workspace(&launch).unwrap();
        assert_eq!(resolved, *discovered);
        assert_eq!(resolved.writable_roots(), discovered.writable_roots());
        let id = parrot_domain::values::identifiers::UserSessionId::parse(&hosted.snapshot().id)
            .unwrap();
        bounded(hosted.close()).await.unwrap();

        std::fs::write(workspace.join(".git"), "not a gitdir pointer").unwrap();
        let reopened = bounded(fixture.store.resume(&id, true, false))
            .await
            .unwrap();
        let resumed = bounded(fixture.factory.bind_opened(reopened))
            .await
            .unwrap();
        let resumed_user = fixture.user();
        let rediscovered = resumed_user.resources.workspace();
        assert_eq!(rediscovered.launch_directory(), launch);
        assert_eq!(rediscovered.physical_identity(), launch);
        assert!(!rediscovered.is_git_repository());
        assert_eq!(rediscovered.repository_root(), None);
        assert_eq!(rediscovered.writable_roots(), [launch]);
        bounded(resumed.close()).await.unwrap();
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn actual_constructor_keeps_launch_spelling_and_physical_identity_with_git_candidates() {
        let fixture = fixture(|config| {
            let workspace = &config.launch_directory;
            std::fs::create_dir_all(workspace.join(".git")).unwrap();
            std::fs::create_dir_all(workspace.join("nested")).unwrap();
            let alias = workspace.parent().unwrap().join("alias");
            std::os::unix::fs::symlink(workspace, &alias).unwrap();
            config.launch_directory = alias.join("nested");
        })
        .await;
        let hosted = bounded(fixture.open()).await.unwrap();
        let alias = fixture.root.join("alias");
        let launch = alias.join("nested");
        let physical = fixture.root.join("workspace/nested");
        let user = fixture.user();
        let discovered = user.resources.workspace();
        assert_eq!(discovered.launch_directory(), launch);
        assert_eq!(discovered.physical_identity(), physical);
        assert_eq!(discovered.repository_root(), Some(alias.as_path()));
        assert!(discovered.is_git_repository());
        assert_eq!(
            discovered.writable_roots(),
            [alias, launch.clone(), physical.clone()]
        );
        assert_eq!(
            fixture.factory.resolve_workspace(&launch).unwrap(),
            *discovered
        );
        assert_eq!(
            fixture.factory.resolve_workspace(&physical).unwrap(),
            *discovered
        );
        bounded(hosted.close()).await.unwrap();
    }
}

mod integrated_adapters;
mod integrated_rpc;
mod original_adapters_v8;
mod original_rpc_v8;
mod resume_histories;
mod session_resume;

#[path = "../../e2e_scenarios/scenarios.rs"]
mod e2e_scenarios;

#[path = "../../../../../tests/stress/task_scheduler.rs"]
mod t77_task_races;
