//! Synthetic HTTP/1 protocol fixtures; production service ownership is separate.
use base64::{Engine, engine::general_purpose::STANDARD};
use bytes::Bytes;
use futures_core::Stream;
use http::{HeaderMap, Request, Response, StatusCode};
use http_body_util::{BodyExt, Full};
use hyper::body::Incoming;
use hyper_util::rt::TokioIo;
use parrot_domain::protocol::{
    bindings::{parrot::parrot_server::Parrot, web::parrot_web_server::ParrotWeb},
    messages::{parrot as wire, web},
};
use parrot_host::{grpc::MESSAGE_BYTES, web::server::WebServer, web_assets::EMBEDDED_WEB_ASSETS};
use protobuf::Message;
use std::{
    net::SocketAddr,
    pin::Pin,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    task::{Context, Poll},
    time::Duration,
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpStream,
    sync::oneshot,
    task::JoinHandle,
};
use tonic::{Request as RpcRequest, Response as RpcResponse, Status};

const MODES: &str = "/parrot.v1.Parrot/ListModes";
const LISTEN: &str = "/parrot.v1.Parrot/Listen";
const DESCRIBE: &str = "/parrot.web.v1.ParrotWeb/DescribeHost";
const RUN: &str = "/parrot.web.v1.ParrotWeb/RunSlashCommand";

#[derive(Clone, Default)]
struct Endpoint {
    calls: Arc<AtomicUsize>,
    dropped: Arc<AtomicUsize>,
    emitted: Arc<AtomicUsize>,
    pending_handler: bool,
    endless_bytes: usize,
    describe_bytes: usize,
    modes_bytes: usize,
    terminal_stream: bool,
}

struct DropCount(Arc<AtomicUsize>);
impl Drop for DropCount {
    fn drop(&mut self) {
        self.0.fetch_add(1, Ordering::SeqCst);
    }
}

