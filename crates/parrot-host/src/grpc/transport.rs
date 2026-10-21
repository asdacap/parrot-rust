//! Native HTTP/2 transport. The caller owns the service and its executor.
//!
//! Stop and join this transport before closing the borrowed service. The native
//! composition owner must retain its existing Runtime through both operations.

use super::{MESSAGE_BYTES, address::TransportAddress, token::TransportToken};
use bytes::Bytes;
use futures_util::StreamExt;
use http::{Request, Response, StatusCode};
use http_body_util::BodyExt;
use hyper::body::{Body, Frame, SizeHint};
use hyper_util::rt::TokioIo;
use parrot_domain::{
    contracts::async_ops::Cancellation,
    protocol::{
        bindings::parrot::parrot_server::{Parrot, ParrotServer},
        messages::parrot as wire,
    },
};
use parrot_storage::diagnostics::{DiagnosticEvent, DiagnosticSeverity, FileDiagnosticLog};
use std::{
    error::Error,
    fmt,
    future::Future,
    io,
    path::Path,
    pin::Pin,
    sync::Arc,
    task::{Context, Poll},
    time::{Duration, Instant},
};
use tokio::{
    io::{AsyncRead, AsyncWrite, ReadBuf},
    net::TcpListener,
    sync::watch,
    task::JoinHandle,
};
use tokio_stream::wrappers::TcpListenerStream;
use tonic::{
    Status,
    body::BoxBody,
    service::{Interceptor, interceptor::InterceptedService},
    transport::{Channel, ClientTlsConfig, Endpoint, Server, ServerTlsConfig, server::Connected},
};
use tower::{Layer, Service, service_fn};

pub type TransportResult<T> = Result<T, Box<dyn Error + Send + Sync>>;
type RawClient = parrot_domain::protocol::bindings::parrot::parrot_client::ParrotClient<
    InterceptedService<Channel, BearerInterceptor>,
>;

/// Owns only the listener, HTTP/2 connections and transport task, not the service.
pub struct GrpcServer {
    address: TransportAddress,
    stopped: watch::Sender<bool>,
    task: Option<JoinHandle<TransportResult<()>>>,
    result: Option<Result<(), String>>,
    diagnostics: Arc<FileDiagnosticLog>,
    host_id: String,
}

impl GrpcServer {
    pub async fn start<S>(
        service: S,
        address: TransportAddress,
        token: Option<TransportToken>,
        tls: Option<ServerTlsConfig>,
        diagnostics: Arc<FileDiagnosticLog>,
    ) -> TransportResult<Self>
    where
        S: Parrot,
    {
        let host_id = FileDiagnosticLog::create_instance_id();
        let started = Instant::now();
        diagnostic(&diagnostics, "host.start", &host_id, None, None, None);
        let result = Self::bind(
            service,
            address,
            token,
            tls,
            diagnostics.clone(),
            host_id.clone(),
        )
        .await;
        match &result {
            Ok(_) => diagnostic(
                &diagnostics,
                "host.ready",
                &host_id,
                Some(started),
                Some("success"),
                None,
            ),
            Err(_) => diagnostic(
                &diagnostics,
                "host.failure",
                &host_id,
                Some(started),
                Some("failure"),
                Some("invalid_operation"),
            ),
        }
        result
    }

