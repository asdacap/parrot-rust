//! Controlled native/terminal observations only; not local application or security acceptance.

#[allow(dead_code)]
#[path = "../support/native_termios.rs"]
mod native_termios;

use std::{
    collections::{BTreeMap, BTreeSet, VecDeque},
    io::{self, Write},
    path::PathBuf,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicIsize, AtomicUsize, Ordering},
    },
    time::{Duration, Instant, SystemTime},
};

use futures_util::FutureExt;
use parrot_cli::{
    attachments::parser::{PromptAttachmentIntent, parse},
    dispatch::interrupts::Interrupts,
    slash_sessions::NavigationFailure,
    slash_settings::AuthServices,
    terminal_dialogs::{
        EnhancedChatRequest, EnhancedCli, EnhancedTerminal, PreparePrompt, TerminalRead,
        TerminalSessionNavigation, TerminalSessionTarget,
    },
    terminal_input::ReadResult,
};
use parrot_domain::{
    contracts::async_ops::{BoxFuture, Cancellation, OperationResult},
    protocol::{bindings::parrot::parrot_server::Parrot, messages::parrot as wire},
};
use parrot_foundation::{
    config::Configuration,
    credentials::{CredentialPresets, CredentialStore, OAuthCredential},
    oauth::{AuthError, DeviceAuthorization, OAuthClient},
};
use parrot_host::{
    grpc::transport::{GrpcServer, GrpcTransportClient},
    web::router::WebClient,
};
use parrot_runtime::agent_turn::lifecycle::Signal;
use parrot_storage::diagnostics::FileDiagnosticLog;
use protobuf::Message;
use tokio::sync::{Notify, mpsc};
use tokio_stream::{StreamExt, wrappers::ReceiverStream};
use tonic::{Request, Response, Status};

static NEXT: AtomicUsize = AtomicUsize::new(0);

#[derive(Debug, Default)]
pub struct RpcState {
    pub sessions: BTreeMap<String, wire::UserSession>,
    pub created: Vec<wire::CreateSessionRequest>,
    pub sent: Vec<wire::SendMessageRequest>,
    pub send_reply_blocked: bool,
    pub updated: Vec<wire::UpdateSessionRequest>,
    pub interrupted: Vec<String>,
    pub questions: BTreeMap<String, Vec<wire::PendingQuestion>>,
    pub permissions: BTreeMap<String, Vec<wire::PendingPermission>>,
    pub question_replies: Vec<wire::ReplyQuestionRequest>,
    pub question_rejections: Vec<wire::RejectQuestionRequest>,
    pub permission_replies: Vec<wire::ReplyPermissionRequest>,
    pub skills: BTreeMap<String, Vec<wire::Skill>>,
    pub aliases: Vec<wire::ModelAlias>,
    pub listens: Vec<wire::ListenRequest>,
    pub replay: BTreeMap<String, Vec<wire::Event>>,
    pub listen_failures: BTreeMap<String, Status>,
    pub first_event_failures: BTreeMap<String, Status>,
    pub silent_listens: BTreeSet<String>,
    pub uploads: Vec<Vec<wire::AttachmentUploadFrame>>,
    pub create_failure: Option<Status>,
    pub loaded_on_create: bool,
    pub question_lists: usize,
    pub question_list_failure: Option<Status>,
    pub streams: BTreeMap<String, Vec<mpsc::Sender<Result<wire::Event, Status>>>>,
}

#[derive(Clone)]
struct Endpoint {
    state: Arc<Mutex<RpcState>>,
    changed: Arc<Notify>,
    stopping: Arc<Signal>,
    workspace: String,
}

fn bytes(message: &impl Message) -> Result<Vec<u8>, Status> {
    message
        .write_to_bytes()
        .map_err(|error| Status::internal(error.to_string()))
}
fn parse_message<M: Message>(body: &[u8]) -> Result<M, Status> {
    M::parse_from_bytes(body).map_err(|error| Status::invalid_argument(error.to_string()))
}

