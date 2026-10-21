//! Real composition fixture; synthetic capabilities deny external work.
//! The test harness, not the borrowed gRPC transport, owns the executor.
use std::{
    collections::BTreeMap,
    io,
    marker::PhantomData,
    net::IpAddr,
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    time::{Duration, Instant, SystemTime},
};

use parrot_domain::{
    contracts::{
        async_ops::{BoxFuture, Cancellation, Cancelled, Clock, OperationError, OperationResult},
        filesystem::{
            AuthorizedMutationService, AuthorizedReadService, BoundRead, FileMetadata,
            FileReadError, FileReadErrorKind, LinkPolicy, MutationRead, MutationRequest,
            MutationTarget, ReadRequest,
        },
        paths::ResolvedRead,
        security::{AccessSnapshot, RestrictableAccessSnapshot},
        storage::AdmissionError,
    },
    protocol::messages::parrot as wire,
    values::{
        config::{ModelProfiles, PathPlatform, SkillConfiguration},
        identifiers::{Identifier, UserSessionId},
        project_workspace::ProjectWorkspace,
    },
};
use parrot_foundation::{
    config::Configuration, paths::AgentScratchDirectory, template_catalog::PromptTemplateCatalog,
};
use parrot_host::composition::{
    context::ContextSelection,
    factory::{
        scope::{AgentSourceConfig, AuthoritativeAgentSource},
        user::{
            AgentFactoryParts, AgentFactorySource, AgentSkillBindings, CatalogUserSkills,
            UserServices, UserSessionFactory, UserSessionFactoryConfig, UserSkills,
            durable_permission_factory,
        },
    },
    protocol::{
        self, HostedSession, HostedSessionFactory, ParrotService, RpcError, SessionHost,
        SessionListener, models::ModelOperations,
    },
};
use parrot_provider::{
    catalogs::Model,
    routing::{
        aliases::{ModelAliasCatalog, ModelRouter, ModelRouting},
        configuration::ModelConfigurationCoordinator,
        registry::ProviderRegistry,
    },
    sessions::{Content, Error, EventStream, Provider, Request, Session},
};
use parrot_runtime::{
    agent_turn::services::SelectionSource,
    children::{
        ChildDiagnostic, ChildDiagnostics, ChildError, ChildProfile, ChildResult, IdGenerator,
        ProfileResolver,
    },
    interactions::{BrokerDiagnostic, InteractionDiagnostics},
    plan_mode::{
        MaterializedModeProfile, ModeRegistry, ModeResult, RetainedModePolicyMaterializer,
    },
    prompt_context::{
        PromptResult, SkillCatalog, SkillDiscoveryContext, SkillMetadata, SkillSnapshot,
        StaticSystemPromptProvider,
    },
    tool_settlement::SharedSecurity,
};
use parrot_storage::{
    database::DatabaseError,
    diagnostics::FileDiagnosticLog,
    events::NeverCancelled,
    history_files::SessionCatalog,
    ownership::{
        ClaimDisposition, OwnershipRoots, ProcessIdentity, ProcessIdentityStatus, RuntimeIdentity,
        SessionAdmissions, SessionModelRouter, SessionModelSelection, SessionStore,
        SessionStoreError, SessionWorkspaceResolver, WorkingDirectoryClaim,
    },
};
use parrot_tools::{
    process::{
        execution::runner::SandboxGate,
        ownership::{
            domain::CapturedProcessLauncher,
            execution::{Execution, ProcessLifetime},
        },
        tools::CommandRequest,
    },
    session::{
        Failure, FailureKind, PermissionReply, PermissionServices, WriteTarget, WriteTargetKind,
    },
    web_fetch::{
        FetchError, GzipDecoder, Options, PublicAddressPolicy, Resolver, ResponseBody, Transport,
        TransportSession, WebFetcher,
    },
};

