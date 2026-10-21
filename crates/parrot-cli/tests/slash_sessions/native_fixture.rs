//! Controlled native endpoint and supplied dialog for slash-component tests.
//! These fixtures are not production provider/service/runtime construction proof.

use std::{
    collections::VecDeque,
    path::PathBuf,
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
};

use parrot_cli::dispatch::dialog::{DialogOption, SlashDialog};
use parrot_domain::{
    contracts::async_ops::{BoxFuture, Cancellation},
    protocol::{bindings::parrot::parrot_server::Parrot, messages::parrot as wire},
};
use parrot_host::{
    grpc::transport::{GrpcServer, GrpcTransportClient},
    web::router::WebClient,
};
use parrot_storage::diagnostics::FileDiagnosticLog;
use protobuf::Message;
use tokio::sync::watch;
use tonic::{Request, Response, Status};

static NEXT: AtomicUsize = AtomicUsize::new(0);

#[derive(Clone)]
pub struct Stop(watch::Sender<bool>);
impl Stop {
    pub fn new() -> Self {
        Self(watch::channel(false).0)
    }
    pub fn cancel(&self) {
        self.0.send_replace(true);
    }
}
impl Default for Stop {
    fn default() -> Self {
        Self::new()
    }
}
impl Cancellation for Stop {
    fn is_cancelled(&self) -> bool {
        *self.0.borrow()
    }
    fn cancelled(&self) -> BoxFuture<'_, ()> {
        Box::pin(async move {
            let mut receiver = self.0.subscribe();
            loop {
                if *receiver.borrow_and_update() {
                    return;
                }
                if receiver.changed().await.is_err() {
                    return;
                }
            }
        })
    }
}

#[derive(Clone, Debug)]
pub struct Call {
    pub method: &'static str,
    pub body: Vec<u8>,
}