impl Endpoint {
    fn call(&self, method: &str, body: &[u8]) -> Result<Vec<u8>, Status> {
        let mut state = self.state.lock().unwrap();
        let result = match method {
            "create_session" => {
                let request: wire::CreateSessionRequest = parse_message(body)?;
                if let Some(failure) = &state.create_failure {
                    return Err(failure.clone());
                }
                state.created.push(request.clone());
                let id = format!("session-{}", state.created.len());
                let session = wire::UserSession {
                    id: id.clone(),
                    model: request.model,
                    mode: request.mode,
                    loaded: state.loaded_on_create,
                    working_directory: self.workspace.clone(),
                    ..Default::default()
                };
                state.sessions.insert(id, session.clone());
                bytes(&session)
            }
            "attach_session" => {
                let request: wire::AttachSessionRequest = parse_message(body)?;
                bytes(
                    state
                        .sessions
                        .get(&request.user_session_id)
                        .ok_or_else(|| Status::not_found("missing selected session"))?,
                )
            }
            "resume_session" => {
                let request: wire::ResumeSessionRequest = parse_message(body)?;
                bytes(
                    state
                        .sessions
                        .get(&request.user_session_id)
                        .ok_or_else(|| Status::not_found("missing selected session"))?,
                )
            }
            "update_session" => {
                let request: wire::UpdateSessionRequest = parse_message(body)?;
                state.updated.push(request.clone());
                let current = state
                    .sessions
                    .get_mut(&request.user_session_id)
                    .ok_or_else(|| Status::not_found("missing session"))?;
                if !request.model.is_empty() {
                    current.model.clone_from(&request.model);
                }
                if !request.mode.is_empty() {
                    current.mode.clone_from(&request.mode);
                }
                bytes(current)
            }
            "send_message" => {
                state.sent.push(parse_message(body)?);
                bytes(&wire::SendMessageResponse::new())
            }
            "interrupt" => {
                let request: wire::InterruptRequest = parse_message(body)?;
                state.interrupted.push(request.user_session_id);
                bytes(&wire::InterruptResponse::new())
            }
            "list_modes" => bytes(&wire::ListModesResponse {
                modes: ["build", "plan", "query"]
                    .into_iter()
                    .map(|id| wire::Mode {
                        id: id.into(),
                        ..Default::default()
                    })
                    .collect(),
                ..Default::default()
            }),
            "list_models" => bytes(&wire::ListModelsResponse {
                models: vec![wire::Model {
                    id: "model".into(),
                    provider_id: "provider".into(),
                    ..Default::default()
                }],
                ..Default::default()
            }),
            "list_skills" => {
                let request: wire::ListSkillsRequest = parse_message(body)?;
                bytes(&wire::ListSkillsResponse {
                    skills: state
                        .skills
                        .get(&request.user_session_id)
                        .cloned()
                        .unwrap_or_default(),
                    ..Default::default()
                })
            }
            "list_model_aliases" => bytes(&wire::ListModelAliasesResponse {
                aliases: state.aliases.clone(),
                ..Default::default()
            }),
            "list_sessions" => bytes(&wire::ListSessionsResponse {
                sessions: state
                    .sessions
                    .values()
                    .map(|session| wire::SessionSummary {
                        user_session_id: session.id.clone(),
                        model: session.model.clone(),
                        mode: session.mode.clone(),
                        created_at: "2026-01-01T00:00:00Z".into(),
                        state: wire::SessionState::SESSION_STATE_ACTIVE.into(),
                        root_agent_name: session.id.clone(),
                        ..Default::default()
                    })
                    .collect(),
                ..Default::default()
            }),
            "list_pending_questions" => {
                let request: wire::ListPendingQuestionsRequest = parse_message(body)?;
                state.question_lists += 1;
                if let Some(failure) = &state.question_list_failure {
                    return Err(failure.clone());
                }
                bytes(&wire::ListPendingQuestionsResponse {
                    questions: state
                        .questions
                        .get(&request.user_session_id)
                        .cloned()
                        .unwrap_or_default(),
                    ..Default::default()
                })
            }
            "list_pending_permissions" => {
                let request: wire::ListPendingPermissionsRequest = parse_message(body)?;
                bytes(&wire::ListPendingPermissionsResponse {
                    permissions: state
                        .permissions
                        .get(&request.user_session_id)
                        .cloned()
                        .unwrap_or_default(),
                    ..Default::default()
                })
            }
            "reply_question" => {
                let request: wire::ReplyQuestionRequest = parse_message(body)?;
                let pending = state
                    .questions
                    .entry(request.user_session_id.clone())
                    .or_default();
                if !pending
                    .iter()
                    .any(|question| question.id == request.question_request_id)
                {
                    return Err(Status::not_found("question closed"));
                }
                pending.retain(|question| question.id != request.question_request_id);
                state.question_replies.push(request);
                bytes(&wire::ReplyQuestionResponse::new())
            }
            "reject_question" => {
                let request: wire::RejectQuestionRequest = parse_message(body)?;
                state
                    .questions
                    .entry(request.user_session_id.clone())
                    .or_default()
                    .retain(|question| question.id != request.question_request_id);
                state.question_rejections.push(request);
                bytes(&wire::RejectQuestionResponse::new())
            }
            "reply_permission" => {
                let request: wire::ReplyPermissionRequest = parse_message(body)?;
                state
                    .permissions
                    .entry(request.user_session_id.clone())
                    .or_default()
                    .retain(|permission| permission.id != request.permission_request_id);
                state.permission_replies.push(request);
                bytes(&wire::ReplyPermissionResponse::new())
            }
            "session_status" => bytes(&wire::SessionStatusResponse::new()),
            _ => Err(Status::unimplemented(format!(
                "unused controlled enhanced RPC: {method}"
            ))),
        };
        self.changed.notify_waiters();
        result
    }
    async fn unary<Q: Message, R: Message>(
        &self,
        method: &str,
        request: Q,
    ) -> Result<Response<R>, Status> {
        let result = self.call(method, &bytes(&request)?)?;
        if method == "send_message" {
            loop {
                let changed = self.changed.notified();
                tokio::pin!(changed);
                changed.as_mut().enable();
                if !self.state.lock().unwrap().send_reply_blocked {
                    break;
                }
                tokio::select! {
                    biased;
                    () = self.stopping.cancelled() => return Err(Status::cancelled("held send reply cancelled")),
                    () = changed => {}
                }
            }
        }
        Ok(Response::new(parse_message(&result)?))
    }
}