    async fn bind<S>(
        service: S,
        address: TransportAddress,
        token: Option<TransportToken>,
        tls: Option<ServerTlsConfig>,
        diagnostics: Arc<FileDiagnosticLog>,
        host_id: String,
    ) -> TransportResult<Self>
    where
        S: Parrot,
    {
        if address.is_tcp() && token.is_none() {
            return Err(invalid(
                "TCP transport requires an owner-only bearer token file",
            ));
        }
        let (stopped, receiver) = watch::channel(false);
        let layer = TransportLayer {
            token,
            stopped: receiver.clone(),
        };
        let mut builder = Server::builder();
        match (&address, tls) {
            (TransportAddress::Tcp(url), Some(tls)) if url.scheme() == "https" => {
                builder = builder.tls_config(tls)?;
            }
            (TransportAddress::Tcp(url), None) if url.scheme() == "https" => {
                return Err(invalid("HTTPS transport requires an explicit TLS identity"));
            }
            (_, Some(_)) => {
                return Err(invalid("TLS identity requires an HTTPS transport address"));
            }
            _ => {}
        }
        let router = builder.layer(layer).add_service(
            ParrotServer::new(service)
                .max_decoding_message_size(MESSAGE_BYTES)
                .max_encoding_message_size(MESSAGE_BYTES),
        );
        let (address, task) = match address {
            TransportAddress::Tcp(mut url) => {
                let socket_address = TransportAddress::Tcp(url.clone()).socket_addr()?;
                let listener = TcpListener::bind(socket_address).await?;
                let port = listener.local_addr()?.port();
                url.set_port(Some(port))
                    .map_err(|()| invalid("invalid TCP port"))?;
                let io_shutdown = receiver.clone();
                let incoming = TcpListenerStream::new(listener).map(move |stream| {
                    stream.map(|stream| ShutdownIo::new(stream, io_shutdown.clone()))
                });
                let task = tokio::spawn(async move {
                    router
                        .serve_with_incoming_shutdown(incoming, shutdown(receiver))
                        .await?;
                    Ok(())
                });
                (TransportAddress::Tcp(url), task)
            }
            #[cfg(unix)]
            TransportAddress::Unix(path) => {
                let (listener, owned) = super::unix::bind(&path)?;
                let listener = tokio::net::UnixListener::from_std(listener)?;
                let io_shutdown = receiver.clone();
                let incoming =
                    tokio_stream::wrappers::UnixListenerStream::new(listener).map(move |stream| {
                        stream.map(|stream| ShutdownIo::new(stream, io_shutdown.clone()))
                    });
                let task = tokio::spawn(async move {
                    let result = router
                        .serve_with_incoming_shutdown(incoming, shutdown(receiver))
                        .await;
                    let cleanup = owned.cleanup();
                    result?;
                    cleanup?;
                    Ok(())
                });
                (TransportAddress::Unix(path), task)
            }
            #[cfg(not(unix))]
            TransportAddress::Unix(_) => {
                return Err(invalid("Unix transport is not supported on this platform"));
            }
        };
        Ok(Self {
            address,
            stopped,
            task: Some(task),
            result: None,
            diagnostics,
            host_id,
        })
    }

    pub async fn start_local<S>(
        service: S,
        path: impl AsRef<Path>,
        diagnostics: Arc<FileDiagnosticLog>,
    ) -> TransportResult<Self>
    where
        S: Parrot,
    {
        Self::start(
            service,
            TransportAddress::Unix(path.as_ref().to_owned()),
            None,
            None,
            diagnostics,
        )
        .await
    }

    pub fn address(&self) -> &TransportAddress {
        &self.address
    }

    /// Signals transport shutdown and joins the actual server task. Forever
    /// response/request bodies are cancelled before HTTP/2 graceful shutdown.
    /// Dropping this observer while it waits retains the join in this owner.
    pub async fn close(&mut self) -> TransportResult<()> {
        if let Some(result) = &self.result {
            return cached_result(result);
        }
        let started = Instant::now();
        diagnostic(
            &self.diagnostics,
            "host.stop.start",
            &self.host_id,
            None,
            None,
            None,
        );
        self.stopped.send_replace(true);
        let result = match self.task.as_mut() {
            Some(task) => match task.await {
                Ok(Ok(())) => Ok(()),
                Ok(Err(error)) => Err(error.to_string()),
                Err(error) => Err(error.to_string()),
            },
            None => Ok(()),
        };
        self.task.take();
        diagnostic(
            &self.diagnostics,
            "host.stop.complete",
            &self.host_id,
            Some(started),
            Some(if result.is_ok() { "success" } else { "failure" }),
            result.as_ref().err().map(|_| "unexpected"),
        );
        self.result = Some(result);
        cached_result(self.result.as_ref().expect("close result was recorded"))
    }
}

