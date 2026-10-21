//! Web adapter fixtures only: these do not implement or accept the production CLI registry/startup join.
use futures_util::StreamExt;
use parrot_domain::{
    contracts::async_ops::{BoxFuture, Cancellation},
    protocol::{
        bindings::web::parrot_web_server::ParrotWeb,
        codec::RustProtobufCodec,
        messages::{parrot as wire, web},
    },
    values::ProjectWorkspace,
};
use parrot_foundation::paths::ApplicationRoots;
use parrot_host::{
    grpc::{address::TransportAddress, token::TransportToken, transport::GrpcTransportClient},
    web::{
        dialog::WebSlashDialog,
        router::{WebClient, WebOpenedSession, WebSessionRouter, WebStartup},
        slash::{
            WebSlashContext, WebSlashFactory, WebSlashFailure, WebSlashRegistry, WebSlashService,
        },
    },
};
use parrot_storage::diagnostics::FileDiagnosticLog;
use std::{
    collections::HashMap,
    future::Future,
    pin::Pin,
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
    task::{Context, Poll},
    time::Duration,
};
use tokio::sync::{mpsc, oneshot, watch};
use tonic::{Request, Response, Status};

struct Cancel(watch::Sender<bool>);
impl Cancel {
    fn new() -> Self {
        Self(watch::channel(false).0)
    }
}
impl Cancellation for Cancel {
    fn is_cancelled(&self) -> bool {
        *self.0.borrow()
    }
    fn cancelled(&self) -> BoxFuture<'_, ()> {
        Box::pin(async move {
            let mut rx = self.0.subscribe();
            loop {
                if *rx.borrow_and_update() {
                    return;
                }
                if rx.changed().await.is_err() {
                    return;
                }
            }
        })
    }
}
fn options() -> Vec<web::SlashOption> {
    ["first", "second"]
        .into_iter()
        .map(|id| web::SlashOption {
            id: id.into(),
            label: id.into(),
            description: format!("the {id} option"),
            ..Default::default()
        })
        .collect()
}
fn prompt_id(frame: &web::SlashFrame) -> &str {
    match frame.payload.as_ref().unwrap() {
        web::slash_frame::Payload::Select(prompt) => &prompt.prompt_id,
        web::slash_frame::Payload::ReadText(prompt) => &prompt.prompt_id,
        web::slash_frame::Payload::Confirm(prompt) => &prompt.prompt_id,
        web::slash_frame::Payload::Show(prompt) => &prompt.prompt_id,
        _ => panic!("not a prompt"),
    }
}
async fn prompt_row(
    method: &str,
    answer: web::answer_slash_prompt_request::Answer,
    expected: Option<&str>,
) {
    let (frames, mut receiver) = mpsc::unbounded_channel();
    let dialog = WebSlashDialog::new(frames);
    let cancellation = Cancel::new();
    let options = options();
    let lines = vec!["Sure?".into()];
    let prompt = async {
        match method {
            "select" => dialog
                .select("Pick", &options, &cancellation)
                .await
                .map(|option| option.map(|option| option.id)),
            "read_text" => dialog.read_text("Name", &cancellation).await,
            "read_secret" => dialog.read_secret("Key", &cancellation).await,
            "confirm" => dialog
                .confirm(&lines, &cancellation)
                .await
                .map(|yes| Some(if yes { "True" } else { "False" }.into())),
            "show" => dialog
                .show(&lines, &cancellation)
                .await
                .map(|()| Some("shown".into())),
            _ => unreachable!(),
        }
    };
    tokio::pin!(prompt);
    let frame = tokio::select! {
        frame = receiver.recv() => frame.unwrap(),
        result = &mut prompt => panic!("prompt completed before its answer: {result:?}"),
    };
    match (method, frame.payload.as_ref().unwrap()) {
        ("select", web::slash_frame::Payload::Select(select)) => {
            assert_eq!(select.title, "Pick");
            assert_eq!(select.options, options);
        }
        ("read_text", web::slash_frame::Payload::ReadText(read)) => {
            assert_eq!(read.prompt, "Name");
            assert!(!read.secret);
        }
        ("read_secret", web::slash_frame::Payload::ReadText(read)) => {
            assert_eq!(read.prompt, "Key");
            assert!(read.secret);
        }
        ("confirm", web::slash_frame::Payload::Confirm(confirm)) => {
            assert_eq!(confirm.lines, lines)
        }
        ("show", web::slash_frame::Payload::Show(show)) => assert_eq!(show.lines, lines),
        _ => panic!("wrong prompt frame"),
    }
    let id = prompt_id(&frame);
    assert_eq!(id.len(), 32);
    assert!(
        id.bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
    );
    assert_eq!(&id[12..13], "4");
    let answer = web::AnswerSlashPromptRequest {
        prompt_id: id.into(),
        answer: Some(answer),
        ..Default::default()
    };
    assert!(dialog.answer(answer.clone()));
    assert_eq!(prompt.await.unwrap().as_deref(), expected);
    assert!(!dialog.answer(answer));
}