macro_rules! endpoint {
    ($($method:ident($request:ty) -> $response:ty),* $(,)?) => {
        #[tonic::async_trait]
        impl Parrot for Endpoint {
            $(async fn $method(&self, request: Request<$request>) -> Result<Response<$response>, Status> {
                self.unary(stringify!($method), request.into_inner()).await
            })*
            async fn upload_attachment(&self, request: Request<tonic::Streaming<wire::AttachmentUploadFrame>>) -> Result<Response<wire::AttachmentUploadResponse>, Status> {
                let mut stream = request.into_inner();
                let mut frames = Vec::new();
                while let Some(frame) = stream.message().await? { frames.push(frame); }
                self.state.lock().unwrap().uploads.push(frames);
                Ok(Response::new(wire::AttachmentUploadResponse {
                    artifact: Some(wire::ArtifactReference {
                        artifact_id: "synthetic-upload".into(),
                        ..Default::default()
                    }).into(),
                    ..Default::default()
                }))
            }
            type ListenStream = std::pin::Pin<Box<dyn tokio_stream::Stream<Item = Result<wire::Event, Status>> + Send + 'static>>;
            async fn listen(&self, request: Request<wire::ListenRequest>) -> Result<Response<Self::ListenStream>, Status> {
                let request = request.into_inner();
                let (sender, receiver) = mpsc::channel(128);
                let initial = {
                    let mut state = self.state.lock().unwrap();
                    state.listens.push(request.clone());
                    if let Some(failure) = state.listen_failures.get(&request.user_session_id) { return Err(failure.clone()); }
                    state.streams.entry(request.user_session_id.clone()).or_default().push(sender.clone());
                    if let Some(failure) = state.first_event_failures.get(&request.user_session_id) {
                        vec![Err(failure.clone())]
                    } else {
                        let mut events = if request.replay { state.replay.get(&request.user_session_id).cloned().unwrap_or_default() } else { Vec::new() };
                        if state.silent_listens.contains(&request.user_session_id) { events.clear(); }
                        else { events.push(event("snapshot", wire::event::Payload::SessionUsageSnapshot(wire::SessionUsageSnapshot::new()))); }
                        events.into_iter().map(Ok).collect()
                    }
                };
                let deferred_failure = initial.first().is_some_and(Result::is_err);
                for published in initial { sender.send(published).await.map_err(|_| Status::cancelled("fixture receiver closed"))?; }
                self.changed.notify_waiters();
                let stream = ReceiverStream::new(receiver).then(move |published| async move {
                    // Yield Pending before the first error so native response headers can arrive first.
                    if deferred_failure { tokio::time::sleep(Duration::from_millis(25)).await; }
                    published
                });
                Ok(Response::new(Box::pin(stream)))
            }
        }
    };
}
endpoint! {
    list_models(wire::ListModelsRequest) -> wire::ListModelsResponse,
    list_model_aliases(wire::ListModelAliasesRequest) -> wire::ListModelAliasesResponse,
    configure_model_alias(wire::ConfigureModelAliasRequest) -> wire::ConfigureModelAliasResponse,
    list_provider_model_alias_defaults(wire::ListProviderModelAliasDefaultsRequest) -> wire::ListProviderModelAliasDefaultsResponse,
    apply_provider_model_alias_defaults(wire::ApplyProviderModelAliasDefaultsRequest) -> wire::ApplyProviderModelAliasDefaultsResponse,
    set_model_preset(wire::SetModelPresetRequest) -> wire::SetModelPresetResponse,
    select_model_preset(wire::SelectModelPresetRequest) -> wire::SelectModelPresetResponse,
    list_model_presets(wire::ListModelPresetsRequest) -> wire::ListModelPresetsResponse,
    list_modes(wire::ListModesRequest) -> wire::ListModesResponse,
    list_skills(wire::ListSkillsRequest) -> wire::ListSkillsResponse,
    configure_skill(wire::ConfigureSkillRequest) -> wire::ConfigureSkillResponse,
    list_sessions(wire::ListSessionsRequest) -> wire::ListSessionsResponse,
    create_session(wire::CreateSessionRequest) -> wire::UserSession,
    resume_session(wire::ResumeSessionRequest) -> wire::UserSession,
    attach_session(wire::AttachSessionRequest) -> wire::UserSession,
    update_session(wire::UpdateSessionRequest) -> wire::UserSession,
    set_goal(wire::SetGoalRequest) -> wire::SetGoalResponse,
    set_standing_instruction(wire::SetStandingInstructionRequest) -> wire::SetStandingInstructionResponse,
    send_message(wire::SendMessageRequest) -> wire::SendMessageResponse,
    interrupt(wire::InterruptRequest) -> wire::InterruptResponse,
    compact(wire::CompactRequest) -> wire::CompactResponse,
    set_context_limit(wire::SetContextLimitRequest) -> wire::SetContextLimitResponse,
    sandbox_enable(wire::SandboxEnableRequest) -> wire::SandboxEnableResponse,
    session_status(wire::SessionStatusRequest) -> wire::SessionStatusResponse,
    list_pending_questions(wire::ListPendingQuestionsRequest) -> wire::ListPendingQuestionsResponse,
    reply_question(wire::ReplyQuestionRequest) -> wire::ReplyQuestionResponse,
    reject_question(wire::RejectQuestionRequest) -> wire::RejectQuestionResponse,
    list_pending_permissions(wire::ListPendingPermissionsRequest) -> wire::ListPendingPermissionsResponse,
    reply_permission(wire::ReplyPermissionRequest) -> wire::ReplyPermissionResponse,
}

