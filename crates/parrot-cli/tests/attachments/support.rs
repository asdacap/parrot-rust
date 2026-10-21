//! T68-only fixtures. Native transport, ParrotService, SessionStore resources,
//! image persistence and registry shutdown are real. Provider execution, hosted
//! agents/listeners and authorized reads below are synthetic contract boundaries.
//! In particular these readers do not prove physical resolution, grants or T36.

use parrot_domain::{
    contracts::{
        async_ops::{BoxFuture, Cancellation, OperationError, OperationResult},
        filesystem::{
            AuthorizedReadService, BoundRead, FileMetadata, FileReadError, FileReadErrorKind,
            LinkPolicy, ReadRequest,
        },
        paths::ResolvedRead,
        security::AccessSnapshot,
    },
    protocol::{bindings::parrot::parrot_server::Parrot, messages::parrot as wire},
    values::{context::ContextSize, project_workspace::ProjectWorkspace},
};
use parrot_foundation::config::{Configuration, runtime::ProfileConfig};
use parrot_host::{
    composition::protocol::{
        self, HostedSession, HostedSessionFactory, ImageUpload, ParrotService, RpcCode, RpcError,
        SessionEventStream, SessionHost, SessionListener, models::ModelOperations,
    },
    grpc::{
        address::TransportAddress,
        service::GrpcService,
        token::TransportToken,
        transport::{GrpcServer, GrpcTransportClient},
    },
    web::router::WebClient,
};
use parrot_provider::{
    catalogs::Model,
    routing::{
        aliases::{ModelAliasCatalog, ModelRouter, ModelRouting, ResolvedModelSelection},
        configuration::ModelConfigurationCoordinator,
        registry::ProviderRegistry,
    },
    sessions::{
        Content, Error as ProviderError, EventStream, Provider, Request as ProviderRequest, Session,
    },
};
use parrot_runtime::{
    interactions::{InteractionError, PendingQuestion, QuestionReply},
    plan_mode::{MaterializedModeProfile, ModeRegistry},
    prompt_context::{SkillMetadata, SkillSnapshot},
};
use parrot_storage::{
    artifacts::ImageArtifactMetadata,
    diagnostics::{DiagnosticEvent, FileDiagnosticLog},
    events::{Admission, ConversationPart, NeverCancelled},
    history_files::SessionCatalog,
    ownership::*,
};
use parrot_tools::process::execution::runner::SandboxGate;
use std::{
    collections::{BTreeMap, BTreeSet},
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering},
    },
    time::Duration,
};
use tokio::sync::{Mutex as AsyncMutex, watch};
use tonic::{Request, Response, Status};

pub const PNG: &[u8] = &[
    0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a, 0x00, 0x00, 0x00, 0x0d, 0x49, 0x48, 0x44, 0x52,
    0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, 0x08, 0x06, 0x00, 0x00, 0x00, 0x1f, 0x15, 0xc4,
    0x89, 0x00, 0x00, 0x00, 0x0d, 0x49, 0x44, 0x41, 0x54, 0x78, 0x9c, 0x63, 0xf8, 0xcf, 0xc0, 0xf0,
    0x1f, 0x00, 0x05, 0x00, 0x01, 0xff, 0x89, 0x99, 0x3d, 0x1d, 0x00, 0x00, 0x00, 0x00, 0x49, 0x45,
    0x4e, 0x44, 0xae, 0x42, 0x60, 0x82,
];
pub const PNG_SHA256: &str = "4ff6ab670a58c14270e034e2090d9a432caa263a14e0a25785386b0c12f880b5";

pub async fn bounded<T>(future: impl std::future::Future<Output = T>) -> T {
    tokio::time::timeout(Duration::from_secs(10), future)
        .await
        .expect("T68 fixture timed out")
}
pub async fn until(mut predicate: impl FnMut() -> bool) {
    bounded(async {
        while !predicate() {
            tokio::task::yield_now().await;
        }
    })
    .await;
}

pub struct Cancel(watch::Sender<bool>);
impl Cancel {
    pub fn active() -> Self {
        Self(watch::channel(false).0)
    }
    pub fn cancel(&self) {
        self.0.send_replace(true);
    }
}
impl Cancellation for Cancel {
    fn is_cancelled(&self) -> bool {
        *self.0.borrow()
    }
    fn cancelled(&self) -> BoxFuture<'_, ()> {
        Box::pin(async move {
            let mut receiver = self.0.subscribe();
            while !*receiver.borrow_and_update() {
                if receiver.changed().await.is_err() {
                    return;
                }
            }
        })
    }
}