macro_rules! implement_parrot {
    ($($method:ident: $input:ident => $output:ident),* $(,)?) => {
#[tonic::async_trait]
impl Parrot for Endpoint {
    type ListenStream = Pin<Box<dyn Stream<Item = Result<wire::Event, Status>> + Send>>;
    $(async fn $method(&self, _: RpcRequest<wire::$input>) -> Result<RpcResponse<wire::$output>, Status> {
        Err(Status::unimplemented("unused HTTP fixture method"))
    })*

    async fn list_modes(&self, _: RpcRequest<wire::ListModesRequest>) -> Result<RpcResponse<wire::ListModesResponse>, Status> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        if self.pending_handler {
            let _drop = DropCount(self.dropped.clone());
            std::future::pending::<()>().await;
        }
        let mut response = wire::ListModesResponse::new();
        if self.modes_bytes != 0 {
            response.modes.push(wire::Mode { id: "m".repeat(self.modes_bytes), ..Default::default() });
        }
        Ok(RpcResponse::new(response))
    }
    async fn list_sessions(&self, request: RpcRequest<wire::ListSessionsRequest>) -> Result<RpcResponse<wire::ListSessionsResponse>, Status> {
        let request = request.into_inner();
        let message = if request.working_directory.is_empty() { "synthetic failure: π\n" } else { &request.working_directory };
        let mut metadata = tonic::metadata::MetadataMap::new();
        metadata.insert("fixture-error", "retained".parse().unwrap());
        Err(Status::with_details_and_metadata(tonic::Code::Internal, message, Bytes::from_static(b"details"), metadata))
    }
    async fn resume_session(&self, _: RpcRequest<wire::ResumeSessionRequest>) -> Result<RpcResponse<wire::UserSession>, Status> {
        Err(Status::already_exists("open elsewhere"))
    }
    async fn attach_session(&self, request: RpcRequest<wire::AttachSessionRequest>) -> Result<RpcResponse<wire::UserSession>, Status> {
        let request = request.into_inner();
        Ok(RpcResponse::new(wire::UserSession { id: request.user_session_id, special_fields: request.special_fields, ..Default::default() }))
    }
    async fn upload_attachment(&self, _: RpcRequest<tonic::Streaming<wire::AttachmentUploadFrame>>) -> Result<RpcResponse<wire::AttachmentUploadResponse>, Status> {
        Err(Status::unimplemented("unused HTTP fixture method"))
    }
    async fn listen(&self, _: RpcRequest<wire::ListenRequest>) -> Result<RpcResponse<Self::ListenStream>, Status> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        let source = EventSource { endpoint: self.clone(), index: 0, _drop: DropCount(self.dropped.clone()) };
        Ok(RpcResponse::new(Box::pin(source)))
    }
}
    };
}
implement_parrot! {
    list_models: ListModelsRequest => ListModelsResponse,
    list_model_aliases: ListModelAliasesRequest => ListModelAliasesResponse,
    configure_model_alias: ConfigureModelAliasRequest => ConfigureModelAliasResponse,
    list_provider_model_alias_defaults: ListProviderModelAliasDefaultsRequest => ListProviderModelAliasDefaultsResponse,
    apply_provider_model_alias_defaults: ApplyProviderModelAliasDefaultsRequest => ApplyProviderModelAliasDefaultsResponse,
    set_model_preset: SetModelPresetRequest => SetModelPresetResponse,
    select_model_preset: SelectModelPresetRequest => SelectModelPresetResponse,
    list_model_presets: ListModelPresetsRequest => ListModelPresetsResponse,
    list_skills: ListSkillsRequest => ListSkillsResponse,
    configure_skill: ConfigureSkillRequest => ConfigureSkillResponse,
    create_session: CreateSessionRequest => UserSession,
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

struct EventSource {
    endpoint: Endpoint,
    index: usize,
    _drop: DropCount,
}
impl Stream for EventSource {
    type Item = Result<wire::Event, Status>;
    fn poll_next(mut self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        if self.endpoint.terminal_stream && self.index == 1 {
            self.index += 1;
            let mut metadata = tonic::metadata::MetadataMap::new();
            metadata.insert("fixture-trailer", "retained".parse().unwrap());
            return Poll::Ready(Some(Err(Status::with_metadata(
                tonic::Code::Unavailable,
                "stream ended",
                metadata,
            ))));
        }
        if self.endpoint.terminal_stream && self.index > 1 {
            return Poll::Ready(None);
        }
        if self.endpoint.endless_bytes == 0 && self.index != 0 {
            return Poll::Pending;
        }
        self.index += 1;
        self.endpoint.emitted.fetch_add(1, Ordering::SeqCst);
        Poll::Ready(Some(Ok(wire::Event {
            id: "e".repeat(self.endpoint.endless_bytes.max(1)),
            ..Default::default()
        })))
    }
}

#[tonic::async_trait]
impl ParrotWeb for Endpoint {
    type RunSlashCommandStream =
        Pin<Box<dyn Stream<Item = Result<web::SlashFrame, Status>> + Send>>;
    async fn describe_host(
        &self,
        _: RpcRequest<web::DescribeHostRequest>,
    ) -> Result<RpcResponse<web::DescribeHostResponse>, Status> {
        Ok(RpcResponse::new(web::DescribeHostResponse {
            working_directory: if self.describe_bytes == 0 {
                "/synthetic/work".into()
            } else {
                "w".repeat(self.describe_bytes)
            },
            ..Default::default()
        }))
    }
    async fn open_default_session(
        &self,
        _: RpcRequest<web::OpenDefaultSessionRequest>,
    ) -> Result<RpcResponse<wire::UserSession>, Status> {
        Err(Status::unimplemented("unused HTTP fixture method"))
    }
    async fn list_slash_commands(
        &self,
        _: RpcRequest<web::ListSlashCommandsRequest>,
    ) -> Result<RpcResponse<web::ListSlashCommandsResponse>, Status> {
        Err(Status::unimplemented("unused HTTP fixture method"))
    }
    async fn answer_slash_prompt(
        &self,
        _: RpcRequest<web::AnswerSlashPromptRequest>,
    ) -> Result<RpcResponse<web::AnswerSlashPromptResponse>, Status> {
        Err(Status::unimplemented("unused HTTP fixture method"))
    }
    async fn upload_attachment(
        &self,
        _: RpcRequest<web::UploadAttachmentRequest>,
    ) -> Result<RpcResponse<wire::AttachmentUploadResponse>, Status> {
        Err(Status::unimplemented("unused HTTP fixture method"))
    }
    async fn run_slash_command(
        &self,
        _: RpcRequest<web::RunSlashCommandRequest>,
    ) -> Result<RpcResponse<Self::RunSlashCommandStream>, Status> {
        let (sender, receiver) = tokio::sync::mpsc::channel(1);
        sender.send(Ok(web::SlashFrame::new())).await.unwrap();
        let source = futures_util::stream::unfold(
            (receiver, sender, DropCount(self.dropped.clone())),
            |(mut receiver, sender, dropped)| async move {
                receiver
                    .recv()
                    .await
                    .map(|item| (item, (receiver, sender, dropped)))
            },
        );
        Ok(RpcResponse::new(Box::pin(source)))
    }
}

type Client = hyper::client::conn::http1::SendRequest<Full<Bytes>>;
async fn connect(address: SocketAddr) -> (Client, oneshot::Sender<()>, JoinHandle<()>) {
    let stream = TcpStream::connect(address).await.unwrap();
    let (client, connection) = hyper::client::conn::http1::handshake(TokioIo::new(stream))
        .await
        .unwrap();
    let (stop, receiver) = oneshot::channel();
    let task = tokio::spawn(async move {
        tokio::select! { _ = receiver => {}, _ = connection => {} }
    });
    (client, stop, task)
}
fn request(method: &str, path: &str, content_type: &str, body: Bytes) -> Request<Full<Bytes>> {
    Request::builder()
        .method(method)
        .uri(path)
        .header("host", "localhost")
        .header("content-type", content_type)
        .body(Full::new(body))
        .unwrap()
}
fn envelope(message: &[u8]) -> Bytes {
    let mut data = vec![0];
    data.extend_from_slice(&u32::try_from(message.len()).unwrap().to_be_bytes());
    data.extend_from_slice(message);
    Bytes::from(data)
}
fn assert_native_message(trailers: &str, message: &str) {
    let native = Status::unknown(message).into_http();
    let expected = native.headers()["grpc-message"].as_bytes();
    let actual = trailers
        .split("\r\n")
        .find_map(|line| line.strip_prefix("grpc-message: "))
        .expect("message trailer");
    assert_eq!(actual.as_bytes(), expected, "raw trailers: {trailers:?}");
}

fn frames(mut body: &[u8]) -> Vec<(u8, Vec<u8>)> {
    let mut result = Vec::new();
    while !body.is_empty() {
        assert!(body.len() >= 5);
        let size = u32::from_be_bytes(body[1..5].try_into().unwrap()) as usize;
        assert!(body.len() >= 5 + size);
        result.push((body[0], body[5..5 + size].to_vec()));
        body = &body[5 + size..];
    }
    result
}
async fn call(
    address: SocketAddr,
    request: Request<Full<Bytes>>,
) -> (StatusCode, HeaderMap, Bytes) {
    let (mut client, stop, task) = connect(address).await;
    let response = client.send_request(request).await.unwrap();
    let status = response.status();
    let headers = response.headers().clone();
    let body = response.into_body().collect().await.unwrap().to_bytes();
    let _ = stop.send(());
    task.await.unwrap();
    (status, headers, body)
}
async fn wait_count(counter: &AtomicUsize, minimum: usize) {
    tokio::time::timeout(Duration::from_secs(3), async {
        while counter.load(Ordering::SeqCst) < minimum {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
}
async fn first_data(body: &mut Incoming) -> Bytes {
    tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            if let Some(frame) = body.frame().await {
                if let Ok(data) = frame.unwrap().into_data() {
                    return data;
                }
            } else {
                panic!("stream ended before a message");
            }
        }
    })
    .await
    .unwrap()
}

#[tokio::test]
async fn serves_real_assets_spa_and_both_generated_services_on_one_http1_origin() {
    let endpoint = Endpoint::default();
    let mut server = WebServer::start(endpoint.clone(), endpoint, 0)
        .await
        .unwrap();
    assert!(server.address().ip().is_loopback());
    let (status, headers, index) =
        call(server.address(), request("GET", "/", "", Bytes::new())).await;
    assert_eq!(status, StatusCode::OK);
    assert!(
        headers["content-type"]
            .to_str()
            .unwrap()
            .starts_with("text/html")
    );
    let (_, _, route) = call(
        server.address(),
        request("GET", "/s/synthetic-session", "", Bytes::new()),
    )
    .await;
    assert_eq!(index, route);
    let asset = EMBEDDED_WEB_ASSETS
        .iter()
        .find(|asset| asset.path.ends_with(".js"))
        .unwrap();
    let (_, headers, script) = call(
        server.address(),
        request("GET", &format!("/{}", asset.path), "", Bytes::new()),
    )
    .await;
    assert!(
        headers["content-type"]
            .to_str()
            .unwrap()
            .starts_with("text/javascript")
    );
    assert_eq!(script.as_ref(), asset.bytes);
    for path in [MODES, DESCRIBE] {
        let (status, headers, body) = call(
            server.address(),
            request("POST", path, "application/grpc-web+proto", envelope(&[])),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(headers["content-type"], "application/grpc-web+proto");
        let frames = frames(&body);
        assert_eq!(frames[0].0, 0);
        assert_eq!(frames[1].0, 0x80);
        assert_eq!(frames[1].1, b"grpc-status: 0\r\n");
        if path == DESCRIBE {
            assert_eq!(
                web::DescribeHostResponse::parse_from_bytes(&frames[0].1)
                    .unwrap()
                    .working_directory,
                "/synthetic/work"
            );
        }
    }
    server.close().await.unwrap();
}

#[tokio::test]
async fn errors_keep_exact_status_encoded_message_details_and_metadata() {
    let endpoint = Endpoint::default();
    let mut server = WebServer::start(endpoint.clone(), endpoint, 0)
        .await
        .unwrap();
    let (_, headers, body) = call(
        server.address(),
        request(
            "POST",
            "/parrot.v1.Parrot/ListSessions",
            "application/grpc-web",
            envelope(&[]),
        ),
    )
    .await;
    let frames = frames(&body);
    assert_eq!(frames.len(), 1);
    assert_eq!(frames[0].0, 0x80);
    let trailers = String::from_utf8(frames[0].1.clone()).unwrap();
    assert!(trailers.contains("grpc-status: 13\r\n"));
    assert_native_message(&trailers, "synthetic failure: π\n");
    assert!(trailers.contains("grpc-status-details-bin: ZGV0YWlscw\r\n"));
    assert_eq!(headers["fixture-error"], "retained");
    server.close().await.unwrap();
}

#[tokio::test]
async fn lossless_unknown_occurrences_cross_the_binary_transport() {
    let endpoint = Endpoint::default();
    let mut server = WebServer::start(endpoint.clone(), endpoint, 0)
        .await
        .unwrap();
    let mut bytes = wire::AttachSessionRequest {
        user_session_id: "synthetic".into(),
        ..Default::default()
    }
    .write_to_bytes()
    .unwrap();
    bytes.extend_from_slice(&[
        0xa0, 0x06, 1, 0xa0, 0x06, 2, 0xaa, 0x06, 3, b'x', b'y', b'z',
    ]);
    let (_, _, body) = call(
        server.address(),
        request(
            "POST",
            "/parrot.v1.Parrot/AttachSession",
            "application/grpc-web+proto",
            envelope(&bytes),
        ),
    )
    .await;
    let frames = frames(&body);
    assert_eq!(frames[0].1, bytes);
    server.close().await.unwrap();
}

#[tokio::test]
async fn both_server_streams_publish_before_completion_and_peer_disconnect_drops_source() {
    let endpoint = Endpoint::default();
    let dropped = endpoint.dropped.clone();
    let mut server = WebServer::start(endpoint.clone(), endpoint, 0)
        .await
        .unwrap();
    for path in [LISTEN, RUN] {
        let (mut client, stop, task) = connect(server.address()).await;
        let response: Response<Incoming> = client
            .send_request(request(
                "POST",
                path,
                "application/grpc-web+proto",
                envelope(&[]),
            ))
            .await
            .unwrap();
        let mut body = response.into_body();
        let data = first_data(&mut body).await;
        assert_eq!(frames(&data)[0].0, 0);
        let _ = stop.send(());
        drop(body);
        task.await.unwrap();
    }
    wait_count(&dropped, 2).await;
    server.close().await.unwrap();
}

#[tokio::test]
async fn a_stream_error_remains_a_final_trailer_frame_with_metadata() {
    let endpoint = Endpoint {
        terminal_stream: true,
        ..Default::default()
    };
    let mut server = WebServer::start(endpoint.clone(), endpoint, 0)
        .await
        .unwrap();
    let (_, _, body) = call(
        server.address(),
        request("POST", LISTEN, "application/grpc-web+proto", envelope(&[])),
    )
    .await;
    let frames = frames(&body);
    assert_eq!(frames.len(), 2);
    assert_eq!(frames[0].0, 0);
    assert_eq!(frames[1].0, 0x80);
    let trailers = String::from_utf8_lossy(&frames[1].1);
    assert!(trailers.contains("grpc-status: 14\r\n"));
    assert_native_message(&trailers, "stream ended");
    assert!(trailers.contains("fixture-trailer: retained\r\n"));
    server.close().await.unwrap();
}

#[tokio::test]
async fn disconnect_cancels_an_inflight_handler_not_only_response_streams() {
    let endpoint = Endpoint {
        pending_handler: true,
        ..Default::default()
    };
    let calls = endpoint.calls.clone();
    let dropped = endpoint.dropped.clone();
    let mut server = WebServer::start(endpoint.clone(), endpoint, 0)
        .await
        .unwrap();
    let (mut client, stop, task) = connect(server.address()).await;
    let pending = tokio::spawn(async move {
        client
            .send_request(request(
                "POST",
                MODES,
                "application/grpc-web+proto",
                envelope(&[]),
            ))
            .await
    });
    wait_count(&calls, 1).await;
    let _ = stop.send(());
    task.await.unwrap();
    assert!(pending.await.unwrap().is_err());
    wait_count(&dropped, 1).await;
    server.close().await.unwrap();
}

#[tokio::test]
async fn close_joins_idle_partial_and_backpressured_http1_connections() {
    let endpoint = Endpoint {
        endless_bytes: 512 * 1024,
        ..Default::default()
    };
    let dropped = endpoint.dropped.clone();
    let emitted = endpoint.emitted.clone();
    let mut server = WebServer::start(endpoint.clone(), endpoint, 0)
        .await
        .unwrap();
    let mut idle = TcpStream::connect(server.address()).await.unwrap();
    let mut partial = TcpStream::connect(server.address()).await.unwrap();
    partial
        .write_all(b"POST /parrot.v1.Parrot/Listen HTTP/1.1\r\nHost:")
        .await
        .unwrap();
    let mut unread = TcpStream::connect(server.address()).await.unwrap();
    unread.write_all(b"POST /parrot.v1.Parrot/Listen HTTP/1.1\r\nHost: localhost\r\nContent-Type: application/grpc-web+proto\r\nContent-Length: 5\r\n\r\n\0\0\0\0\0").await.unwrap();
    wait_count(&emitted, 3).await;
    tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            let before = emitted.load(Ordering::SeqCst);
            tokio::time::sleep(Duration::from_millis(30)).await;
            if emitted.load(Ordering::SeqCst) == before {
                break;
            }
        }
    })
    .await
    .unwrap();
    let joined = tokio::time::timeout(Duration::from_secs(3), server.close()).await;
    if joined.is_err() {
        drop(unread);
        server.close().await.unwrap();
    }
    assert!(
        joined.is_ok(),
        "close retained a backpressured HTTP1 connection"
    );
    joined.unwrap().unwrap();
    assert_eq!(dropped.load(Ordering::SeqCst), 1);
    let mut buffer = [0];
    assert_eq!(idle.read(&mut buffer).await.unwrap(), 0);
    assert_eq!(partial.read(&mut buffer).await.unwrap(), 0);
    server.close().await.unwrap();
}