pub fn event(id: &str, payload: wire::event::Payload) -> wire::Event {
    wire::Event {
        id: id.into(),
        agent_session_id: "root".into(),
        payload: Some(payload),
        ..Default::default()
    }
}

pub struct Fixture {
    pub root: PathBuf,
    pub state: Arc<Mutex<RpcState>>,
    pub diagnostics: Arc<FileDiagnosticLog>,
    pub stopping: Arc<Signal>,
    pub initial_mode: String,
    connection: GrpcTransportClient,
    server: GrpcServer,
    changed: Arc<Notify>,
}
impl Fixture {
    pub async fn start() -> Self {
        let root = PathBuf::from(format!(
            "/tmp/t72-flow-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::SeqCst)
        ));
        std::fs::create_dir_all(&root).unwrap();
        let diagnostics = Arc::new(
            FileDiagnosticLog::open_global(root.join("logs"), "enhanced-flow", |_| {}).unwrap(),
        );
        let state = Arc::new(Mutex::new(RpcState::default()));
        let changed = Arc::new(Notify::new());
        let stopping = Arc::new(Signal::new());
        let endpoint = Endpoint {
            state: state.clone(),
            changed: changed.clone(),
            stopping: stopping.clone(),
            workspace: root.to_string_lossy().into_owned(),
        };
        let server =
            GrpcServer::start_local(endpoint, root.join("native.sock"), diagnostics.clone())
                .await
                .unwrap();
        let connection =
            GrpcTransportClient::connect(server.address(), None, None, diagnostics.clone())
                .unwrap();
        Self {
            root,
            state,
            diagnostics,
            stopping,
            initial_mode: "build".into(),
            connection,
            server,
            changed,
        }
    }
    pub fn release_send_reply(&self) {
        self.state.lock().unwrap().send_reply_blocked = false;
        self.changed.notify_waiters();
    }
    pub fn client(&mut self) -> WebClient {
        self.connection.client().unwrap().clone()
    }
    pub async fn publish(&self, session: &str, published: wire::Event) {
        let senders = self
            .state
            .lock()
            .unwrap()
            .streams
            .get(session)
            .cloned()
            .unwrap_or_default();
        assert!(
            !senders.is_empty(),
            "session stream must be open before publishing"
        );
        for sender in senders {
            let _ = sender.send(Ok(published.clone())).await;
        }
    }
    pub async fn complete_turn(&self, session: &str, label: &str) {
        self.publish(
            session,
            event(
                &format!("{label}-started"),
                wire::event::Payload::TurnStarted(wire::TurnStarted::new()),
            ),
        )
        .await;
        self.publish(
            session,
            event(
                &format!("{label}-completed"),
                wire::event::Payload::ModeTurnCompleted(wire::ModeTurnCompleted::new()),
            ),
        )
        .await;
    }
    pub async fn fail_stream(&self, session: &str, failure: Status) {
        let senders = self
            .state
            .lock()
            .unwrap()
            .streams
            .get(session)
            .cloned()
            .unwrap_or_default();
        for sender in senders {
            let _ = sender.send(Err(failure.clone())).await;
        }
    }
    #[track_caller]
    pub fn wait(
        &self,
        predicate: impl Fn(&RpcState) -> bool,
    ) -> impl std::future::Future<Output = ()> {
        let location = std::panic::Location::caller();
        wait_until_described(
            move || predicate(&self.state.lock().unwrap()),
            move || {
                format!(
                    "RPC state predicate at {location}; state: {:#?}",
                    self.state.lock().unwrap()
                )
            },
        )
    }
    pub fn cli(
        &mut self,
        terminal: EnhancedTerminal,
        prompt: &str,
        initial: Option<wire::UserSession>,
    ) -> EnhancedCli {
        self.cli_with(terminal, prompt, initial, None, prepare_without_files())
    }
    pub fn cli_with(
        &mut self,
        terminal: EnhancedTerminal,
        prompt: &str,
        initial: Option<wire::UserSession>,
        navigation: Option<Arc<dyn TerminalSessionNavigation>>,
        prepare: PreparePrompt,
    ) -> EnhancedCli {
        let auth = Arc::new(AuthServices {
            credentials: Arc::new(CredentialStore::at_selected_path(
                self.root.join("credentials.json"),
            )),
            presets: Arc::new(CredentialPresets::at_selected_config_directory(&self.root)),
            oauth: Arc::new(UnusedOAuth),
            provider_ids: vec!["provider".into()],
            diagnostics: self.diagnostics.clone(),
        });
        let environment = BTreeMap::from([
            ("HOME".into(), self.root.join("home").display().to_string()),
            (
                "XDG_CONFIG_HOME".into(),
                self.root.join("xdg/config").display().to_string(),
            ),
            (
                "XDG_DATA_HOME".into(),
                self.root.join("xdg/data").display().to_string(),
            ),
            (
                "XDG_CACHE_HOME".into(),
                self.root.join("xdg/cache").display().to_string(),
            ),
            (
                "XDG_STATE_HOME".into(),
                self.root.join("xdg/state").display().to_string(),
            ),
            (
                "XDG_RUNTIME_DIR".into(),
                self.root.join("xdg/runtime").display().to_string(),
            ),
        ]);
        let config =
            Configuration::from_content("model: provider/model\n", &environment, |_| Ok(()))
                .unwrap();
        let interrupts = Arc::new(Interrupts::new({
            let stop = self.stopping.clone();
            Arc::new(move || stop.cancel())
        }));
        EnhancedCli::new(
            self.client(),
            interrupts,
            auth,
            Arc::new(Mutex::new(config)),
            EnhancedChatRequest {
                session: wire::CreateSessionRequest {
                    model: "provider/model".into(),
                    mode: self.initial_mode.clone(),
                    ..Default::default()
                },
                prompt: prompt.into(),
                initial_session: initial,
                navigation,
            },
            "test-version".into(),
            terminal,
            prepare,
        )
    }
    pub async fn close(mut self) {
        self.connection.close();
        self.server.close().await.unwrap();
    }
}

