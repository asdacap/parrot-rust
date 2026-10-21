//! Loopback HTTP/1.1 hosting for binary and text gRPC-Web clients.
//!
//! This transport borrows its services. The caller retains their construction
//! Runtime and closes them only after `close().await` joins the transport.

use crate::{
    grpc::{MESSAGE_BYTES, transport::TransportResult},
    web_assets::EMBEDDED_WEB_ASSETS,
};
use base64::{Engine, engine::general_purpose::STANDARD};
use bytes::{BufMut, Bytes, BytesMut};
use http::{HeaderMap, Method, Request, Response, StatusCode, Version, header};
use http_body_util::{BodyExt, Full, combinators::UnsyncBoxBody};
use hyper::{
    body::{Body, Frame, Incoming, SizeHint},
    server::conn::http1,
    service::service_fn,
};
use hyper_util::rt::TokioIo;
use parrot_domain::protocol::bindings::{
    parrot::parrot_server::{Parrot, ParrotServer},
    web::parrot_web_server::{ParrotWeb, ParrotWebServer},
};
use std::{
    convert::Infallible,
    io,
    net::{Ipv4Addr, SocketAddr},
    pin::Pin,
    task::{Context, Poll},
};
use tokio::{
    net::TcpListener,
    sync::watch,
    task::{JoinHandle, JoinSet},
};
use tonic::{Status, body::BoxBody};
use tower::ServiceExt;

type WebBody = UnsyncBoxBody<Bytes, Infallible>;

/// Owns the listener and HTTP connections, never the borrowed service lifetime.
pub struct WebServer {
    address: SocketAddr,
    stopped: watch::Sender<bool>,
    task: Option<JoinHandle<TransportResult<()>>>,
    result: Option<Result<(), String>>,
}

impl WebServer {
    pub async fn start<S: Parrot, W: ParrotWeb>(
        service: S,
        web_service: W,
        port: u16,
    ) -> TransportResult<Self> {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, port))
            .await
            .map_err(|failure| {
                io::Error::other(format!("cannot start the web UI on port {port}: {failure}"))
            })?;
        let address = listener.local_addr()?;
        let service = ParrotServer::new(service)
            .max_decoding_message_size(MESSAGE_BYTES)
            .max_encoding_message_size(MESSAGE_BYTES);
        let web_service = ParrotWebServer::new(web_service)
            .max_decoding_message_size(MESSAGE_BYTES)
            .max_encoding_message_size(MESSAGE_BYTES);
        let (stopped, receiver) = watch::channel(false);
        let task = tokio::spawn(async move {
            let mut connections = JoinSet::new();
            let mut stopped = receiver.clone();
            loop {
                tokio::select! {
                    biased;
                    _ = shutdown(&mut stopped) => break,
                    accepted = listener.accept() => {
                        let (stream, _) = accepted?;
                        let service = service.clone();
                        let web_service = web_service.clone();
                        let mut stopped = receiver.clone();
                        connections.spawn(async move {
                            let connection = http1::Builder::new().serve_connection(
                                TokioIo::new(stream),
                                service_fn(move |request| serve(request, service.clone(), web_service.clone())),
                            );
                            tokio::select! {
                                biased;
                                _ = shutdown(&mut stopped) => {},
                                _ = connection => {},
                            }
                        });
                    }
                    Some(joined) = connections.join_next(), if !connections.is_empty() => {
                        joined.map_err(io::Error::other)?;
                    }
                }
            }
            drop(listener);
            while let Some(joined) = connections.join_next().await {
                joined.map_err(io::Error::other)?;
            }
            Ok(())
        });
        Ok(Self {
            address,
            stopped,
            task: Some(task),
            result: None,
        })
    }

    pub fn address(&self) -> SocketAddr {
        self.address
    }

    /// Requests shutdown and joins every connection, including unread responses.
    /// Cancelling this observer retains the actual join in this server owner.
    pub async fn close(&mut self) -> TransportResult<()> {
        if self.result.is_none() {
            self.stopped.send_replace(true);
            let result = match self.task.as_mut() {
                Some(task) => match task.await {
                    Ok(result) => result.map_err(|failure| failure.to_string()),
                    Err(failure) => Err(failure.to_string()),
                },
                None => Ok(()),
            };
            self.task.take();
            self.result = Some(result);
        }
        self.result
            .as_ref()
            .expect("close result was recorded")
            .clone()
            .map_err(|failure| io::Error::other(failure).into())
    }
}

impl Drop for WebServer {
    fn drop(&mut self) {
        self.stopped.send_replace(true);
    }
}

async fn shutdown(receiver: &mut watch::Receiver<bool>) {
    while !*receiver.borrow_and_update() {
        if receiver.changed().await.is_err() {
            return;
        }
    }
}