/// Immutable policy tokens and denies only exercise the reader contract. They
/// neither resolve links nor authorize a physical filesystem operation.
pub struct Policy {
    pub identity: Arc<()>,
    pub denied: BTreeSet<PathBuf>,
}
impl Policy {
    pub fn new(denied: &[&str]) -> Arc<Self> {
        Arc::new(Self {
            identity: Arc::new(()),
            denied: denied.iter().map(PathBuf::from).collect(),
        })
    }
}
impl AccessSnapshot for Policy {
    type Error = FileReadError;
    fn check_read(&self, _: &ResolvedRead) -> Result<(), Self::Error> {
        panic!("the T68 contract fake is not a physical authorization backend")
    }
    fn check_write(&self, _: &Path) -> Result<(), Self::Error> {
        panic!("T68 never authorizes writes")
    }
}
pub fn modes(build: Arc<Policy>, plan: Arc<Policy>) -> Arc<ModeRegistry<Policy>> {
    let configuration = ProfileConfig {
        prompt: String::new(),
        usage: String::new(),
        allowed_tools: None,
        max_turns: 1,
        recursion_limit: 1,
        read_only: false,
        is_user_selectable: true,
        is_agent_selectable: false,
        sandbox_rules: Vec::new(),
    };
    Arc::new(
        ModeRegistry::new(
            vec![
                MaterializedModeProfile::new("build".into(), &configuration, &[], build).unwrap(),
                MaterializedModeProfile::new("plan".into(), &configuration, &[], plan).unwrap(),
            ],
            "build",
        )
        .unwrap(),
    )
}
pub struct ReadObservation {
    pub request: ReadRequest,
    pub identity: Arc<()>,
}
#[derive(Default)]
pub struct Reads {
    entries: Mutex<BTreeMap<PathBuf, (PathBuf, Vec<u8>)>>,
    pub observed: Mutex<Vec<ReadObservation>>,
    pub returned_bytes: AtomicUsize,
}
impl Reads {
    pub fn insert(&self, requested: &str, actual: &str, bytes: &[u8]) {
        self.entries
            .lock()
            .unwrap()
            .insert(requested.into(), (actual.into(), bytes.to_vec()));
    }
}
impl AuthorizedReadService for Reads {
    type Security = Policy;
    fn inspect<'a>(
        &'a self,
        _: &'a Path,
        _: LinkPolicy,
        _: &'a Policy,
        _: &'a dyn Cancellation,
    ) -> BoxFuture<'a, OperationResult<FileMetadata, FileReadError>> {
        panic!("uploader must use the one bound read, not inspect then reopen")
    }
    fn list_directory<'a>(
        &'a self,
        _: &'a Path,
        _: LinkPolicy,
        _: &'a Policy,
        _: &'a dyn Cancellation,
    ) -> BoxFuture<'a, OperationResult<Vec<FileMetadata>, FileReadError>> {
        panic!("uploader must not enumerate directories")
    }
    fn read<'a>(
        &'a self,
        request: &'a ReadRequest,
        policy: &'a Policy,
        cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, OperationResult<BoundRead, FileReadError>> {
        Box::pin(async move {
            cancellation
                .check()
                .map_err(|_| OperationError::Cancelled)?;
            self.observed.lock().unwrap().push(ReadObservation {
                request: request.clone(),
                identity: policy.identity.clone(),
            });
            let entries = self.entries.lock().unwrap();
            let (actual, bytes) = entries
                .get(&request.requested)
                .expect("test requested an unregistered fixture path");
            if policy.denied.contains(&request.requested) || policy.denied.contains(actual) {
                return Err(OperationError::Failed(FileReadError::new(
                    FileReadErrorKind::Denied,
                    "fixture lexical/actual target denied",
                )));
            }
            self.returned_bytes.fetch_add(bytes.len(), Ordering::SeqCst);
            Ok(BoundRead {
                lexical_path: request.requested.clone(),
                physical_path: actual.clone(),
                bytes: bytes.clone(),
            })
        })
    }
}