impl Drop for GrpcServer {
    fn drop(&mut self) {
        // Drop requests shutdown; only close().await is a join receipt. The
        // actual server task retains its listener and Unix identity cleanup.
        self.stopped.send_replace(true);
    }
}

fn cached_result(result: &Result<(), String>) -> TransportResult<()> {
    result
        .clone()
        .map_err(|message| io::Error::other(message).into())
}

/// Cancellation is observed below HTTP/2 (and TLS), independently of response
/// body polling. A peer withholding flow-control credit cannot retain shutdown.
struct ShutdownIo<T> {
    inner: T,
    stopped: Pin<Box<dyn Future<Output = ()> + Send>>,
    ended: bool,
}
impl<T> ShutdownIo<T> {
    fn new(inner: T, receiver: watch::Receiver<bool>) -> Self {
        Self {
            inner,
            stopped: Box::pin(shutdown(receiver)),
            ended: false,
        }
    }
    fn is_stopped(&mut self, cx: &mut Context<'_>) -> bool {
        if !self.ended && self.stopped.as_mut().poll(cx).is_ready() {
            self.ended = true;
        }
        self.ended
    }
}
impl<T: Connected> Connected for ShutdownIo<T> {
    type ConnectInfo = T::ConnectInfo;
    fn connect_info(&self) -> Self::ConnectInfo {
        self.inner.connect_info()
    }
}
impl<T: AsyncRead + Unpin> AsyncRead for ShutdownIo<T> {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buffer: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        if self.is_stopped(cx) {
            return Poll::Ready(Err(io::Error::new(
                io::ErrorKind::ConnectionAborted,
                "transport stopped",
            )));
        }
        Pin::new(&mut self.inner).poll_read(cx, buffer)
    }
}
impl<T: AsyncWrite + Unpin> AsyncWrite for ShutdownIo<T> {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buffer: &[u8],
    ) -> Poll<io::Result<usize>> {
        if self.is_stopped(cx) {
            return Poll::Ready(Err(io::Error::new(
                io::ErrorKind::ConnectionAborted,
                "transport stopped",
            )));
        }
        Pin::new(&mut self.inner).poll_write(cx, buffer)
    }
    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        if self.is_stopped(cx) {
            return Poll::Ready(Err(io::Error::new(
                io::ErrorKind::ConnectionAborted,
                "transport stopped",
            )));
        }
        Pin::new(&mut self.inner).poll_flush(cx)
    }
    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        if self.is_stopped(cx) {
            return Poll::Ready(Err(io::Error::new(
                io::ErrorKind::ConnectionAborted,
                "transport stopped",
            )));
        }
        Pin::new(&mut self.inner).poll_shutdown(cx)
    }
}

#[derive(Clone)]
struct TransportLayer {
    token: Option<TransportToken>,
    stopped: watch::Receiver<bool>,
}
impl<S> Layer<S> for TransportLayer {
    type Service = TransportService<S>;
    fn layer(&self, inner: S) -> Self::Service {
        TransportService {
            inner,
            token: self.token.clone(),
            stopped: self.stopped.clone(),
        }
    }
}
#[derive(Clone)]
struct TransportService<S> {
    inner: S,
    token: Option<TransportToken>,
    stopped: watch::Receiver<bool>,
}
impl<S> Service<Request<BoxBody>> for TransportService<S>
where
    S: Service<Request<BoxBody>, Response = Response<BoxBody>> + Send,
    S::Future: Send + 'static,
{
    type Response = Response<BoxBody>;
    type Error = S::Error;
    type Future = Pin<Box<dyn Future<Output = Result<Self::Response, Self::Error>> + Send>>;
    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.inner.poll_ready(cx)
    }
    fn call(&mut self, request: Request<BoxBody>) -> Self::Future {
        if self.token.as_ref().is_some_and(|token| {
            !token.authenticate(
                request
                    .headers()
                    .get(http::header::AUTHORIZATION)
                    .and_then(|value| value.to_str().ok()),
            )
        }) {
            return Box::pin(async {
                Ok(Response::builder()
                    .status(StatusCode::UNAUTHORIZED)
                    .body(tonic::body::empty_body())
                    .expect("fixed HTTP response"))
            });
        }
        let incoming_shutdown = self.stopped.clone();
        let future = self
            .inner
            .call(request.map(|body| ShutdownBody::new(body, incoming_shutdown).boxed_unsync()));
        let stopped = self.stopped.clone();
        let body_shutdown = self.stopped.clone();
        Box::pin(async move {
            tokio::select! {
                biased;
                _ = shutdown(stopped) => Ok(Status::unavailable("transport stopped").into_http()),
                result = future => result.map(|response| response.map(|body| ShutdownBody::new(body, body_shutdown).boxed_unsync())),
            }
        })
    }
}

