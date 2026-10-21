//! Original-owner continuation of the rejected v7 source obligations.
//! Real factory/store/catalog/brokers; synthetic credential and denying tools.
//! The reader/resolver below are bounded fixture capabilities, NOT a production
//! OS workspace adapter, T36 implementation, grant, or security-scope witness.
use super::*;
use parrot_domain::{
    contracts::{filesystem::EntryKind, interactions::QuestionOption, paths::ReadOrigin},
    protocol::messages::parrot as wire,
};
use parrot_host::composition::protocol::{
    ParrotService, SessionHost, SessionListener, models::ModelOperations,
};
use parrot_provider::{
    catalogs::ModelVariant, routing::configuration::ModelConfigurationCoordinator,
};
use parrot_runtime::{
    interactions::{QuestionDefinition, QuestionReplyKind},
    prompt_context::{SkillRoot, SkillScope},
};
use parrot_storage::{
    history_files::{SessionCatalog, SessionMeta},
    ownership::WorkingDirectoryClaim,
};
use parrot_tools::process::execution::runner::SandboxGate;
use std::{fs, io::Read, os::unix::fs::MetadataExt};

#[derive(Clone)]
struct ReadOnlyFixture(PathBuf);
impl AccessSnapshot for ReadOnlyFixture {
    type Error = FileReadError;
    fn check_read(&self, read: &ResolvedRead) -> Result<(), Self::Error> {
        if read.requested_path().starts_with(&self.0) && read.actual_path().starts_with(&self.0) {
            Ok(())
        } else {
            Err(denied())
        }
    }
    fn check_write(&self, _: &Path) -> Result<(), Self::Error> {
        Err(denied())
    }
}
impl RestrictableAccessSnapshot for ReadOnlyFixture {
    fn restrict_with(&self, other: &Self) -> Self {
        assert_eq!(self.0, other.0, "fixture never widens a captured read root");
        self.clone()
    }
}
fn file_error(error: io::Error) -> FileReadError {
    FileReadError::new(
        if error.kind() == io::ErrorKind::NotFound {
            FileReadErrorKind::NotFound
        } else {
            FileReadErrorKind::Io
        },
        error.to_string(),
    )
}
struct SkillReads;
impl SkillReads {
    fn inspect_bound(
        path: &Path,
        links: LinkPolicy,
        security: &ReadOnlyFixture,
    ) -> Result<FileMetadata, FileReadError> {
        if !path.starts_with(&security.0) {
            return Err(denied());
        }
        let meta = fs::symlink_metadata(path).map_err(file_error)?;
        let physical = if meta.file_type().is_symlink() && links == LinkPolicy::NoFollow {
            path.to_owned()
        } else {
            if links == LinkPolicy::RejectLinks {
                let mut component = security.0.clone();
                for part in path.strip_prefix(&security.0).unwrap() {
                    component.push(part);
                    if fs::symlink_metadata(&component)
                        .map_err(file_error)?
                        .file_type()
                        .is_symlink()
                    {
                        return Err(denied());
                    }
                }
            }
            fs::canonicalize(path).map_err(file_error)?
        };
        security.check_read(&ResolvedRead::new(
            path.into(),
            physical.clone(),
            ReadOrigin::Explicit,
        ))?;
        let meta = if links == LinkPolicy::Follow {
            fs::metadata(path).map_err(file_error)?
        } else {
            meta
        };
        Ok(FileMetadata {
            lexical_path: path.into(),
            physical_path: physical,
            kind: if meta.file_type().is_symlink() {
                EntryKind::SymbolicLink
            } else if meta.is_dir() {
                EntryKind::Directory
            } else if meta.is_file() {
                EntryKind::Regular
            } else {
                EntryKind::Other
            },
            hidden: false,
        })
    }
}
impl AuthorizedReadService for SkillReads {
    type Security = ReadOnlyFixture;
    fn inspect<'a>(
        &'a self,
        path: &'a Path,
        links: LinkPolicy,
        security: &'a ReadOnlyFixture,
        cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, OperationResult<FileMetadata, FileReadError>> {
        Box::pin(async move {
            cancellation
                .check()
                .map_err(|_| OperationError::Cancelled)?;
            Self::inspect_bound(path, links, security).map_err(OperationError::Failed)
        })
    }
    fn list_directory<'a>(
        &'a self,
        path: &'a Path,
        links: LinkPolicy,
        security: &'a ReadOnlyFixture,
        cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, OperationResult<Vec<FileMetadata>, FileReadError>> {
        Box::pin(async move {
            self.inspect(path, links, security, cancellation).await?;
            fs::read_dir(path)
                .map_err(file_error)
                .map_err(OperationError::Failed)?
                .map(|entry| {
                    let path = entry.map_err(file_error)?.path();
                    Self::inspect_bound(&path, LinkPolicy::NoFollow, security)
                })
                .collect::<Result<Vec<_>, _>>()
                .map_err(OperationError::Failed)
        })
    }
    fn read<'a>(
        &'a self,
        request: &'a ReadRequest,
        security: &'a ReadOnlyFixture,
        cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, OperationResult<BoundRead, FileReadError>> {
        Box::pin(async move {
            let meta = self
                .inspect(&request.requested, request.links, security, cancellation)
                .await?;
            let read = || -> Result<BoundRead, FileReadError> {
                if meta.kind != EntryKind::Regular
                    || request
                        .expected_physical
                        .as_ref()
                        .is_some_and(|expected| expected != &meta.physical_path)
                {
                    return Err(FileReadError::new(
                        FileReadErrorKind::TargetChanged,
                        "fixture regular target changed",
                    ));
                }
                let before = fs::metadata(&meta.physical_path).map_err(file_error)?;
                let file = fs::File::open(&request.requested).map_err(file_error)?;
                let opened = file.metadata().map_err(file_error)?;
                if !opened.is_file() || (before.dev(), before.ino()) != (opened.dev(), opened.ino())
                {
                    return Err(FileReadError::new(
                        FileReadErrorKind::TargetChanged,
                        "fixture opened target changed",
                    ));
                }
                let mut bytes = Vec::new();
                file.take(request.max_bytes.map_or(u64::MAX, |n| n as u64 + 1))
                    .read_to_end(&mut bytes)
                    .map_err(file_error)?;
                if request.max_bytes.is_some_and(|limit| bytes.len() > limit) {
                    return Err(FileReadError::new(
                        FileReadErrorKind::TooLarge,
                        "fixture skill exceeds limit",
                    ));
                }
                Ok(BoundRead {
                    lexical_path: request.requested.clone(),
                    physical_path: meta.physical_path,
                    bytes,
                })
            };
            read().map_err(OperationError::Failed)
        })
    }
}
struct DenyTools;
impl CapturedProcessLauncher for DenyTools {
    type Request = CommandRequest;
    type Security = SharedSecurity<ReadOnlyFixture>;
    fn launch(
        &self,
        _: CommandRequest,
        _: Self::Security,
        _: Arc<ProcessLifetime>,
    ) -> BoxFuture<'_, io::Result<Arc<dyn Execution>>> {
        Box::pin(async { Err(io::Error::other("v8 fixture never launches")) })
    }
}
impl PermissionServices for DenyTools {
    type Security = SharedSecurity<ReadOnlyFixture>;
    type Selection = ContextSelection<ReadOnlyFixture>;
    fn resolve_target(&self, path: &str) -> Result<WriteTarget, Failure> {
        Ok(WriteTarget {
            path: path.into(),
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
                "v8 fixture never grants",
            )))
        })
    }
}
struct ReadPolicies;
impl RetainedModePolicyMaterializer<ReadOnlyFixture> for ReadPolicies {
    fn for_agent(
        &self,
        raw: &Arc<ReadOnlyFixture>,
        _: &AgentScratchDirectory,
    ) -> ModeResult<Arc<ReadOnlyFixture>> {
        Ok(raw.clone())
    }
}
impl ProfileResolver<ReadOnlyFixture> for ReadPolicies {
    fn resolve_child(&self, _: &str) -> ChildResult<ChildProfile<ReadOnlyFixture>> {
        Err(ChildError::registry("v8 fixture has no child profiles"))
    }
}
struct CredentialProvider(LocalProvider);
impl CredentialProvider {
    fn models() -> Vec<Model> {
        let mut models = LocalProvider::models();
        models[0].capabilities.variants = vec![
            ModelVariant::new("low", "low"),
            ModelVariant::new("high", "xhigh"),
        ];
        models
    }
}
impl Provider for CredentialProvider {
    fn id(&self) -> &str {
        "local"
    }
    fn call(&self, request: Request) -> Result<Box<dyn EventStream>, Error> {
        self.0.call(request)
    }
    fn has_credential<'a>(
        &'a self,
        cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<bool, Error>> {
        Box::pin(async move {
            cancellation.check().map_err(|_| Error::Cancelled)?;
            Ok(true)
        })
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
        self.0.open_session()
    }
}
type CapturedUser = Arc<Mutex<Option<Arc<UserServices<ReadOnlyFixture>>>>>;
struct TrackedSource {
    actual: Arc<dyn AgentFactorySource<ReadOnlyFixture>>,
    user: CapturedUser,
}
impl AgentFactorySource<ReadOnlyFixture> for TrackedSource {
    fn create(
        &self,
        user: Arc<UserServices<ReadOnlyFixture>>,
    ) -> protocol::Result<AgentFactoryParts<ReadOnlyFixture>> {
        *self.user.lock().unwrap() = Some(user.clone());
        self.actual.create(user)
    }
}
#[derive(Default)]
struct StageGate {
    entered: tokio::sync::Notify,
    release: tokio::sync::Notify,
    completed: std::sync::atomic::AtomicBool,
    abandoned: std::sync::atomic::AtomicBool,
}
struct InFlight(Arc<StageGate>);
impl Drop for InFlight {
    fn drop(&mut self) {
        if !self.0.completed.load(std::sync::atomic::Ordering::SeqCst) {
            self.0
                .abandoned
                .store(true, std::sync::atomic::Ordering::SeqCst);
        }
    }
}
impl StageGate {
    async fn wait(self: &Arc<Self>) {
        let _in_flight = InFlight(self.clone());
        self.entered.notify_one();
        self.release.notified().await;
        self.completed
            .store(true, std::sync::atomic::Ordering::SeqCst);
    }
}
type OptionalGate = Arc<Mutex<Option<Arc<StageGate>>>>;
struct Listener {
    session: Arc<dyn HostedSession>,
    closing: OptionalGate,
}
impl SessionListener for Listener {
    fn close(&self) -> BoxFuture<'_, protocol::Result<()>> {
        Box::pin(async move {
            self.session.pending_questions()?;
            let gate = self.closing.lock().unwrap().clone();
            if let Some(gate) = gate {
                gate.wait().await;
            }
            Ok(())
        })
    }
}
#[derive(Default)]
struct Host {
    opening: OptionalGate,
    closing: OptionalGate,
}
impl SessionHost for Host {
    fn host(
        &self,
        session: Arc<dyn HostedSession>,
        _: Arc<dyn Cancellation>,
    ) -> BoxFuture<'_, protocol::Result<Arc<dyn SessionListener>>> {
        let opening = self.opening.lock().unwrap().clone();
        let closing = self.closing.clone();
        Box::pin(async move {
            if let Some(gate) = opening {
                gate.wait().await;
            }
            Ok(Arc::new(Listener { session, closing }) as Arc<dyn SessionListener>)
        })
    }
}
struct Physical(PathBuf);
impl Physical {
    fn resolve(&self, path: &Path) -> protocol::Result<ProjectWorkspace> {
        let invalid = || {
            RpcError::new(
                RpcCode::InvalidArgument,
                "a valid fixture workspace is required",
            )
        };
        if !path.is_absolute() {
            return Err(invalid());
        }
        let actual = fs::canonicalize(path).map_err(|_| invalid())?;
        if !actual.is_dir() || !actual.starts_with(&self.0) {
            return Err(invalid());
        }
        Ok(ProjectWorkspace::from_resolved(
            path.into(),
            actual,
            None,
            None,
        ))
    }
}
impl SessionWorkspaceResolver for Physical {
    fn resolve(&self, path: &Path) -> Result<PathBuf, SessionStoreError> {
        Physical::resolve(self, path)
            .map(|workspace| workspace.physical_identity().into())
            .map_err(SessionStoreError::failure)
    }
}
struct Rpc {
    service: Arc<ParrotService>,
    store: Arc<SessionStore>,
    factory: Arc<UserSessionFactory<ReadOnlyFixture>>,
    models: Arc<ModelOperations>,
    host: Arc<Host>,
    owner_runtime: tokio::runtime::Handle,
    claim: Arc<WorkingDirectoryClaim>,
    diagnostics: Arc<FileDiagnosticLog>,
    user: CapturedUser,
    configuration: Arc<Mutex<Configuration>>,
    root: PathBuf,
    state: PathBuf,
    workspace: PathBuf,
    config_path: PathBuf,
}
fn identity() -> RuntimeIdentity {
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
impl Rpc {
    async fn new(timeout: Option<Duration>) -> Self {
        Self::with_clock(timeout, None).await
    }
    async fn with_clock(timeout: Option<Duration>, question_clock: Option<Arc<dyn Clock>>) -> Self {
        // Existing harness Runtime owns registry work through close/join. This
        // fixture does not establish the missing T63 production ownership edge.
        let owner_runtime = tokio::runtime::Handle::current();
        let root = std::env::temp_dir().join(format!(
            "t62-original-rpc-v8-{}",
            Identifier::event_id().unwrap()
        ));
        let workspace = root.join("workspace");
        fs::create_dir_all(workspace.join(".git")).unwrap();
        let skill_root = root.join("skills");
        fs::create_dir_all(skill_root.join("demo")).unwrap();
        fs::write(
            skill_root.join("demo/SKILL.md"),
            "---\nname: demo\ndescription: A real discovered fixture skill\n---\nsecret body\n",
        )
        .unwrap();
        let config_path = root.join("config.yaml");
        let content = "model: local/model\nprovider_model_alias_defaults:\n  local:\n    low_llm: local/model/low\n    medium_llm: local/model\n    high_llm: local/model/high\n    xhigh_llm: local/model-next\n";
        fs::write(&config_path, content).unwrap();
        let environment = BTreeMap::from([("HOME".into(), root.to_string_lossy().into_owned())]);
        let configuration = Configuration::load(
            &config_path,
            &root.join("predefined.yaml"),
            &environment,
            |_| Ok(()),
        )
        .unwrap();
        let (_, templates) = PromptTemplateCatalog::from_content(content, &environment).unwrap();
        let templates = Arc::new(templates);
        let registry = Arc::new(
            ProviderRegistry::new(
                vec![Arc::new(CredentialProvider(LocalProvider {
                    requests: Arc::new(Mutex::new(Vec::new())),
                    scripts: integrated_adapters::Scripts::default(),
                    turn_barriers: TurnBarriers::default(),
                }))],
                BTreeMap::from([("local".into(), CredentialProvider::models())]),
            )
            .unwrap(),
        );
        let aliases = Arc::new(ModelAliasCatalog::new(registry.clone(), Vec::new()).unwrap());
        let routing = Arc::new(ModelRouting::new(aliases, "local/model"));
        let router = Arc::new(ModelRouter::new(registry.clone(), routing.clone()));
        let coordinator = Arc::new(ModelConfigurationCoordinator::new(
            configuration.clone(),
            routing,
            router.clone(),
        ));
        let models = Arc::new(ModelOperations::new(
            coordinator.clone(),
            registry,
            router.clone(),
        ));
        let profile = MaterializedModeProfile::new(
            "build".into(),
            &configuration.runtime.profiles["build"],
            &[],
            Arc::new(ReadOnlyFixture(skill_root.clone())),
        )
        .unwrap();
        let modes = Arc::new(ModeRegistry::new(vec![profile], "build").unwrap());
        let user = Arc::new(Mutex::new(None));
        let actual = AuthoritativeAgentSource::new(AgentSourceConfig {
            configuration: Arc::new(configuration.clone()),
            router: router.clone(),
            filesystem: Files::<SharedSecurity<ReadOnlyFixture>>(PhantomData),
            launcher: Arc::new(DenyTools),
            permission_authority: Arc::new(|_, _| Ok(Arc::new(DenyTools))),
            web_fetcher: Arc::new(WebFetcher::new(
                Arc::new(PublicAddressPolicy),
                Arc::new(NoExternal),
                Arc::new(NoExternal),
                Arc::new(NoExternal),
                Options::default(),
            )),
            prompts: Vec::new(),
            clock: Arc::new(FixtureClock::default()),
            profiles: ModelProfiles::new(&BTreeMap::new()),
        });
        let configuration = Arc::new(Mutex::new(configuration));
        let capture = configuration.clone();
        let persist = configuration.clone();
        let skill_templates = templates.clone();
        let resolver = Arc::new(Physical(root.clone()));
        let factory_resolver = resolver.clone();
        let factory_config = UserSessionFactoryConfig {
            router,
            modes,
            workflows: Vec::new(),
            materializer: Arc::new(ReadPolicies),
            source: Arc::new(TrackedSource {
                actual: Arc::new(actual),
                user: user.clone(),
            }),
            profiles: Arc::new(ReadPolicies),
            ids: Arc::new(Policies),
            diagnostics: Arc::new(Diagnostics),
            interaction_diagnostics: Arc::new(Diagnostics),
            templates,
            launch_directory: workspace.clone(),
            workspace: Arc::new(move |path| Physical::resolve(&factory_resolver, path)),
            skills: Arc::new(move |workspace| {
                let reader = Arc::new(SkillReads);
                let capture = capture.clone();
                let persist = persist.clone();
                let catalog = SkillCatalog::new(
                    reader.clone(),
                    vec![SkillRoot {
                        path: skill_root.to_string_lossy().into_owned(),
                        scope: SkillScope::User,
                        follow_directory_links: false,
                    }],
                    SkillDiscoveryContext {
                        working_directory: workspace.launch_directory().into(),
                        platform: PathPlatform::Unix,
                    },
                    Arc::new(move || {
                        let configuration = capture.lock().unwrap();
                        (
                            configuration.runtime.skills.clone(),
                            configuration.skills_generation,
                        )
                    }),
                )
                .with_configuration(Arc::new(move |path, enabled| {
                    persist
                        .lock()
                        .unwrap()
                        .set_skill_enabled(path, enabled)
                        .map_err(|error| {
                            FileReadError::new(FileReadErrorKind::Io, error.to_string())
                        })
                }));
                Ok(Arc::new(CatalogUserSkills {
                    catalog: Arc::new(catalog),
                    reader,
                    templates: skill_templates.clone(),
                    working_directory: workspace.launch_directory().to_string_lossy().into_owned(),
                }))
            }),
            permissions: durable_permission_factory(None, Arc::new(Diagnostics)),
            user_input_timeout: timeout,
        };
        let factory = Arc::new(match question_clock {
            Some(clock) => UserSessionFactory::with_question_clock(factory_config, clock),
            None => UserSessionFactory::new(factory_config),
        });
        let state = root.join("state");
        let diagnostics = Arc::new(
            FileDiagnosticLog::open_global(root.join("logs"), "original-rpc-v8", |_| {}).unwrap(),
        );
        let admissions = Arc::new(SessionAdmissions::new(
            OwnershipRoots::new(state.clone(), root.join("old")).unwrap(),
            identity(),
            true,
        ));
        let store = Arc::new(SessionStore::new(
            admissions,
            workspace.clone(),
            "build".into(),
            factory.clone(),
            Arc::new(Policies),
            resolver,
            diagnostics.clone(),
        ));
        let claim = Arc::new(WorkingDirectoryClaim::new(&state, identity()));
        let host = Arc::new(Host::default());
        let service = Arc::new(ParrotService::new(
            models.clone(),
            store.clone(),
            SessionCatalog::new(&state),
            factory.clone(),
            host.clone(),
            claim.clone(),
            Arc::new(SandboxGate::new(true)),
            diagnostics.clone(),
            owner_runtime.clone(),
        ));
        Self {
            service,
            store,
            factory,
            models,
            host,
            owner_runtime,
            claim,
            diagnostics,
            user,
            configuration,
            root,
            state,
            workspace,
            config_path,
        }
    }
    async fn create(&self) -> wire::UserSession {
        bounded(self.service.create_session(
            &wire::CreateSessionRequest {
                model: "local/model".into(),
                mode: "build".into(),
                interactive_permissions: true,
                ..Default::default()
            },
            Arc::new(NeverCancelled),
        ))
        .await
        .unwrap()
    }
    fn user(&self) -> Arc<UserServices<ReadOnlyFixture>> {
        self.user.lock().unwrap().clone().unwrap()
    }
    fn next_service(&self) -> Arc<ParrotService> {
        Arc::new(ParrotService::new(
            self.models.clone(),
            self.store.clone(),
            SessionCatalog::new(&self.state),
            self.factory.clone(),
            self.host.clone(),
            self.claim.clone(),
            Arc::new(SandboxGate::new(true)),
            self.diagnostics.clone(),
            self.owner_runtime.clone(),
        ))
    }
    async fn close(&self) {
        bounded(self.service.close()).await.unwrap();
    }
    fn seed(&self, id: &str, workspace: &Path) -> PathBuf {
        let directory = self.state.join("sessions").join(id);
        fs::create_dir_all(&directory).unwrap();
        let mut metadata = SessionMeta::new(id, workspace.to_string_lossy(), "local", "model");
        metadata.mode = Some("build".into());
        metadata.root_agent_name = Some("Saved root".into());
        fs::write(
            directory.join("meta.json"),
            serde_json::to_vec(&metadata).unwrap(),
        )
        .unwrap();
        directory
    }
}

#[tokio::test]
async fn credentialed_fake_provider_lists_variants_and_applies_persisted_defaults_with_real_factory()
 {
    let f = Rpc::new(None).await;
    let session = f.create().await;
    assert_eq!(f.user().id, session.id);
    let models = f
        .service
        .list_models(&wire::ListModelsRequest::new(), &NeverCancelled)
        .await
        .unwrap();
    assert_eq!(models.models.len(), 2);
    let model = models
        .models
        .iter()
        .find(|model| model.id == "model")
        .unwrap();
    assert_eq!(model.provider_id, "local");
    assert_eq!(
        model
            .variants
            .iter()
            .map(|variant| format!("{}:{}", variant.name, variant.reasoning_effort))
            .collect::<Vec<_>>(),
        ["low:low", "high:xhigh"]
    );
    let defaults = f
        .service
        .list_provider_model_alias_defaults(
            &wire::ListProviderModelAliasDefaultsRequest::new(),
            &NeverCancelled,
        )
        .await
        .unwrap();
    assert_eq!(
        defaults
            .providers
            .iter()
            .map(|provider| provider.provider_id.as_str())
            .collect::<Vec<_>>(),
        ["local"]
    );
    let applied = f
        .service
        .apply_provider_model_alias_defaults(
            &wire::ApplyProviderModelAliasDefaultsRequest {
                provider_id: "local".into(),
                ..Default::default()
            },
            &NeverCancelled,
        )
        .await
        .unwrap();
    assert_eq!(
        applied
            .aliases
            .iter()
            .map(|alias| format!("{}={}", alias.name, alias.model_string))
            .collect::<Vec<_>>(),
        [
            "high_llm=local/model/high",
            "low_llm=local/model/low",
            "medium_llm=local/model",
            "xhigh_llm=local/model-next"
        ]
    );
    let changed = f
        .service
        .update_session(&wire::UpdateSessionRequest {
            user_session_id: session.id.clone(),
            model: "high_llm".into(),
            ..Default::default()
        })
        .await
        .unwrap();
    assert_eq!(changed.model, "high_llm");
    let metadata: SessionMeta = serde_json::from_slice(
        &fs::read(f.user().lease.selected_root().path().join("meta.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(metadata.selector.as_deref(), Some("high_llm"));
    let reloaded = Configuration::load(
        &f.config_path,
        &f.root.join("predefined.yaml"),
        &BTreeMap::from([("HOME".into(), f.root.to_string_lossy().into_owned())]),
        |_| Ok(()),
    )
    .unwrap();
    assert_eq!(
        reloaded.models.model_aliases["low_llm"].model_string,
        "local/model/low"
    );
    assert_eq!(
        reloaded.models.model_aliases["xhigh_llm"].model_string,
        "local/model-next"
    );
    assert_eq!(
        f.models
            .resolve("high_llm")
            .unwrap()
            .canonical_model
            .variant
            .unwrap()
            .reasoning_effort,
        "xhigh"
    );
    f.close().await;
}

#[tokio::test]
async fn actual_factory_creates_variant_and_persists_unlisted_model_with_main_root() {
    let f = Rpc::new(None).await;
    let created = f
        .service
        .create_session(
            &wire::CreateSessionRequest {
                model: "local/model/high".into(),
                mode: "build".into(),
                ..Default::default()
            },
            Arc::new(NeverCancelled),
        )
        .await
        .unwrap();
    assert_eq!(created.model, "local/model/high");
    let updated = f
        .service
        .update_session(&wire::UpdateSessionRequest {
            user_session_id: created.id.clone(),
            model: "local/model".into(),
            ..Default::default()
        })
        .await
        .unwrap();
    assert_eq!(updated.model, "local/model");
    let unlisted = f
        .service
        .update_session(&wire::UpdateSessionRequest {
            user_session_id: created.id,
            model: "local/model/missing".into(),
            ..Default::default()
        })
        .await
        .unwrap();
    assert_eq!(unlisted.model, "local/model/missing");
    let metadata: SessionMeta = serde_json::from_slice(
        &fs::read(f.user().lease.selected_root().path().join("meta.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(metadata.provider_id.as_deref(), Some("local"));
    assert_eq!(metadata.model.as_deref(), Some("local/model/missing"));
    assert_eq!(metadata.root_agent_name.as_deref(), Some("main"));
    f.close().await;
}

#[tokio::test]
async fn actual_configuration_persistence_failure_does_not_publish_candidate_alias() {
    let f = Rpc::new(None).await;
    f.create().await;
    fs::create_dir(f.root.join("config.yaml.tmp")).unwrap();
    let error = f
        .service
        .configure_model_alias(&wire::ConfigureModelAliasRequest {
            name: "high_llm".into(),
            model_string: "local/model".into(),
            ..Default::default()
        })
        .unwrap_err();
    assert_eq!(error.code, RpcCode::Internal);
    let aliases = f.models.list_model_aliases();
    assert!(
        aliases
            .aliases
            .iter()
            .find(|alias| alias.name == "high_llm")
            .unwrap()
            .model_string
            .is_empty()
    );
    f.close().await;
}

#[tokio::test]
async fn actual_applied_provider_defaults_are_visible_in_subsequent_medium_alias_listing() {
    let f = Rpc::new(None).await;
    f.service
        .apply_provider_model_alias_defaults(
            &wire::ApplyProviderModelAliasDefaultsRequest {
                provider_id: "local".into(),
                ..Default::default()
            },
            &NeverCancelled,
        )
        .await
        .unwrap();
    let aliases = f
        .service
        .list_model_aliases(&wire::ListModelAliasesRequest::new());
    let medium = aliases
        .aliases
        .iter()
        .filter(|alias| alias.name == "medium_llm")
        .collect::<Vec<_>>();
    assert_eq!(medium.len(), 1);
    assert_eq!(medium[0].model_string, "local/model");
    f.close().await;
}

#[tokio::test]
async fn real_nonempty_skill_catalog_exact_path_disable_refresh_and_persisted_configuration() {
    let f = Rpc::new(None).await;
    let session = f.create().await;
    let request = wire::ListSkillsRequest {
        user_session_id: session.id.clone(),
        ..Default::default()
    };
    let listed = f
        .service
        .list_skills(&request, &NeverCancelled)
        .await
        .unwrap();
    assert!(listed.errors.is_empty(), "{:?}", listed.errors);
    assert_eq!(listed.skills.len(), 1);
    let skill = &listed.skills[0];
    let exact = fs::canonicalize(f.root.join("skills/demo/SKILL.md"))
        .unwrap()
        .to_string_lossy()
        .into_owned();
    assert_eq!(skill.name, "demo");
    assert_eq!(skill.path, exact);
    assert!(skill.enabled);
    assert!(!format!("{listed:?}").contains("secret body"));
    let configured = f
        .service
        .configure_skill(
            &wire::ConfigureSkillRequest {
                user_session_id: session.id,
                path: exact.clone(),
                enabled: false,
                ..Default::default()
            },
            &NeverCancelled,
        )
        .await
        .unwrap();
    assert!(!configured.skill.as_ref().unwrap().enabled);
    let refreshed = f
        .service
        .list_skills(&request, &NeverCancelled)
        .await
        .unwrap();
    assert_eq!(refreshed.skills.len(), 1);
    assert_eq!(refreshed.skills[0].path, exact);
    assert!(!refreshed.skills[0].enabled);
    {
        let configuration = f.configuration.lock().unwrap();
        assert_eq!(configuration.runtime.skills.entries.len(), 1);
        assert_eq!(configuration.runtime.skills.entries[0].path, exact);
        assert!(!configuration.runtime.skills.entries[0].enabled);
    }
    let reloaded = Configuration::load(
        &f.config_path,
        &f.root.join("predefined.yaml"),
        &BTreeMap::from([("HOME".into(), f.root.to_string_lossy().into_owned())]),
        |_| Ok(()),
    )
    .unwrap();
    assert_eq!(reloaded.runtime.skills.entries.len(), 1);
    assert_eq!(reloaded.runtime.skills.entries[0].path, exact);
    assert!(!reloaded.runtime.skills.entries[0].enabled);
    f.close().await;
}

#[tokio::test]
async fn actual_broker_preserves_ordered_questions_options_multiple_flags_and_ordered_answers() {
    let f = Rpc::new(None).await;
    let session = f.create().await;
    let user = f.user();
    let definitions = vec![
        QuestionDefinition {
            header: "First".into(),
            prompt: "Pick several".into(),
            options: ["One", "Two", "Three"]
                .into_iter()
                .map(|label| QuestionOption {
                    label: label.into(),
                    description: format!("{label} description"),
                })
                .collect(),
            multiple: true,
            custom: false,
        },
        QuestionDefinition {
            header: "Second".into(),
            prompt: "Explain".into(),
            options: vec![],
            multiple: false,
            custom: true,
        },
    ];
    let questions = user.questions.clone();
    let original = definitions.clone();
    let ask = tokio::spawn(async move { questions.ask(definitions, &NeverCancelled).await });
    bounded(async {
        while user.questions.pending().is_empty() {
            tokio::task::yield_now().await;
        }
    })
    .await;
    let pending = f
        .service
        .list_pending_questions(&wire::ListPendingQuestionsRequest {
            user_session_id: session.id.clone(),
            ..Default::default()
        })
        .unwrap();
    assert_eq!(pending.questions.len(), 1);
    let request = &pending.questions[0];
    assert!(request.remaining_timeout_ms.is_none());
    assert_eq!(request.questions.len(), 2);
    for (actual, expected) in request.questions.iter().zip(&original) {
        assert_eq!(actual.header, expected.header);
        assert_eq!(actual.prompt, expected.prompt);
        assert_eq!(actual.multiple, expected.multiple);
        assert_eq!(actual.custom, expected.custom);
        assert_eq!(
            actual
                .options
                .iter()
                .map(|option| (&option.label, &option.description))
                .collect::<Vec<_>>(),
            expected
                .options
                .iter()
                .map(|option| (&option.label, &option.description))
                .collect::<Vec<_>>()
        );
    }
    f.service
        .reply_question(&wire::ReplyQuestionRequest {
            user_session_id: session.id,
            question_request_id: request.id.clone(),
            answers: vec![
                wire::QuestionAnswer {
                    text: "One, Two".into(),
                    ..Default::default()
                },
                wire::QuestionAnswer {
                    text: "Second answer".into(),
                    ..Default::default()
                },
            ],
            ..Default::default()
        })
        .unwrap();
    let reply = bounded(ask).await.unwrap().unwrap();
    assert_eq!(reply.kind, QuestionReplyKind::Answered);
    assert_eq!(reply.answers, ["One, Two", "Second answer"]);
    assert!(user.questions.pending().is_empty());
    f.close().await;
}

fn single_log_line(log: &str, matches: impl Fn(&str) -> bool) -> &str {
    let lines = log.lines().filter(|line| matches(line)).collect::<Vec<_>>();
    assert_eq!(
        lines.len(),
        1,
        "expected exactly one matching global row: {log}"
    );
    lines[0]
}

#[tokio::test]
async fn actual_startup_create_and_resume_global_diagnostics_have_original_single_cardinality() {
    for resume in [false, true] {
        let f = Rpc::new(None).await;
        let session = f.create().await;
        f.close().await;
        if resume {
            let resumed_service = f.next_service();
            let resumed = bounded(resumed_service.resume_session(
                &wire::ResumeSessionRequest {
                    user_session_id: session.id.clone(),
                    working_directory: f.workspace.to_string_lossy().into_owned(),
                    ..Default::default()
                },
                Arc::new(NeverCancelled),
            ))
            .await
            .unwrap();
            assert_eq!(resumed.id, session.id);
            assert!(resumed.loaded);
            assert_eq!(f.user().id, session.id);
            bounded(resumed_service.close()).await.unwrap();
        }
        let log = fs::read_to_string(f.root.join("logs/parrot-original-rpc-v8.log")).unwrap();
        let operation = if resume {
            "resume_session"
        } else {
            "create_session"
        };
        let completed = single_log_line(&log, |line| {
            line.contains(&format!("event=\"{operation}_complete\""))
        });
        let session_field = format!("session=\"{}\"", session.id);
        assert!(completed.contains(&session_field));
        assert!(completed.contains("duration_ms="));
        assert!(completed.contains("outcome=\"succeeded\""));
        let started = single_log_line(&log, |line| {
            line.contains(&format!("event=\"{operation}_start\""))
        });
        let correlations = started
            .split_whitespace()
            .filter(|field| field.starts_with("correlation="))
            .collect::<Vec<_>>();
        assert_eq!(correlations.len(), 1, "{started}");
        let correlation = correlations[0];
        assert!(completed.contains(correlation));
        for phase in ["start", "complete"] {
            let host = single_log_line(&log, |line| {
                line.contains(&format!("event=\"host_{phase}\"")) && line.contains(correlation)
            });
            assert!(host.contains(&session_field));
        }
    }
}

struct ControlledClock(Mutex<Instant>);
impl ControlledClock {
    fn advance(&self, elapsed: Duration) {
        *self.0.lock().unwrap() += elapsed;
    }
}
impl Clock for ControlledClock {
    fn now(&self) -> Instant {
        *self.0.lock().unwrap()
    }
    fn utc_now(&self) -> SystemTime {
        SystemTime::UNIX_EPOCH
    }
    fn sleep_until<'a>(
        &'a self,
        _: Instant,
        cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<(), Cancelled>> {
        // Advance logical observation without delivering a timer, so the exact
        // source zero-remaining row can still be listed and answered at deadline.
        Box::pin(async move {
            cancellation.cancelled().await;
            Err(Cancelled)
        })
    }
}

#[tokio::test]
async fn actual_factory_question_timeout_projection_preserves_exact_750_zero_and_absent_source_rows()
 {
    // ParrotServiceTests.cs764-797 at pin5f27a8a: this is the real factory's
    // broker clock, not the separate scope clock or a substituted broker.
    for (timeout, elapsed, expected) in [
        (
            Some(Duration::from_millis(1000)),
            Duration::from_millis(250),
            Some(750),
        ),
        (
            Some(Duration::from_millis(1000)),
            Duration::from_millis(1000),
            Some(0),
        ),
        (None, Duration::from_millis(1000), None),
    ] {
        let clock = Arc::new(ControlledClock(Mutex::new(Instant::now())));
        let f = Rpc::with_clock(timeout, Some(clock.clone())).await;
        let session = f.create().await;
        let user = f.user();
        let questions = user.questions.clone();
        let ask = tokio::spawn(async move {
            questions
                .ask(
                    vec![QuestionDefinition {
                        header: "Color".into(),
                        prompt: "Choose a color".into(),
                        options: vec![],
                        multiple: false,
                        custom: true,
                    }],
                    &NeverCancelled,
                )
                .await
        });
        bounded(async {
            while user.questions.pending().is_empty() {
                tokio::task::yield_now().await;
            }
        })
        .await;
        clock.advance(elapsed);
        let pending = f
            .service
            .list_pending_questions(&wire::ListPendingQuestionsRequest {
                user_session_id: session.id.clone(),
                ..Default::default()
            })
            .unwrap();
        assert_eq!(pending.questions.len(), 1);
        let pending = &pending.questions[0];
        assert_eq!(
            pending.remaining_timeout_ms, expected,
            "timeout={timeout:?}; elapsed={elapsed:?}"
        );
        f.service
            .reply_question(&wire::ReplyQuestionRequest {
                user_session_id: session.id,
                question_request_id: pending.id.clone(),
                answers: vec![wire::QuestionAnswer {
                    text: "Blue".into(),
                    ..Default::default()
                }],
                ..Default::default()
            })
            .unwrap();
        let reply = bounded(ask).await.unwrap().unwrap();
        assert_eq!(reply.kind, QuestionReplyKind::Answered);
        assert_eq!(reply.answers, ["Blue"]);
        assert!(user.questions.pending().is_empty());
        f.close().await;
    }
}

async fn lose_temporary_rpc_caller(
    service: Arc<ParrotService>,
    gate: Arc<StageGate>,
    closing: bool,
) {
    let (left, gone) = tokio::sync::oneshot::channel();
    let caller = std::thread::spawn(move || {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        runtime.block_on(async move {
            let observer = tokio::spawn(async move {
                if closing {
                    service.close().await
                } else {
                    service
                        .create_session(
                            &wire::CreateSessionRequest {
                                model: "local/model".into(),
                                mode: "build".into(),
                                ..Default::default()
                            },
                            Arc::new(NeverCancelled),
                        )
                        .await
                        .map(|_| ())
                }
            });
            gate.entered.notified().await;
            drop(observer);
        });
        // Explicit simulation of losing only the RPC caller Runtime. The
        // existing test-harness owner Runtime remains alive on the main test.
        drop(runtime);
        left.send(()).unwrap();
    });
    bounded(gone).await.unwrap();
    caller.join().unwrap();
}
fn assert_real_database_closed(user: &UserServices<ReadOnlyFixture>) {
    use parrot_domain::contracts::storage::AdmissionError;
    use parrot_storage::database::DatabaseError;
    assert!(matches!(
        user.lease.database.admit(
            "after-actual-close-owner-join".to_owned(),
            &NeverCancelled,
            |_| Ok(())
        ),
        Err(AdmissionError::Rejected(DatabaseError::Closed))
    ));
}

#[tokio::test]
async fn actual_factory_open_survives_rpc_caller_runtime_loss_and_joins_late_close() {
    let f = Rpc::new(None).await;
    let gate = Arc::new(StageGate::default());
    *f.host.opening.lock().unwrap() = Some(gate.clone());
    lose_temporary_rpc_caller(f.service.clone(), gate.clone(), false).await;
    let user = f.user();
    assert_eq!(
        user.lease.selected_root().path().file_name().unwrap(),
        user.id.as_str()
    );
    assert!(!gate.abandoned.load(std::sync::atomic::Ordering::SeqCst));
    assert!(!gate.completed.load(std::sync::atomic::Ordering::SeqCst));
    assert!(!user.lifetime.is_cancelled());
    let closing = f.service.close();
    tokio::pin!(closing);
    use std::{future::Future, task::Poll};
    assert!(std::future::poll_fn(|cx| Poll::Ready(closing.as_mut().poll(cx).is_pending())).await);
    gate.release.notify_one();
    bounded(closing).await.unwrap();
    assert!(gate.completed.load(std::sync::atomic::Ordering::SeqCst));
    assert!(!gate.abandoned.load(std::sync::atomic::Ordering::SeqCst));
    assert!(user.lifetime.is_cancelled());
    assert_real_database_closed(&user);
    // Public close observes actual owner JoinHandle before cached publication;
    // the retained harness Runtime has not been dropped in either observation.
    bounded(f.service.close()).await.unwrap();
}

#[tokio::test]
async fn actual_factory_close_survives_rpc_caller_runtime_loss_through_listener_and_core_join() {
    let f = Rpc::new(None).await;
    let session = f.create().await;
    let user = f.user();
    assert_eq!(user.id, session.id);
    let gate = Arc::new(StageGate::default());
    *f.host.closing.lock().unwrap() = Some(gate.clone());
    lose_temporary_rpc_caller(f.service.clone(), gate.clone(), true).await;
    assert!(!gate.abandoned.load(std::sync::atomic::Ordering::SeqCst));
    assert!(!gate.completed.load(std::sync::atomic::Ordering::SeqCst));
    assert!(
        !user.lifetime.is_cancelled(),
        "listener must settle before actual core closure"
    );
    gate.release.notify_one();
    bounded(f.service.close()).await.unwrap();
    assert!(gate.completed.load(std::sync::atomic::Ordering::SeqCst));
    assert!(!gate.abandoned.load(std::sync::atomic::Ordering::SeqCst));
    assert!(user.lifetime.is_cancelled());
    assert_real_database_closed(&user);
    bounded(f.service.close()).await.unwrap();
}

fn listed(f: &Rpc, workspace: &str) -> Vec<wire::SessionSummary> {
    f.service
        .list_sessions(&wire::ListSessionsRequest {
            working_directory: workspace.into(),
            ..Default::default()
        })
        .unwrap()
        .sessions
}

#[tokio::test]
async fn caller_scoped_physical_filters_match_launch_identity_not_repository_child_or_worktree() {
    let f = Rpc::new(None).await;
    let session = f.create().await;
    let child = f.workspace.join("child");
    let worktree = f.root.join("worktree");
    fs::create_dir_all(&child).unwrap();
    fs::create_dir_all(&worktree).unwrap();
    fs::write(
        worktree.join(".git"),
        "gitdir: ../workspace/.git/worktrees/demo\n",
    )
    .unwrap();
    let alias = f.root.join("workspace-link");
    std::os::unix::fs::symlink(&f.workspace, &alias).unwrap();
    let child_id = Identifier::user_session().unwrap().to_string();
    let unrelated_id = Identifier::user_session().unwrap().to_string();
    f.seed(&child_id, &child);
    f.seed(&unrelated_id, &f.root);
    let by_alias = listed(&f, alias.to_str().unwrap());
    assert_eq!(by_alias.len(), 1);
    assert_eq!(by_alias[0].user_session_id, session.id);
    let by_child = listed(&f, alias.join("child").to_str().unwrap());
    assert_eq!(by_child.len(), 1);
    assert_eq!(by_child[0].user_session_id, child_id);
    assert!(listed(&f, worktree.to_str().unwrap()).is_empty());
    assert_eq!(listed(&f, "").len(), 3);
    let attached = f
        .service
        .attach_session(
            &wire::AttachSessionRequest {
                user_session_id: session.id.clone(),
                working_directory: alias.to_string_lossy().into_owned(),
                ..Default::default()
            },
            &NeverCancelled,
        )
        .unwrap();
    assert_eq!(attached.id, session.id);
    for invalid in [
        " ".to_owned(),
        "relative/workspace".into(),
        f.root.join("missing").to_string_lossy().into_owned(),
        format!("{}\0bad", f.workspace.display()),
    ] {
        let result = f.service.list_sessions(&wire::ListSessionsRequest {
            working_directory: invalid.clone(),
            ..Default::default()
        });
        assert_eq!(
            result.unwrap_err().code,
            RpcCode::InvalidArgument,
            "filter={invalid:?}"
        );
        assert_eq!(listed(&f, "").len(), 3);
    }
    f.close().await;
}

#[tokio::test]
async fn actual_catalog_existing_then_deleted_workspace_preserves_unfiltered_row_and_excludes_filter()
 {
    let f = Rpc::new(None).await;
    let session = f.create().await;
    let removed_workspace = f.root.join("inactive-work");
    fs::create_dir_all(&removed_workspace).unwrap();
    let saved_id = Identifier::user_session().unwrap().to_string();
    f.seed(&saved_id, &removed_workspace);
    assert!(removed_workspace.is_dir());
    let before = listed(&f, removed_workspace.to_str().unwrap());
    assert_eq!(before.len(), 1);
    assert_eq!(before[0].user_session_id, saved_id);
    assert_eq!(listed(&f, "").len(), 2);

    // ROOT authorizes this exact original transition of our empty private dir.
    fs::remove_dir(&removed_workspace).unwrap();
    assert!(!removed_workspace.exists());
    let filtered = listed(&f, f.workspace.to_str().unwrap());
    assert_eq!(filtered.len(), 1);
    assert_eq!(filtered[0].user_session_id, session.id);
    let unfiltered = listed(&f, "");
    assert_eq!(unfiltered.len(), 2);
    assert!(unfiltered.iter().any(|row| row.user_session_id == saved_id));
    f.close().await;
}

#[tokio::test]
async fn actual_catalog_preserves_active_inactive_corrupt_rows_and_excludes_invalid_filtered_workspaces()
 {
    let f = Rpc::new(None).await;
    let session = f.create().await;
    let inactive_workspace = f.root.join("inactive-workspace");
    fs::create_dir_all(&inactive_workspace).unwrap();
    let inactive_id = Identifier::user_session().unwrap().to_string();
    f.seed(&inactive_id, &inactive_workspace);
    let missing_id = Identifier::user_session().unwrap().to_string();
    f.seed(&missing_id, &f.root.join("never-existing-workspace"));
    let corrupt_id = Identifier::user_session().unwrap().to_string();
    let corrupt = f.state.join("sessions").join(&corrupt_id);
    fs::create_dir_all(&corrupt).unwrap();
    fs::write(corrupt.join("meta.json"), "{bad json").unwrap();
    fs::write(
        f.user().lease.selected_root().path().join("meta.json"),
        "{bad active json",
    )
    .unwrap();
    fs::create_dir_all(f.state.join("sessions/invalid session id")).unwrap();
    let all = listed(&f, "");
    assert_eq!(all.len(), 4);
    let active = all
        .iter()
        .find(|row| row.user_session_id == session.id)
        .unwrap();
    assert_eq!(
        active.state.enum_value().unwrap(),
        wire::SessionState::SESSION_STATE_ACTIVE
    );
    let inactive = all
        .iter()
        .find(|row| row.user_session_id == inactive_id)
        .unwrap();
    assert_eq!(
        inactive.state.enum_value().unwrap(),
        wire::SessionState::SESSION_STATE_INACTIVE
    );
    assert_eq!(inactive.model, "model");
    assert_eq!(inactive.mode, "build");
    assert_eq!(inactive.root_agent_name, "Saved root");
    let corrupt = all
        .iter()
        .find(|row| row.user_session_id == corrupt_id)
        .unwrap();
    assert_eq!(
        corrupt.state.enum_value().unwrap(),
        wire::SessionState::SESSION_STATE_CORRUPT
    );
    assert!(corrupt.model.is_empty());
    let filtered = listed(&f, inactive_workspace.to_str().unwrap());
    assert_eq!(
        filtered
            .iter()
            .map(|row| row.user_session_id.as_str())
            .collect::<Vec<_>>(),
        [inactive_id.as_str()]
    );
    assert!(
        listed(&f, f.workspace.to_str().unwrap()).is_empty(),
        "corrupt active metadata has no resolvable catalog workspace"
    );
    f.close().await;
}
