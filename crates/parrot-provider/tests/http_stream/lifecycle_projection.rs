//! Source-identity adapters label typed Rust outcomes for comparison only.
//! Native CLR class names are not claimed as Rust runtime exception types.
use super::family_projection::source_error;
use parrot_domain::contracts::async_ops::{BoxFuture, Cancellation};
use parrot_provider::http_stream::*;
use serde_json::{Value, json};
use std::{
    future::pending,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    time::Duration,
};
use url::Url;

struct Never;
impl Cancellation for Never {
    fn is_cancelled(&self) -> bool {
        false
    }
    fn cancelled(&self) -> BoxFuture<'_, ()> {
        Box::pin(pending())
    }
}
struct Token(tokio::sync::watch::Sender<bool>);
impl Token {
    fn new() -> Self {
        Self(tokio::sync::watch::channel(false).0)
    }
    fn cancel(&self) {
        self.0.send_replace(true);
    }
}
impl Cancellation for Token {
    fn is_cancelled(&self) -> bool {
        *self.0.borrow()
    }
    fn cancelled(&self) -> BoxFuture<'_, ()> {
        Box::pin(async move {
            let mut receiver = self.0.subscribe();
            while !*receiver.borrow_and_update() {
                if receiver.changed().await.is_err() {
                    break;
                }
            }
        })
    }
}
struct DropMarker(Arc<AtomicBool>);
impl Drop for DropMarker {
    fn drop(&mut self) {
        self.0.store(true, Ordering::SeqCst);
    }
}
struct Body {
    bytes: Vec<u8>,
    position: Arc<AtomicUsize>,
    source: DropMarker,
    owner: DropMarker,
}
impl Body {
    fn new(bytes: Vec<u8>) -> Self {
        Self {
            bytes,
            position: Arc::default(),
            source: DropMarker(Arc::default()),
            owner: DropMarker(Arc::default()),
        }
    }
    fn markers(&self) -> (Arc<AtomicBool>, Arc<AtomicBool>) {
        (self.source.0.clone(), self.owner.0.clone())
    }
}
impl ByteStream for Body {
    fn read<'a>(
        &'a mut self,
        buffer: &'a mut [u8],
        cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<usize, HttpError>> {
        Box::pin(async move {
            if cancellation.is_cancelled() {
                return Err(HttpError::Cancelled);
            }
            let offset = self.position.load(Ordering::SeqCst);
            let count = buffer.len().min(self.bytes.len() - offset);
            buffer[..count].copy_from_slice(&self.bytes[offset..offset + count]);
            self.position.fetch_add(count, Ordering::SeqCst);
            Ok(count)
        })
    }
}
struct PendingBody(tokio::sync::mpsc::UnboundedReceiver<u8>, DropMarker);
impl ByteStream for PendingBody {
    fn read<'a>(
        &'a mut self,
        buffer: &'a mut [u8],
        cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<usize, HttpError>> {
        Box::pin(async move {
            if cancellation.is_cancelled() {
                return Err(HttpError::Cancelled);
            }
            if buffer.is_empty() {
                return Ok(0);
            }
            tokio::select! { biased;
                _ = cancellation.cancelled() => Err(HttpError::Cancelled),
                byte = self.0.recv() => match byte { Some(byte) => { buffer[0] = byte; Ok(1) }, None => Ok(0) },
            }
        })
    }
}
fn channel() -> (tokio::sync::mpsc::UnboundedSender<u8>, PendingBody) {
    let (sender, receiver) = tokio::sync::mpsc::unbounded_channel();
    (sender, PendingBody(receiver, DropMarker(Arc::default())))
}
struct Transport {
    body: Mutex<Option<Box<dyn ByteStream>>>,
    requests: Mutex<Vec<HttpRequest>>,
    status: u16,
    stall: bool,
    headers: Vec<(String, String)>,
}
impl Transport {
    fn new(body: impl ByteStream + 'static, status: u16) -> Self {
        Self {
            body: Mutex::new(Some(Box::new(body))),
            requests: Mutex::new(Vec::new()),
            status,
            stall: false,
            headers: vec![],
        }
    }
    fn calls(&self) -> usize {
        self.requests.lock().unwrap().len()
    }
}
impl HttpTransport for Transport {
    fn send<'a>(
        &'a self,
        request: HttpRequest,
        cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<HttpResponse, HttpError>> {
        Box::pin(async move {
            self.requests.lock().unwrap().push(request);
            if self.stall {
                cancellation.cancelled().await;
                return Err(HttpError::Cancelled);
            }
            Ok(HttpResponse {
                status: self.status,
                reason: reqwest::StatusCode::from_u16(self.status)
                    .unwrap()
                    .canonical_reason()
                    .unwrap_or_default()
                    .to_owned(),
                headers: self.headers.clone(),
                body: self.body.lock().unwrap().take().unwrap(),
            })
        })
    }
}
fn endpoint() -> Url {
    Url::parse("https://example.test/responses").unwrap()
}
fn options() -> StreamOptions {
    StreamOptions {
        header_timeout: Duration::ZERO,
        idle_timeout: Duration::ZERO,
        maximum_request_bytes: 10,
    }
}
fn error_value(error: Option<HttpError>) -> Value {
    error.as_ref().map_or(Value::Null, source_error)
}
pub async fn observe(case_id: &str, input: &Value) -> Option<Value> {
    if case_id.starts_with("idle.") {
        return Some(observe_idle(case_id).await);
    }
    if case_id.starts_with("provider-idle.") {
        return Some(observe_provider_idle(input).await);
    }
    if !case_id.starts_with("http.") {
        return None;
    }
    Some(observe_http(case_id, input).await)
}