struct ShutdownBody {
    inner: BoxBody,
    stopped: Pin<Box<dyn Future<Output = ()> + Send>>,
    ended: bool,
}
impl ShutdownBody {
    fn new(inner: BoxBody, receiver: watch::Receiver<bool>) -> Self {
        Self {
            inner,
            stopped: Box::pin(shutdown(receiver)),
            ended: false,
        }
    }
}
impl Body for ShutdownBody {
    type Data = Bytes;
    type Error = Status;
    fn poll_frame(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Bytes>, Status>>> {
        if self.ended {
            return Poll::Ready(None);
        }
        if self.stopped.as_mut().poll(cx).is_ready() {
            self.ended = true;
            return Poll::Ready(Some(Err(Status::unavailable("transport stopped"))));
        }
        Pin::new(&mut self.inner).poll_frame(cx)
    }
    fn is_end_stream(&self) -> bool {
        self.ended || self.inner.is_end_stream()
    }
    fn size_hint(&self) -> SizeHint {
        self.inner.size_hint()
    }
}
async fn shutdown(mut receiver: watch::Receiver<bool>) {
    loop {
        if *receiver.borrow_and_update() {
            return;
        }
        if receiver.changed().await.is_err() {
            return;
        }
    }
}

#[derive(Clone)]
pub struct BearerInterceptor {
    authorization: Option<tonic::metadata::MetadataValue<tonic::metadata::Ascii>>,
}
impl Interceptor for BearerInterceptor {
    fn call(&mut self, mut request: tonic::Request<()>) -> Result<tonic::Request<()>, Status> {
        if let Some(authorization) = &self.authorization {
            request
                .metadata_mut()
                .insert("authorization", authorization.clone());
        }
        Ok(request)
    }
}

pub struct GrpcTransportClient {
    client: Option<RawClient>,
    diagnostics: Arc<FileDiagnosticLog>,
    connection_id: String,
}
impl GrpcTransportClient {
    /// Channel creation is deliberately lazy, including UDS: Attach may precede
    /// the local owner's listener becoming available.
    pub fn connect(
        address: &TransportAddress,
        token: Option<&TransportToken>,
        tls: Option<ClientTlsConfig>,
        diagnostics: Arc<FileDiagnosticLog>,
    ) -> TransportResult<Self> {
        let connection_id = FileDiagnosticLog::create_instance_id();
        let started = Instant::now();
        diagnostic(
            &diagnostics,
            "channel.open.start",
            &connection_id,
            None,
            None,
            None,
        );
        let result = Self::open_channel(address, token, tls);
        diagnostic(
            &diagnostics,
            "channel.open.complete",
            &connection_id,
            Some(started),
            Some(if result.is_ok() { "success" } else { "failure" }),
            result.as_ref().err().map(|_| "invalid_operation"),
        );
        Ok(Self {
            client: Some(result?),
            diagnostics,
            connection_id,
        })
    }

