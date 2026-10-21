//! Owned executable-consumer tests. No OS signal registration/delivery.

use super::*;
use futures_util::Stream;
use parrot_domain::protocol::bindings::parrot::parrot_server::Parrot;
use parrot_host::grpc::transport::GrpcServer;
use std::{
    path::PathBuf,
    pin::Pin,
    sync::atomic::{AtomicU64, Ordering},
    time::Duration,
};
use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;
use tonic::{Request, Response, Status};

static NEXT: AtomicU64 = AtomicU64::new(0);

fn application() -> (Application, PathBuf) {
    let base = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join(".tmp/t65-process")
        .join(format!(
            "{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
    std::fs::create_dir_all(&base).unwrap();
    let paths = StatePaths::new(base.join("state"), base.join("config"), base.join("data"));
    let diagnostics = Arc::new(
        FileDiagnosticLog::open_global(base.join("logs"), "process-test", |_| {}).unwrap(),
    );
    let stop = Arc::new(Signal::new());
    let interrupts = Arc::new(Interrupts::new(Arc::new(move || stop.cancel())));
    (
        Application {
            paths,
            diagnostics,
            interrupts,
        },
        base,
    )
}

#[tokio::test]
async fn serve_rejects_unsafe_or_unauthenticated_tcp_before_local_initialization() {
    for (listen, expected) in [
        (
            "http://0.0.0.0:0",
            "external plaintext HTTP requires --unsafe-allow-external",
        ),
        (
            "http://127.0.0.1:0",
            "TCP transport requires --token-file <owner-only-file>",
        ),
    ] {
        let (mut app, base) = application();
        let result = app
            .execute(
                RuntimeCommand::Serve(ServeOptions {
                    listen: Some(listen.into()),
                    ..Default::default()
                }),
                &Signal::new(),
            )
            .await;
        assert!(matches!(result, Err(DispatchError::Command(ref message)) if message == expected));
        for directory in ["state", "config", "data"] {
            assert!(!base.join(directory).exists());
        }
    }
}

#[tokio::test]
async fn prior_cancellation_precedes_every_runtime_backend() {
    let (mut app, base) = application();
    let cancellation = Signal::new();
    cancellation.cancel();
    assert!(matches!(
        app.execute(RuntimeCommand::Models, &cancellation).await,
        Err(DispatchError::Cancelled)
    ));
    assert!(!base.join("config").exists());
    assert!(!base.join("state").exists());
}

#[test]
fn help_runs_on_retained_actual_runtime_without_runtime_command_or_signal_registration() {
    let (app, base) = application();
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    assert_eq!(
        run_process(&runtime, vec!["--help".into()], app.paths, app.diagnostics).unwrap(),
        dispatch::EXIT_SUCCESS
    );
    assert!(!base.join("config").exists());
    assert!(!base.join("state").exists());
    runtime.block_on(async { tokio::task::yield_now().await });
    drop(runtime);
}

#[test]
fn enhanced_terminal_color_honors_no_color_existence() {
    use parrot_cli::terminal_layout::MarkdownRenderer;
    const EXPECTED: &str = "PARROT_TEST_ENHANCED_COLOR";
    if let Ok(expected) = std::env::var(EXPECTED) {
        let color = enhanced_terminal_color_enabled();
        assert_eq!(color, expected == "enabled");
        assert_eq!(
            MarkdownRenderer::render("", "**answer**", 80, color),
            [if color {
                "\x1b[1;38;5;195manswer\x1b[0m"
            } else {
                "answer"
            }]
        );
        for (color, expected) in [(true, "\x1b[1;38;5;195manswer\x1b[0m"), (false, "answer")] {
            assert_eq!(
                MarkdownRenderer::render("", "**answer**", 80, color),
                [expected]
            );
        }
        return;
    }
    for value in [None, Some(""), Some("1"), Some("0"), Some("false")] {
        let mut command = std::process::Command::new(std::env::current_exe().unwrap());
        command
            .args([
                "--exact",
                "tests::enhanced_terminal_color_honors_no_color_existence",
                "--nocapture",
            ])
            .env_clear()
            .env(
                EXPECTED,
                if value.is_none() {
                    "enabled"
                } else {
                    "disabled"
                },
            );
        if let Some(value) = value {
            command.env("NO_COLOR", value);
        }
        let output = command.output().unwrap();
        assert!(
            output.status.success(),
            "NO_COLOR={value:?}: {}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }
}

#[test]
fn root_preserves_explicit_rpc_io_and_cancellation_failure_categories() {
    let status = Status::permission_denied("native detail");
    let failure = enhanced_failure(parrot_cli::terminal_dialogs::EnhancedFailure::Rpc(status));
    assert!(
        matches!(failure, DispatchError::Rpc(ref status) if status.code() == tonic::Code::PermissionDenied && status.message() == "native detail")
    );
    let failure = basic_failure(dispatch::basic_loop::BasicFailure::Slash(
        parrot_cli::slash_sessions::SlashFailure::Rpc(Status::cancelled("remote cancelled")),
    ));
    assert!(
        matches!(failure, DispatchError::Rpc(ref status) if status.code() == tonic::Code::Cancelled)
    );
    assert!(matches!(
        attachment_failure(AttachmentError::Cancelled),
        DispatchError::Cancelled
    ));
    assert!(
        matches!(navigation_failure(parrot_cli::slash_sessions::NavigationFailure::Recoverable("recoverable detail".into())), DispatchError::Command(ref message) if message == "recoverable detail")
    );
}

#[derive(Default)]
struct Calls {
    created: Vec<wire::CreateSessionRequest>,
    listened: Vec<wire::ListenRequest>,
    messages: Vec<wire::SendMessageRequest>,
    sender: Option<mpsc::Sender<Result<wire::Event, Status>>>,
}
#[derive(Clone)]
struct Endpoint(Arc<Mutex<Calls>>);
type Events = Pin<Box<dyn Stream<Item = Result<wire::Event, Status>> + Send>>;

macro_rules! endpoint {
    ($($method:ident($request:ty) -> $response:ty),* $(,)?) => {
        #[tonic::async_trait]
        impl Parrot for Endpoint {
            $(async fn $method(&self, _: Request<$request>) -> Result<Response<$response>, Status> {
                Err(Status::unimplemented("unused executable-consumer fixture operation"))
            })*
            async fn create_session(&self, request: Request<wire::CreateSessionRequest>) -> Result<Response<wire::UserSession>, Status> {
                let request = request.into_inner();
                self.0.lock().unwrap().created.push(request.clone());
                Ok(Response::new(wire::UserSession { id: "native-session".into(), model: request.model, mode: request.mode, ..Default::default() }))
            }
            async fn list_model_aliases(&self, _: Request<wire::ListModelAliasesRequest>) -> Result<Response<wire::ListModelAliasesResponse>, Status> {
                Ok(Response::new(wire::ListModelAliasesResponse::new()))
            }
            async fn list_models(&self, _: Request<wire::ListModelsRequest>) -> Result<Response<wire::ListModelsResponse>, Status> {
                Ok(Response::new(wire::ListModelsResponse {
                    models: vec![wire::Model { provider_id: "provider".into(), id: "model".into(), ..Default::default() }],
                    ..Default::default()
                }))
            }
            type ListenStream = Events;
            async fn listen(&self, request: Request<wire::ListenRequest>) -> Result<Response<Self::ListenStream>, Status> {
                let (sender, receiver) = mpsc::channel(8);
                let mut calls = self.0.lock().unwrap();
                calls.listened.push(request.into_inner());
                calls.sender = Some(sender);
                Ok(Response::new(Box::pin(ReceiverStream::new(receiver))))
            }
            async fn send_message(&self, request: Request<wire::SendMessageRequest>) -> Result<Response<wire::SendMessageResponse>, Status> {
                let sender = {
                    let mut calls = self.0.lock().unwrap();
                    calls.messages.push(request.into_inner());
                    calls.sender.clone().ok_or_else(|| Status::failed_precondition("message admitted before Listen"))?
                };
                for payload in [
                    wire::event::Payload::TurnStarted(Default::default()),
                    wire::event::Payload::TextChunk(wire::TextChunk { fragment: "native answer".into(), ..Default::default() }),
                    wire::event::Payload::ModeTurnCompleted(Default::default()),
                ] {
                    sender.send(Ok(wire::Event { agent_session_id: "root".into(), payload: Some(payload), ..Default::default() })).await.map_err(|_| Status::cancelled("test listener closed"))?;
                }
                Ok(Response::new(wire::SendMessageResponse::new()))
            }
        }
    };
}
endpoint! {
    resume_session(wire::ResumeSessionRequest) -> wire::UserSession,
    attach_session(wire::AttachSessionRequest) -> wire::UserSession,
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
    update_session(wire::UpdateSessionRequest) -> wire::UserSession,
    set_goal(wire::SetGoalRequest) -> wire::SetGoalResponse,
    set_standing_instruction(wire::SetStandingInstructionRequest) -> wire::SetStandingInstructionResponse,
    upload_attachment(tonic::Streaming<wire::AttachmentUploadFrame>) -> wire::AttachmentUploadResponse,
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

#[tokio::test]
async fn actual_remote_invalid_variant_remains_usage_before_session_creation() {
    let (app, base) = application();
    let calls = Arc::new(Mutex::new(Calls::default()));
    let socket_root = PathBuf::from("/tmp").join(format!(
        "t17-{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ));
    let mut server = GrpcServer::start(
        Endpoint(calls.clone()),
        TransportAddress::Unix(socket_root.join("rpc.sock")),
        None,
        None,
        app.diagnostics.clone(),
    )
    .await
    .unwrap();
    let options = ChatOptions {
        model: "provider/model".into(),
        model_overridden: true,
        variant: Some("impossible".into()),
        connect: server.address().to_string(),
        basic: true,
        prompt: "synthetic prompt".into(),
        ..Default::default()
    };
    let actual = tokio::time::timeout(Duration::from_secs(5), app.chat(options, &Signal::new()))
        .await
        .unwrap();
    server.close().await.unwrap();
    assert!(matches!(actual, Ok(dispatch::EXIT_USAGE)), "{actual:?}");
    assert!(calls.lock().unwrap().created.is_empty());
    assert!(calls.lock().unwrap().messages.is_empty());
    std::fs::remove_dir_all(socket_root).unwrap();
    std::fs::remove_dir_all(base).unwrap();
}

async fn native_once(prompt: &str) -> (Result<i32, DispatchError>, Arc<Mutex<Calls>>) {
    let (app, _) = application();
    let calls = Arc::new(Mutex::new(Calls::default()));
    let socket_root = PathBuf::from("/tmp").join(format!(
        "t65-process-{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ));
    std::fs::create_dir(&socket_root).unwrap();
    let mut server = GrpcServer::start(
        Endpoint(calls.clone()),
        TransportAddress::Unix(socket_root.join("rpc.sock")),
        None,
        None,
        app.diagnostics.clone(),
    )
    .await
    .unwrap();
    let options = ChatOptions {
        model: "provider/model".into(),
        model_overridden: true,
        mode: "build".into(),
        connect: server.address().to_string(),
        basic: true,
        prompt: prompt.into(),
        ..Default::default()
    };
    let cancellation = Signal::new();
    let result = {
        let work = app.chat(options, &cancellation);
        tokio::pin!(work);
        tokio::select! {
            result = &mut work => result,
            _ = tokio::time::sleep(Duration::from_secs(5)) => {
                cancellation.cancel();
                work.await
            }
        }
    };
    // Join the actual native listener and connections while this SAME Runtime lives.
    server.close().await.unwrap();
    std::fs::remove_dir(socket_root).unwrap();
    (result, calls)
}

#[tokio::test]
async fn actual_remote_basic_once_listens_then_sends_structured_text_and_finishes() {
    let (result, calls) = native_once("hello native").await;
    assert_eq!(result.unwrap(), dispatch::EXIT_SUCCESS);
    let calls = calls.lock().unwrap();
    assert_eq!(calls.created.len(), 1);
    assert_eq!(calls.created[0].model, "provider/model");
    assert_eq!(calls.created[0].mode, "build");
    assert!(!calls.created[0].interactive_permissions);
    assert_eq!(calls.listened.len(), 1);
    assert!(!calls.listened[0].replay);
    assert_eq!(calls.messages.len(), 1);
    assert_eq!(calls.messages[0].user_session_id, "native-session");
    assert!(calls.messages[0].text.is_empty());
    assert_eq!(calls.messages[0].parts.len(), 1);
    assert!(
        matches!(&calls.messages[0].parts[0].content, Some(wire::message_content_part::Content::Text(text)) if text == "hello native")
    );
}

#[tokio::test]
async fn actual_remote_attachment_boundary_never_admits_a_partial_prompt() {
    let (result, calls) = native_once("before @missing.png after").await;
    assert_eq!(result.unwrap(), dispatch::EXIT_FAILURE);
    let calls = calls.lock().unwrap();
    assert_eq!(calls.created.len(), 1);
    assert_eq!(calls.listened.len(), 1);
    assert!(calls.messages.is_empty());
}