#[tokio::test]
async fn unsupported_content_types_do_not_dispatch_or_serve_post_assets() {
    let endpoint = Endpoint::default();
    let calls = endpoint.calls.clone();
    let mut server = WebServer::start(endpoint.clone(), endpoint, 0)
        .await
        .unwrap();
    for content_type in ["application/grpc", "application/json"] {
        let (status, _, _) = call(
            server.address(),
            request("POST", MODES, content_type, envelope(&[])),
        )
        .await;
        assert_eq!(status, StatusCode::UNSUPPORTED_MEDIA_TYPE);
    }
    assert_eq!(calls.load(Ordering::SeqCst), 0);
    let (status, _, _) = call(
        server.address(),
        request(
            "POST",
            "/index.html",
            "application/grpc-web+proto",
            envelope(&[]),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let (status, _, _) = call(server.address(), request("PUT", "/", "", Bytes::new())).await;
    assert_eq!(status, StatusCode::METHOD_NOT_ALLOWED);
    server.close().await.unwrap();
}

#[tokio::test]
async fn generated_receive_and_send_limits_apply_to_both_service_paths() {
    let endpoint = Endpoint {
        describe_bytes: MESSAGE_BYTES,
        modes_bytes: MESSAGE_BYTES,
        ..Default::default()
    };
    let calls = endpoint.calls.clone();
    let mut server = WebServer::start(endpoint.clone(), endpoint, 0)
        .await
        .unwrap();
    let mut oversized = vec![0];
    oversized.extend_from_slice(&u32::try_from(MESSAGE_BYTES + 1).unwrap().to_be_bytes());
    for path in [MODES, RUN] {
        let (_, _, body) = call(
            server.address(),
            request(
                "POST",
                path,
                "application/grpc-web+proto",
                Bytes::from(oversized.clone()),
            ),
        )
        .await;
        let frames = frames(&body);
        assert_eq!(frames.last().unwrap().0, 0x80);
        assert!(String::from_utf8_lossy(&frames.last().unwrap().1).contains("grpc-status: 11\r\n"));
    }
    assert_eq!(calls.load(Ordering::SeqCst), 0);
    for path in [MODES, DESCRIBE] {
        let (_, _, body) = call(
            server.address(),
            request("POST", path, "application/grpc-web+proto", envelope(&[])),
        )
        .await;
        let frames = frames(&body);
        assert_eq!(frames.len(), 1);
        assert!(String::from_utf8_lossy(&frames[0].1).contains("grpc-status: 11\r\n"));
    }
    server.close().await.unwrap();
}

fn decode_text(bytes: &[u8]) -> Vec<u8> {
    assert_eq!(bytes.len() % 4, 0);
    bytes
        .chunks(4)
        .flat_map(|quad| STANDARD.decode(quad).unwrap())
        .collect()
}

async fn call_text_chunks(address: SocketAddr, path: &str, chunks: Vec<Bytes>) -> Bytes {
    let stream = TcpStream::connect(address).await.unwrap();
    let (mut client, connection) = hyper::client::conn::http1::handshake(TokioIo::new(stream))
        .await
        .unwrap();
    let (stop, receiver) = oneshot::channel();
    let task = tokio::spawn(async move {
        tokio::select! { _ = receiver => {}, _ = connection => {} }
    });
    let body = http_body_util::StreamBody::new(futures_util::stream::iter(
        chunks
            .into_iter()
            .map(|bytes| Ok::<_, std::convert::Infallible>(hyper::body::Frame::data(bytes))),
    ));
    let request = Request::builder()
        .method("POST")
        .uri(path)
        .header("host", "localhost")
        .header("content-type", "application/grpc-web-text+proto")
        .body(body)
        .unwrap();
    let response = client.send_request(request).await.unwrap();
    assert_eq!(
        response.headers()["content-type"],
        "application/grpc-web-text+proto"
    );
    let body = response.into_body().collect().await.unwrap().to_bytes();
    let _ = stop.send(());
    task.await.unwrap();
    body
}

async fn first_text_message(body: &mut Incoming) -> Vec<u8> {
    tokio::time::timeout(Duration::from_secs(3), async {
        let mut encoded = Vec::new();
        loop {
            let data = first_data(body).await;
            encoded.extend_from_slice(&data);
            let complete = encoded.len() / 4 * 4;
            let decoded = decode_text(&encoded[..complete]);
            if decoded.len() >= 5 {
                let length = u32::from_be_bytes(decoded[1..5].try_into().unwrap()) as usize;
                if decoded.len() >= 5 + length {
                    return decoded;
                }
            }
        }
    })
    .await
    .unwrap()
}

#[tokio::test]
async fn text_mode_serves_both_services_and_exact_error_trailers() {
    let endpoint = Endpoint::default();
    let mut server = WebServer::start(endpoint.clone(), endpoint, 0)
        .await
        .unwrap();
    for (path, mime) in [
        (MODES, "application/grpc-web-text"),
        (DESCRIBE, "application/grpc-web-text+proto"),
    ] {
        let (_, headers, body) = call(
            server.address(),
            request(
                "POST",
                path,
                mime,
                Bytes::from(STANDARD.encode(envelope(&[]))),
            ),
        )
        .await;
        assert_eq!(headers["content-type"], "application/grpc-web-text+proto");
        let frames = frames(&decode_text(&body));
        assert_eq!(frames[0].0, 0);
        assert_eq!(frames[1], (0x80, b"grpc-status: 0\r\n".to_vec()));
    }
    let (_, headers, body) = call(
        server.address(),
        request(
            "POST",
            "/parrot.v1.Parrot/ListSessions",
            "application/grpc-web-text",
            Bytes::from(STANDARD.encode(envelope(&[]))),
        ),
    )
    .await;
    assert_eq!(headers["fixture-error"], "retained");
    let frames = frames(&decode_text(&body));
    let trailers = String::from_utf8_lossy(&frames[0].1);
    assert_eq!(frames[0].0, 0x80);
    assert!(trailers.contains("grpc-status: 13\r\n"));
    assert_native_message(&trailers, "synthetic failure: π\n");
    assert!(trailers.contains("grpc-status-details-bin: ZGV0YWlscw\r\n"));
    server.close().await.unwrap();
}

#[tokio::test]
async fn text_requests_decode_across_every_quad_split_and_midstream_padding() {
    let endpoint = Endpoint::default();
    let mut server = WebServer::start(endpoint.clone(), endpoint, 0)
        .await
        .unwrap();
    let mut message = wire::AttachSessionRequest {
        user_session_id: "synthetic-text".into(),
        ..Default::default()
    }
    .write_to_bytes()
    .unwrap();
    message.extend_from_slice(&[0xa0, 0x06, 1, 0xa0, 0x06, 2]);
    let envelope = envelope(&message);
    let encoded = STANDARD.encode(&envelope);
    let chunks = encoded
        .bytes()
        .map(|byte| Bytes::from(vec![byte]))
        .collect();
    let body = call_text_chunks(server.address(), "/parrot.v1.Parrot/AttachSession", chunks).await;
    assert_eq!(frames(&decode_text(&body))[0].1, message);
    let padded: String = envelope
        .chunks(1)
        .map(|bytes| STANDARD.encode(bytes))
        .collect();
    for width in [1, 2, 3, 5, 7] {
        let chunks = padded
            .as_bytes()
            .chunks(width)
            .map(Bytes::copy_from_slice)
            .collect();
        let body =
            call_text_chunks(server.address(), "/parrot.v1.Parrot/AttachSession", chunks).await;
        assert_eq!(frames(&decode_text(&body))[0].1, message);
    }
    server.close().await.unwrap();
}

#[tokio::test]
async fn malformed_and_truncated_text_returns_status_without_rpc_dispatch() {
    let endpoint = Endpoint::default();
    let calls = endpoint.calls.clone();
    let mut server = WebServer::start(endpoint.clone(), endpoint, 0)
        .await
        .unwrap();
    for invalid in ["!!!!", "AAA", "AA=A", "A==="] {
        let body = call_text_chunks(
            server.address(),
            MODES,
            invalid
                .bytes()
                .map(|byte| Bytes::from(vec![byte]))
                .collect(),
        )
        .await;
        let frames = frames(&decode_text(&body));
        assert_eq!(frames.len(), 1);
        assert_eq!(frames[0].0, 0x80);
        assert!(String::from_utf8_lossy(&frames[0].1).contains("grpc-status: 3\r\n"));
    }
    assert_eq!(calls.load(Ordering::SeqCst), 0);
    server.close().await.unwrap();
}

#[tokio::test]
async fn text_streams_flush_complete_messages_before_completion_and_cancel_on_disconnect() {
    let endpoint = Endpoint::default();
    let dropped = endpoint.dropped.clone();
    let mut server = WebServer::start(endpoint.clone(), endpoint, 0)
        .await
        .unwrap();
    for path in [LISTEN, RUN] {
        let (mut client, stop, task) = connect(server.address()).await;
        let response = client
            .send_request(request(
                "POST",
                path,
                "application/grpc-web-text+proto",
                Bytes::from(STANDARD.encode(envelope(&[]))),
            ))
            .await
            .unwrap();
        let mut body = response.into_body();
        let message = first_text_message(&mut body).await;
        assert_eq!(frames(&message)[0].0, 0);
        let _ = stop.send(());
        drop(body);
        task.await.unwrap();
    }
    wait_count(&dropped, 2).await;
    server.close().await.unwrap();
}

#[tokio::test]
async fn text_stream_errors_preserve_terminal_metadata_and_status() {
    let endpoint = Endpoint {
        terminal_stream: true,
        ..Default::default()
    };
    let mut server = WebServer::start(endpoint.clone(), endpoint, 0)
        .await
        .unwrap();
    let (_, _, body) = call(
        server.address(),
        request(
            "POST",
            LISTEN,
            "application/grpc-web-text+proto",
            Bytes::from(STANDARD.encode(envelope(&[]))),
        ),
    )
    .await;
    let frames = frames(&decode_text(&body));
    assert_eq!(frames.len(), 2);
    assert_eq!(frames[0].0, 0);
    assert_eq!(frames[1].0, 0x80);
    let trailers = String::from_utf8_lossy(&frames[1].1);
    assert!(trailers.contains("grpc-status: 14\r\n"));
    assert_native_message(&trailers, "stream ended");
    assert!(trailers.contains("fixture-trailer: retained\r\n"));
    server.close().await.unwrap();
}

const STATUS_MESSAGES: [&str; 4] = [
    "literal %",
    "literal %41",
    "unicode café π",
    "ordinary message",
];

#[tokio::test]
async fn native_status_messages_round_trip_literal_percent_unicode_and_ordinary_text() {
    use parrot_host::grpc::{
        address::TransportAddress,
        token::TransportToken,
        transport::{GrpcServer, GrpcTransportClient},
    };
    use parrot_storage::diagnostics::FileDiagnosticLog;

    let root = std::env::temp_dir().join(format!(
        "t64-percent-{}",
        FileDiagnosticLog::create_instance_id()
    ));
    let diagnostics = Arc::new(FileDiagnosticLog::open_global(&root, "percent", |_| {}).unwrap());
    let token = TransportToken::from_hex(&"ab".repeat(32)).unwrap();
    let mut server = GrpcServer::start(
        Endpoint::default(),
        TransportAddress::parse("http://127.0.0.1:0").unwrap(),
        Some(token.clone()),
        None,
        diagnostics.clone(),
    )
    .await
    .unwrap();
    let mut client =
        GrpcTransportClient::connect(server.address(), Some(&token), None, diagnostics.clone())
            .unwrap();
    let mut outcomes = Vec::new();
    for message in STATUS_MESSAGES {
        let result = client
            .client()
            .unwrap()
            .list_sessions(wire::ListSessionsRequest {
                working_directory: message.into(),
                ..Default::default()
            })
            .await;
        outcomes.push((message, result));
    }
    client.close();
    server.close().await.unwrap();
    drop(client);
    drop(server);
    drop(diagnostics);
    std::fs::remove_dir_all(root).unwrap();
    for (message, result) in outcomes {
        let status = result.unwrap_err();
        assert_eq!(status.code(), tonic::Code::Internal);
        assert_eq!(
            status.message(),
            message,
            "raw native message case {message:?}"
        );
        assert_eq!(status.details(), b"details");
        assert_eq!(status.metadata().get("fixture-error").unwrap(), "retained");
    }
}

#[tokio::test]
async fn binary_and_text_status_messages_keep_native_encoding_and_percent_escape() {
    let endpoint = Endpoint::default();
    let mut server = WebServer::start(endpoint.clone(), endpoint, 0)
        .await
        .unwrap();
    let mut outcomes = Vec::new();
    for message in STATUS_MESSAGES {
        let request_bytes = wire::ListSessionsRequest {
            working_directory: message.into(),
            ..Default::default()
        }
        .write_to_bytes()
        .unwrap();
        for text in [false, true] {
            let bytes = envelope(&request_bytes);
            let (mime, body) = if text {
                (
                    "application/grpc-web-text+proto",
                    Bytes::from(STANDARD.encode(bytes)),
                )
            } else {
                ("application/grpc-web+proto", bytes)
            };
            let (_, headers, body) = call(
                server.address(),
                request("POST", "/parrot.v1.Parrot/ListSessions", mime, body),
            )
            .await;
            let bytes = if text {
                decode_text(&body)
            } else {
                body.to_vec()
            };
            outcomes.push((message, text, headers, bytes));
        }
    }
    server.close().await.unwrap();
    for (message, text, headers, bytes) in outcomes {
        let frames = frames(&bytes);
        assert_eq!(frames.len(), 1, "text={text}, message={message:?}");
        assert_eq!(frames[0].0, 0x80);
        let trailers = String::from_utf8_lossy(&frames[0].1);
        assert!(trailers.contains("grpc-status: 13\r\n"));
        assert_native_message(&trailers, message);
        if message.contains('%') {
            assert!(
                trailers.contains("%25"),
                "literal percent was not escaped: {trailers:?}"
            );
        }
        assert!(trailers.contains("grpc-status-details-bin: ZGV0YWlscw\r\n"));
        assert_eq!(headers["fixture-error"], "retained");
    }
}