#[derive(Clone)]
pub struct Security;
fn denied() -> FileReadError {
    FileReadError::new(
        FileReadErrorKind::Denied,
        "fixture authorizes no filesystem operations",
    )
}
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
struct DenyExternal;
impl CapturedProcessLauncher for DenyExternal {
    type Request = CommandRequest;
    type Security = SharedSecurity<Security>;
    fn launch(
        &self,
        _: CommandRequest,
        _: Self::Security,
        _: Arc<ProcessLifetime>,
    ) -> BoxFuture<'_, io::Result<Arc<dyn Execution>>> {
        Box::pin(async { Err(io::Error::other("fixture never launches processes")) })
    }
}
impl Resolver for DenyExternal {
    fn resolve<'a>(&'a self, _: &'a str) -> BoxFuture<'a, io::Result<Vec<IpAddr>>> {
        Box::pin(async { Err(io::Error::other("fixture never resolves hosts")) })
    }
}
impl Transport for DenyExternal {
    fn session(&self) -> Result<Box<dyn TransportSession + '_>, FetchError> {
        Err(FetchError::Io(io::Error::other("fixture never fetches")))
    }
}
impl GzipDecoder for DenyExternal {
    fn decode(&self, _: Box<dyn ResponseBody>) -> Result<Box<dyn ResponseBody>, FetchError> {
        Err(FetchError::Io(io::Error::other("fixture never decodes")))
    }
}
impl PermissionServices for DenyExternal {
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
                "fixture never grants permissions",
            )))
        })
    }
}
struct DenyProvider;
impl DenyProvider {
    fn models() -> Vec<Model> {
        let mut model = Model::new("model", "local");
        model.context_window = 128_000;
        model.max_output_tokens = 4_096;
        model.capabilities.tools = true;
        vec![model]
    }
}
impl Provider for DenyProvider {
    fn id(&self) -> &str {
        "local"
    }
    fn call(&self, _: Request) -> Result<Box<dyn EventStream>, Error> {
        Err(Error::Cancelled)
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
        Err(Error::Cancelled)
    }
}
struct TestClock;
impl Clock for TestClock {
    fn now(&self) -> Instant {
        Instant::now()
    }
    fn utc_now(&self) -> SystemTime {
        SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_000)
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
        Err(ChildError::registry("fixture admits no child profiles"))
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
struct Diagnostics;
impl ChildDiagnostics for Diagnostics {
    fn record(&self, _: ChildDiagnostic) {}
}
impl InteractionDiagnostics for Diagnostics {
    fn write(&self, _: BrokerDiagnostic) {}
}

#[derive(Default)]
pub struct RefreshGate {
    armed: AtomicBool,
    pub entered: tokio::sync::Notify,
    pub release: tokio::sync::Notify,
    pub finished: AtomicBool,
}
impl RefreshGate {
    pub fn arm(&self) {
        self.armed.store(true, Ordering::SeqCst);
    }
}
struct Skills {
    actual: CatalogUserSkills<Files<Security>>,
    gate: Arc<RefreshGate>,
}
impl UserSkills<Security> for Skills {
    fn refresh<'a>(
        &'a self,
        security: &'a Security,
        cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, protocol::Result<SkillSnapshot>> {
        Box::pin(async move {
            let snapshot = self.actual.refresh(security, cancellation).await?;
            if self.gate.armed.swap(false, Ordering::SeqCst) {
                let gate = self.gate.clone();
                // This is real supplied refresh work with a retained child join.
                // Cancellation is deliberately not settlement of its cleanup.
                let cleanup = tokio::spawn(async move {
                    gate.entered.notify_one();
                    gate.release.notified().await;
                    gate.finished.store(true, Ordering::SeqCst);
                });
                cleanup.await.expect("refresh cleanup joins");
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
    ) -> BoxFuture<'a, protocol::Result<SkillMetadata>> {
        self.actual.configure(security, path, enabled, cancellation)
    }
    fn new_agent(
        &self,
        selection: Arc<dyn SelectionSource<Security>>,
    ) -> BoxFuture<'_, PromptResult<AgentSkillBindings>> {
        self.actual.new_agent(selection)
    }
}
struct ObservedSource {
    actual: Arc<dyn AgentFactorySource<Security>>,
    users: Arc<Mutex<Vec<Arc<UserServices<Security>>>>>,
}
impl AgentFactorySource<Security> for ObservedSource {
    fn create(
        &self,
        user: Arc<UserServices<Security>>,
    ) -> protocol::Result<AgentFactoryParts<Security>> {
        self.users.lock().unwrap().push(user.clone());
        self.actual.create(user)
    }
}
#[derive(Default)]
pub struct ListenerHost {
    pub fail_next: AtomicBool,
    pub opened: AtomicUsize,
    pub closed: Arc<AtomicUsize>,
}
struct Listener {
    session: Arc<dyn HostedSession>,
    closed: Arc<AtomicUsize>,
}
impl SessionHost for ListenerHost {
    fn host(
        &self,
        session: Arc<dyn HostedSession>,
        cancellation: Arc<dyn Cancellation>,
    ) -> BoxFuture<'_, protocol::Result<Arc<dyn SessionListener>>> {
        Box::pin(async move {
            cancellation.check().map_err(|_| RpcError::cancelled())?;
            if self.fail_next.swap(false, Ordering::SeqCst) {
                return Err(RpcError::new(
                    protocol::RpcCode::Internal,
                    "synthetic listener admission failure",
                ));
            }
            self.opened.fetch_add(1, Ordering::SeqCst);
            Ok(Arc::new(Listener {
                session,
                closed: self.closed.clone(),
            }) as Arc<dyn SessionListener>)
        })
    }
}
impl SessionListener for Listener {
    fn close(&self) -> BoxFuture<'_, protocol::Result<()>> {
        Box::pin(async move {
            self.session.pending_questions()?;
            self.closed.fetch_add(1, Ordering::SeqCst);
            Ok(())
        })
    }
}