pub struct SuppliedNavigation {
    pub client: WebClient,
    pub selected: wire::UserSession,
}
impl TerminalSessionNavigation for SuppliedNavigation {
    fn list<'a>(
        &'a self,
        _: &'a wire::UserSession,
        cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<wire::ListSessionsResponse, NavigationFailure>> {
        Box::pin(async move {
            cancellation.check().map_err(|_| {
                NavigationFailure::Rpc(Status::cancelled("supplied navigation cancelled"))
            })?;
            Ok(wire::ListSessionsResponse {
                sessions: vec![wire::SessionSummary {
                    user_session_id: self.selected.id.clone(),
                    root_agent_name: self.selected.id.clone(),
                    created_at: "2026-01-01T00:00:00Z".into(),
                    ..Default::default()
                }],
                ..Default::default()
            })
        })
    }
    fn open<'a>(
        &'a self,
        _: &'a wire::UserSession,
        id: &'a str,
        cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<TerminalSessionTarget, NavigationFailure>> {
        Box::pin(async move {
            cancellation.check().map_err(|_| {
                NavigationFailure::Rpc(Status::cancelled("supplied navigation cancelled"))
            })?;
            assert_eq!(id, self.selected.id);
            Ok(TerminalSessionTarget::new(
                self.client.clone(),
                self.selected.clone(),
                None,
            ))
        })
    }
}

fn prepare_without_files() -> PreparePrompt {
    Arc::new(|_, session, _, entered, delivery, error, cancellation| {
        Box::pin(async move {
            cancellation
                .check()
                .map_err(|_| parrot_cli::attachments::uploader::AttachmentError::Cancelled)?;
            if parse(entered)
                .iter()
                .any(|intent| matches!(intent, PromptAttachmentIntent::Path(_)))
            {
                writeln!(
                    error,
                    "parrot: attachments unavailable without authorized read service"
                )?;
                return Ok(None);
            }
            Ok(Some(wire::SendMessageRequest {
                user_session_id: session.into(),
                text: entered.into(),
                delivery: delivery.into(),
                ..Default::default()
            }))
        })
    })
}