#[tokio::test]
async fn source_prompt_select_second() {
    prompt_row(
        "select",
        web::answer_slash_prompt_request::Answer::OptionId("second".into()),
        Some("second"),
    )
    .await;
}
#[tokio::test]
async fn source_prompt_select_dismissed() {
    prompt_row(
        "select",
        web::answer_slash_prompt_request::Answer::Dismissed(true),
        None,
    )
    .await;
}
#[tokio::test]
async fn source_prompt_read_text_typed() {
    prompt_row(
        "read_text",
        web::answer_slash_prompt_request::Answer::Text("typed".into()),
        Some("typed"),
    )
    .await;
}
#[tokio::test]
async fn source_prompt_read_secret_dismissed() {
    prompt_row(
        "read_secret",
        web::answer_slash_prompt_request::Answer::Dismissed(true),
        None,
    )
    .await;
}
#[tokio::test]
async fn source_prompt_confirm_confirmed() {
    prompt_row(
        "confirm",
        web::answer_slash_prompt_request::Answer::Confirmed(true),
        Some("True"),
    )
    .await;
}
#[tokio::test]
async fn source_prompt_confirm_dismissed() {
    prompt_row(
        "confirm",
        web::answer_slash_prompt_request::Answer::Dismissed(true),
        Some("False"),
    )
    .await;
}
#[tokio::test]
async fn source_prompt_show_dismissed() {
    prompt_row(
        "show",
        web::answer_slash_prompt_request::Answer::Dismissed(true),
        Some("shown"),
    )
    .await;
}
#[tokio::test]
async fn wrong_answer_kind_consumes_prompt() {
    prompt_row(
        "read_text",
        web::answer_slash_prompt_request::Answer::Confirmed(true),
        None,
    )
    .await;
}

#[tokio::test]
async fn cancelling_or_dropping_prompt_removes_pending_answer() {
    for cancel in [true, false] {
        let (frames, mut receiver) = mpsc::unbounded_channel();
        let dialog = WebSlashDialog::new(frames);
        let cancellation = Cancel::new();
        let mut prompt = Box::pin(dialog.read_text("Name", &cancellation));
        let frame = tokio::select! { frame = receiver.recv() => frame.unwrap(), result = &mut prompt => panic!("unexpected {result:?}") };
        if cancel {
            cancellation.0.send_replace(true);
            assert_eq!(
                prompt.as_mut().await.unwrap_err().code(),
                tonic::Code::Cancelled
            );
        }
        drop(prompt);
        assert!(!dialog.answer(web::AnswerSlashPromptRequest {
            prompt_id: prompt_id(&frame).into(),
            answer: Some(web::answer_slash_prompt_request::Answer::Text(
                "late".into()
            )),
            ..Default::default()
        }));
    }
}

