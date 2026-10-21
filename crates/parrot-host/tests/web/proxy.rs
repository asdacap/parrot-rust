//! Synthetic UDS endpoints exercise routing, not production CLI construction.
#![cfg(unix)]

use bytes::Bytes;
use futures_util::StreamExt;
use parrot_domain::{
    contracts::async_ops::{BoxFuture, Cancellation},
    protocol::{bindings::parrot::parrot_server::Parrot, messages::parrot as wire},
    values::identifiers::UserSessionId,
};
use parrot_foundation::paths::{ApplicationRoots, UserSessionResources, discover_workspace};
use parrot_host::{
    grpc::transport::{GrpcServer, GrpcTransportClient},
    web::{
        proxy::WebParrotProxy,
        router::{WebOpenedSession, WebSessionRouter, WebStartup},
    },
};
use parrot_storage::{diagnostics::FileDiagnosticLog, events::NeverCancelled};
use protobuf::Message;
use std::{
    collections::HashMap,
    fs,
    path::PathBuf,
    pin::Pin,
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};
use tonic::{Code, Request, Response, Status};

const REMOTE: &str = "user-session-remote";
const LOCAL: &str = "user-session-local";
type Events = Pin<Box<dyn futures_core::Stream<Item = Result<wire::Event, Status>> + Send>>;

#[derive(Clone)]
struct Endpoint {
    name: &'static str,
    calls: Arc<Mutex<Vec<String>>>,
    sent: Arc<Mutex<Vec<Vec<u8>>>>,
    active: Arc<AtomicUsize>,
    wrong_identity: bool,
}
impl Endpoint {
    fn new(name: &'static str) -> Self {
        Self {
            name,
            calls: Arc::new(Mutex::new(Vec::new())),
            sent: Arc::new(Mutex::new(Vec::new())),
            active: Arc::new(AtomicUsize::new(0)),
            wrong_identity: false,
        }
    }
    fn record(&self, method: &str) {
        self.calls.lock().unwrap().push(method.to_owned());
    }
    fn response<T>(&self, value: T) -> Response<T> {
        let mut response = Response::new(value);
        response
            .metadata_mut()
            .insert("x-fixture", self.name.parse().unwrap());
        response
    }
}