struct OfflineProvider;
impl Provider for OfflineProvider {
    fn id(&self) -> &str {
        "t68"
    }
    fn call(&self, _: ProviderRequest) -> Result<Box<dyn EventStream>, ProviderError> {
        panic!("no provider execution in T68")
    }
    fn seed_models(&self) -> Vec<Model> {
        vec![Model::new("fixture", "t68")]
    }
    fn list_models<'a>(
        &'a self,
        _: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<Vec<Model>, ProviderError>> {
        Box::pin(async { Ok(self.seed_models()) })
    }
    fn has_credential<'a>(
        &'a self,
        _: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<bool, ProviderError>> {
        Box::pin(async { Ok(true) })
    }
    fn calculate_image_tokens(&self, _: &Model, _: &Content) -> i64 {
        0
    }
    fn open_session(&self) -> Result<Arc<dyn Session>, ProviderError> {
        panic!("no live provider in T68")
    }
}
struct ModelRoute;
impl SessionModelRouter for ModelRoute {
    fn resolve(&self, selector: &str) -> Result<SessionModelSelection, SessionStoreError> {
        Ok(SessionModelSelection {
            provider_id: "t68".into(),
            canonical_model: "fixture".into(),
            selector: selector.into(),
        })
    }
}
struct Workspace;
impl SessionWorkspaceResolver for Workspace {
    fn resolve(&self, path: &Path) -> Result<PathBuf, SessionStoreError> {
        std::fs::canonicalize(path).map_err(SessionStoreError::failure)
    }
}
struct Stored {
    initialization: SessionInitialization,
    resources: Arc<SessionResourceLease>,
}
impl StoredUserSession for Stored {
    fn model(&self) -> SessionModelSelection {
        self.initialization.model.clone()
    }
    fn mode(&self) -> String {
        self.initialization.mode.clone()
    }
    fn dispose(&mut self) -> BoxFuture<'_, Result<(), SessionStoreError>> {
        Box::pin(async move {
            // This synthetic stored agent starts no producers. Attest only when
            // its real OpenedSession disposer is polled, not at construction.
            self.resources.confirm_producers_settled();
            Ok(())
        })
    }
}
#[derive(Default)]
struct Factory {
    sessions: Mutex<BTreeMap<String, Arc<Hosted>>>,
}
impl StoredUserSessionFactory for Factory {
    fn create(
        &self,
        resources: Arc<SessionResourceLease>,
        initialization: SessionInitialization,
    ) -> BoxFuture<'_, Result<Box<dyn StoredUserSession>, SessionStoreError>> {
        Box::pin(async move {
            let hosted = Arc::new(Hosted {
                snapshot: wire::UserSession {
                    id: initialization.id.value().into(),
                    model: initialization.model.selector.clone(),
                    mode: initialization.mode.clone(),
                    ..Default::default()
                },
                resources: resources.clone(),
                opened: AsyncMutex::new(None),
                persist_calls: AtomicUsize::new(0),
                sends: AtomicUsize::new(0),
                closed: AtomicBool::new(false),
            });
            self.sessions
                .lock()
                .unwrap()
                .insert(initialization.id.value().into(), hosted);
            Ok(Box::new(Stored {
                initialization,
                resources,
            }) as Box<dyn StoredUserSession>)
        })
    }
}
impl HostedSessionFactory for Factory {
    fn bind_opened(
        &self,
        opened: OpenedSession,
    ) -> BoxFuture<'_, protocol::Result<Arc<dyn HostedSession>>> {
        Box::pin(async move {
            let hosted = self
                .sessions
                .lock()
                .unwrap()
                .get(opened.initialization.id.value())
                .unwrap()
                .clone();
            assert!(Arc::ptr_eq(&hosted.resources, &opened.resources));
            *hosted.opened.lock().await = Some(opened);
            Ok(hosted as Arc<dyn HostedSession>)
        })
    }
    fn list_modes(&self) -> Vec<String> {
        vec!["build".into(), "plan".into()]
    }
    fn resolve_mode(&self, mode: &str) -> protocol::Result<()> {
        if ["", "build", "plan"].contains(&mode) {
            Ok(())
        } else {
            Err(RpcError::new(
                RpcCode::InvalidArgument,
                "unknown fixture mode",
            ))
        }
    }
    fn resolve_workspace(&self, path: &Path) -> protocol::Result<ProjectWorkspace> {
        let path = std::fs::canonicalize(path).map_err(internal)?;
        Ok(ProjectWorkspace::from_resolved(
            path.clone(),
            path,
            None,
            None,
        ))
    }
}
fn internal(error: impl std::fmt::Display) -> RpcError {
    RpcError::new(RpcCode::Internal, error.to_string())
}