#[tokio::test]
async fn loading_is_finally_balanced_on_failure_and_future_drop() {
    let (frames, mut receiver) = mpsc::unbounded_channel();
    let dialog = WebSlashDialog::new(frames);
    let cancellation = Cancel::new();
    let error = dialog
        .load(
            "Fail",
            async { Err::<(), _>(Status::data_loss("original failure")) },
            &cancellation,
        )
        .await
        .unwrap_err();
    assert_eq!(error.message(), "original failure");
    assert!(matches!(
        receiver.recv().await.unwrap().payload,
        Some(web::slash_frame::Payload::Loading(_))
    ));
    assert!(matches!(
        receiver.recv().await.unwrap().payload,
        Some(web::slash_frame::Payload::Loaded(_))
    ));
    let mut loading = Box::pin(dialog.load(
        "Wait",
        std::future::pending::<Result<(), Status>>(),
        &cancellation,
    ));
    std::future::poll_fn(|cx| {
        assert!(loading.as_mut().poll(cx).is_pending());
        Poll::Ready(())
    })
    .await;
    assert!(matches!(
        receiver.recv().await.unwrap().payload,
        Some(web::slash_frame::Payload::Loading(_))
    ));
    cancellation.0.send_replace(true);
    drop(loading);
    assert!(matches!(
        receiver.recv().await.unwrap().payload,
        Some(web::slash_frame::Payload::Loaded(_))
    ));
}

#[tokio::test]
async fn print_error_and_replacement_emit_without_prompts() {
    let (frames, mut receiver) = mpsc::unbounded_channel();
    let dialog = WebSlashDialog::new(frames);
    let cancellation = Cancel::new();
    dialog
        .print(&["printed".into()], &cancellation)
        .await
        .unwrap();
    dialog.show_error("bad", &cancellation).await.unwrap();
    let session = wire::UserSession {
        id: "replacement".into(),
        ..Default::default()
    };
    dialog
        .session_replaced(session.clone(), &cancellation)
        .await
        .unwrap();
    assert!(
        matches!(receiver.recv().await.unwrap().payload, Some(web::slash_frame::Payload::Print(print)) if print.lines == ["printed"])
    );
    assert!(
        matches!(receiver.recv().await.unwrap().payload, Some(web::slash_frame::Payload::Error(error)) if error.message == "bad")
    );
    assert_eq!(
        receiver.recv().await.unwrap().payload,
        Some(web::slash_frame::Payload::SessionReplaced(session))
    );
}

// A synthetic two-route native endpoint avoids implementing a replacement Parrot service.
#[derive(Clone, Default)]
struct NativeEndpoint {
    attached: Arc<Mutex<Vec<wire::AttachSessionRequest>>>,
    uploads: Arc<Mutex<Vec<wire::AttachmentUploadFrame>>>,
}
struct Attach(NativeEndpoint);
impl tonic::server::UnaryService<wire::AttachSessionRequest> for Attach {
    type Response = wire::UserSession;
    type Future = BoxFuture<'static, Result<Response<Self::Response>, Status>>;
    fn call(&mut self, request: Request<wire::AttachSessionRequest>) -> Self::Future {
        let endpoint = self.0.clone();
        Box::pin(async move {
            let request = request.into_inner();
            endpoint.attached.lock().unwrap().push(request.clone());
            Ok(Response::new(wire::UserSession {
                id: request.user_session_id,
                model: "old-model".into(),
                mode: "old-mode".into(),
                ..Default::default()
            }))
        })
    }
}
struct Upload(NativeEndpoint);
impl tonic::server::ClientStreamingService<wire::AttachmentUploadFrame> for Upload {
    type Response = wire::AttachmentUploadResponse;
    type Future = BoxFuture<'static, Result<Response<Self::Response>, Status>>;
    fn call(
        &mut self,
        request: Request<tonic::Streaming<wire::AttachmentUploadFrame>>,
    ) -> Self::Future {
        let endpoint = self.0.clone();
        Box::pin(async move {
            let mut stream = request.into_inner();
            while let Some(frame) = stream.message().await? {
                endpoint.uploads.lock().unwrap().push(frame);
            }
            Ok(Response::new(wire::AttachmentUploadResponse {
                artifact: protobuf::MessageField::some(wire::ArtifactReference {
                    artifact_id: "fixture-image".into(),
                    ..Default::default()
                }),
                ..Default::default()
            }))
        })
    }
}
impl tonic::server::NamedService for NativeEndpoint {
    const NAME: &'static str = "parrot.v1.Parrot";
}
impl tower::Service<http::Request<tonic::body::BoxBody>> for NativeEndpoint {
    type Response = http::Response<tonic::body::BoxBody>;
    type Error = std::convert::Infallible;
    type Future = BoxFuture<'static, Result<Self::Response, Self::Error>>;
    fn poll_ready(&mut self, _: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }
    fn call(&mut self, request: http::Request<tonic::body::BoxBody>) -> Self::Future {
        let endpoint = self.clone();
        Box::pin(async move {
            let response = match request.uri().path() {
                "/parrot.v1.Parrot/AttachSession" => {
                    tonic::server::Grpc::new(RustProtobufCodec::<
                        wire::UserSession,
                        wire::AttachSessionRequest,
                    >::default())
                    .unary(Attach(endpoint), request)
                    .await
                }
                "/parrot.v1.Parrot/UploadAttachment" => {
                    tonic::server::Grpc::new(RustProtobufCodec::<
                        wire::AttachmentUploadResponse,
                        wire::AttachmentUploadFrame,
                    >::default())
                    .client_streaming(Upload(endpoint), request)
                    .await
                }
                _ => Status::unimplemented("unused synthetic route").into_http(),
            };
            Ok(response)
        })
    }
}