struct UnusedOAuth;
impl OAuthClient for UnusedOAuth {
    fn now(&self) -> SystemTime {
        SystemTime::UNIX_EPOCH
    }
    fn authorization_url(&self, _: &str, _: &str, _: &str) -> String {
        panic!("OAuth must not be used in enhanced flow fixtures")
    }
    fn browser_login<'a>(
        &'a self,
        _: &'a dyn Cancellation,
    ) -> BoxFuture<'a, OperationResult<OAuthCredential, AuthError>> {
        Box::pin(async { panic!("unexpected OAuth login") })
    }
    fn start_device_authorization<'a>(
        &'a self,
        _: &'a dyn Cancellation,
    ) -> BoxFuture<'a, OperationResult<DeviceAuthorization, AuthError>> {
        Box::pin(async { panic!("unexpected device authorization") })
    }
    fn await_device_authorization<'a>(
        &'a self,
        _: &'a DeviceAuthorization,
        _: &'a dyn Cancellation,
    ) -> BoxFuture<'a, OperationResult<OAuthCredential, AuthError>> {
        Box::pin(async { panic!("unexpected device polling") })
    }
    fn refresh<'a>(
        &'a self,
        _: &'a OAuthCredential,
        _: &'a dyn Cancellation,
    ) -> BoxFuture<'a, OperationResult<OAuthCredential, AuthError>> {
        Box::pin(async { panic!("unexpected OAuth refresh") })
    }
}

#[derive(Clone, Default)]
pub struct Capture(pub Arc<Mutex<Vec<u8>>>);
impl Capture {
    pub fn text(&self) -> String {
        String::from_utf8_lossy(&self.0.lock().unwrap()).into_owned()
    }
}
impl Write for Capture {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

pub struct ScriptedInput {
    bytes: Arc<Mutex<VecDeque<u8>>>,
    pub restored: Arc<AtomicBool>,
}
#[derive(Clone)]
pub struct InputHandle {
    bytes: Arc<Mutex<VecDeque<u8>>>,
}
impl InputHandle {
    pub fn type_text(&self, value: &str) {
        self.bytes.lock().unwrap().extend(value.bytes());
    }
    pub fn end(&self) {
        self.type_text("\x04");
    }
}
impl ScriptedInput {
    pub fn new() -> (Self, InputHandle) {
        let bytes = Arc::new(Mutex::new(VecDeque::new()));
        (
            Self {
                bytes: bytes.clone(),
                restored: Arc::new(AtomicBool::new(false)),
            },
            InputHandle { bytes },
        )
    }
}
impl TerminalRead for ScriptedInput {
    fn read(&mut self, buffer: &mut [u8], cancelled: &AtomicBool) -> io::Result<ReadResult> {
        let start = Instant::now();
        loop {
            if cancelled.load(Ordering::Acquire) {
                return Err(io::Error::new(io::ErrorKind::Interrupted, "input stopped"));
            }
            let mut bytes = self.bytes.lock().unwrap();
            let count = buffer.len().min(bytes.len());
            if count > 0 {
                for byte in &mut buffer[..count] {
                    *byte = bytes.pop_front().unwrap();
                }
                return Ok(ReadResult::Bytes(count));
            }
            drop(bytes);
            if start.elapsed() >= Duration::from_millis(10) {
                return Ok(ReadResult::PollTick);
            }
            std::thread::sleep(Duration::from_millis(1));
        }
    }
    fn restore(&mut self) -> io::Result<()> {
        self.restored.store(true, Ordering::Release);
        Ok(())
    }
}

pub struct TerminalFixture {
    pub input: InputHandle,
    pub output: Capture,
    pub error: Capture,
    pub restored: Arc<AtomicBool>,
    pub columns: Arc<AtomicIsize>,
    pub terminal: Option<EnhancedTerminal>,
}
impl TerminalFixture {
    pub fn new() -> Self {
        let (input, handle) = ScriptedInput::new();
        let restored = input.restored.clone();
        let output = Capture::default();
        let error = Capture::default();
        let columns = Arc::new(AtomicIsize::new(80));
        let dimensions = {
            let columns = columns.clone();
            Arc::new(move || (columns.load(Ordering::Acquire), 30))
        };
        let terminal = EnhancedTerminal::with_input(
            Box::new(input),
            Box::new(output.clone()),
            Box::new(error.clone()),
            dimensions,
            false,
        );
        Self {
            input: handle,
            output,
            error,
            restored,
            columns,
            terminal: Some(terminal),
        }
    }
    pub fn take(&mut self) -> EnhancedTerminal {
        self.terminal.take().unwrap()
    }
    pub fn end_input(&self) -> EndInput {
        let input = self.input.clone();
        Arc::new(move || input.end())
    }
    #[track_caller]
    pub fn output_contains<'a>(
        &'a self,
        text: &'a str,
    ) -> impl std::future::Future<Output = ()> + 'a {
        let location = std::panic::Location::caller();
        wait_until_described(
            move || self.output.text().contains(text),
            move || {
                format!(
                    "terminal output {text:?} at {location}; output: {:?}; error: {:?}",
                    self.output.text(),
                    self.error.text()
                )
            },
        )
    }
}