async fn serve<S: Parrot, W: ParrotWeb>(
    mut request: Request<Incoming>,
    service: ParrotServer<S>,
    web_service: ParrotWebServer<W>,
) -> Result<Response<WebBody>, Infallible> {
    if request.method() == Method::GET {
        let path = request.uri().path().trim_start_matches('/');
        let asset = EMBEDDED_WEB_ASSETS
            .iter()
            .find(|asset| asset.path == path)
            .or_else(|| {
                EMBEDDED_WEB_ASSETS
                    .iter()
                    .find(|asset| asset.path == "index.html")
            });
        return Ok(match asset {
            Some(asset) => Response::builder()
                .header(header::CONTENT_TYPE, asset.content_type)
                .body(full(Bytes::from_static(asset.bytes)))
                .expect("fixed embedded asset response"),
            None => plain(
                StatusCode::INTERNAL_SERVER_ERROR,
                "the web UI was not embedded in this build",
            ),
        });
    }
    if request.method() != Method::POST {
        return Ok(plain(StatusCode::METHOD_NOT_ALLOWED, "method not allowed"));
    }
    let content_type = request
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .unwrap_or("")
        .split(';')
        .next()
        .unwrap_or("")
        .trim();
    let text = matches!(
        content_type,
        "application/grpc-web-text" | "application/grpc-web-text+proto"
    );
    if !text
        && !matches!(
            content_type,
            "application/grpc-web" | "application/grpc-web+proto"
        )
    {
        return Ok(plain(
            StatusCode::UNSUPPORTED_MEDIA_TYPE,
            "gRPC-Web is required",
        ));
    }
    let parrot = request.uri().path().starts_with("/parrot.v1.Parrot/");
    let web = request
        .uri()
        .path()
        .starts_with("/parrot.web.v1.ParrotWeb/");
    if !parrot && !web {
        return Ok(plain(StatusCode::NOT_FOUND, "not found"));
    }
    // Binary gRPC-Web request message envelopes are native gRPC envelopes.
    *request.version_mut() = Version::HTTP_2;
    request.headers_mut().insert(
        header::CONTENT_TYPE,
        header::HeaderValue::from_static("application/grpc"),
    );
    request
        .headers_mut()
        .insert(header::TE, header::HeaderValue::from_static("trailers"));
    let request = request.map(|body| {
        if text {
            TextRequestBody {
                inner: Some(body),
                carry: Vec::new(),
            }
            .boxed_unsync()
        } else {
            body.map_err(|failure| Status::internal(failure.to_string()))
                .boxed_unsync()
        }
    });
    let response = if parrot {
        service.oneshot(request).await
    } else {
        web_service.oneshot(request).await
    };
    let response = match response {
        Ok(response) => response,
        Err(never) => match never {},
    };
    let (mut parts, body) = response.into_parts();
    let initial = if parts.headers.contains_key("grpc-status") {
        let mut trailers = HeaderMap::new();
        for name in ["grpc-status", "grpc-message", "grpc-status-details-bin"] {
            if let Some(value) = parts.headers.remove(name) {
                trailers.insert(name, value);
            }
        }
        Some(trailers)
    } else {
        None
    };
    parts.version = Version::HTTP_11;
    parts.headers.remove(header::CONTENT_LENGTH);
    parts.headers.insert(
        header::CONTENT_TYPE,
        header::HeaderValue::from_static(if text {
            "application/grpc-web-text+proto"
        } else {
            "application/grpc-web+proto"
        }),
    );
    let body = GrpcWebBody {
        inner: Some(body),
        initial,
        ended: false,
    }
    .boxed_unsync();
    let body = if text {
        TextResponseBody { inner: body }.boxed_unsync()
    } else {
        body
    };
    Ok(Response::from_parts(parts, body))
}

fn full(bytes: Bytes) -> WebBody {
    Full::new(bytes)
        .map_err(|never| match never {})
        .boxed_unsync()
}

fn plain(status: StatusCode, message: &'static str) -> Response<WebBody> {
    Response::builder()
        .status(status)
        .header(header::CONTENT_TYPE, "text/plain; charset=utf-8")
        .body(full(Bytes::from_static(message.as_bytes())))
        .expect("fixed HTTP response")
}

struct TextRequestBody {
    inner: Option<Incoming>,
    carry: Vec<u8>,
}

impl TextRequestBody {
    fn decode(&mut self, bytes: &[u8]) -> Result<Bytes, Status> {
        let mut input = std::mem::take(&mut self.carry);
        input.extend_from_slice(bytes);
        let complete = input.len() / 4 * 4;
        self.carry.extend_from_slice(&input[complete..]);
        let mut decoded = Vec::with_capacity(complete / 4 * 3);
        let mut start = 0;
        while start < complete {
            // Flushes may insert padding before the end of a text stream.
            let end = input[start..complete]
                .iter()
                .position(|byte| *byte == b'=')
                .map_or(complete, |index| (start + index) / 4 * 4 + 4);
            decoded.extend_from_slice(
                &STANDARD
                    .decode(&input[start..end])
                    .map_err(|_| Status::invalid_argument("invalid gRPC-Web base64 request"))?,
            );
            start = end;
        }
        Ok(Bytes::from(decoded))
    }

