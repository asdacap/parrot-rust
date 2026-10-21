//! HTTP/2 transport tests with a synthetic RPC endpoint, not a composition owner.
use bytes::Bytes;
use http_body_util::Full;
use parrot_domain::{
    contracts::async_ops::{BoxFuture, Cancellation},
    protocol::{bindings::parrot::parrot_server::Parrot, messages::parrot as wire},
};
use parrot_host::grpc::{
    address::TransportAddress,
    token::TransportToken,
    transport::{AttachError, GrpcServer, GrpcTransportClient},
};
use parrot_storage::diagnostics::FileDiagnosticLog;
use std::{
    pin::Pin,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};
use tonic::{Request, Response, Status};

#[derive(Clone, Default)]
struct Endpoint {
    calls: Arc<AtomicUsize>,
    missing: Arc<AtomicUsize>,
    status: Option<tonic::Code>,
    reply_id: Option<String>,
    response_bytes: usize,
    listen_bytes: usize,
    emitted: Arc<AtomicUsize>,
}
#[tonic::async_trait]
impl Parrot for Endpoint {
    type ListenStream =
        Pin<Box<dyn futures_core::Stream<Item = Result<wire::Event, Status>> + Send>>;
    async fn list_models(
        &self,
        _request: Request<wire::ListModelsRequest>,
    ) -> Result<Response<wire::ListModelsResponse>, Status> {
        Err(Status::unimplemented("unused synthetic fixture method"))
    }
    async fn list_model_aliases(
        &self,
        _request: Request<wire::ListModelAliasesRequest>,
    ) -> Result<Response<wire::ListModelAliasesResponse>, Status> {
        Err(Status::unimplemented("unused synthetic fixture method"))
    }
    async fn configure_model_alias(
        &self,
        _request: Request<wire::ConfigureModelAliasRequest>,
    ) -> Result<Response<wire::ConfigureModelAliasResponse>, Status> {
        Err(Status::unimplemented("unused synthetic fixture method"))
    }
    async fn list_provider_model_alias_defaults(
        &self,
        _request: Request<wire::ListProviderModelAliasDefaultsRequest>,
    ) -> Result<Response<wire::ListProviderModelAliasDefaultsResponse>, Status> {
        Err(Status::unimplemented("unused synthetic fixture method"))
    }
    async fn apply_provider_model_alias_defaults(
        &self,
        _request: Request<wire::ApplyProviderModelAliasDefaultsRequest>,
    ) -> Result<Response<wire::ApplyProviderModelAliasDefaultsResponse>, Status> {
        Err(Status::unimplemented("unused synthetic fixture method"))
    }
    async fn set_model_preset(
        &self,
        _request: Request<wire::SetModelPresetRequest>,
    ) -> Result<Response<wire::SetModelPresetResponse>, Status> {
        Err(Status::unimplemented("unused synthetic fixture method"))
    }
    async fn select_model_preset(
        &self,
        _request: Request<wire::SelectModelPresetRequest>,
    ) -> Result<Response<wire::SelectModelPresetResponse>, Status> {
        Err(Status::unimplemented("unused synthetic fixture method"))
    }
    async fn list_model_presets(
        &self,
        _request: Request<wire::ListModelPresetsRequest>,
    ) -> Result<Response<wire::ListModelPresetsResponse>, Status> {
        Err(Status::unimplemented("unused synthetic fixture method"))
    }
    async fn list_modes(
        &self,
        _request: Request<wire::ListModesRequest>,
    ) -> Result<Response<wire::ListModesResponse>, Status> {
        Err(Status::unimplemented("unused synthetic fixture method"))
    }
    async fn list_skills(
        &self,
        _request: Request<wire::ListSkillsRequest>,
    ) -> Result<Response<wire::ListSkillsResponse>, Status> {
        Err(Status::unimplemented("unused synthetic fixture method"))
    }
    async fn configure_skill(
        &self,
        _request: Request<wire::ConfigureSkillRequest>,
    ) -> Result<Response<wire::ConfigureSkillResponse>, Status> {
        Err(Status::unimplemented("unused synthetic fixture method"))
    }
    async fn list_sessions(
        &self,
        _request: Request<wire::ListSessionsRequest>,
    ) -> Result<Response<wire::ListSessionsResponse>, Status> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Ok(Response::new(wire::ListSessionsResponse::new()))
    }
    async fn create_session(
        &self,
        _request: Request<wire::CreateSessionRequest>,
    ) -> Result<Response<wire::UserSession>, Status> {
        Err(Status::unimplemented("unused synthetic fixture method"))
    }
    async fn resume_session(
        &self,
        _request: Request<wire::ResumeSessionRequest>,
    ) -> Result<Response<wire::UserSession>, Status> {
        Err(Status::unimplemented("unused synthetic fixture method"))
    }
    async fn attach_session(
        &self,
        request: Request<wire::AttachSessionRequest>,
    ) -> Result<Response<wire::UserSession>, Status> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        if let Some(code) = self.status {
            return Err(Status::new(code, "synthetic failure"));
        }
        if self
            .missing
            .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |n| n.checked_sub(1))
            .is_ok()
        {
            return Err(Status::not_found("owner not ready"));
        }
        Ok(Response::new(wire::UserSession {
            id: self
                .reply_id
                .clone()
                .unwrap_or(request.into_inner().user_session_id),
            model: "x".repeat(self.response_bytes),
            ..Default::default()
        }))
    }
    async fn update_session(
        &self,
        _request: Request<wire::UpdateSessionRequest>,
    ) -> Result<Response<wire::UserSession>, Status> {
        Err(Status::unimplemented("unused synthetic fixture method"))
    }
    async fn set_goal(
        &self,
        _request: Request<wire::SetGoalRequest>,
    ) -> Result<Response<wire::SetGoalResponse>, Status> {
        Err(Status::unimplemented("unused synthetic fixture method"))
    }
    async fn set_standing_instruction(
        &self,
        _request: Request<wire::SetStandingInstructionRequest>,
    ) -> Result<Response<wire::SetStandingInstructionResponse>, Status> {
        Err(Status::unimplemented("unused synthetic fixture method"))
    }
    async fn send_message(
        &self,
        _request: Request<wire::SendMessageRequest>,
    ) -> Result<Response<wire::SendMessageResponse>, Status> {
        Err(Status::unimplemented("unused synthetic fixture method"))
    }
    async fn upload_attachment(
        &self,
        _request: Request<tonic::Streaming<wire::AttachmentUploadFrame>>,
    ) -> Result<Response<wire::AttachmentUploadResponse>, Status> {
        Err(Status::unimplemented("unused synthetic fixture method"))
    }
    async fn interrupt(
        &self,
        _request: Request<wire::InterruptRequest>,
    ) -> Result<Response<wire::InterruptResponse>, Status> {
        Err(Status::unimplemented("unused synthetic fixture method"))
    }
    async fn compact(
        &self,
        _request: Request<wire::CompactRequest>,
    ) -> Result<Response<wire::CompactResponse>, Status> {
        Err(Status::unimplemented("unused synthetic fixture method"))
    }
    async fn set_context_limit(
        &self,
        _request: Request<wire::SetContextLimitRequest>,
    ) -> Result<Response<wire::SetContextLimitResponse>, Status> {
        Err(Status::unimplemented("unused synthetic fixture method"))
    }
    async fn sandbox_enable(
        &self,
        _request: Request<wire::SandboxEnableRequest>,
    ) -> Result<Response<wire::SandboxEnableResponse>, Status> {
        Err(Status::unimplemented("unused synthetic fixture method"))
    }
    async fn session_status(
        &self,
        _request: Request<wire::SessionStatusRequest>,
    ) -> Result<Response<wire::SessionStatusResponse>, Status> {
        Err(Status::unimplemented("unused synthetic fixture method"))
    }
    async fn list_pending_questions(
        &self,
        _request: Request<wire::ListPendingQuestionsRequest>,
    ) -> Result<Response<wire::ListPendingQuestionsResponse>, Status> {
        Err(Status::unimplemented("unused synthetic fixture method"))
    }
    async fn reply_question(
        &self,
        _request: Request<wire::ReplyQuestionRequest>,
    ) -> Result<Response<wire::ReplyQuestionResponse>, Status> {
        Err(Status::unimplemented("unused synthetic fixture method"))
    }
    async fn reject_question(
        &self,
        _request: Request<wire::RejectQuestionRequest>,
    ) -> Result<Response<wire::RejectQuestionResponse>, Status> {
        Err(Status::unimplemented("unused synthetic fixture method"))
    }
    async fn list_pending_permissions(
        &self,
        _request: Request<wire::ListPendingPermissionsRequest>,
    ) -> Result<Response<wire::ListPendingPermissionsResponse>, Status> {
        Err(Status::unimplemented("unused synthetic fixture method"))
    }
    async fn reply_permission(
        &self,
        _request: Request<wire::ReplyPermissionRequest>,
    ) -> Result<Response<wire::ReplyPermissionResponse>, Status> {
        Err(Status::unimplemented("unused synthetic fixture method"))
    }
    async fn listen(
        &self,
        _request: Request<wire::ListenRequest>,
    ) -> Result<Response<Self::ListenStream>, Status> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        let stream: Self::ListenStream = if self.listen_bytes == 0 {
            Box::pin(futures_util::stream::pending())
        } else {
            let bytes = self.listen_bytes;
            let emitted = self.emitted.clone();
            Box::pin(futures_util::stream::repeat_with(move || {
                emitted.fetch_add(1, Ordering::SeqCst);
                Ok(wire::Event {
                    id: "x".repeat(bytes),
                    ..Default::default()
                })
            }))
        };
        Ok(Response::new(stream))
    }
}