macro_rules! endpoint {
    ($( $method:ident : $request:ident => $response:ident ),* $(,)?) => {
        #[tonic::async_trait]
        impl Parrot for Endpoint {
            $(async fn $method(&self, _: Request<wire::$request>) -> Result<Response<wire::$response>, Status> {
                self.record(stringify!($method));
                Ok(self.response(wire::$response::new()))
            })*
            async fn resume_session(&self, _: Request<wire::ResumeSessionRequest>) -> Result<Response<wire::UserSession>, Status> {
                self.record("resume_session");
                Err(Status::already_exists("the session is open elsewhere"))
            }
            async fn attach_session(&self, request: Request<wire::AttachSessionRequest>) -> Result<Response<wire::UserSession>, Status> {
                self.record("attach_session");
                if self.name == "service" { return Err(Status::not_found("no user session")); }
                Ok(self.response(wire::UserSession { id: if self.wrong_identity { "user-session-wrong".into() } else { request.into_inner().user_session_id }, ..Default::default() }))
            }
            async fn send_message(&self, request: Request<wire::SendMessageRequest>) -> Result<Response<wire::SendMessageResponse>, Status> {
                self.record("send_message");
                let request = request.into_inner();
                self.sent.lock().unwrap().push(request.write_to_bytes().unwrap());
                if request.text == "error" || request.text == "unavailable" {
                    let mut metadata = tonic::metadata::MetadataMap::new();
                    metadata.insert("x-error", "retained".parse().unwrap());
                    return Err(Status::with_details_and_metadata(if request.text == "error" { Code::FailedPrecondition } else { Code::Unavailable }, "remote detail % \u{03bb}", Bytes::from_static(b"opaque-status-details"), metadata));
                }
                let mut response = wire::SendMessageResponse::new();
                response.special_fields.mut_unknown_fields().add_varint(900, 321);
                Ok(self.response(response))
            }
            async fn upload_attachment(&self, mut request: Request<tonic::Streaming<wire::AttachmentUploadFrame>>) -> Result<Response<wire::AttachmentUploadResponse>, Status> {
                self.record("upload_attachment");
                while request.get_mut().message().await?.is_some() {}
                Ok(self.response(wire::AttachmentUploadResponse::new()))
            }
            type ListenStream = Events;
            async fn listen(&self, request: Request<wire::ListenRequest>) -> Result<Response<Events>, Status> {
                self.record("listen");
                let id = request.into_inner().user_session_id;
                let mut event = wire::Event { agent_session_id: "main".into(), payload: Some(wire::event::Payload::TurnStarted(wire::TurnStarted::new())), ..Default::default() };
                event.special_fields.mut_unknown_fields().add_length_delimited(901, vec![1, 2, 3]);
                let guard = Subscription::new(self.active.clone());
                let stream = futures_util::stream::unfold((Some(event), Some(guard), id), |(mut event, guard, id)| async move {
                    if let Some(event) = event.take() { return Some((Ok(event), (None, guard, id))); }
                    if id == "user-session-error-stream" {
                        let mut metadata = tonic::metadata::MetadataMap::new();
                        metadata.insert("x-trailer", "stream-error".parse().unwrap());
                        return Some((Err(Status::with_details_and_metadata(Code::Unavailable, "stream gone", Bytes::from_static(b"stream-details"), metadata)), (None, None, "ended".into())));
                    }
                    if id == "user-session-success-stream" {
                        let mut metadata = tonic::metadata::MetadataMap::new();
                        metadata.insert("x-success", "retained-success-trailer".parse().unwrap());
                        return Some((Err(Status::with_metadata(Code::Ok, "", metadata)), (None, None, "ended".into())));
                    }
                    guard.as_ref()?;
                    std::future::pending().await
                });
                Ok(self.response(Box::pin(stream) as Events))
            }
        }
    };
}
endpoint! {
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
    update_session: UpdateSessionRequest => UserSession,
    set_goal: SetGoalRequest => SetGoalResponse,
    set_standing_instruction: SetStandingInstructionRequest => SetStandingInstructionResponse,
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

struct Subscription(Arc<AtomicUsize>);
impl Subscription {
    fn new(active: Arc<AtomicUsize>) -> Self {
        active.fetch_add(1, Ordering::SeqCst);
        Self(active)
    }
}
impl Drop for Subscription {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}

struct Startup(Mutex<Option<WebOpenedSession>>);
impl WebStartup for Startup {
    fn open<'a>(
        &'a self,
        _: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<WebOpenedSession, Status>> {
        Box::pin(async {
            self.0
                .lock()
                .unwrap()
                .take()
                .ok_or_else(|| Status::not_found("no scripted startup"))
        })
    }
}
struct Directory(PathBuf);
impl Directory {
    fn new() -> Self {
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        let root = PathBuf::from(format!(
            "/tmp/t64p-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::SeqCst)
        ));
        fs::create_dir_all(&root).unwrap();
        Self(root)
    }
}
impl Drop for Directory {
    fn drop(&mut self) {
        fs::remove_dir_all(&self.0).unwrap();
    }
}
struct Fixture {
    _directory: Directory,
    diagnostics: Arc<FileDiagnosticLog>,
    roots: ApplicationRoots,
    workspace: parrot_domain::values::ProjectWorkspace,
    local: Endpoint,
    remote: Endpoint,
    local_server: GrpcServer,
    remote_server: GrpcServer,
    router: Arc<WebSessionRouter>,
}
impl Fixture {
    async fn new() -> Self {
        Self::with_remote(Endpoint::new("remote")).await
    }
    async fn with_remote(remote: Endpoint) -> Self {
        let directory = Directory::new();
        let diagnostics =
            Arc::new(FileDiagnosticLog::open_global(&directory.0, "web", |_| {}).unwrap());
        let roots = ApplicationRoots::resolve(&HashMap::from([
            ("HOME".into(), directory.0.to_string_lossy().into_owned()),
            (
                "XDG_STATE_HOME".into(),
                directory.0.join("s").to_string_lossy().into_owned(),
            ),
        ]));
        let workspace = discover_workspace(&directory.0).unwrap();
        let id = UserSessionId::parse(REMOTE).unwrap();
        let selected = roots.select_new(&id, workspace.launch_directory()).unwrap();
        let resources = UserSessionResources::new(selected, id, workspace.clone()).unwrap();
        fs::create_dir_all(resources.root()).unwrap();
        let local = Endpoint::new("local");
        let local_server = GrpcServer::start_local(
            local.clone(),
            directory.0.join("l.sock"),
            diagnostics.clone(),
        )
        .await
        .unwrap();
        let remote_server =
            GrpcServer::start_local(remote.clone(), resources.socket_path(), diagnostics.clone())
                .await
                .unwrap();
        let mut connection =
            GrpcTransportClient::connect(local_server.address(), None, None, diagnostics.clone())
                .unwrap();
        let router = Arc::new(WebSessionRouter::new(
            connection.client().unwrap().clone(),
            roots.clone(),
            workspace.clone(),
            Arc::new(Startup(Mutex::new(None))),
            diagnostics.clone(),
        ));
        Self {
            _directory: directory,
            diagnostics,
            roots,
            workspace,
            local,
            remote,
            local_server,
            remote_server,
            router,
        }
    }
    fn proxy(&self) -> WebParrotProxy<Endpoint> {
        WebParrotProxy::new(Endpoint::new("service"), self.router.clone())
    }
    async fn close(mut self) {
        self.router.forget(REMOTE);
        self.local_server.close().await.unwrap();
        self.remote_server.close().await.unwrap();
    }
    async fn attach(&self) {
        self.proxy()
            .resume_session(Request::new(wire::ResumeSessionRequest {
                user_session_id: REMOTE.into(),
                working_directory: self
                    .workspace
                    .launch_directory()
                    .to_string_lossy()
                    .into_owned(),
                ..Default::default()
            }))
            .await
            .unwrap();
    }
}
fn send(id: &str, text: &str) -> Request<wire::SendMessageRequest> {
    Request::new(wire::SendMessageRequest {
        user_session_id: id.into(),
        text: text.into(),
        ..Default::default()
    })
}
fn listen(id: &str) -> Request<wire::ListenRequest> {
    Request::new(wire::ListenRequest {
        user_session_id: id.into(),
        ..Default::default()
    })
}

#[tokio::test]
async fn remote_session_attaches_through_real_resource_socket_and_local_stays_local() {
    let fixture = Fixture::new().await;
    fixture.attach().await;
    assert!(fixture.router.is_remote(REMOTE));
    fixture
        .proxy()
        .send_message(send(REMOTE, "to remote"))
        .await
        .unwrap();
    fixture
        .proxy()
        .send_message(send(LOCAL, "to local"))
        .await
        .unwrap();
    assert_eq!(fixture.remote.sent.lock().unwrap().len(), 1);
    assert_eq!(fixture.local.sent.lock().unwrap().len(), 1);
    fixture.close().await;
}

#[tokio::test]
async fn route_map_global_methods_use_service_and_session_methods_use_router() {
    let fixture = Fixture::new().await;
    fixture.attach().await;
    let service = Endpoint::new("service");
    let proxy = WebParrotProxy::new(service.clone(), fixture.router.clone());
    macro_rules! local_calls { ($($method:ident:$request:ident),* $(,)?) => { $(assert_eq!(proxy.$method(Request::new(wire::$request::new())).await.unwrap().metadata().get("x-fixture").unwrap(), "service");)* }; }
    local_calls! {
        list_models:ListModelsRequest, list_model_aliases:ListModelAliasesRequest,
        configure_model_alias:ConfigureModelAliasRequest, list_provider_model_alias_defaults:ListProviderModelAliasDefaultsRequest,
        apply_provider_model_alias_defaults:ApplyProviderModelAliasDefaultsRequest, list_model_presets:ListModelPresetsRequest,
        list_modes:ListModesRequest, create_session:CreateSessionRequest, list_sessions:ListSessionsRequest,
    }
    macro_rules! routed_calls { ($($method:ident:$request:ident),* $(,)?) => { $(assert_eq!(proxy.$method(Request::new(wire::$request { user_session_id: REMOTE.into(), ..Default::default() })).await.unwrap().metadata().get("x-fixture").unwrap(), "remote");)* }; }
    routed_calls! {
        set_model_preset:SetModelPresetRequest, select_model_preset:SelectModelPresetRequest,
        list_skills:ListSkillsRequest, configure_skill:ConfigureSkillRequest, update_session:UpdateSessionRequest,
        set_goal:SetGoalRequest, set_standing_instruction:SetStandingInstructionRequest, interrupt:InterruptRequest,
        compact:CompactRequest, set_context_limit:SetContextLimitRequest, sandbox_enable:SandboxEnableRequest,
        session_status:SessionStatusRequest, list_pending_questions:ListPendingQuestionsRequest, reply_question:ReplyQuestionRequest,
        reject_question:RejectQuestionRequest, list_pending_permissions:ListPendingPermissionsRequest, reply_permission:ReplyPermissionRequest,
    }
    assert_eq!(service.calls.lock().unwrap().len(), 9);
    fixture.close().await;
}

#[tokio::test]
async fn native_proxy_upload_stays_on_direct_service_even_when_session_is_remote() {
    let fixture = Fixture::new().await;
    fixture.attach().await;
    let service = Endpoint::new("service");
    let mut server = GrpcServer::start_local(
        WebParrotProxy::new(service.clone(), fixture.router.clone()),
        fixture._directory.0.join("p.sock"),
        fixture.diagnostics.clone(),
    )
    .await
    .unwrap();
    let mut client =
        GrpcTransportClient::connect(server.address(), None, None, fixture.diagnostics.clone())
            .unwrap();
    let response = client
        .client()
        .unwrap()
        .upload_attachment(tokio_stream::iter([wire::AttachmentUploadFrame::new()]))
        .await
        .unwrap();
    assert_eq!(response.metadata().get("x-fixture").unwrap(), "service");
    assert_eq!(
        service.calls.lock().unwrap().as_slice(),
        ["upload_attachment"]
    );
    assert!(
        !fixture
            .remote
            .calls
            .lock()
            .unwrap()
            .iter()
            .any(|call| call == "upload_attachment")
    );
    client.close();
    server.close().await.unwrap();
    fixture.close().await;
}

#[tokio::test]
async fn forwarding_preserves_unknown_fields_response_metadata_status_details_and_trailers() {
    let fixture = Fixture::new().await;
    fixture.attach().await;
    let mut request = send(REMOTE, "opaque");
    request
        .get_mut()
        .special_fields
        .mut_unknown_fields()
        .add_length_delimited(902, vec![0, 255, 8]);
    let bytes = request.get_ref().write_to_bytes().unwrap();
    let response = fixture.proxy().send_message(request).await.unwrap();
    assert_eq!(fixture.remote.sent.lock().unwrap()[0], bytes);
    assert_eq!(response.metadata().get("x-fixture").unwrap(), "remote");
    assert!(
        response
            .get_ref()
            .special_fields
            .unknown_fields()
            .iter()
            .any(|(number, _)| number == 900)
    );
    let failure = fixture
        .proxy()
        .send_message(send(REMOTE, "error"))
        .await
        .unwrap_err();
    assert_eq!(failure.code(), Code::FailedPrecondition);
    assert_eq!(failure.message(), "remote detail % \u{03bb}");
    assert_eq!(failure.details(), b"opaque-status-details");
    assert_eq!(failure.metadata().get("x-error").unwrap(), "retained");
    assert!(fixture.router.is_remote(REMOTE));
    fixture.close().await;
}

#[tokio::test]
async fn unavailable_forgets_only_failed_session_route() {
    let fixture = Fixture::new().await;
    fixture.attach().await;
    let failure = fixture
        .proxy()
        .send_message(send(REMOTE, "unavailable"))
        .await
        .unwrap_err();
    assert_eq!(failure.code(), Code::Unavailable);
    assert!(!fixture.router.is_remote(REMOTE));
    fixture
        .proxy()
        .send_message(send(REMOTE, "now local"))
        .await
        .unwrap();
    assert_eq!(fixture.local.sent.lock().unwrap().len(), 1);
    fixture.close().await;
}

#[tokio::test]
async fn wrong_remote_identity_is_not_adopted_and_missing_session_is_not_invented() {
    let mut endpoint = Endpoint::new("remote");
    endpoint.wrong_identity = true;
    let fixture = Fixture::with_remote(endpoint).await;
    let request = wire::AttachSessionRequest {
        user_session_id: REMOTE.into(),
        ..Default::default()
    };
    let error = fixture
        .router
        .attach_remote(request, &NeverCancelled)
        .await
        .unwrap_err();
    assert_eq!(error.code(), Code::Internal);
    assert!(error.message().contains("different user session"));
    assert!(!fixture.router.is_remote(REMOTE));
    let error = fixture
        .proxy()
        .attach_session(Request::new(wire::AttachSessionRequest {
            user_session_id: "user-session-missing".into(),
            ..Default::default()
        }))
        .await
        .unwrap_err();
    assert_eq!(error.code(), Code::NotFound);
    fixture.close().await;
}

#[tokio::test]
async fn startup_adopts_only_explicit_remote_owner() {
    let fixture = Fixture::new().await;
    let connection = GrpcTransportClient::connect(
        fixture.remote_server.address(),
        None,
        None,
        fixture.diagnostics.clone(),
    )
    .unwrap();
    let mut session = wire::UserSession {
        id: REMOTE.into(),
        ..Default::default()
    };
    session
        .special_fields
        .mut_unknown_fields()
        .add_varint(903, 47);
    let expected = session.write_to_bytes().unwrap();
    let mut local = GrpcTransportClient::connect(
        fixture.local_server.address(),
        None,
        None,
        fixture.diagnostics.clone(),
    )
    .unwrap();
    let router = WebSessionRouter::new(
        local.client().unwrap().clone(),
        fixture.roots.clone(),
        fixture.workspace.clone(),
        Arc::new(Startup(Mutex::new(Some(WebOpenedSession {
            session,
            remote: Some(connection),
        })))),
        fixture.diagnostics.clone(),
    );
    assert_eq!(
        router
            .open_default(&NeverCancelled)
            .await
            .unwrap()
            .write_to_bytes()
            .unwrap(),
        expected
    );
    assert!(router.is_remote(REMOTE));
    router.forget(REMOTE);
    assert!(!router.is_remote(REMOTE));
    fixture.close().await;
}

#[tokio::test]
async fn listen_observes_unknown_event_and_drop_releases_upstream_subscription() {
    let fixture = Fixture::new().await;
    fixture.attach().await;
    let response = fixture.proxy().listen(listen(REMOTE)).await.unwrap();
    assert_eq!(response.metadata().get("x-fixture").unwrap(), "remote");
    let mut stream = response.into_inner();
    let event = stream.next().await.unwrap().unwrap();
    assert!(
        event
            .special_fields
            .unknown_fields()
            .iter()
            .any(|(number, _)| number == 901)
    );
    assert!(fixture.router.is_busy(REMOTE));
    assert_eq!(fixture.remote.active.load(Ordering::SeqCst), 1);
    drop(stream);
    tokio::time::timeout(Duration::from_secs(2), async {
        while fixture.remote.active.load(Ordering::SeqCst) != 0 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    fixture.close().await;
}

#[tokio::test]
async fn listen_failure_keeps_exact_status_metadata_and_releases_observer() {
    let fixture = Fixture::new().await;
    let mut stream = fixture
        .proxy()
        .listen(listen("user-session-error-stream"))
        .await
        .unwrap()
        .into_inner();
    stream.next().await.unwrap().unwrap();
    let error = stream.next().await.unwrap().unwrap_err();
    assert_eq!(error.code(), Code::Unavailable);
    assert_eq!(error.message(), "stream gone");
    assert_eq!(error.details(), b"stream-details");
    assert_eq!(error.metadata().get("x-trailer").unwrap(), "stream-error");
    assert!(stream.next().await.is_none());
    assert_eq!(fixture.local.active.load(Ordering::SeqCst), 0);
    fixture.close().await;
}

#[tokio::test]
async fn native_proxy_stream_success_retains_upstream_trailers_and_initial_metadata() {
    let fixture = Fixture::new().await;
    let mut server = GrpcServer::start_local(
        fixture.proxy(),
        fixture._directory.0.join("p.sock"),
        fixture.diagnostics.clone(),
    )
    .await
    .unwrap();
    let mut client =
        GrpcTransportClient::connect(server.address(), None, None, fixture.diagnostics.clone())
            .unwrap();
    let response = client
        .client()
        .unwrap()
        .listen(listen("user-session-success-stream"))
        .await
        .unwrap();
    assert_eq!(response.metadata().get("x-fixture").unwrap(), "local");
    let mut stream = response.into_inner();
    assert!(stream.message().await.unwrap().is_some());
    assert!(stream.message().await.unwrap().is_none());
    let trailers = stream.trailers().await.unwrap().unwrap();
    assert_eq!(
        trailers.get("x-success").unwrap(),
        "retained-success-trailer"
    );
    assert!(stream.message().await.unwrap().is_none());
    assert_eq!(fixture.local.active.load(Ordering::SeqCst), 0);
    drop(stream);
    client.close();
    server.close().await.unwrap();
    fixture.close().await;
}

#[tokio::test]
async fn source_busy_activity_eight_static_rows() {
    let fixture = Fixture::new().await;
    for (index, (events, expected)) in [
        ("", false),
        ("main:turn_started", true),
        ("main:turn_started,main:turn_ended", true),
        (
            "main:turn_started,main:turn_ended,main:mode_turn_completed",
            false,
        ),
        ("main:turn_started,main:turn_failed", false),
        (
            "child:agent_started,main:turn_started,child:turn_ended",
            true,
        ),
        ("child:agent_started,child:turn_started", false),
        ("main:root_started,main:turn_started", true),
    ]
    .into_iter()
    .enumerate()
    {
        let id = format!("busy-{index}");
        for entry in events.split(',').filter(|entry| !entry.is_empty()) {
            let (agent, kind) = entry.split_once(':').unwrap();
            use wire::event::Payload;
            let payload = match kind {
                "agent_started" => Payload::AgentStarted(wire::AgentStarted {
                    parent_agent_session_id: "main".into(),
                    ..Default::default()
                }),
                "root_started" => Payload::AgentStarted(wire::AgentStarted::new()),
                "turn_started" => Payload::TurnStarted(wire::TurnStarted::new()),
                "turn_ended" => Payload::TurnEnded(wire::TurnEnded::new()),
                "mode_turn_completed" => Payload::ModeTurnCompleted(wire::ModeTurnCompleted::new()),
                _ => Payload::TurnFailed(wire::TurnFailed::new()),
            };
            fixture.router.observe(
                &id,
                &wire::Event {
                    agent_session_id: agent.into(),
                    payload: Some(payload),
                    ..Default::default()
                },
            );
        }
        assert_eq!(fixture.router.is_busy(&id), expected, "{events}");
    }
    fixture.close().await;
}