pub struct Hosted {
    snapshot: wire::UserSession,
    pub resources: Arc<SessionResourceLease>,
    opened: AsyncMutex<Option<OpenedSession>>,
    pub persist_calls: AtomicUsize,
    pub sends: AtomicUsize,
    pub closed: AtomicBool,
}
impl HostedSession for Hosted {
    fn snapshot(&self) -> wire::UserSession {
        self.snapshot.clone()
    }
    fn resolve_mode(&self, mode: &str) -> protocol::Result<String> {
        Ok(mode.into())
    }
    fn update(
        &self,
        _: Option<ResolvedModelSelection>,
        _: Option<String>,
    ) -> BoxFuture<'_, protocol::Result<()>> {
        panic!("not a T68 operation")
    }
    fn update_selection(
        &self,
        _: ResolvedModelSelection,
    ) -> parrot_provider::routing::configuration::Result<()> {
        panic!("not a T68 operation")
    }
    fn persist_image<'a>(
        &'a self,
        upload: ImageUpload,
        cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, protocol::Result<ImageArtifactMetadata>> {
        Box::pin(async move {
            self.persist_calls.fetch_add(1, Ordering::SeqCst);
            self.resources
                .images
                .persist(
                    std::io::Cursor::new(upload.content),
                    &upload.upload_id,
                    &upload.display_name,
                    "upload",
                    Some(&upload.media_type),
                    Some(upload.byte_length),
                    cancellation,
                )
                .await
                .map_err(RpcError::from)
        })
    }
    fn resolve_image<'a>(
        &'a self,
        id: &'a str,
    ) -> BoxFuture<'a, protocol::Result<Option<ImageArtifactMetadata>>> {
        Box::pin(async move {
            self.resources
                .images
                .resolve(id, &NeverCancelled)
                .await
                .map_err(RpcError::from)
        })
    }
    fn pending_questions(&self) -> protocol::Result<Vec<PendingQuestion>> {
        panic!("not a T68 operation")
    }
    fn reply_question(&self, _: &str, _: QuestionReply) -> Result<(), InteractionError> {
        panic!("not a T68 operation")
    }
    fn reject_question(&self, _: &str) -> Result<(), InteractionError> {
        panic!("not a T68 operation")
    }
    fn pending_permissions(&self) -> BoxFuture<'_, protocol::Result<Vec<wire::PendingPermission>>> {
        panic!("not a T68 operation")
    }
    fn reply_permission<'a>(
        &'a self,
        _: &'a str,
        _: &'a str,
        _: &'a str,
    ) -> BoxFuture<'a, Result<(), InteractionError>> {
        panic!("not a T68 operation")
    }
    fn refresh_skills<'a>(
        &'a self,
        _: &'a dyn Cancellation,
    ) -> BoxFuture<'a, protocol::Result<SkillSnapshot>> {
        panic!("not a T68 operation")
    }
    fn configure_skill<'a>(
        &'a self,
        _: &'a str,
        _: bool,
        _: &'a dyn Cancellation,
    ) -> BoxFuture<'a, protocol::Result<SkillMetadata>> {
        panic!("not a T68 operation")
    }
    fn send(
        &self,
        _: Vec<ConversationPart>,
        _: String,
        _: wire::Delivery,
        _: Arc<dyn Cancellation>,
    ) -> BoxFuture<'_, protocol::Result<Admission>> {
        self.sends.fetch_add(1, Ordering::SeqCst);
        panic!("prepare must never call SendMessage")
    }
    fn interrupt(&self, _: Arc<dyn Cancellation>) -> BoxFuture<'_, protocol::Result<()>> {
        panic!("not a T68 operation")
    }
    fn compact(
        &self,
        _: Option<ContextSize>,
        _: Arc<dyn Cancellation>,
    ) -> BoxFuture<'_, protocol::Result<()>> {
        panic!("not a T68 operation")
    }
    fn set_goal(
        &self,
        _: Option<String>,
        _: Arc<dyn Cancellation>,
    ) -> BoxFuture<'_, protocol::Result<()>> {
        panic!("not a T68 operation")
    }
    fn set_standing_instruction(&self, _: String) -> protocol::Result<()> {
        panic!("not a T68 operation")
    }
    fn observe_status<'a>(
        &'a self,
        _: &'a dyn Cancellation,
    ) -> BoxFuture<'a, protocol::Result<String>> {
        panic!("not a T68 operation")
    }
    fn listen(
        &self,
        _: Option<String>,
        _: Arc<dyn Cancellation>,
    ) -> protocol::Result<Box<dyn SessionEventStream>> {
        panic!("synthetic listener does not subscribe")
    }
    fn record_opened(&self) -> protocol::Result<()> {
        Ok(())
    }
    fn write_diagnostic(&self, event: &DiagnosticEvent) {
        self.resources.write_diagnostic(event);
    }
    fn close(&self) -> BoxFuture<'_, protocol::Result<()>> {
        Box::pin(async move {
            if let Some(mut opened) = self.opened.lock().await.take() {
                opened.close().await.map_err(internal)?;
            }
            self.closed.store(true, Ordering::SeqCst);
            Ok(())
        })
    }
}
struct Host(Arc<AtomicUsize>);
struct Listener(Arc<AtomicUsize>);
impl SessionListener for Listener {
    fn close(&self) -> BoxFuture<'_, protocol::Result<()>> {
        Box::pin(async {
            self.0.fetch_add(1, Ordering::SeqCst);
            Ok(())
        })
    }
}
impl SessionHost for Host {
    fn host(
        &self,
        _: Arc<dyn HostedSession>,
        _: Arc<dyn Cancellation>,
    ) -> BoxFuture<'_, protocol::Result<Arc<dyn SessionListener>>> {
        Box::pin(async { Ok(Arc::new(Listener(self.0.clone())) as Arc<dyn SessionListener>) })
    }
}