struct Startup;
impl WebStartup for Startup {
    fn open<'a>(
        &'a self,
        _: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<WebOpenedSession, Status>> {
        Box::pin(async {
            Ok(WebOpenedSession {
                session: wire::UserSession {
                    id: "default-fixture".into(),
                    ..Default::default()
                },
                remote: None,
            })
        })
    }
}
#[derive(Default)]
struct Factory {
    dropped: Arc<AtomicUsize>,
    dialog: Arc<Mutex<Option<Arc<WebSlashDialog>>>>,
}
impl WebSlashFactory for Factory {
    fn create(&self, context: WebSlashContext) -> Box<dyn WebSlashRegistry> {
        if context.initial_session.id.is_empty() {
            assert!(!context.activity.is_busy());
        }
        *self.dialog.lock().unwrap() = Some(context.dialog.clone());
        Box::new(Registry {
            session: context.initial_session.clone(),
            context,
            dropped: self.dropped.clone(),
        })
    }
}
struct Registry {
    session: wire::UserSession,
    context: WebSlashContext,
    dropped: Arc<AtomicUsize>,
}
impl Drop for Registry {
    fn drop(&mut self) {
        self.dropped.fetch_add(1, Ordering::SeqCst);
    }
}
impl WebSlashRegistry for Registry {
    fn commands(&self) -> Vec<web::SlashCommand> {
        vec![web::SlashCommand {
            name: "fixture".into(),
            summary: "synthetic test command".into(),
            ..Default::default()
        }]
    }
    fn current_session(&self) -> wire::UserSession {
        self.session.clone()
    }
    fn dispatch<'a>(
        &'a mut self,
        text: &'a str,
        cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<(), WebSlashFailure>> {
        Box::pin(async move {
            match text {
                "ask" => {
                    self.session.model = self
                        .context
                        .dialog
                        .read_text("Model", cancellation)
                        .await?
                        .unwrap_or_default();
                }
                "replace" => {
                    self.session.id = "replacement-fixture".into();
                    self.session.model = "replacement-model".into();
                    self.context
                        .dialog
                        .session_replaced(self.session.clone(), cancellation)
                        .await?;
                }
                "command-error" => {
                    return Err(WebSlashFailure::Command("fixture command failure".into()));
                }
                "rpc-error" => {
                    return Err(WebSlashFailure::Rpc(Status::with_details(
                        tonic::Code::PermissionDenied,
                        "fixture denied",
                        bytes::Bytes::from_static(b"fixture-details"),
                    )));
                }
                "unchanged" => {}
                _ => {
                    return Err(WebSlashFailure::Command(
                        "unknown synthetic fixture command".into(),
                    ));
                }
            }
            Ok(())
        })
    }
}

