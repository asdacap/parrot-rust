//! HTTP/1.1 transport against actual composition/storage. The fixture owns the
//! construction executor through transport join and explicit service close.
use crate::authoritative::{Fixture, active, bounded, create_request};
use bytes::Bytes;
use futures_core::Stream;
use http_body_util::{BodyExt, Full};
use hyper::{Request, body::Incoming, client::conn::http1};
use hyper_util::rt::TokioIo;
use parrot_domain::protocol::{
    bindings::web::parrot_web_server::ParrotWeb,
    messages::{parrot as wire, web},
};
use parrot_host::{grpc::service::GrpcService, web::server::WebServer};
use protobuf::Message;
use std::{
    net::SocketAddr,
    pin::Pin,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    task::{Context, Poll},
    time::Duration,
};
use tonic::{Response, Status};

#[derive(Default)]
pub(super) struct NoWeb {
    released: Arc<AtomicBool>,
}

pub(super) struct BrowserFrames {
    released: Arc<AtomicBool>,
    sent: bool,
    failure: Option<String>,
}
impl Stream for BrowserFrames {
    type Item = Result<web::SlashFrame, Status>;
    fn poll_next(mut self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        if !self.sent {
            self.sent = true;
            let mut frame = web::SlashFrame::new();
            frame.set_started(web::SlashStarted {
                run_id: "browser-run".into(),
                ..Default::default()
            });
            return Poll::Ready(Some(Ok(frame)));
        }
        if let Some(message) = self.failure.take() {
            let mut metadata = tonic::metadata::MetadataMap::new();
            metadata.insert("x-web-proof", "trailer".parse().unwrap());
            return Poll::Ready(Some(Err(Status::with_metadata(
                tonic::Code::InvalidArgument,
                message,
                metadata,
            ))));
        }
        Poll::Pending
    }
}
impl Drop for BrowserFrames {
    fn drop(&mut self) {
        self.released.store(true, Ordering::SeqCst);
    }
}
#[tonic::async_trait]
impl ParrotWeb for NoWeb {
    async fn describe_host(
        &self,
        _: tonic::Request<web::DescribeHostRequest>,
    ) -> Result<Response<web::DescribeHostResponse>, Status> {
        Ok(Response::new(web::DescribeHostResponse {
            working_directory: "/synthetic-browser-fixture".into(),
            ..Default::default()
        }))
    }
    async fn open_default_session(
        &self,
        _: tonic::Request<web::OpenDefaultSessionRequest>,
    ) -> Result<Response<wire::UserSession>, Status> {
        Err(Status::unimplemented("web fixture is not used"))
    }
    async fn list_slash_commands(
        &self,
        _: tonic::Request<web::ListSlashCommandsRequest>,
    ) -> Result<Response<web::ListSlashCommandsResponse>, Status> {
        Err(Status::unimplemented("web fixture is not used"))
    }
    type RunSlashCommandStream = BrowserFrames;
    async fn run_slash_command(
        &self,
        request: tonic::Request<web::RunSlashCommandRequest>,
    ) -> Result<Response<Self::RunSlashCommandStream>, Status> {
        self.released.store(false, Ordering::SeqCst);
        Ok(Response::new(BrowserFrames {
            released: self.released.clone(),
            sent: false,
            failure: request
                .get_ref()
                .text
                .strip_prefix("failure:")
                .map(str::to_owned)
                .or_else(|| {
                    (request.get_ref().text == "failure").then(|| "prompt failure % café".into())
                }),
        }))
    }
    async fn answer_slash_prompt(
        &self,
        _: tonic::Request<web::AnswerSlashPromptRequest>,
    ) -> Result<Response<web::AnswerSlashPromptResponse>, Status> {
        Err(Status::unimplemented("web fixture is not used"))
    }
    async fn upload_attachment(
        &self,
        _: tonic::Request<web::UploadAttachmentRequest>,
    ) -> Result<Response<wire::AttachmentUploadResponse>, Status> {
        Err(Status::unimplemented("web fixture is not used"))
    }
}

pub(super) async fn request(
    address: SocketAddr,
    method: &str,
    message: &impl Message,
) -> (
    hyper::Response<Incoming>,
    tokio::task::JoinHandle<Result<(), hyper::Error>>,
) {
    let io = bounded(tokio::net::TcpStream::connect(address))
        .await
        .unwrap();
    let (mut sender, connection) = bounded(http1::handshake(TokioIo::new(io))).await.unwrap();
    let task = tokio::spawn(connection);
    let bytes = message.write_to_bytes().unwrap();
    let mut frame = vec![0];
    frame.extend_from_slice(&(u32::try_from(bytes.len()).unwrap()).to_be_bytes());
    frame.extend_from_slice(&bytes);
    let response = bounded(
        sender.send_request(
            Request::builder()
                .method("POST")
                .uri(format!("http://{address}/parrot.v1.Parrot/{method}"))
                .header("content-type", "application/grpc-web+proto")
                .header("x-grpc-web", "1")
                .body(Full::new(Bytes::from(frame)))
                .unwrap(),
        ),
    )
    .await
    .unwrap();
    (response, task)
}