    fn open_channel(
        address: &TransportAddress,
        token: Option<&TransportToken>,
        tls: Option<ClientTlsConfig>,
    ) -> TransportResult<RawClient> {
        if address.is_tcp() && token.is_none() {
            return Err(invalid(
                "TCP transport requires an owner-only bearer token file",
            ));
        }
        let channel = match address {
            TransportAddress::Tcp(url) => {
                let mut endpoint = Endpoint::from_shared(url.to_string())?;
                if url.scheme() == "https" {
                    endpoint = endpoint.tls_config(
                        tls.unwrap_or_else(|| ClientTlsConfig::new().with_webpki_roots()),
                    )?;
                } else if tls.is_some() {
                    return Err(invalid(
                        "TLS configuration requires an HTTPS transport address",
                    ));
                }
                endpoint.connect_lazy()
            }
            #[cfg(unix)]
            TransportAddress::Unix(path) => {
                if tls.is_some() {
                    return Err(invalid(
                        "TLS configuration requires an HTTPS transport address",
                    ));
                }
                let path = path.clone();
                Endpoint::from_static("http://localhost").connect_with_connector_lazy(service_fn(
                    move |_| {
                        let path = path.clone();
                        async move {
                            tokio::net::UnixStream::connect(path)
                                .await
                                .map(TokioIo::new)
                        }
                    },
                ))
            }
            #[cfg(not(unix))]
            TransportAddress::Unix(_) => {
                return Err(invalid("Unix transport is not supported on this platform"));
            }
        };
        let interceptor = BearerInterceptor {
            authorization: token
                .map(|token| format!("Bearer {}", token.bearer()).parse())
                .transpose()?,
        };
        Ok(parrot_domain::protocol::bindings::parrot::parrot_client::ParrotClient::with_interceptor(channel, interceptor)
            .max_decoding_message_size(MESSAGE_BYTES)
            .max_encoding_message_size(MESSAGE_BYTES))
    }

    pub fn client(&mut self) -> TransportResult<&mut RawClient> {
        self.client
            .as_mut()
            .ok_or_else(|| invalid("transport client has been disposed"))
    }

    pub async fn attach(
        &mut self,
        request: wire::AttachSessionRequest,
        cancellation: &dyn Cancellation,
    ) -> Result<wire::UserSession, AttachError> {
        let attachment_id = FileDiagnosticLog::create_instance_id();
        let started = Instant::now();
        diagnostic(
            &self.diagnostics,
            "attach.start",
            &attachment_id,
            None,
            None,
            None,
        );
        let result = self.attach_inner(request, cancellation).await;
        let (outcome, error) = match &result {
            Ok(_) => ("success", None),
            Err(AttachError::Cancelled) => ("cancelled", Some("cancelled")),
            Err(AttachError::Timeout) => ("failure", Some("timeout")),
            Err(AttachError::DifferentSession) => ("failure", Some("invalid_operation")),
            Err(AttachError::Rpc(status)) => (
                if status.code() == tonic::Code::Cancelled {
                    "cancelled"
                } else {
                    "failure"
                },
                Some(rpc_code(status.code())),
            ),
            Err(AttachError::Disposed) => ("failure", Some("invalid_operation")),
        };
        diagnostic(
            &self.diagnostics,
            "attach.complete",
            &attachment_id,
            Some(started),
            Some(outcome),
            error,
        );
        result
    }

    async fn attach_inner(
        &mut self,
        request: wire::AttachSessionRequest,
        cancellation: &dyn Cancellation,
    ) -> Result<wire::UserSession, AttachError> {
        let client = self.client.as_mut().ok_or(AttachError::Disposed)?;
        let deadline = tokio::time::Instant::now() + Duration::from_secs(3);
        loop {
            if cancellation.is_cancelled() {
                return Err(AttachError::Cancelled);
            }
            let mut call = tonic::Request::new(request.clone());
            call.set_timeout(deadline.saturating_duration_since(tokio::time::Instant::now()));
            let result = tokio::select! {
                biased;
                _ = cancellation.cancelled() => return Err(AttachError::Cancelled),
                _ = tokio::time::sleep_until(deadline) => return Err(AttachError::Timeout),
                result = client.attach_session(call) => result,
            };
            match result {
                Ok(session) if session.get_ref().id == request.user_session_id => {
                    return Ok(session.into_inner());
                }
                Ok(_) => return Err(AttachError::DifferentSession),
                Err(error)
                    if matches!(
                        error.code(),
                        tonic::Code::Unavailable | tonic::Code::NotFound
                    ) =>
                {
                    tokio::select! {
                        biased;
                        _ = cancellation.cancelled() => return Err(AttachError::Cancelled),
                        _ = tokio::time::sleep_until(deadline) => return Err(AttachError::Timeout),
                        _ = tokio::time::sleep(Duration::from_millis(100)) => {}
                    }
                }
                Err(error) => return Err(AttachError::Rpc(error)),
            }
        }
    }