struct Harness {
    service: WebSlashService,
    endpoint: NativeEndpoint,
    factory: Arc<Factory>,
    owner: GrpcTransportClient,
    stopped: oneshot::Sender<()>,
    task: tokio::task::JoinHandle<()>,
    root: std::path::PathBuf,
}
impl Harness {
    async fn new() -> Self {
        let root = std::env::temp_dir().join(format!(
            "t64-slash-{}",
            FileDiagnosticLog::create_instance_id()
        ));
        std::fs::create_dir_all(&root).unwrap();
        let diagnostics =
            Arc::new(FileDiagnosticLog::open_global(&root, "fixture", |_| {}).unwrap());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address =
            TransportAddress::parse(&format!("http://{}", listener.local_addr().unwrap())).unwrap();
        let endpoint = NativeEndpoint::default();
        let (stopped, receiver) = oneshot::channel();
        let task_endpoint = endpoint.clone();
        let task = tokio::spawn(async move {
            tonic::transport::Server::builder()
                .add_service(task_endpoint)
                .serve_with_incoming_shutdown(
                    tokio_stream::wrappers::TcpListenerStream::new(listener),
                    async {
                        let _ = receiver.await;
                    },
                )
                .await
                .unwrap();
        });
        let token = TransportToken::generate().unwrap();
        let mut owner =
            GrpcTransportClient::connect(&address, Some(&token), None, diagnostics.clone())
                .unwrap();
        let client: WebClient = owner.client().unwrap().clone();
        let roots = ApplicationRoots::resolve(&HashMap::from([
            ("HOME".into(), root.to_string_lossy().into_owned()),
            (
                "XDG_STATE_HOME".into(),
                root.join("state").to_string_lossy().into_owned(),
            ),
        ]));
        let workspace = ProjectWorkspace::from_resolved(root.clone(), root.clone(), None, None);
        let router = Arc::new(WebSessionRouter::new(
            client.clone(),
            roots,
            workspace,
            Arc::new(Startup),
            diagnostics,
        ));
        let factory = Arc::new(Factory::default());
        let service =
            WebSlashService::new(router, client, factory.clone(), "fixture-workspace".into());
        Self {
            service,
            endpoint,
            factory,
            owner,
            stopped,
            task,
            root,
        }
    }
    async fn close(mut self) {
        self.owner.close();
        drop(self.service);
        drop(self.factory);
        let _ = self.stopped.send(());
        tokio::time::timeout(Duration::from_secs(3), self.task)
            .await
            .unwrap()
            .unwrap();
        std::fs::remove_dir_all(self.root).unwrap();
    }
    async fn run(
        &self,
        text: &str,
    ) -> Pin<Box<dyn futures_core::Stream<Item = Result<web::SlashFrame, Status>> + Send>> {
        self.service
            .run_slash_command(Request::new(web::RunSlashCommandRequest {
                user_session_id: "user-session-fixture".into(),
                text: text.into(),
                ..Default::default()
            }))
            .await
            .unwrap()
            .into_inner()
    }
}
fn started(frame: web::SlashFrame) -> String {
    match frame.payload.unwrap() {
        web::slash_frame::Payload::Started(started) => started.run_id,
        _ => panic!("started must be first"),
    }
}

#[tokio::test]
async fn six_method_service_lists_describes_opens_and_streams_prompt_then_update() {
    let harness = Harness::new().await;
    assert_eq!(
        harness
            .service
            .describe_host(Request::new(web::DescribeHostRequest::new()))
            .await
            .unwrap()
            .into_inner()
            .working_directory,
        "fixture-workspace"
    );
    assert_eq!(
        harness
            .service
            .open_default_session(Request::new(web::OpenDefaultSessionRequest::new()))
            .await
            .unwrap()
            .into_inner()
            .id,
        "default-fixture"
    );
    assert_eq!(
        harness
            .service
            .list_slash_commands(Request::new(web::ListSlashCommandsRequest::new()))
            .await
            .unwrap()
            .into_inner()
            .commands[0]
            .name,
        "fixture"
    );
    let mut run = harness.run("ask").await;
    let run_id = started(run.next().await.unwrap().unwrap());
    assert!(
        harness.endpoint.attached.lock().unwrap().is_empty(),
        "Started precedes even native attach"
    );
    let frame = run.next().await.unwrap().unwrap();
    let answer = web::AnswerSlashPromptRequest {
        run_id: run_id.clone(),
        prompt_id: prompt_id(&frame).into(),
        answer: Some(web::answer_slash_prompt_request::Answer::Text(
            "new-model".into(),
        )),
        ..Default::default()
    };
    harness
        .service
        .answer_slash_prompt(Request::new(answer.clone()))
        .await
        .unwrap();
    let updated = run.next().await.unwrap().unwrap();
    assert!(
        matches!(updated.payload, Some(web::slash_frame::Payload::SessionUpdated(session)) if session.id == "user-session-fixture" && session.model == "new-model" && session.mode == "old-mode")
    );
    assert!(run.next().await.is_none());
    let failure = harness
        .service
        .answer_slash_prompt(Request::new(answer))
        .await
        .unwrap_err();
    assert_eq!(failure.code(), tonic::Code::NotFound);
    assert_eq!(
        failure.message(),
        "the slash prompt is not waiting for an answer"
    );
    assert_eq!(
        harness.endpoint.attached.lock().unwrap()[0].working_directory,
        "fixture-workspace"
    );
    drop(run);
    harness.close().await;
}