async fn observe_idle(case_id: &str) -> Value {
    match case_id {
        "idle.first" => {
            let (_sender, body) = channel();
            let mut stream = IdleTimeoutStream::new(body, Duration::from_secs(300));
            json!({"error": source_error(&stream.read(&mut [0;1], &Never).await.unwrap_err())})
        }
        "idle.reset-probe" => {
            let (sender, body) = channel();
            let mut stream = BoundedStream::new(
                IdleTimeoutStream::new(body, Duration::from_secs(300)),
                3,
                ResponseByteCounter::default(),
            );
            let mut counts = Vec::new();
            for _ in 0..2 {
                let send = sender.clone();
                tokio::spawn(async move {
                    tokio::time::sleep(Duration::from_secs(240)).await;
                    send.send(b':').unwrap();
                });
                counts.push(stream.read(&mut [0; 1], &Never).await.unwrap());
            }
            sender.send(b'\n').unwrap();
            let error = stream.read(&mut [0; 1], &Never).await.unwrap_err();
            json!({"counts":counts,"error":source_error(&error)})
        }
        "idle.cancel.False" | "idle.cancel.True" => {
            let disabled = case_id.ends_with("True");
            let (_sender, body) = channel();
            let mut stream = IdleTimeoutStream::new(
                body,
                if disabled {
                    Duration::ZERO
                } else {
                    Duration::from_secs(300)
                },
            );
            let token = Arc::new(Token::new());
            let mut buffer = [0; 1];
            let read = stream.read(&mut buffer, &*token);
            tokio::pin!(read);
            let completed_before_cancel = std::future::poll_fn(|context| {
                std::task::Poll::Ready(std::future::Future::poll(read.as_mut(), context).is_ready())
            })
            .await;
            assert!(!completed_before_cancel);
            if disabled {
                tokio::time::advance(Duration::from_secs(86400)).await;
            }
            token.cancel();
            tokio::time::advance(Duration::from_secs(300)).await;
            let error = read.await.unwrap_err();
            assert_eq!(error, HttpError::Cancelled);
            // T07's typed cancellation carries the caller context, not a CLR
            // exception token object; this labels that source assertion seam.
            json!({"completed_before_cancel": completed_before_cancel, "caller_token_preserved": token.is_cancelled(), "error":{"category":"System.OperationCanceledException","message":"The operation was canceled."}})
        }
        "idle.empty-eof-dispose" => {
            let source = Body::new(vec![]);
            let (dropped, _) = source.markers();
            let mut stream = IdleTimeoutStream::new(source, Duration::from_secs(300));
            let empty = stream.read(&mut [], &Never).await.unwrap();
            let eof = stream.read(&mut [0; 1], &Never).await.unwrap();
            drop(stream);
            json!({"empty":empty,"eof":eof,"source_disposed":dropped.load(Ordering::SeqCst)})
        }
        _ => panic!("unknown independent idle input {case_id}"),
    }
}