    pub fn close(&mut self) {
        if self.client.is_none() {
            return;
        }
        let started = Instant::now();
        diagnostic(
            &self.diagnostics,
            "disconnect.start",
            &self.connection_id,
            None,
            None,
            None,
        );
        self.client.take();
        diagnostic(
            &self.diagnostics,
            "disconnect.complete",
            &self.connection_id,
            Some(started),
            Some("success"),
            None,
        );
    }
}
impl Drop for GrpcTransportClient {
    fn drop(&mut self) {
        self.close();
    }
}

#[derive(Debug)]
pub enum AttachError {
    Cancelled,
    Timeout,
    DifferentSession,
    Disposed,
    Rpc(Status),
}
impl fmt::Display for AttachError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Cancelled => formatter.write_str("operation cancelled"),
            Self::Timeout => formatter
                .write_str("unable to attach to the existing user session within 3 seconds"),
            Self::DifferentSession => {
                formatter.write_str("the transport returned a different user session")
            }
            Self::Disposed => formatter.write_str("transport client has been disposed"),
            Self::Rpc(status) => status.fmt(formatter),
        }
    }
}
impl Error for AttachError {}
fn invalid(message: &'static str) -> Box<dyn Error + Send + Sync> {
    io::Error::new(io::ErrorKind::InvalidInput, message).into()
}
fn rpc_code(code: tonic::Code) -> &'static str {
    match code {
        tonic::Code::Ok => "OK",
        tonic::Code::Cancelled => "Cancelled",
        tonic::Code::Unknown => "Unknown",
        tonic::Code::InvalidArgument => "InvalidArgument",
        tonic::Code::DeadlineExceeded => "DeadlineExceeded",
        tonic::Code::NotFound => "NotFound",
        tonic::Code::AlreadyExists => "AlreadyExists",
        tonic::Code::PermissionDenied => "PermissionDenied",
        tonic::Code::ResourceExhausted => "ResourceExhausted",
        tonic::Code::FailedPrecondition => "FailedPrecondition",
        tonic::Code::Aborted => "Aborted",
        tonic::Code::OutOfRange => "OutOfRange",
        tonic::Code::Unimplemented => "Unimplemented",
        tonic::Code::Internal => "Internal",
        tonic::Code::Unavailable => "Unavailable",
        tonic::Code::DataLoss => "DataLoss",
        tonic::Code::Unauthenticated => "Unauthenticated",
    }
}
fn diagnostic(
    log: &FileDiagnosticLog,
    operation: &str,
    correlation: &str,
    started: Option<Instant>,
    outcome: Option<&str>,
    error: Option<&str>,
) {
    let mut entry = DiagnosticEvent::new(
        "transport",
        operation,
        if error.is_some() && outcome != Some("cancelled") {
            DiagnosticSeverity::Error
        } else {
            DiagnosticSeverity::Information
        },
    );
    entry.correlation_id = Some(correlation.to_owned());
    entry.duration_milliseconds =
        started.map(|started| i64::try_from(started.elapsed().as_millis()).unwrap_or(i64::MAX));
    entry.outcome = outcome.map(str::to_owned);
    entry.error_code = error.map(str::to_owned);
    log.write(&entry);
}