pub struct Fixture {
    pub service: Arc<ParrotService>,
    pub diagnostics: Arc<FileDiagnosticLog>,
    pub store: Arc<SessionStore>,
    pub gate: Arc<RefreshGate>,
    pub host: Arc<ListenerHost>,
    users: Arc<Mutex<Vec<Arc<UserServices<Security>>>>>,
    root: PathBuf,
}
fn runtime_identity() -> RuntimeIdentity {
    RuntimeIdentity::new(
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
    .with_proven_comparable_process_start_tokens()
}
impl Fixture {
    pub async fn new() -> Self {
        let root = std::env::temp_dir().join(format!(
            "t63-authoritative-{}",
            Identifier::event_id().unwrap()
        ));
        let workspace = root.join("workspace");
        std::fs::create_dir_all(&workspace).unwrap();
        let admissions = Arc::new(SessionAdmissions::new(
            OwnershipRoots::new(root.join("new"), root.join("old")).unwrap(),
            runtime_identity(),
            true,
        ));
        let diagnostics = Arc::new(
            FileDiagnosticLog::open_global(root.join("logs"), "grpc-lifecycle", |_| {}).unwrap(),
        );
        let registry = Arc::new(
            ProviderRegistry::new(
                vec![Arc::new(DenyProvider)],
                BTreeMap::from([("local".into(), DenyProvider::models())]),
            )
            .unwrap(),
        );
        let aliases = Arc::new(ModelAliasCatalog::new(registry.clone(), Vec::new()).unwrap());
        let routing = Arc::new(ModelRouting::new(aliases, "local/model"));
        let router = Arc::new(ModelRouter::new(registry.clone(), routing.clone()));
        let environment = BTreeMap::from([("HOME".into(), root.to_string_lossy().into_owned())]);
        let (configuration, templates) =
            PromptTemplateCatalog::from_content("", &environment).unwrap();
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
        let users = Arc::new(Mutex::new(Vec::new()));
        let gate = Arc::new(RefreshGate::default());
        let actual = AuthoritativeAgentSource::new(AgentSourceConfig {
            configuration,
            router: router.clone(),
            filesystem: Files::<SharedSecurity<Security>>(PhantomData),
            launcher: Arc::new(DenyExternal),
            permission_authority: Arc::new(|_, _| Ok(Arc::new(DenyExternal))),
            web_fetcher: Arc::new(WebFetcher::new(
                Arc::new(PublicAddressPolicy),
                Arc::new(DenyExternal),
                Arc::new(DenyExternal),
                Arc::new(DenyExternal),
                Options::default(),
            )),
            prompts: vec![Arc::new(StaticSystemPromptProvider::new(
                "fixture:lifecycle",
                "local lifecycle fixture",
            ))],
            clock: Arc::new(TestClock),
            profiles: ModelProfiles::new(&BTreeMap::new()),
        });
        let skill_templates = templates.clone();
        let skill_gate = gate.clone();
        let factory = Arc::new(UserSessionFactory::new(UserSessionFactoryConfig {
            router: router.clone(),
            modes,
            workflows: Vec::new(),
            materializer: Arc::new(Policies),
            source: Arc::new(ObservedSource {
                actual: Arc::new(actual),
                users: users.clone(),
            }),
            profiles: Arc::new(Policies),
            ids: Arc::new(Policies),
            diagnostics: Arc::new(Diagnostics),
            interaction_diagnostics: Arc::new(Diagnostics),
            templates,
            launch_directory: workspace.clone(),
            workspace: Arc::new(|path| {
                Ok(ProjectWorkspace::from_resolved(
                    path.into(),
                    path.into(),
                    None,
                    None,
                ))
            }),
            skills: Arc::new(move |workspace| {
                let reader = Arc::new(Files::<Security>(PhantomData));
                Ok(Arc::new(Skills {
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
                    gate: skill_gate.clone(),
                }))
            }),
            permissions: durable_permission_factory(None, Arc::new(Diagnostics)),
            user_input_timeout: None,
        }));
        let store = Arc::new(SessionStore::new(
            admissions,
            workspace,
            "build".into(),
            factory.clone(),
            Arc::new(Policies),
            Arc::new(Policies),
            diagnostics.clone(),
        ));
        // Discover catalog roots through an actual admitted/opened session.
        let opened = bounded(store.create_fresh(
            SessionModelRouter::resolve(&Policies, "local/model").unwrap(),
            "build",
            true,
        ))
        .await
        .unwrap();
        let hosted = bounded(factory.bind_opened(opened)).await.unwrap();
        let state = users
            .lock()
            .unwrap()
            .last()
            .unwrap()
            .lease
            .selected_root()
            .path()
            .parent()
            .unwrap()
            .parent()
            .unwrap()
            .to_owned();
        bounded(hosted.close()).await.unwrap();
        users.lock().unwrap().clear();
        let config_path = root.join("config.yaml");
        std::fs::write(&config_path, "model: local/model\n").unwrap();
        let coordinator = Arc::new(ModelConfigurationCoordinator::new(
            Configuration::load(
                &config_path,
                &root.join("predefined.yaml"),
                &environment,
                |_| Ok(()),
            )
            .unwrap(),
            routing,
            router.clone(),
        ));
        let models = Arc::new(ModelOperations::new(coordinator, registry, router));
        let host = Arc::new(ListenerHost::default());
        let service = Arc::new(ParrotService::new(
            models,
            store.clone(),
            SessionCatalog::new(&state),
            factory,
            host.clone(),
            Arc::new(WorkingDirectoryClaim::new(&state, runtime_identity())),
            Arc::new(SandboxGate::new(true)),
            diagnostics.clone(),
            tokio::runtime::Handle::current(),
        ));
        Self {
            service,
            diagnostics,
            store,
            gate,
            host,
            users,
            root,
        }
    }
    pub fn user(&self) -> Arc<UserServices<Security>> {
        self.users.lock().unwrap().last().unwrap().clone()
    }
    pub async fn assert_live_activation(&self) {
        let user = self.user();
        assert!(matches!(
            bounded(
                self.store
                    .resume(&UserSessionId::parse(&user.id).unwrap(), true, false)
            )
            .await,
            Err(SessionStoreError::Admission(ClaimDisposition::Live))
        ));
    }
    pub fn assert_database_open(&self) {
        self.user()
            .lease
            .database
            .admit(
                "transport-borrows-service".to_owned(),
                &NeverCancelled,
                |_| Ok(()),
            )
            .unwrap();
    }
    pub fn assert_database_closed(&self) {
        assert!(matches!(
            self.user().lease.database.admit(
                "actual-resource-joined".to_owned(),
                &NeverCancelled,
                |_| Ok(())
            ),
            Err(AdmissionError::Rejected(DatabaseError::Closed))
        ));
    }
    pub async fn close(&self) {
        bounded(self.service.close()).await.unwrap();
        self.assert_database_closed();
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}
pub fn active() -> Arc<dyn Cancellation> {
    Arc::new(NeverCancelled)
}
pub fn create_request() -> wire::CreateSessionRequest {
    wire::CreateSessionRequest {
        model: "local/model".into(),
        mode: "build".into(),
        interactive_permissions: true,
        ..Default::default()
    }
}
pub async fn bounded<T>(future: impl std::future::Future<Output = T>) -> T {
    tokio::time::timeout(Duration::from_secs(10), future)
        .await
        .expect("authoritative lifecycle operation timed out")
}