async fn observe_provider_idle(input: &Value) -> Value {
    let timeout = input["timeout_milliseconds"].as_u64().unwrap();
    let (_sender, body) = channel();
    let disposed = body.1.0.clone();
    let transport = Arc::new(Transport::new(body, 200));
    let token = Arc::new(Token::new());
    if timeout == 0 {
        let caller = token.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(100)).await;
            caller.cancel();
        });
    }
    let mut stream = HttpEventStream::new(
        transport.clone(),
        endpoint(),
        vec![],
        vec![],
        StreamOptions {
            idle_timeout: Duration::from_millis(timeout),
            ..options()
        },
        ResponseByteCounter::default(),
    )
    .unwrap();
    let mut events = Vec::new();
    let error = loop {
        match stream.next(&*token).await {
            Ok(Some(HttpStreamEvent::RequestStarted)) => events.push("HttpRequestStarted"),
            Ok(Some(HttpStreamEvent::ResponseHeadersReceived)) => {
                events.push("HttpResponseHeadersReceived")
            }
            Ok(Some(HttpStreamEvent::Event(_))) => panic!("stalled body produced an event"),
            Ok(None) => panic!("stalled stream completed without cancellation"),
            Err(error) => break error,
        }
    };
    json!({"events":events,"error":source_error(&error),"response_bytes":stream.response_bytes(),"body_disposed":disposed.load(Ordering::SeqCst),"handler_calls":transport.calls()})
}