type Handler =
    Arc<dyn Fn(&'static str, Vec<u8>) -> BoxFuture<'static, Result<Vec<u8>, Status>> + Send + Sync>;

struct Endpoint {
    calls: Arc<Mutex<Vec<Call>>>,
    handler: Handler,
}
impl Endpoint {
    async fn invoke<Q: Message, R: Message>(
        &self,
        method: &'static str,
        request: Q,
    ) -> Result<Response<R>, Status> {
        let body = request
            .write_to_bytes()
            .map_err(|failure| Status::internal(failure.to_string()))?;
        self.calls.lock().unwrap().push(Call {
            method,
            body: body.clone(),
        });
        let body = (self.handler)(method, body).await?;
        let response =
            R::parse_from_bytes(&body).map_err(|failure| Status::internal(failure.to_string()))?;
        Ok(Response::new(response))
    }
}

macro_rules! endpoint {
    ($($method:ident($request:ty) -> $response:ty),* $(,)?) => {
        #[tonic::async_trait]
        impl Parrot for Endpoint {
            $(async fn $method(&self, request: Request<$request>) -> Result<Response<$response>, Status> {
                self.invoke(stringify!($method), request.into_inner()).await
            })*
            async fn upload_attachment(&self, _: Request<tonic::Streaming<wire::AttachmentUploadFrame>>) -> Result<Response<wire::AttachmentUploadResponse>, Status> {
                Err(Status::unimplemented("unused slash fixture streaming upload"))
            }
            type ListenStream = tokio_stream::Empty<Result<wire::Event, Status>>;
            async fn listen(&self, _: Request<wire::ListenRequest>) -> Result<Response<Self::ListenStream>, Status> {
                Err(Status::unimplemented("unused slash fixture event stream"))
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

pub struct Fixture {
    pub root: PathBuf,
    pub calls: Arc<Mutex<Vec<Call>>>,
    connection: GrpcTransportClient,
    server: GrpcServer,
}
impl Fixture {
    pub async fn start(
        handler: impl Fn(&'static str, &[u8]) -> Result<Vec<u8>, Status> + Send + Sync + 'static,
    ) -> Self {
        Self::start_async(move |method, body| {
            let result = handler(method, &body);
            Box::pin(async move { result })
        })
        .await
    }

    pub async fn start_async(
        handler: impl Fn(&'static str, Vec<u8>) -> BoxFuture<'static, Result<Vec<u8>, Status>>
        + Send
        + Sync
        + 'static,
    ) -> Self {
        let root = PathBuf::from(format!(
            "/tmp/t67-cat-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::SeqCst)
        ));
        std::fs::create_dir_all(&root).unwrap();
        let diagnostics = Arc::new(
            FileDiagnosticLog::open_global(root.join("logs"), "catalog-fixture", |_| {}).unwrap(),
        );
        let calls = Arc::new(Mutex::new(Vec::new()));
        let endpoint = Endpoint {
            calls: calls.clone(),
            handler: Arc::new(handler),
        };
        let server =
            GrpcServer::start_local(endpoint, root.join("native.sock"), diagnostics.clone())
                .await
                .unwrap();
        let connection =
            GrpcTransportClient::connect(server.address(), None, None, diagnostics).unwrap();
        Self {
            root,
            calls,
            connection,
            server,
        }
    }
    pub fn client(&mut self) -> WebClient {
        self.connection.client().unwrap().clone()
    }
    pub async fn close(mut self) {
        self.connection.close();
        self.server.close().await.unwrap();
    }
}

#[derive(Default)]
pub struct RecordingDialog {
    pub selections: VecDeque<Option<String>>,
    pub texts: VecDeque<Option<String>>,
    pub secrets: VecDeque<Option<String>>,
    pub confirmations: VecDeque<bool>,
    pub pickers: Vec<(String, Vec<DialogOption>)>,
    pub shown: Vec<String>,
    pub printed: Vec<String>,
    pub errors: Vec<String>,
    pub loads: Vec<String>,
    pub trace: Arc<Mutex<Vec<String>>>,
}
impl RecordingDialog {
    fn observe(&self, value: impl Into<String>) {
        self.trace.lock().unwrap().push(value.into());
    }
}
fn check(cancellation: &dyn Cancellation) -> Result<(), Status> {
    cancellation
        .check()
        .map_err(|_| Status::cancelled("fixture cancelled"))
}
impl SlashDialog for RecordingDialog {
    fn select<'a>(
        &'a mut self,
        title: &'a str,
        options: &'a [DialogOption],
        c: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<Option<DialogOption>, Status>> {
        Box::pin(async move {
            check(c)?;
            self.observe(format!("select:{title}"));
            self.pickers.push((title.into(), options.to_vec()));
            let selected = self.selections.pop_front().flatten();
            Ok(selected
                .and_then(|selected| options.iter().find(|option| option.id == selected).cloned()))
        })
    }
    fn read_text<'a>(
        &'a mut self,
        _: &'a str,
        c: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<Option<String>, Status>> {
        Box::pin(async move {
            check(c)?;
            Ok(self.texts.pop_front().flatten())
        })
    }
    fn read_secret<'a>(
        &'a mut self,
        _: &'a str,
        c: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<Option<String>, Status>> {
        Box::pin(async move {
            check(c)?;
            Ok(self.secrets.pop_front().flatten())
        })
    }
    fn confirm<'a>(
        &'a mut self,
        _: &'a [String],
        c: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<bool, Status>> {
        Box::pin(async move {
            check(c)?;
            Ok(self.confirmations.pop_front().unwrap_or(false))
        })
    }
    fn show<'a>(
        &'a mut self,
        lines: &'a [String],
        c: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<(), Status>> {
        Box::pin(async move {
            check(c)?;
            self.observe(format!("show:{}", lines.join("|")));
            self.shown.extend_from_slice(lines);
            Ok(())
        })
    }
    fn print<'a>(
        &'a mut self,
        lines: &'a [String],
        c: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<(), Status>> {
        Box::pin(async move {
            check(c)?;
            self.observe(format!("print:{}", lines.join("|")));
            self.printed.extend_from_slice(lines);
            Ok(())
        })
    }
    fn show_error<'a>(
        &'a mut self,
        message: &'a str,
        c: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<(), Status>> {
        Box::pin(async move {
            check(c)?;
            self.observe(format!("error:{message}"));
            self.errors.push(message.into());
            Ok(())
        })
    }
    fn load<'a>(
        &'a mut self,
        activity: &'a str,
        work: BoxFuture<'a, Result<(), Status>>,
        c: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<(), Status>> {
        Box::pin(async move {
            check(c)?;
            self.loads.push(activity.into());
            self.observe(format!("loading:{activity}"));
            let result = work.await;
            self.observe(format!("loaded:{activity}"));
            result
        })
    }
}