#[tokio::test]
async fn run_replacement_unchanged_command_error_and_rpc_error_keep_source_completion_semantics() {
    let harness = Harness::new().await;
    for text in ["replace", "unchanged", "command-error", "rpc-error"] {
        let mut run = harness.run(text).await;
        started(run.next().await.unwrap().unwrap());
        match text {
            "replace" => assert!(
                matches!(run.next().await.unwrap().unwrap().payload, Some(web::slash_frame::Payload::SessionReplaced(session)) if session.id == "replacement-fixture")
            ),
            "unchanged" => {}
            "command-error" => assert!(
                matches!(run.next().await.unwrap().unwrap().payload, Some(web::slash_frame::Payload::Error(error)) if error.message == "fixture command failure")
            ),
            "rpc-error" => {
                let error = run.next().await.unwrap().unwrap_err();
                assert_eq!(error.code(), tonic::Code::PermissionDenied);
                assert_eq!(error.message(), "fixture denied");
                assert_eq!(error.details(), b"fixture-details");
            }
            _ => unreachable!(),
        }
        assert!(
            run.next().await.is_none(),
            "no session_updated for replacement, unchanged or failure"
        );
    }
    harness.close().await;
}

#[tokio::test]
async fn dropping_run_drops_dispatch_and_unregisters_prompt_without_detached_work() {
    let harness = Harness::new().await;
    let mut run = harness.run("ask").await;
    let run_id = started(run.next().await.unwrap().unwrap());
    let frame = run.next().await.unwrap().unwrap();
    let before = harness.factory.dropped.load(Ordering::SeqCst);
    drop(run);
    assert_eq!(harness.factory.dropped.load(Ordering::SeqCst), before + 1);
    let answer = web::AnswerSlashPromptRequest {
        run_id,
        prompt_id: prompt_id(&frame).into(),
        answer: Some(web::answer_slash_prompt_request::Answer::Text(
            "late".into(),
        )),
        ..Default::default()
    };
    assert!(
        !harness
            .factory
            .dialog
            .lock()
            .unwrap()
            .as_ref()
            .unwrap()
            .answer(answer.clone())
    );
    assert_eq!(
        harness
            .service
            .answer_slash_prompt(Request::new(answer))
            .await
            .unwrap_err()
            .code(),
        tonic::Code::NotFound
    );
    harness.close().await;
}

#[tokio::test]
async fn web_upload_emits_exact_native_header_mib_chunks_description_and_artifact() {
    let harness = Harness::new().await;
    let content = vec![0x5a; 1024 * 1024 + 13];
    let response = harness
        .service
        .upload_attachment(Request::new(web::UploadAttachmentRequest {
            user_session_id: "user-session-fixture".into(),
            display_name: "image.png".into(),
            media_type: "image/png".into(),
            content: content.clone(),
            ..Default::default()
        }))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(
        response.artifact.as_ref().unwrap().artifact_id,
        "fixture-image"
    );
    {
        let frames = harness.endpoint.uploads.lock().unwrap();
        assert_eq!(frames.len(), 4);
        match frames[0].payload.as_ref().unwrap() {
            wire::attachment_upload_frame::Payload::Header(header) => {
                assert_eq!(header.user_session_id, "user-session-fixture");
                assert_eq!(header.upload_id.len(), 32);
                assert_eq!(&header.upload_id[12..13], "7");
            }
            _ => panic!("header first"),
        }
        assert!(
            matches!(&frames[1].payload, Some(wire::attachment_upload_frame::Payload::Chunk(bytes)) if bytes == &content[..1024 * 1024])
        );
        assert!(
            matches!(&frames[2].payload, Some(wire::attachment_upload_frame::Payload::Chunk(bytes)) if bytes == &content[1024 * 1024..])
        );
        assert!(
            matches!(&frames[3].payload, Some(wire::attachment_upload_frame::Payload::Description(description)) if description.display_name == "image.png" && description.media_type == "image/png" && description.byte_length == content.len() as i64)
        );
    }
    harness.close().await;
}