pub(super) fn message<T: Message>(body: &[u8]) -> T {
    assert_eq!(body[0], 0);
    let length = u32::from_be_bytes(body[1..5].try_into().unwrap()) as usize;
    T::parse_from_bytes(&body[5..5 + length]).unwrap()
}

#[tokio::test]
async fn web_stop_joins_http1_subscription_but_borrows_actual_service() {
    let fixture = Fixture::new().await;
    let mut server = bounded(WebServer::start(
        GrpcService::new(fixture.service.clone()),
        NoWeb::default(),
        0,
    ))
    .await
    .unwrap();
    let address = server.address();
    let (response, connection) = request(address, "CreateSession", &create_request()).await;
    let body = bounded(response.into_body().collect())
        .await
        .unwrap()
        .to_bytes();
    let created: wire::UserSession = message(&body);
    bounded(connection).await.unwrap().unwrap();
    assert!(!created.id.is_empty());
    let (mut response, stream_connection) = request(
        address,
        "Listen",
        &wire::ListenRequest {
            user_session_id: created.id.clone(),
            replay: true,
            ..Default::default()
        },
    )
    .await;
    let first = bounded(response.body_mut().frame()).await.unwrap().unwrap();
    assert!(
        !first.into_data().unwrap().is_empty(),
        "actual replay arrives before stream completion"
    );
    bounded(server.close()).await.unwrap();
    while bounded(response.body_mut().frame())
        .await
        .is_some_and(|result| result.is_ok())
    {}
    drop(response);
    let _ = bounded(stream_connection).await.unwrap();
    bounded(server.close()).await.unwrap();
    assert!(
        bounded(tokio::net::TcpStream::connect(address))
            .await
            .is_err()
    );
    assert_eq!(fixture.host.closed.load(Ordering::SeqCst), 0);
    fixture.assert_database_open();
    fixture.assert_live_activation().await;
    bounded(fixture.service.session_status(
        &wire::SessionStatusRequest {
            user_session_id: created.id,
            ..Default::default()
        },
        active().as_ref(),
    ))
    .await
    .unwrap();
    fixture.close().await;
    fixture.close().await;
    assert_eq!(fixture.host.opened.load(Ordering::SeqCst), 1);
    assert_eq!(fixture.host.closed.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn unchanged_generated_node_connect_client_calls_both_services_streams_errors_and_cancels() {
    let fixture = Fixture::new().await;
    let web = NoWeb::default();
    let released = web.released.clone();
    let mut server = bounded(WebServer::start(
        GrpcService::new(fixture.service.clone()),
        web,
        0,
    ))
    .await
    .unwrap();
    let workspace = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..");
    // Caller supplies an already-installed, approved read-only module tree.
    // Missing inputs fail rather than substituting manual framing or installing.
    let modules = std::path::PathBuf::from(
        std::env::var_os("PARROT_T64_BROWSER_MODULES")
            .expect("PARROT_T64_BROWSER_MODULES must name the approved installed browser modules"),
    );
    let output = workspace.join(".tmp/t64-native").join(format!(
        "node-client-{}",
        parrot_domain::values::identifiers::Identifier::event_id().unwrap()
    ));
    let script = workspace.join("crates/parrot-host/tests/web/browser-client.cjs");
    let address = format!("http://{}", server.address());
    let result = tokio::time::timeout(
        Duration::from_secs(30),
        tokio::task::spawn_blocking(move || {
            std::process::Command::new("node")
                .arg(script)
                .arg(address)
                .arg(workspace)
                .arg(modules)
                .arg(output)
                .output()
        }),
    )
    .await
    .expect("bounded Node Connect observation")
    .unwrap()
    .unwrap();
    print!("{}", String::from_utf8_lossy(&result.stdout));
    eprint!("{}", String::from_utf8_lossy(&result.stderr));
    // Always join transport and actual composition before asserting client result.
    let cancellation_released = tokio::time::timeout(Duration::from_secs(2), async {
        while !released.load(Ordering::SeqCst) {
            tokio::task::yield_now().await;
        }
    })
    .await
    .is_ok();
    bounded(server.close()).await.unwrap();
    fixture.close().await;
    assert!(
        result.status.success(),
        "actual Node Connect client failed: {}",
        result.status
    );
    assert!(
        cancellation_released,
        "HTTP1 disconnect must release the web upstream subscription before host stop"
    );
}