struct Context {
    root: std::path::PathBuf,
    log: Arc<FileDiagnosticLog>,
}
impl Context {
    fn new() -> Self {
        let root = std::path::Path::new("/tmp").join(format!(
            "t63-transport-{}",
            FileDiagnosticLog::create_instance_id()
        ));
        let log = Arc::new(FileDiagnosticLog::open_global(&root, "transport", |_| {}).unwrap());
        Self { root, log }
    }
    fn token(&self) -> TransportToken {
        TransportToken::from_hex(&"ab".repeat(32)).unwrap()
    }
    async fn server(&self, endpoint: Endpoint) -> GrpcServer {
        GrpcServer::start(
            endpoint,
            TransportAddress::parse("http://127.0.0.1:0").unwrap(),
            Some(self.token()),
            None,
            self.log.clone(),
        )
        .await
        .unwrap()
    }
    fn client(&self, server: &GrpcServer) -> GrpcTransportClient {
        GrpcTransportClient::connect(
            server.address(),
            Some(&self.token()),
            None,
            self.log.clone(),
        )
        .unwrap()
    }
}
impl Drop for Context {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}
struct Active;
impl Cancellation for Active {
    fn is_cancelled(&self) -> bool {
        false
    }
    fn cancelled(&self) -> BoxFuture<'_, ()> {
        Box::pin(std::future::pending())
    }
}
struct Cancel(tokio::sync::watch::Receiver<bool>);
impl Cancellation for Cancel {
    fn is_cancelled(&self) -> bool {
        *self.0.borrow()
    }
    fn cancelled(&self) -> BoxFuture<'_, ()> {
        Box::pin(async move {
            let mut receiver = self.0.clone();
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
fn attachment(id: &str) -> wire::AttachSessionRequest {
    wire::AttachSessionRequest {
        user_session_id: id.into(),
        ..Default::default()
    }
}
async fn bounded<T>(future: impl std::future::Future<Output = T>) -> T {
    tokio::time::timeout(Duration::from_secs(12), future)
        .await
        .expect("transport test exceeded bound")
}

#[tokio::test]
async fn tcp_is_http2_and_client_disposal_does_not_stop_owner() {
    bounded(async {
        let context = Context::new();
        let mut server = context.server(Endpoint::default()).await;
        let mut first = context.client(&server);
        first
            .client()
            .unwrap()
            .list_sessions(wire::ListSessionsRequest::new())
            .await
            .unwrap();
        first.close();
        assert!(first.client().is_err());
        assert!(matches!(
            first.attach(attachment("id"), &Active).await,
            Err(AttachError::Disposed)
        ));
        let mut second = context.client(&server);
        second
            .client()
            .unwrap()
            .list_sessions(wire::ListSessionsRequest::new())
            .await
            .unwrap();
        server.close().await.unwrap();
        server.close().await.unwrap();
    })
    .await;
}

#[tokio::test]
async fn tcp_requires_token_and_https_requires_identity() {
    let context = Context::new();
    let address = TransportAddress::parse("http://127.0.0.1:0").unwrap();
    assert!(
        GrpcServer::start(
            Endpoint::default(),
            address.clone(),
            None,
            None,
            context.log.clone()
        )
        .await
        .is_err()
    );
    assert!(GrpcTransportClient::connect(&address, None, None, context.log.clone()).is_err());
    let address = TransportAddress::parse("https://127.0.0.1:0").unwrap();
    assert!(
        GrpcServer::start(
            Endpoint::default(),
            address,
            Some(context.token()),
            None,
            context.log.clone()
        )
        .await
        .is_err()
    );
}

#[tokio::test]
async fn malformed_unauthorized_http2_is_401_before_rpc_dispatch() {
    bounded(async {
        let context = Context::new();
        let endpoint = Endpoint::default();
        let calls = endpoint.calls.clone();
        let mut server = context.server(endpoint).await;
        for authorization in [
            None,
            Some("Bearer incorrect"),
            Some("bearer incorrect"),
            Some("Basic incorrect"),
        ] {
            let socket = tokio::net::TcpStream::connect(server.address().socket_addr().unwrap())
                .await
                .unwrap();
            let (mut sender, connection) = hyper::client::conn::http2::handshake(
                hyper_util::rt::TokioExecutor::new(),
                hyper_util::rt::TokioIo::new(socket),
            )
            .await
            .unwrap();
            let task = tokio::spawn(connection);
            let mut request = http::Request::builder()
                .method("POST")
                .uri(format!(
                    "{}/parrot.v1.Parrot/ListSessions",
                    server.address()
                ))
                .header("content-type", "application/grpc");
            if let Some(value) = authorization {
                request = request.header("authorization", value);
            }
            let response = sender
                .send_request(
                    request
                        .body(Full::new(Bytes::from_static(b"not a grpc frame")))
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(response.status(), http::StatusCode::UNAUTHORIZED);
            assert_eq!(response.version(), http::Version::HTTP_2);
            drop(response);
            drop(sender);
            task.await.unwrap().unwrap();
        }
        assert_eq!(calls.load(Ordering::SeqCst), 0);
        let mut client = context.client(&server);
        client
            .client()
            .unwrap()
            .list_sessions(wire::ListSessionsRequest::new())
            .await
            .unwrap();
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        server.close().await.unwrap();
    })
    .await;
}

#[tokio::test]
async fn attach_retries_only_not_found_and_returns_exact_session() {
    bounded(async {
        let context = Context::new();
        let endpoint = Endpoint::default();
        endpoint.missing.store(2, Ordering::SeqCst);
        let calls = endpoint.calls.clone();
        let mut server = context.server(endpoint).await;
        let mut client = context.client(&server);
        let session = client
            .attach(attachment("exact-id"), &Active)
            .await
            .unwrap();
        assert_eq!(session.id, "exact-id");
        assert_eq!(calls.load(Ordering::SeqCst), 3);
        server.close().await.unwrap();
    })
    .await;
}

#[tokio::test]
async fn attach_rejects_different_session_and_non_retryable_status() {
    bounded(async {
        let context = Context::new();
        for code in [
            None,
            Some(tonic::Code::PermissionDenied),
            Some(tonic::Code::InvalidArgument),
            Some(tonic::Code::Unauthenticated),
        ] {
            let endpoint = Endpoint {
                status: code,
                reply_id: Some("wrong-id".into()),
                ..Default::default()
            };
            let calls = endpoint.calls.clone();
            let mut server = context.server(endpoint).await;
            let mut client = context.client(&server);
            match (
                client.attach(attachment("expected-id"), &Active).await,
                code,
            ) {
                (Err(AttachError::DifferentSession), None) => {}
                (Err(AttachError::Rpc(error)), Some(expected)) => {
                    assert_eq!(error.code(), expected)
                }
                other => panic!("unexpected attach result: {other:?}"),
            }
            assert_eq!(calls.load(Ordering::SeqCst), 1);
            server.close().await.unwrap();
        }
    })
    .await;
}

#[tokio::test]
async fn attach_total_deadline_and_caller_cancellation() {
    bounded(async {
        let context = Context::new();
        let endpoint = Endpoint {
            status: Some(tonic::Code::Unavailable),
            ..Default::default()
        };
        let mut server = context.server(endpoint).await;
        let mut client = context.client(&server);
        let start = tokio::time::Instant::now();
        assert!(matches!(
            client.attach(attachment("id"), &Active).await,
            Err(AttachError::Timeout)
        ));
        assert!(start.elapsed() >= Duration::from_millis(2900));
        assert!(start.elapsed() < Duration::from_secs(5));
        let (sender, receiver) = tokio::sync::watch::channel(false);
        let cancellation = Cancel(receiver);
        let cancel = async {
            tokio::task::yield_now().await;
            sender.send_replace(true);
        };
        let (result, ()) = tokio::join!(client.attach(attachment("id"), &cancellation), cancel);
        assert!(matches!(result, Err(AttachError::Cancelled)));
        server.close().await.unwrap();
    })
    .await;
}

#[tokio::test]
async fn shutdown_terminates_forever_listen_and_retains_join_when_close_observer_drops() {
    bounded(async {
        let context = Context::new();
        let mut server = context.server(Endpoint::default()).await;
        let mut client = context.client(&server);
        let mut stream = client
            .client()
            .unwrap()
            .listen(wire::ListenRequest::new())
            .await
            .unwrap()
            .into_inner();
        // One poll requests shutdown. Dropping that observer must leave the actual
        // JoinHandle in the owner; a second close observes the same completion.
        let mut close = Box::pin(server.close());
        std::future::poll_fn(|cx| {
            let _ = std::future::Future::poll(close.as_mut(), cx);
            std::task::Poll::Ready(())
        })
        .await;
        drop(close);
        server.close().await.unwrap();
        assert!(matches!(stream.message().await, Ok(None) | Err(_)));
    })
    .await;
}

#[cfg(unix)]
#[tokio::test]
async fn unix_lazy_attach_can_precede_owner_and_socket_is_cleaned_after_join() {
    bounded(async {
        let context = Context::new();
        let path = context.root.join("socket/parrot.sock");
        let address = TransportAddress::Unix(path.clone());
        let mut client =
            GrpcTransportClient::connect(&address, None, None, context.log.clone()).unwrap();
        let attach = client.attach(attachment("owner-id"), &Active);
        let owner = async {
            tokio::time::sleep(Duration::from_millis(150)).await;
            GrpcServer::start_local(Endpoint::default(), &path, context.log.clone())
                .await
                .unwrap()
        };
        let (result, mut server) = tokio::join!(attach, owner);
        assert_eq!(result.unwrap().id, "owner-id");
        assert!(path.exists());
        server.close().await.unwrap();
        assert!(!path.exists());
    })
    .await;
}

#[tokio::test]
async fn exact_64mib_request_is_accepted_and_both_outgoing_and_incoming_limits_reject_one_more() {
    bounded(async {
        use protobuf::Message;
        let context = Context::new();
        let endpoint = Endpoint::default();
        let calls = endpoint.calls.clone();
        let mut server = context.server(endpoint).await;
        let mut client = context.client(&server);
        let exact = attachment(&"x".repeat(parrot_host::grpc::MESSAGE_BYTES - 5));
        assert_eq!(
            exact.compute_size(),
            parrot_host::grpc::MESSAGE_BYTES as u64
        );
        assert_eq!(
            client
                .client()
                .unwrap()
                .attach_session(exact)
                .await
                .unwrap()
                .get_ref()
                .id
                .len(),
            parrot_host::grpc::MESSAGE_BYTES - 5
        );
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        let oversized = attachment(&"x".repeat(parrot_host::grpc::MESSAGE_BYTES - 4));
        assert_eq!(
            oversized.compute_size(),
            parrot_host::grpc::MESSAGE_BYTES as u64 + 1
        );
        let error = client
            .client()
            .unwrap()
            .attach_session(oversized.clone())
            .await
            .unwrap_err();
        // Tonic wraps its outgoing request-body encoding failure as Internal.
        assert_eq!(error.code(), tonic::Code::Internal);
        assert_eq!(
            calls.load(Ordering::SeqCst),
            1,
            "client encoding rejected before dispatch"
        );
        let mut bypass_client_send = client
            .client()
            .unwrap()
            .clone()
            .max_encoding_message_size(usize::MAX);
        let error = bypass_client_send
            .attach_session(oversized)
            .await
            .unwrap_err();
        assert_eq!(error.code(), tonic::Code::OutOfRange);
        assert_eq!(
            calls.load(Ordering::SeqCst),
            1,
            "server decoding rejected before dispatch"
        );
        server.close().await.unwrap();
    })
    .await;
}

#[tokio::test]
async fn exact_64mib_response_is_accepted_and_server_encoding_rejects_one_more() {
    bounded(async {
        use protobuf::Message;
        let context = Context::new();
        for extra in [0, 1] {
            let endpoint = Endpoint {
                reply_id: Some(String::new()),
                response_bytes: parrot_host::grpc::MESSAGE_BYTES - 5 + extra,
                ..Default::default()
            };
            let expected = wire::UserSession {
                model: "x".repeat(endpoint.response_bytes),
                ..Default::default()
            };
            assert_eq!(
                expected.compute_size(),
                (parrot_host::grpc::MESSAGE_BYTES + extra) as u64
            );
            drop(expected);
            let mut server = context.server(endpoint).await;
            let mut client = context.client(&server);
            let mut bypass_client_receive = client
                .client()
                .unwrap()
                .clone()
                .max_decoding_message_size(usize::MAX);
            let result = bypass_client_receive.attach_session(attachment("")).await;
            if extra == 0 {
                assert_eq!(
                    result.unwrap().get_ref().model.len(),
                    parrot_host::grpc::MESSAGE_BYTES - 5
                );
            } else {
                assert_eq!(result.unwrap_err().code(), tonic::Code::OutOfRange);
            }
            server.close().await.unwrap();
        }
    })
    .await;
}

#[tokio::test]
async fn client_decoding_limit_rejects_oversized_response_even_when_server_encoding_is_unbounded() {
    bounded(async {
        use parrot_domain::protocol::bindings::parrot::parrot_server::ParrotServer;
        let context = Context::new();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address =
            TransportAddress::parse(&format!("http://{}", listener.local_addr().unwrap())).unwrap();
        let (stop, stopped) = tokio::sync::oneshot::channel();
        let task = tokio::spawn(async move {
            tonic::transport::Server::builder()
                .add_service(
                    ParrotServer::new(Endpoint {
                        reply_id: Some(String::new()),
                        response_bytes: parrot_host::grpc::MESSAGE_BYTES - 4,
                        ..Default::default()
                    })
                    .max_encoding_message_size(usize::MAX),
                )
                .serve_with_incoming_shutdown(
                    tokio_stream::wrappers::TcpListenerStream::new(listener),
                    async {
                        let _ = stopped.await;
                    },
                )
                .await
        });
        let mut client = GrpcTransportClient::connect(
            &address,
            Some(&context.token()),
            None,
            context.log.clone(),
        )
        .unwrap();
        assert_eq!(
            client
                .client()
                .unwrap()
                .attach_session(attachment(""))
                .await
                .unwrap_err()
                .code(),
            tonic::Code::OutOfRange
        );
        stop.send(()).unwrap();
        task.await.unwrap().unwrap();
    })
    .await;
}

#[tokio::test]
async fn tls_rejects_untrusted_fixture_without_disabling_certificate_verification() {
    bounded(async {
        let context = Context::new();
        let identity = tonic::transport::Identity::from_pem(
            include_bytes!("fixtures/server.pem"),
            include_bytes!("fixtures/server-key.pem"),
        );
        let mut server = GrpcServer::start(
            Endpoint::default(),
            TransportAddress::parse("https://127.0.0.1:0").unwrap(),
            Some(context.token()),
            Some(tonic::transport::ServerTlsConfig::new().identity(identity)),
            context.log.clone(),
        )
        .await
        .unwrap();
        let mut client = context.client(&server);
        assert_eq!(
            client
                .client()
                .unwrap()
                .list_sessions(wire::ListSessionsRequest::new())
                .await
                .unwrap_err()
                .code(),
            tonic::Code::Unavailable
        );
        server.close().await.unwrap();
    })
    .await;
}

#[tokio::test]
async fn tls_explicit_synthetic_root_validates_normal_verified_http2() {
    bounded(async {
        let context = Context::new();
        let certificate = include_bytes!("fixtures/server.pem");
        let identity = tonic::transport::Identity::from_pem(
            certificate,
            include_bytes!("fixtures/server-key.pem"),
        );
        let mut server = GrpcServer::start(
            Endpoint::default(),
            TransportAddress::parse("https://127.0.0.1:0").unwrap(),
            Some(context.token()),
            Some(tonic::transport::ServerTlsConfig::new().identity(identity)),
            context.log.clone(),
        )
        .await
        .unwrap();
        let tls = tonic::transport::ClientTlsConfig::new()
            .ca_certificate(tonic::transport::Certificate::from_pem(include_bytes!(
                "fixtures/root.pem"
            )))
            .domain_name("localhost");
        let mut client = GrpcTransportClient::connect(
            server.address(),
            Some(&context.token()),
            Some(tls),
            context.log.clone(),
        )
        .unwrap();
        let result = client
            .client()
            .unwrap()
            .list_sessions(wire::ListSessionsRequest::new())
            .await;
        server.close().await.unwrap();
        result.expect("approved fixture must establish an actually verified trust chain");
    })
    .await;
}

#[tokio::test]
async fn global_transport_diagnostics_correlate_lifetimes_without_paths_tokens_or_payloads() {
    bounded(async {
        let context = Context::new();
        let mut server = context.server(Endpoint::default()).await;
        let mut client = context.client(&server);
        client
            .attach(attachment("PRIVATE-SESSION-PAYLOAD"), &Active)
            .await
            .unwrap();
        client.close();
        server.close().await.unwrap();
        let text = std::fs::read_to_string(context.log.path()).unwrap();
        for operation in [
            "host.start",
            "host.ready",
            "host.stop.start",
            "host.stop.complete",
            "channel.open.start",
            "channel.open.complete",
            "attach.start",
            "attach.complete",
            "disconnect.start",
            "disconnect.complete",
        ] {
            assert!(
                text.contains(&format!("event=\"{operation}\"")),
                "missing {operation}: {text}"
            );
        }
        for group in [
            ["host.start", "host.ready"],
            ["host.start", "host.stop.complete"],
            ["channel.open.start", "disconnect.complete"],
            ["attach.start", "attach.complete"],
        ] {
            let ids: Vec<_> = group
                .iter()
                .map(|operation| {
                    let line = text
                        .lines()
                        .find(|line| line.contains(&format!("event=\"{operation}\"")))
                        .unwrap();
                    line.split("correlation=\"")
                        .nth(1)
                        .unwrap()
                        .split('"')
                        .next()
                        .unwrap()
                })
                .collect();
            assert_eq!(ids[0], ids[1]);
            assert!(!ids[0].is_empty());
        }
        assert!(text.contains("duration_ms=\""));
        assert!(text.contains("outcome=\"success\""));
        assert!(!text.contains("PRIVATE-SESSION-PAYLOAD"));
        assert!(!text.contains(&context.token().bearer()));
        assert!(!text.contains(&context.root.to_string_lossy().to_string()));
        assert!(!text.contains("user_session="));
    })
    .await;
}

#[tokio::test]
async fn separate_global_sinks_record_exact_attach_pair_and_sanitized_rejection_codes() {
    bounded(async {
        for code in [
            None,
            Some(tonic::Code::PermissionDenied),
            Some(tonic::Code::InvalidArgument),
            Some(tonic::Code::Aborted),
        ] {
            let server_context = Context::new();
            let client_context = Context::new();
            let mut server = server_context
                .server(Endpoint {
                    status: code,
                    ..Default::default()
                })
                .await;
            let mut client = client_context.client(&server);
            let result = client
                .attach(
                    wire::AttachSessionRequest {
                        user_session_id: "DIAGNOSTIC-PRIVATE-ID".into(),
                        working_directory: "/DIAGNOSTIC-PRIVATE-WORKSPACE".into(),
                        ..Default::default()
                    },
                    &Active,
                )
                .await;
            assert_eq!(result.is_ok(), code.is_none());
            client.close();
            server.close().await.unwrap();
            let client_log = std::fs::read_to_string(client_context.log.path()).unwrap();
            let server_log = std::fs::read_to_string(server_context.log.path()).unwrap();
            let entries: Vec<_> = client_log
                .lines()
                .filter(|line| line.contains("event=\"attach."))
                .collect();
            assert_eq!(entries.len(), 2);
            assert!(entries[0].contains("event=\"attach.start\""));
            assert!(entries[1].contains("event=\"attach.complete\""));
            let correlation = |line: &str| {
                line.split("correlation=\"")
                    .nth(1)
                    .unwrap()
                    .split('"')
                    .next()
                    .unwrap()
                    .to_owned()
            };
            assert_eq!(correlation(entries[0]), correlation(entries[1]));
            assert!(!correlation(entries[0]).is_empty());
            assert!(entries[1].contains("duration_ms=\""));
            assert!(entries[1].contains(if code.is_none() {
                "outcome=\"success\""
            } else {
                "outcome=\"failure\""
            }));
            if let Some(code) = code {
                let expected = match code {
                    tonic::Code::PermissionDenied => "PermissionDenied",
                    tonic::Code::InvalidArgument => "InvalidArgument",
                    tonic::Code::Aborted => "Aborted",
                    _ => unreachable!(),
                };
                assert!(entries[1].contains(&format!("error=\"{expected}\"")));
            } else {
                assert!(!entries[1].contains("error="));
            }
            assert!(client_log.contains("event=\"disconnect.complete\""));
            assert!(!client_log.contains("event=\"host."));
            assert!(server_log.contains("event=\"host.stop.complete\""));
            assert!(!server_log.contains("event=\"attach."));
            assert!(!server_log.contains("event=\"disconnect."));
            for log in [&client_log, &server_log] {
                for sentinel in [
                    "DIAGNOSTIC-PRIVATE-ID",
                    "/DIAGNOSTIC-PRIVATE-WORKSPACE",
                    "synthetic failure",
                    &server_context.token().bearer(),
                    &server.address().to_string(),
                ] {
                    assert!(!log.contains(sentinel));
                }
            }
        }
    })
    .await;
}

#[tokio::test]
async fn shutdown_joins_while_http2_response_is_unread_and_flow_control_is_exhausted() {
    bounded(async {
        let context = Context::new();
        let endpoint = Endpoint {
            listen_bytes: 256 * 1024,
            ..Default::default()
        };
        let emitted = endpoint.emitted.clone();
        let mut server = context.server(endpoint).await;
        let socket = tokio::net::TcpStream::connect(server.address().socket_addr().unwrap())
            .await
            .unwrap();
        let (mut sender, connection) =
            hyper::client::conn::http2::Builder::new(hyper_util::rt::TokioExecutor::new())
                .initial_stream_window_size(1024)
                .initial_connection_window_size(1024)
                .handshake::<_, Full<Bytes>>(hyper_util::rt::TokioIo::new(socket))
                .await
                .unwrap();
        let task = tokio::spawn(connection);
        let request = http::Request::builder()
            .method("POST")
            .uri(format!("{}/parrot.v1.Parrot/Listen", server.address()))
            .header("content-type", "application/grpc")
            .header(
                "authorization",
                format!("Bearer {}", context.token().bearer()),
            )
            .body(Full::new(Bytes::from_static(&[0; 5])))
            .unwrap();
        // Keep this body alive without polling it: HTTP/2 credit cannot replenish.
        let response = sender.send_request(request).await.unwrap();
        assert_eq!(response.status(), http::StatusCode::OK);
        tokio::time::sleep(Duration::from_millis(100)).await;
        let produced = emitted.load(Ordering::SeqCst);
        assert!(produced > 0);
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert_eq!(
            emitted.load(Ordering::SeqCst),
            produced,
            "producer must be flow-controlled before shutdown"
        );
        let closed_without_peer_cooperation =
            tokio::time::timeout(Duration::from_millis(500), server.close()).await;
        // Even on the red epoch, explicitly release the peer and observe the
        // retained actual server join before asserting; timeout is not a join.
        drop(response);
        drop(sender);
        server.close().await.unwrap();
        let _ = task.await.unwrap();
        closed_without_peer_cooperation
            .expect("shutdown depended on unread peer granting HTTP/2 send credit")
            .unwrap();
    })
    .await;
}

#[tokio::test]
async fn shutdown_releases_idle_http2_and_partial_tls_handshake_connections() {
    bounded(async {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let context = Context::new();
        for tls in [false, true] {
            let identity = tonic::transport::Identity::from_pem(
                include_bytes!("fixtures/server.pem"),
                include_bytes!("fixtures/server-key.pem"),
            );
            let mut server = GrpcServer::start(
                Endpoint::default(),
                TransportAddress::parse(if tls {
                    "https://127.0.0.1:0"
                } else {
                    "http://127.0.0.1:0"
                })
                .unwrap(),
                Some(context.token()),
                tls.then(|| tonic::transport::ServerTlsConfig::new().identity(identity)),
                context.log.clone(),
            )
            .await
            .unwrap();
            let mut socket =
                tokio::net::TcpStream::connect(server.address().socket_addr().unwrap())
                    .await
                    .unwrap();
            socket
                .write_all(if tls { &[22, 3, 1, 0] } else { b"PRI" })
                .await
                .unwrap();
            tokio::time::sleep(Duration::from_millis(30)).await;
            tokio::time::timeout(Duration::from_millis(500), server.close())
                .await
                .expect("partial handshake retained transport shutdown")
                .unwrap();
            // EOF or a connection reset both prove the accepted native IO was
            // released; a still-pending read would reveal a detached handshake.
            let _ = tokio::time::timeout(
                Duration::from_millis(500),
                socket.read_to_end(&mut Vec::new()),
            )
            .await
            .expect("accepted connection survived the actual server join");
        }
    })
    .await;
}