mod http_fixture {
    use super::*;
    use crate::authoritative::{Fixture, active, bounded, create_request};
    use bytes::{Bytes, BytesMut};
    use http_body_util::{BodyExt, Full};
    use hyper::{body::Incoming, client::conn::http1};
    use hyper_util::rt::TokioIo;
    use parrot_host::{grpc::service::GrpcService, web::server::WebServer};
    use protobuf::Message;
    use std::net::SocketAddr;

    struct HttpCall {
        body: Incoming,
        pending: BytesMut,
        stop: oneshot::Sender<()>,
        task: tokio::task::JoinHandle<()>,
    }
    impl HttpCall {
        async fn new(address: SocketAddr, method: &str, message: &impl Message) -> Self {
            let socket = bounded(tokio::net::TcpStream::connect(address))
                .await
                .unwrap();
            let (mut sender, connection) = bounded(http1::handshake(TokioIo::new(socket)))
                .await
                .unwrap();
            let (stop, stopped) = oneshot::channel();
            let task = tokio::spawn(async move {
                tokio::select! {
                    _ = stopped => {},
                    result = connection => result.unwrap(),
                }
            });
            let message = message.write_to_bytes().unwrap();
            let mut envelope = vec![0];
            envelope.extend_from_slice(&u32::try_from(message.len()).unwrap().to_be_bytes());
            envelope.extend_from_slice(&message);
            let response = bounded(
                sender.send_request(
                    http::Request::builder()
                        .method("POST")
                        .uri(format!("http://{address}/parrot.web.v1.ParrotWeb/{method}"))
                        .header("content-type", "application/grpc-web+proto")
                        .header("x-grpc-web", "1")
                        .body(Full::new(Bytes::from(envelope)))
                        .unwrap(),
                ),
            )
            .await
            .unwrap();
            assert_eq!(response.status(), http::StatusCode::OK);
            assert_eq!(response.version(), http::Version::HTTP_11);
            assert_eq!(
                response.headers()["content-type"],
                "application/grpc-web+proto"
            );
            Self {
                body: response.into_body(),
                pending: BytesMut::new(),
                stop,
                task,
            }
        }
        async fn next(&mut self) -> Option<(u8, Bytes)> {
            loop {
                if self.pending.len() >= 5 {
                    let length =
                        u32::from_be_bytes(self.pending[1..5].try_into().unwrap()) as usize;
                    if self.pending.len() >= 5 + length {
                        let frame = self.pending.split_to(5 + length).freeze();
                        return Some((frame[0], frame.slice(5..)));
                    }
                }
                let Some(frame) = bounded(self.body.frame()).await else {
                    assert!(self.pending.is_empty(), "truncated gRPC-Web frame");
                    return None;
                };
                self.pending
                    .extend_from_slice(&frame.unwrap().into_data().unwrap());
            }
        }
        async fn message<T: Message>(&mut self) -> T {
            let (flag, bytes) = self.next().await.expect("message before trailers");
            assert_eq!(flag, 0);
            T::parse_from_bytes(&bytes).unwrap()
        }
        async fn finish(mut self, status: u32) {
            let (flag, bytes) = self.next().await.expect("gRPC-Web status trailer");
            assert_eq!(flag, 0x80);
            let trailers = std::str::from_utf8(&bytes).unwrap();
            let expected = format!("grpc-status: {status}");
            assert!(
                trailers.lines().any(|line| line == expected),
                "{trailers:?}"
            );
            assert!(
                self.next().await.is_none(),
                "no frames after status trailer"
            );
            self.disconnect().await;
        }
        async fn disconnect(self) {
            let Self {
                body, stop, task, ..
            } = self;
            let _ = stop.send(());
            bounded(task).await.unwrap();
            drop(body);
        }
    }
    async fn unary<T: Message>(address: SocketAddr, method: &str, request: &impl Message) -> T {
        let mut call = HttpCall::new(address, method, request).await;
        let response = call.message().await;
        call.finish(0).await;
        response
    }