async fn observe_http(case_id: &str, input: &Value) -> Value {
    if case_id.starts_with("http.bytes.") {
        let is_error = input["error"].as_bool().unwrap();
        let body = Body::new(if input["oversized"].as_bool().unwrap() {
            "é".repeat(MAX_ERROR_BYTES).into_bytes()
        } else {
            "éé!".as_bytes().to_vec()
        });
        let (disposed, _) = body.markers();
        let transport = Transport::new(body, if is_error { 400 } else { 200 });
        let counter = ResponseByteCounter::default();
        let mut before = Value::Null;
        let mut count = Value::Null;
        let error = match open_stream(
            &transport,
            endpoint(),
            vec![],
            vec![],
            options(),
            counter.clone(),
            &Never,
        )
        .await
        {
            Ok(mut response) => {
                before = json!(counter.get());
                count = json!(response.content.read(&mut [0; 2], &Never).await.unwrap());
                None
            }
            Err(error) => Some(error),
        };
        return json!({"response_bytes_before_read":before,"read_count":count,"response_bytes":counter.get(),"error":error_value(error),"disposed":disposed.load(Ordering::SeqCst),"handler_calls":transport.calls()});
    }
    if case_id == "http.request" || case_id == "http.success" {
        let mut transport = Transport::new(Body::new(b"data: hi\n\n".to_vec()), 200);
        transport.headers = vec![("X-Codex-Turn-State".into(), "one, two".into())];
        let counter = ResponseByteCounter::default();
        let response = open_stream(
            &transport,
            endpoint(),
            br#"{"x":1}"#.to_vec(),
            vec![("X-Test".into(), "custom".into())],
            StreamOptions {
                maximum_request_bytes: 100,
                ..options()
            },
            counter.clone(),
            &Never,
        )
        .await
        .unwrap();
        if case_id == "http.request" {
            let requests = transport.requests.lock().unwrap();
            let request = &requests[0];
            let header = |name: &str| {
                request
                    .headers
                    .iter()
                    .find(|(key, _)| key.eq_ignore_ascii_case(name))
                    .unwrap()
                    .1
                    .clone()
            };
            let method = match request.method {
                HttpMethod::Get => "GET",
                HttpMethod::Post => "POST",
            };
            return json!({"method":method,"uri":request.endpoint.as_str(),"body_hex":hex_encode(&request.body),"content_type":header("Content-Type"),"accept":header("Accept"),"custom":[header("X-Test")]});
        }
        let headers: serde_json::Map<String, Value> = response
            .headers
            .into_iter()
            .map(|(key, value)| (key, json!(value)))
            .collect();
        let mut stream = SseStream::new(response.content, 100);
        let mut events = Vec::new();
        while let Some(event) = stream.next(&Never).await.unwrap() {
            events.push(event.data);
        }
        return json!({"headers":headers,"events":events,"response_bytes":counter.get()});
    }
    if case_id.starts_with("http.error.") {
        let transport = Transport::new(
            Body::new(input["body"].as_str().unwrap().as_bytes().to_vec()),
            429,
        );
        let counter = ResponseByteCounter::default();
        let error = open_stream(
            &transport,
            endpoint(),
            vec![],
            vec![],
            options(),
            counter.clone(),
            &Never,
        )
        .await
        .err()
        .unwrap();
        return json!({"error":source_error(&error),"response_bytes":counter.get()});
    }
    if case_id.starts_with("http.control.") {
        let mut transport = Transport::new(Body::new(vec![]), 200);
        transport.stall = true;
        let caller = Token::new();
        if case_id.ends_with("caller-cancel") {
            caller.cancel();
        }
        let mut options = options();
        if case_id.ends_with("request-too-large") {
            options.maximum_request_bytes = 1;
        }
        if case_id.ends_with("header-timeout") {
            options.header_timeout = Duration::from_millis(10);
        }
        let error = if case_id.ends_with("negative-idle") {
            StreamOptions::from_millis(0, -1, 10).unwrap_err()
        } else {
            open_stream(
                &transport,
                endpoint(),
                vec![1, 2],
                vec![],
                options,
                ResponseByteCounter::default(),
                &caller,
            )
            .await
            .err()
            .unwrap()
        };
        return json!({"error":source_error(&error),"handler_calls":transport.calls()});
    }
    if case_id.starts_with("http.get.") {
        let transport = Transport::new(
            Body::new(input["body"].as_str().unwrap().as_bytes().to_vec()),
            200,
        );
        return match get(
            &transport,
            endpoint(),
            vec![],
            Duration::ZERO,
            input["max_bytes"].as_u64().unwrap() as usize,
            &Never,
        )
        .await
        {
            Ok(result) => json!({"result":result,"error":null}),
            Err(error) => json!({"result":null,"error":source_error(&error)}),
        };
    }
    if case_id == "http.wrapper-bound" {
        let body = Body::new(vec![1, 2, 3, 4]);
        let (source, owner) = body.markers();
        let counter = ResponseByteCounter::default();
        let mut stream = BoundedStream::new(
            IdleTimeoutStream::new(body, Duration::ZERO),
            input["limit"].as_i64().unwrap(),
            counter.clone(),
        );
        let error = stream.read(&mut [0; 8], &Never).await.unwrap_err();
        drop(stream);
        return json!({"error":source_error(&error),"response_bytes":counter.get(),"source_disposed":source.load(Ordering::SeqCst),"owner_disposed":owner.load(Ordering::SeqCst)});
    }
    panic!("unknown independent HTTP input {case_id}")
}

fn hex_encode(bytes: &[u8]) -> String {
    use std::fmt::Write;
    bytes.iter().fold(String::new(), |mut output, byte| {
        write!(output, "{byte:02X}").unwrap();
        output
    })
}