    fn fail(&mut self, status: Status) -> Poll<Option<Result<Frame<Bytes>, Status>>> {
        self.inner.take();
        self.carry.clear();
        Poll::Ready(Some(Err(status)))
    }
}

impl Body for TextRequestBody {
    type Data = Bytes;
    type Error = Status;

    fn poll_frame(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Bytes>, Status>>> {
        loop {
            let Some(inner) = self.inner.as_mut() else {
                return Poll::Ready(None);
            };
            match Pin::new(inner).poll_frame(cx) {
                Poll::Pending => return Poll::Pending,
                Poll::Ready(Some(Ok(frame))) => match frame.into_data() {
                    Ok(data) => match self.decode(&data) {
                        Ok(decoded) if decoded.is_empty() => continue,
                        Ok(decoded) => return Poll::Ready(Some(Ok(Frame::data(decoded)))),
                        Err(status) => return self.fail(status),
                    },
                    Err(frame) => {
                        if !self.carry.is_empty() {
                            return self.fail(Status::invalid_argument(
                                "incomplete gRPC-Web base64 request",
                            ));
                        }
                        return Poll::Ready(Some(Ok(frame)));
                    }
                },
                Poll::Ready(Some(Err(failure))) => {
                    return self.fail(Status::internal(failure.to_string()));
                }
                Poll::Ready(None) => {
                    self.inner.take();
                    if !self.carry.is_empty() {
                        return self.fail(Status::invalid_argument(
                            "incomplete gRPC-Web base64 request",
                        ));
                    }
                    return Poll::Ready(None);
                }
            }
        }
    }

    fn is_end_stream(&self) -> bool {
        self.inner.is_none()
    }
}

struct TextResponseBody {
    inner: WebBody,
}

impl Body for TextResponseBody {
    type Data = Bytes;
    type Error = Infallible;

    fn poll_frame(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Bytes>, Infallible>>> {
        match Pin::new(&mut self.inner).poll_frame(cx) {
            Poll::Ready(Some(Ok(frame))) => Poll::Ready(Some(Ok(match frame.into_data() {
                // Flush each source frame, so a complete message is visible even
                // when the next event will not arrive. Midstream padding is valid.
                Ok(data) => Frame::data(Bytes::from(STANDARD.encode(data))),
                Err(frame) => frame,
            }))),
            other => other,
        }
    }

    fn is_end_stream(&self) -> bool {
        self.inner.is_end_stream()
    }
}

struct GrpcWebBody {
    inner: Option<BoxBody>,
    initial: Option<HeaderMap>,
    ended: bool,
}

impl GrpcWebBody {
    fn finish(&mut self, trailers: HeaderMap) -> Poll<Option<Result<Frame<Bytes>, Infallible>>> {
        self.ended = true;
        self.inner.take();
        Poll::Ready(Some(Ok(Frame::data(trailer_frame(trailers)))))
    }
}

impl Body for GrpcWebBody {
    type Data = Bytes;
    type Error = Infallible;

    fn poll_frame(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Bytes>, Infallible>>> {
        if self.ended {
            return Poll::Ready(None);
        }
        if let Some(initial) = self.initial.take() {
            return self.finish(initial);
        }
        let result =
            Pin::new(self.inner.as_mut().expect("unfinished body retains source")).poll_frame(cx);
        match result {
            Poll::Pending => Poll::Pending,
            Poll::Ready(Some(Ok(frame))) => match frame.into_trailers() {
                Ok(trailers) => self.finish(trailers),
                Err(frame) => Poll::Ready(Some(Ok(frame))),
            },
            Poll::Ready(Some(Err(status))) => self.finish(status.into_http().headers().clone()),
            Poll::Ready(None) => self.finish(HeaderMap::new()),
        }
    }

    fn is_end_stream(&self) -> bool {
        self.ended
    }

    fn size_hint(&self) -> SizeHint {
        SizeHint::default()
    }
}

fn trailer_frame(mut trailers: HeaderMap) -> Bytes {
    if !trailers.contains_key("grpc-status") {
        trailers.insert("grpc-status", header::HeaderValue::from_static("0"));
    }
    let mut payload = BytesMut::new();
    for (name, value) in &trailers {
        payload.extend_from_slice(name.as_str().as_bytes());
        payload.extend_from_slice(b": ");
        payload.extend_from_slice(value.as_bytes());
        payload.extend_from_slice(b"\r\n");
    }
    let mut frame = BytesMut::with_capacity(5 + payload.len());
    frame.put_u8(0x80);
    frame.put_u32(u32::try_from(payload.len()).expect("HTTP trailers fit in a gRPC-Web frame"));
    frame.extend_from_slice(&payload);
    frame.freeze()
}