    #[tokio::test]
    async fn real_web_slash_service_streams_and_cleans_up_disconnected_http1_prompt() {
        // The real adapter is hosted here; the registry/startup remain synthetic,
        // not the production CLI binding, which is still the mandatory P09 join.
        let harness = Harness::new().await;
        let fixture = Fixture::new().await;
        let created = bounded(fixture.service.create_session(&create_request(), active()))
            .await
            .unwrap();
        assert!(!created.id.is_empty());
        let mut server = bounded(WebServer::start(
            GrpcService::new(fixture.service.clone()),
            harness.service.clone(),
            0,
        ))
        .await
        .unwrap();
        let address = server.address();
        let described: web::DescribeHostResponse =
            unary(address, "DescribeHost", &web::DescribeHostRequest::new()).await;
        assert_eq!(described.working_directory, "fixture-workspace");
        let listed: web::ListSlashCommandsResponse = unary(
            address,
            "ListSlashCommands",
            &web::ListSlashCommandsRequest::new(),
        )
        .await;
        assert_eq!(listed.commands[0].name, "fixture");
        let opened: wire::UserSession = unary(
            address,
            "OpenDefaultSession",
            &web::OpenDefaultSessionRequest::new(),
        )
        .await;
        assert_eq!(opened.id, "default-fixture");
        let request = web::RunSlashCommandRequest {
            user_session_id: "user-session-fixture".into(),
            text: "ask".into(),
            ..Default::default()
        };
        let mut run = HttpCall::new(address, "RunSlashCommand", &request).await;
        let run_id = started(run.message().await);
        assert!(!run_id.is_empty());
        let prompt: web::SlashFrame = run.message().await;
        assert!(matches!(
            &prompt.payload,
            Some(web::slash_frame::Payload::ReadText(read))
                if read.prompt == "Model" && !read.secret
        ));
        let answer = web::AnswerSlashPromptRequest {
            run_id,
            prompt_id: prompt_id(&prompt).into(),
            answer: Some(web::answer_slash_prompt_request::Answer::Text(
                "http-model".into(),
            )),
            ..Default::default()
        };
        let _: web::AnswerSlashPromptResponse = unary(address, "AnswerSlashPrompt", &answer).await;
        let updated: web::SlashFrame = run.message().await;
        assert!(matches!(
            updated.payload,
            Some(web::slash_frame::Payload::SessionUpdated(session))
                if session.id == request.user_session_id
                    && session.model == "http-model" && session.mode == "old-mode"
        ));
        run.finish(0).await;

        let mut pending = HttpCall::new(address, "RunSlashCommand", &request).await;
        let run_id = started(pending.message().await);
        let prompt: web::SlashFrame = pending.message().await;
        assert!(matches!(
            prompt.payload,
            Some(web::slash_frame::Payload::ReadText(_))
        ));
        let before = harness.factory.dropped.load(Ordering::SeqCst);
        let late = web::AnswerSlashPromptRequest {
            run_id,
            prompt_id: prompt_id(&prompt).into(),
            answer: Some(web::answer_slash_prompt_request::Answer::Text(
                "late".into(),
            )),
            ..Default::default()
        };
        pending.disconnect().await;
        bounded(async {
            while harness.factory.dropped.load(Ordering::SeqCst) == before {
                tokio::task::yield_now().await;
            }
        })
        .await;
        assert_eq!(harness.factory.dropped.load(Ordering::SeqCst), before + 1);
        assert!(
            !harness
                .factory
                .dialog
                .lock()
                .unwrap()
                .as_ref()
                .unwrap()
                .answer(late.clone()),
            "disconnect unregisters the pending prompt"
        );
        HttpCall::new(address, "AnswerSlashPrompt", &late)
            .await
            .finish(tonic::Code::NotFound as u32)
            .await;
        assert_eq!(harness.endpoint.attached.lock().unwrap().len(), 2);

        bounded(server.close()).await.unwrap();
        drop(server);
        harness.close().await;
        fixture.close().await;
    }
}