pub struct Native {
    pub service: Arc<ParrotService>,
    pub hosted: Arc<Hosted>,
    pub session: String,
    pub client: WebClient,
    diagnostics: Arc<FileDiagnosticLog>,
    server: GrpcServer,
    owner: GrpcTransportClient,
    listener_closes: Arc<AtomicUsize>,
}
impl Native {
    /// Constructed and closed on the same current-thread test Runtime. The test
    /// Runtime remains alive through native transport join and service.close.
    pub async fn new() -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let root = std::env::current_dir().unwrap().join(".tmp").join(format!(
            "t68-native-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        let workspace = root.join("workspace");
        let state = root.join("state");
        std::fs::create_dir_all(&workspace).unwrap();
        let config = root.join("config.yaml");
        std::fs::write(&config, "model: t68/fixture\n").unwrap();
        let environment = [
            "HOME",
            "XDG_CONFIG_HOME",
            "XDG_DATA_HOME",
            "XDG_STATE_HOME",
            "WORKDIR",
        ]
        .into_iter()
        .map(|key| (key.into(), root.join(key).to_string_lossy().into_owned()))
        .collect();
        let configuration =
            Configuration::load(&config, &root.join("predefined.yaml"), &environment, |_| {
                Ok(())
            })
            .unwrap();
        let providers = Arc::new(
            ProviderRegistry::new(vec![Arc::new(OfflineProvider)], BTreeMap::new()).unwrap(),
        );
        let aliases = Arc::new(ModelAliasCatalog::new(providers.clone(), Vec::new()).unwrap());
        let routing = Arc::new(ModelRouting::new(aliases, "t68/fixture"));
        let router = Arc::new(ModelRouter::new(providers.clone(), routing.clone()));
        let coordinator = Arc::new(ModelConfigurationCoordinator::new(
            configuration,
            routing,
            router.clone(),
        ));
        let models = Arc::new(ModelOperations::new(coordinator, providers, router));
        let identity = capture_runtime_identity("t68-native-test").unwrap();
        let claim = Arc::new(WorkingDirectoryClaim::new(&state, identity.clone()));
        let admissions = Arc::new(SessionAdmissions::new(
            OwnershipRoots::new(state.clone(), root.join("legacy")).unwrap(),
            identity,
            false,
        ));
        let diagnostics = Arc::new(
            FileDiagnosticLog::open_global(
                root.join("logs"),
                FileDiagnosticLog::create_instance_id(),
                |message| panic!("fixture diagnostics disabled: {message}"),
            )
            .unwrap(),
        );
        let factory = Arc::new(Factory::default());
        let store = Arc::new(SessionStore::new(
            admissions,
            workspace,
            "build".into(),
            factory.clone(),
            Arc::new(ModelRoute),
            Arc::new(Workspace),
            diagnostics.clone(),
        ));
        let listener_closes = Arc::new(AtomicUsize::new(0));
        let service = Arc::new(ParrotService::new(
            models,
            store,
            SessionCatalog::new(&state),
            factory.clone(),
            Arc::new(Host(listener_closes.clone())),
            claim,
            Arc::new(SandboxGate::new(true)),
            diagnostics.clone(),
            tokio::runtime::Handle::current(),
        ));
        let token = TransportToken::generate().unwrap();
        let server = bounded(GrpcServer::start(
            GrpcService::new(service.clone()),
            TransportAddress::parse("http://127.0.0.1:0").unwrap(),
            Some(token.clone()),
            None,
            diagnostics.clone(),
        ))
        .await
        .unwrap();
        let mut owner =
            GrpcTransportClient::connect(server.address(), Some(&token), None, diagnostics.clone())
                .unwrap();
        let client = owner.client().unwrap().clone();
        let session = bounded(
            owner
                .client()
                .unwrap()
                .create_session(wire::CreateSessionRequest {
                    model: "t68/fixture".into(),
                    mode: "build".into(),
                    ..Default::default()
                }),
        )
        .await
        .unwrap()
        .into_inner()
        .id;
        let hosted = factory
            .sessions
            .lock()
            .unwrap()
            .get(&session)
            .unwrap()
            .clone();
        Self {
            service,
            hosted,
            session,
            client,
            diagnostics,
            server,
            owner,
            listener_closes,
        }
    }
    pub async fn recorder(&self) -> Capture {
        Capture::new(self.diagnostics.clone()).await
    }
    pub async fn close(mut self) {
        drop(self.client);
        self.owner.close();
        bounded(self.server.close()).await.unwrap();
        bounded(self.service.close()).await.unwrap();
        assert!(self.hosted.closed.load(Ordering::SeqCst));
        assert_eq!(self.listener_closes.load(Ordering::SeqCst), 1);
    }
    pub fn image_files(&self) -> Vec<PathBuf> {
        let directory = self
            .hosted
            .resources
            .images
            .store()
            .directory()
            .to_path_buf();
        if !directory.exists() {
            return Vec::new();
        }
        std::fs::read_dir(directory)
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .collect()
    }
}

/// The frame recorder is a separate generated-service observation fixture. Its
/// artifact response is synthetic; only tests on Native.client exercise actual
/// GrpcService -> ParrotService -> session ImageArtifactStore persistence.
#[derive(Default)]
pub struct CaptureState {
    pub uploads: Mutex<Vec<Vec<wire::AttachmentUploadFrame>>>,
    pub entered: AtomicUsize,
    pub active: AtomicUsize,
    pub ended: AtomicUsize,
    pub rpc_cancelled: AtomicBool,
    pub missing_artifact: AtomicBool,
}
struct RequestScope(Arc<CaptureState>);
impl Drop for RequestScope {
    fn drop(&mut self) {
        self.0.active.fetch_sub(1, Ordering::SeqCst);
        self.0.ended.fetch_add(1, Ordering::SeqCst);
    }
}
#[derive(Clone)]
struct RecordingService(Arc<CaptureState>);
macro_rules! recorder_service {
    ($( $method:ident : $input:ident => $output:ident ),* $(,)?) => {
        #[tonic::async_trait]
        impl Parrot for RecordingService {
            $(async fn $method(&self, _: Request<wire::$input>) -> Result<Response<wire::$output>, Status> {
                Err(Status::unimplemented("T68 frame recorder only implements upload"))
            })*
            type ListenStream = tokio_stream::Empty<Result<wire::Event, Status>>;
            async fn listen(&self, _: Request<wire::ListenRequest>) -> Result<Response<Self::ListenStream>, Status> {
                Err(Status::unimplemented("T68 frame recorder does not listen"))
            }
            async fn upload_attachment(&self, request: Request<tonic::Streaming<wire::AttachmentUploadFrame>>) -> Result<Response<wire::AttachmentUploadResponse>, Status> {
                self.0.active.fetch_add(1, Ordering::SeqCst);
                self.0.entered.fetch_add(1, Ordering::SeqCst);
                let _scope = RequestScope(self.0.clone());
                let mut incoming = request.into_inner();
                let mut frames = Vec::new();
                while let Some(frame) = incoming.message().await? {
                    frames.push(frame);
                }
                if self.0.rpc_cancelled.load(Ordering::SeqCst) { return Err(Status::cancelled("fixture native RPC cancelled: 100% \"quoted\" Ω")); }
                self.0.uploads.lock().unwrap().push(frames);
                if self.0.missing_artifact.load(Ordering::SeqCst) { return Ok(Response::new(wire::AttachmentUploadResponse::default())); }
                Ok(Response::new(wire::AttachmentUploadResponse { artifact: Some(wire::ArtifactReference { artifact_id: "synthetic-frame-receipt".into(), ..Default::default() }).into(), ..Default::default() }))
            }
        }
    };
}
recorder_service! {
    list_models: ListModelsRequest => ListModelsResponse,
    list_model_aliases: ListModelAliasesRequest => ListModelAliasesResponse,
    configure_model_alias: ConfigureModelAliasRequest => ConfigureModelAliasResponse,
    list_provider_model_alias_defaults: ListProviderModelAliasDefaultsRequest => ListProviderModelAliasDefaultsResponse,
    apply_provider_model_alias_defaults: ApplyProviderModelAliasDefaultsRequest => ApplyProviderModelAliasDefaultsResponse,
    set_model_preset: SetModelPresetRequest => SetModelPresetResponse,
    select_model_preset: SelectModelPresetRequest => SelectModelPresetResponse,
    list_model_presets: ListModelPresetsRequest => ListModelPresetsResponse,
    list_modes: ListModesRequest => ListModesResponse,
    list_skills: ListSkillsRequest => ListSkillsResponse,
    configure_skill: ConfigureSkillRequest => ConfigureSkillResponse,
    list_sessions: ListSessionsRequest => ListSessionsResponse,
    create_session: CreateSessionRequest => UserSession,
    resume_session: ResumeSessionRequest => UserSession,
    attach_session: AttachSessionRequest => UserSession,
    update_session: UpdateSessionRequest => UserSession,
    set_goal: SetGoalRequest => SetGoalResponse,
    set_standing_instruction: SetStandingInstructionRequest => SetStandingInstructionResponse,
    send_message: SendMessageRequest => SendMessageResponse,
    interrupt: InterruptRequest => InterruptResponse,
    compact: CompactRequest => CompactResponse,
    set_context_limit: SetContextLimitRequest => SetContextLimitResponse,
    sandbox_enable: SandboxEnableRequest => SandboxEnableResponse,
    session_status: SessionStatusRequest => SessionStatusResponse,
    list_pending_questions: ListPendingQuestionsRequest => ListPendingQuestionsResponse,
    reply_question: ReplyQuestionRequest => ReplyQuestionResponse,
    reject_question: RejectQuestionRequest => RejectQuestionResponse,
    list_pending_permissions: ListPendingPermissionsRequest => ListPendingPermissionsResponse,
    reply_permission: ReplyPermissionRequest => ReplyPermissionResponse,
}
pub struct Capture {
    pub state: Arc<CaptureState>,
    pub client: WebClient,
    server: GrpcServer,
    owner: GrpcTransportClient,
}
impl Capture {
    async fn new(diagnostics: Arc<FileDiagnosticLog>) -> Self {
        let state = Arc::new(CaptureState::default());
        let token = TransportToken::generate().unwrap();
        let server = bounded(GrpcServer::start(
            RecordingService(state.clone()),
            TransportAddress::parse("http://127.0.0.1:0").unwrap(),
            Some(token.clone()),
            None,
            diagnostics.clone(),
        ))
        .await
        .unwrap();
        let mut owner =
            GrpcTransportClient::connect(server.address(), Some(&token), None, diagnostics)
                .unwrap();
        let client = owner.client().unwrap().clone();
        Self {
            state,
            client,
            server,
            owner,
        }
    }
    pub async fn close(mut self) {
        drop(self.client);
        self.owner.close();
        bounded(self.server.close()).await.unwrap();
        assert_eq!(self.state.active.load(Ordering::SeqCst), 0);
        assert_eq!(
            self.state.entered.load(Ordering::SeqCst),
            self.state.ended.load(Ordering::SeqCst)
        );
    }
}