pub type EndInput = Arc<dyn Fn() + Send + Sync>;

pub async fn run_flow(
    cli: EnhancedCli,
    stopping: Arc<Signal>,
    end_input: EndInput,
    scenario: impl std::future::Future<Output = ()>,
) -> Result<i32, parrot_cli::terminal_dialogs::EnhancedFailure> {
    let joined = Signal::new();
    let running = async {
        let result = cli.run(stopping.as_ref()).await;
        joined.cancel();
        result
    };
    let observing = async {
        let result = std::panic::AssertUnwindSafe(scenario).catch_unwind().await;
        if result.is_err() {
            stopping.cancel();
            end_input();
        }
        result
    };
    let watchdog = async {
        tokio::select! {
            biased;
            () = joined.cancelled() => false,
            () = tokio::time::sleep(Duration::from_secs(20)) => {
                stopping.cancel();
                end_input();
                true
            }
        }
    };
    let (result, observation, timed_out) = tokio::join!(running, observing, watchdog);
    if let Err(panic) = observation {
        std::panic::resume_unwind(panic);
    }
    assert!(
        !timed_out,
        "enhanced flow exceeded 20s; root cancellation and input EOF were requested and the actual CLI joined before this assertion"
    );
    result
}

#[track_caller]
pub fn wait_until(predicate: impl Fn() -> bool) -> impl std::future::Future<Output = ()> {
    let location = std::panic::Location::caller();
    wait_until_described(predicate, move || format!("predicate at {location}"))
}

async fn wait_until_described(predicate: impl Fn() -> bool, description: impl Fn() -> String) {
    let reached = tokio::time::timeout(Duration::from_secs(8), async {
        while !predicate() {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await;
    assert!(
        reached.is_ok(),
        "controlled enhanced flow did not reach expected observation: {}",
        description()
    );
}

pub fn question(id: &str, prompt: &str) -> wire::PendingQuestion {
    wire::PendingQuestion {
        id: id.into(),
        questions: vec![wire::QuestionDefinition {
            header: "Question".into(),
            prompt: prompt.into(),
            custom: true,
            ..Default::default()
        }],
        ..Default::default()
    }
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
mod pty {
    use super::*;
    #[cfg(target_os = "macos")]
    use native_termios::DarwinTermios as PlatformTermios;
    #[cfg(target_os = "linux")]
    use native_termios::LinuxTermios as PlatformTermios;
    use parrot_cli::terminal_input::RawTerminal;
    #[cfg(target_os = "linux")]
    const O_NONBLOCK: i32 = 0x800;
    #[cfg(target_os = "macos")]
    const O_NONBLOCK: i32 = 0x4;
    #[cfg(target_os = "linux")]
    const ISIG: u32 = 1;
    #[cfg(target_os = "macos")]
    const ISIG: u64 = 0x80;
    use std::{
        ffi::c_void,
        fs::File,
        os::fd::{AsRawFd, FromRawFd},
        thread::JoinHandle,
    };

    #[cfg_attr(target_os = "linux", link(name = "util"))]
    unsafe extern "C" {
        fn openpty(
            master: *mut i32,
            slave: *mut i32,
            name: *mut u8,
            attributes: *const c_void,
            size: *const c_void,
        ) -> i32;
    }
    unsafe extern "C" {
        fn tcgetattr(fd: i32, attributes: *mut PlatformTermios) -> i32;
        fn fcntl(fd: i32, command: i32, ...) -> i32;
        fn read(fd: i32, buffer: *mut c_void, count: usize) -> isize;
    }

    pub struct PtyFixture {
        master: File,
        slave: File,
        original: PlatformTermios,
        stop: Arc<AtomicBool>,
        reader: Option<JoinHandle<()>>,
        pub output: Capture,
        pub columns: Arc<AtomicIsize>,
        terminal: Option<EnhancedTerminal>,
    }
    impl PtyFixture {
        pub fn new() -> Self {
            let (mut master, mut slave) = (-1, -1);
            // SAFETY: valid out-pointers, optional termios/winsize omitted.
            assert_eq!(
                unsafe {
                    openpty(
                        &mut master,
                        &mut slave,
                        std::ptr::null_mut(),
                        std::ptr::null(),
                        std::ptr::null(),
                    )
                },
                0
            );
            // SAFETY: successful openpty transferred two distinct owned descriptors.
            let (master, slave) = unsafe { (File::from_raw_fd(master), File::from_raw_fd(slave)) };
            let mut original = PlatformTermios::default();
            // SAFETY: target-selected native libc termios layout and owned PTY slave.
            assert_eq!(unsafe { tcgetattr(slave.as_raw_fd(), &mut original) }, 0);
            let raw = RawTerminal::open(slave.as_raw_fd())
                .unwrap()
                .expect("PTY must support real raw input");
            let mut applied = PlatformTermios::default();
            // SAFETY: same descriptor/layout as above.
            assert_eq!(unsafe { tcgetattr(slave.as_raw_fd(), &mut applied) }, 0);
            assert_eq!(
                applied.local_flags & ISIG,
                0,
                "ISIG must be disabled before raw Ctrl-C bytes"
            );
            // Nonblocking drain prevents the real PTY output buffer blocking the renderer.
            // SAFETY: native F_GETFL/F_SETFL accept a valid descriptor and target-selected O_NONBLOCK.
            let flags = unsafe { fcntl(master.as_raw_fd(), 3) };
            assert!(flags >= 0);
            assert_eq!(
                unsafe { fcntl(master.as_raw_fd(), 4, flags | O_NONBLOCK) },
                0
            );
            let output = Capture::default();
            let capture = output.clone();
            let stop = Arc::new(AtomicBool::new(false));
            let stopping = stop.clone();
            let drain = master.try_clone().unwrap();
            let reader = std::thread::spawn(move || {
                let mut buffer = [0u8; 4096];
                loop {
                    // SAFETY: writable buffer and a retained owned PTY descriptor.
                    let count = unsafe {
                        read(drain.as_raw_fd(), buffer.as_mut_ptr().cast(), buffer.len())
                    };
                    if count > 0 {
                        capture
                            .0
                            .lock()
                            .unwrap()
                            .extend_from_slice(&buffer[..count as usize]);
                        continue;
                    }
                    if stopping.load(Ordering::Acquire) {
                        break;
                    }
                    if count < 0
                        && !matches!(
                            io::Error::last_os_error().kind(),
                            io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted
                        )
                    {
                        break;
                    }
                    std::thread::sleep(Duration::from_millis(2));
                }
            });
            let columns = Arc::new(AtomicIsize::new(80));
            let dimensions = {
                let columns = columns.clone();
                Arc::new(move || (columns.load(Ordering::Acquire), 30))
            };
            let terminal = EnhancedTerminal::new(
                raw,
                Box::new(slave.try_clone().unwrap()),
                Box::new(Capture::default()),
                dimensions,
                false,
            );
            Self {
                master,
                slave,
                original,
                stop,
                reader: Some(reader),
                output,
                columns,
                terminal: Some(terminal),
            }
        }
        pub fn take(&mut self) -> EnhancedTerminal {
            self.terminal.take().unwrap()
        }
        pub fn end_input(&self) -> EndInput {
            let master = Mutex::new(self.master.try_clone().unwrap());
            Arc::new(move || {
                let _ = master.lock().unwrap().write_all(b"\x04");
            })
        }
        pub fn type_text(&mut self, value: &str) {
            self.master.write_all(value.as_bytes()).unwrap();
        }
        #[track_caller]
        pub fn output_contains<'a>(
            &'a self,
            text: &'a str,
        ) -> impl std::future::Future<Output = ()> + 'a {
            let location = std::panic::Location::caller();
            wait_until_described(
                move || self.output.text().contains(text),
                move || {
                    format!(
                        "native PTY output {text:?} at {location}; output: {:?}",
                        self.output.text()
                    )
                },
            )
        }
        pub fn assert_restored_and_join(&mut self) {
            let mut restored = PlatformTermios::default();
            // SAFETY: descriptor owner remains alive until after the enhanced run joined.
            assert_eq!(
                unsafe { tcgetattr(self.slave.as_raw_fd(), &mut restored) },
                0
            );
            assert_eq!(
                restored, self.original,
                "normal/error return must restore exact termios"
            );
            self.stop.store(true, Ordering::Release);
            self.reader.take().unwrap().join().unwrap();
        }
    }
    impl Drop for PtyFixture {
        fn drop(&mut self) {
            // Drop the raw lease first if setup/flow assertion failed early.
            self.terminal.take();
            self.stop.store(true, Ordering::Release);
            if let Some(reader) = self.reader.take() {
                let _ = reader.join();
            }
        }
    }
}
#[cfg(any(target_os = "linux", target_os = "macos"))]
pub use pty::PtyFixture;
