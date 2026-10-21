//! Input-only observations of actual production image providers and decorators.
use parrot_domain::contracts::async_ops::{
    BoxFuture, Cancellation, OperationError, OperationResult,
};
use parrot_foundation::{
    credentials::Credential,
    oauth::{AuthError, CredentialStorage, StoredApiKeySource},
};
use parrot_provider::{
    catalogs::Model,
    http_stream::{
        ByteStream, Endpoint, HttpError, HttpFailure, HttpRequest, HttpResponse, HttpTransport,
    },
    image_generation::{
        ImageGenerationReference, ImageGenerationRequest, ImageGenerationResult, ImageHttpRequest,
        ImageTransport,
        bucket::{
            BucketConfig, Clock, CredentialSource, ImageBucket, S3ImageBucket, StoredCredential,
        },
        linking::ImageLinkingProvider,
        signer::{AwsV4Signer, sha256},
    },
    responses::ResponsesError,
    responses_websocket::{ConnectedSocket, Connector, Headers as SocketHeaders, WebSocketError},
    sessions::{
        Completion, Content, Error, Event, EventStream, Message, Provider, Request, Session,
        decorators::{RetryingProvider, SubscriptionUsage, UsageReporter, UsageReportingProvider},
        provider::{
            ApiKeySource, ChatGptOptions, ChatGptProvider, CompatibleOptions, OAuthAccess,
            OAuthTokenSource, OpenAICompatibleProvider,
        },
    },
};
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, VecDeque},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    time::Duration,
};
use tokio::sync::Notify;
#[path = "review_projection.rs"]
mod review_projection;
const COMMIT: &str = "5f27a8a92625ee31d8bec094e5c3e09550193781";
const PNG: &str = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR4nGP4z8DwHwAFAAH/iZk9HQAAAABJRU5ErkJggg==";
const JPEG: &str = "FFD8FFE000104A46494600010100000100010000FFDB004300100B0C0E0C0A100E0D0E1211101318281A181616183123251D283A333D3C3933383740485C4E404457453738506D51575F626768673E4D71797064785C656763FFC0000B080002000301011100FFC40014000100000000000000000000000000000000FFC40014100100000000000000000000000000000000FFDA0008010100003F003FFFD9";
const WEBP: &str = "524946461C000000574542505650384C0F0000002F024000000710FD8FFE0722A2FF0100";
fn s<'a>(c: &'a Value, key: &str) -> &'a str {
    c.get(key).and_then(Value::as_str).unwrap_or("")
}
fn n(c: &Value, key: &str) -> usize {
    c.get(key).and_then(Value::as_u64).unwrap_or(0) as usize
}
fn b(c: &Value, key: &str) -> bool {
    c.get(key).and_then(Value::as_bool).unwrap_or(false)
}
fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write;
    bytes.iter().fold(String::new(), |mut output, value| {
        write!(output, "{value:02x}").expect("writing to String cannot fail");
        output
    })
}
fn unhex(text: &str) -> Vec<u8> {
    (0..text.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&text[i..i + 2], 16).unwrap())
        .collect()
}
fn base64(bytes: &[u8]) -> String {
    parrot_provider::image_inspection::image_reference::<()>(None, "", || Ok(bytes.to_vec()))
        .unwrap()
        .split_once(',')
        .unwrap()
        .1
        .to_owned()
}
fn png() -> Vec<u8> {
    unhex(
        "89504e470d0a1a0a0000000d49484452000000010000000108060000001f15c4890000000d49444154789c63f8cfc0f01f00050001ff89993d1d0000000049454e44ae426082",
    )
}
#[derive(Default)]
struct Token {
    value: AtomicBool,
    changed: Notify,
}
impl Token {
    fn cancel(&self) {
        self.value.store(true, Ordering::Release);
        self.changed.notify_waiters();
    }
}
impl Cancellation for Token {
    fn is_cancelled(&self) -> bool {
        self.value.load(Ordering::Acquire)
    }
    fn cancelled(&self) -> BoxFuture<'_, ()> {
        Box::pin(async move {
            loop {
                let notified = self.changed.notified();
                tokio::pin!(notified);
                notified.as_mut().enable();
                if self.is_cancelled() {
                    return;
                }
                notified.await;
            }
        })
    }
}
fn native(error: &Error) -> Value {
    json!({"rust_type":std::any::type_name::<Error>(),"variant":format!("{error:?}"),"message":error.to_string()})
}
fn semantic(error: &Error) -> Value {
    if matches!(
        error,
        Error::Cancelled | Error::Cancellation(_) | Error::Http(HttpError::Cancelled)
    ) {
        return json!({"classification":"cancellation"});
    }
    let kind = error_category(error);
    let (inner_type, inner_message) = if let Error::ProviderWithCause { cause, .. } = error {
        (json!(error_category(cause)), json!(cause.to_string()))
    } else {
        (Value::Null, Value::Null)
    };
    json!({"type":kind,"message":error.to_string(),"inner_type":inner_type,"inner_message":inner_message})
}
fn error_category(error: &Error) -> &'static str {
    match error {
        Error::Provider(_) | Error::ProviderWithCause { .. } => "LLMProviderException",
        Error::Authentication(_) => "AuthException",
        Error::Cancelled | Error::Cancellation(_) | Error::Http(HttpError::Cancelled) => {
            "OperationCanceledException"
        }
        Error::HeaderTimeout(_) => "HeaderTimeoutException",
        Error::Wire(_) | Error::Http(HttpError::Wire(_)) => "WireProtocolException",
        Error::Http(HttpError::Response(_)) => "ProviderHttpException",
        Error::Response(ResponsesError::ProviderResponse { .. }) => "ProviderResponseException",
        _ => "UNMAPPED_RUST_ERROR",
    }
}
#[derive(Default)]
struct Errors {
    raw: Vec<Value>,
    selected: Vec<Value>,
}
impl Errors {
    fn push(&mut self, error: Option<&Error>) {
        self.raw.push(error.map(native).unwrap_or(Value::Null));
        self.selected
            .push(error.map(semantic).unwrap_or(Value::Null));
    }
}
#[derive(Clone)]
struct Capture {
    method: String,
    url: String,
    headers: BTreeMap<String, String>,
    content_type: String,
    body: Vec<u8>,
}
impl Capture {
    fn from(request: ImageHttpRequest) -> Self {
        let mut headers = BTreeMap::<String, String>::new();
        for (k, v) in request.headers {
            headers
                .entry(k.to_ascii_lowercase())
                .and_modify(|old| {
                    old.push(',');
                    old.push_str(&v)
                })
                .or_insert(v);
        }
        Self {
            method: request.method,
            url: request.endpoint.as_str().to_owned(),
            headers,
            content_type: request.content_type.unwrap_or_default(),
            body: request.body,
        }
    }
    fn generation(&self) -> Value {
        json!({"method":self.method,"url":self.url,"headers":self.headers,"content_type":self.content_type,"body_raw":String::from_utf8(self.body.clone()).unwrap()})
    }
    fn upload(&self) -> Value {
        json!({"method":self.method,"url":self.url,"headers":self.headers,"content_type":self.content_type,"body_hex":hex(&self.body)})
    }
}
#[derive(Default)]
struct Counters {
    disposed: AtomicUsize,
    read: AtomicUsize,
}
struct Body {
    bytes: Vec<u8>,
    at: usize,
    spaces: usize,
    counters: Arc<Counters>,
}
impl Drop for Body {
    fn drop(&mut self) {
        self.counters.disposed.fetch_add(1, Ordering::SeqCst);
    }
}
impl ByteStream for Body {
    fn read<'a>(
        &'a mut self,
        out: &'a mut [u8],
        ct: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<usize, HttpError>> {
        Box::pin(async move {
            if ct.is_cancelled() {
                return Err(HttpError::Cancelled);
            }
            let count = if self.spaces > 0 {
                let count = out.len().min(self.spaces);
                out[..count].fill(b' ');
                self.spaces -= count;
                count
            } else {
                let count = out.len().min(self.bytes.len() - self.at);
                out[..count].copy_from_slice(&self.bytes[self.at..self.at + count]);
                self.at += count;
                count
            };
            self.counters.read.fetch_add(count, Ordering::SeqCst);
            Ok(count)
        })
    }
}
struct Transport {
    c: Value,
    kind: &'static str,
    requests: Mutex<Vec<Capture>>,
    counters: Arc<Counters>,
    cancel: Arc<Token>,
    release: Arc<Notify>,
    head_waiting: AtomicBool,
    uploaded: Arc<Notify>,
}
impl Transport {
    fn new(c: &Value, kind: &'static str, cancel: Arc<Token>) -> Self {
        Self {
            c: c.clone(),
            kind,
            requests: Mutex::new(vec![]),
            counters: Arc::new(Counters::default()),
            cancel,
            release: Arc::new(Notify::new()),
            head_waiting: AtomicBool::new(false),
            uploaded: Arc::new(Notify::new()),
        }
    }
    fn upload_records(&self) -> Value {
        json!(
            self.requests
                .lock()
                .unwrap()
                .iter()
                .map(Capture::upload)
                .collect::<Vec<_>>()
        )
    }
}
impl ImageTransport for Transport {
    fn send<'a>(
        &'a self,
        request: ImageHttpRequest,
        ct: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<HttpResponse, Error>> {
        Box::pin(async move {
            let method = request.method.clone();
            let path = request.endpoint.as_url().path().to_owned();
            self.requests.lock().unwrap().push(Capture::from(request));
            let (status, body, length, spaces) = if self.kind == "generation" {
                if s(&self.c, "cancel") == "during-send" {
                    self.cancel.cancel();
                    return Err(Error::Cancelled);
                }
                let delay = n(&self.c, "header_delay_ms");
                if delay > 0 {
                    tokio::select! {_ = ct.cancelled()=>return Err(Error::Cancelled),_ = tokio::time::sleep(Duration::from_millis(delay as u64))=>{}}
                }
                let bytes = response_body(&self.c).into_bytes();
                let spaces = if s(&self.c, "boundary") == "stream-overflow" {
                    (64 << 20) + 1
                } else {
                    0
                };
                (
                    n(&self.c, "status") as u16,
                    bytes,
                    if spaces > 0 {
                        None
                    } else {
                        self.c
                            .get("advertised_length")
                            .and_then(Value::as_u64)
                            .or_else(|| Some(response_body(&self.c).len() as u64))
                    },
                    spaces,
                )
            } else if self.kind == "bucket" {
                if ct.is_cancelled() {
                    return Err(Error::Cancelled);
                }
                if s(&self.c, "mode") == "timeout" {
                    return Err(Error::Cancellation(
                        "synthetic dispatch cancellation".into(),
                    ));
                }
                let configured = n(&self.c, "status") as u16;
                if configured == 0 {
                    return Err(Error::Http(HttpError::Transport("unreachable".into())));
                }
                let exists = self
                    .c
                    .get("exists")
                    .and_then(Value::as_bool)
                    .unwrap_or(true);
                let status = if method == "HEAD"
                    && path == format!("/{}", s(&self.c, "bucket"))
                    && !exists
                {
                    404
                } else {
                    configured
                };
                (
                    status,
                    if status >= 400 {
                        b"denied".to_vec()
                    } else {
                        vec![]
                    },
                    None,
                    0,
                )
            } else {
                if s(&self.c, "mode") == "cancel-before" && method == "HEAD" {
                    self.head_waiting.store(true, Ordering::Release);
                    self.release.notified().await;
                }
                if method == "PUT" {
                    self.uploaded.notify_one();
                }
                (
                    if s(&self.c, "mode") == "missing" && method == "HEAD" && path.ends_with(".png")
                    {
                        404
                    } else {
                        200
                    },
                    vec![],
                    None,
                    0,
                )
            };
            Ok(HttpResponse {
                status,
                reason: String::new(),
                headers: length
                    .map(|n| vec![("Content-Length".into(), n.to_string())])
                    .unwrap_or_default(),
                body: Box::new(Body {
                    bytes: body,
                    at: 0,
                    spaces,
                    counters: self.counters.clone(),
                }),
            })
        })
    }
}
struct NoHttp;
impl HttpTransport for NoHttp {
    fn send<'a>(
        &'a self,
        _: HttpRequest,
        _: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<HttpResponse, HttpError>> {
        Box::pin(async { panic!("generation must not invoke streaming HTTP") })
    }
}
struct NoSocket;
impl Connector for NoSocket {
    fn connect<'a>(
        &'a self,
        _: &'a str,
        _: &'a SocketHeaders,
        _: Option<Duration>,
        _: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<ConnectedSocket, WebSocketError>> {
        Box::pin(async { panic!("image generation must not connect a socket") })
    }
}
#[derive(Default)]
struct Keys {
    reads: AtomicUsize,
    missing: bool,
}
impl ApiKeySource for Keys {
    fn api_key<'a>(&'a self, _: &'a dyn Cancellation) -> BoxFuture<'a, Result<String, Error>> {
        Box::pin(async move {
            let count = self.reads.fetch_add(1, Ordering::SeqCst) + 1;
            Ok(if self.missing {
                String::new()
            } else {
                format!("key-{count}")
            })
        })
    }
    fn has_credential<'a>(&'a self, _: &'a dyn Cancellation) -> BoxFuture<'a, Result<bool, Error>> {
        Box::pin(async move { Ok(!self.missing) })
    }
}
struct Tokens {
    keys: Arc<Keys>,
    calls: AtomicUsize,
}
impl OAuthTokenSource for Tokens {
    fn token<'a>(&'a self, ct: &'a dyn Cancellation) -> BoxFuture<'a, Result<OAuthAccess, Error>> {
        Box::pin(async move {
            let n = self.calls.fetch_add(1, Ordering::SeqCst) + 1;
            Ok(OAuthAccess {
                access_token: self.keys.api_key(ct).await?,
                account_id: format!("account-{n}"),
            })
        })
    }
    fn has_credential<'a>(&'a self, _: &'a dyn Cancellation) -> BoxFuture<'a, Result<bool, Error>> {
        Box::pin(async move { Ok(!self.keys.missing) })
    }
}
struct UsageBoundary;
impl UsageReporter for UsageBoundary {
    fn usage<'a>(
        &'a self,
        _: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<SubscriptionUsage, Error>> {
        Box::pin(async { panic!("quota selection/HTTP is not part of the generation fixture") })
    }
}
#[derive(Default)]
struct KeyStore {
    reads: AtomicUsize,
    value: Mutex<Option<Credential>>,
}
impl CredentialStorage for KeyStore {
    fn get<'a>(
        &'a self,
        _: &'a str,
        ct: &'a dyn Cancellation,
    ) -> BoxFuture<'a, OperationResult<Option<Credential>, AuthError>> {
        Box::pin(async move {
            self.reads.fetch_add(1, Ordering::SeqCst);
            if ct.is_cancelled() {
                Err(OperationError::Cancelled)
            } else {
                Ok(self.value.lock().unwrap().clone())
            }
        })
    }
    fn set<'a>(
        &'a self,
        _: &'a str,
        value: &'a Credential,
        _: &'a dyn Cancellation,
    ) -> BoxFuture<'a, OperationResult<(), AuthError>> {
        Box::pin(async move {
            *self.value.lock().unwrap() = Some(value.clone());
            Ok(())
        })
    }
}
struct StoredKeys(StoredApiKeySource);
impl ApiKeySource for StoredKeys {
    fn api_key<'a>(&'a self, ct: &'a dyn Cancellation) -> BoxFuture<'a, Result<String, Error>> {
        Box::pin(async move {
            self.0
                .api_key(ct)
                .await
                .map(|key| key.value().to_owned())
                .map_err(|error| match error {
                    OperationError::Cancelled => Error::Cancelled,
                    OperationError::Failed(error) => Error::Provider(error.to_string()),
                })
        })
    }
    fn has_credential<'a>(
        &'a self,
        ct: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<bool, Error>> {
        Box::pin(async move {
            self.0
                .has_credential(ct)
                .await
                .map_err(|error| Error::Provider(format!("{error:?}")))
        })
    }
}
fn response_body(c: &Value) -> String {
    let one = |encoded: &str| format!("{{\"data\":[{{\"b64_json\":\"{encoded}\"}}]}}");
    match s(c, "boundary") {
        "duplicate-data-last-success" => {
            format!("{{\"data\":[],\"data\":[{{\"b64_json\":\"{PNG}\"}}]}}")
        }
        "duplicate-data-last-empty" => {
            format!("{{\"data\":[{{\"b64_json\":\"{PNG}\"}}],\"data\":[]}}")
        }
        "duplicate-base64-last-success" => {
            format!("{{\"data\":[{{\"b64_json\":\"\",\"b64_json\":\"{PNG}\"}}]}}")
        }
        "duplicate-base64-last-empty" => {
            format!("{{\"data\":[{{\"b64_json\":\"{PNG}\",\"b64_json\":\"\"}}]}}")
        }
        "base64-whitespace" => one(&format!("{} \\r\\n\\t{}", &PNG[..20], &PNG[20..])),
        "malformed-json-type" => "{\"data\":123}".into(),
        "oversized-encoded" => one(&"A".repeat((32usize << 20).div_ceil(3) * 4 + 1)),
        "oversized-decoded" => one(&base64(&vec![0; (32 << 20) + 1])),
        _ => match s(c, "response_kind") {
            "success" => one(PNG),
            "sensitive" => format!("request secret key-1 data:image/png;base64,{PNG}"),
            "invalid-json" => "not JSON".into(),
            "null" => "null".into(),
            "empty-object" => "{}".into(),
            "empty-data" => "{\"data\":[]}".into(),
            "null-image" => "{\"data\":[null]}".into(),
            "empty-image" => "{\"data\":[{}]}".into(),
            "empty-base64" => one(""),
            "bad-base64" => one("%%%"),
            "not-image" => one("aGVsbG8="),
            "url-only" => {
                "{\"data\":[{\"url\":\"https://example.test/not-downloaded.png\"}]}".into()
            }
            "two-images" => {
                format!("{{\"data\":[{{\"b64_json\":\"{PNG}\"}},{{\"b64_json\":\"{PNG}\"}}]}}")
            }
            "jpeg" => one(&base64(&unhex(JPEG))),
            other => panic!("unknown response fixture {other}"),
        },
    }
}
fn generation_request(c: &Value) -> ImageGenerationRequest {
    let reference = |media: &str, data: Vec<u8>| ImageGenerationReference {
        media_type: media.into(),
        data,
    };
    let png_ref = || reference("image/png", png());
    let references = if s(c, "reference_kind") == "png-jpeg" {
        vec![png_ref(), reference("image/jpeg", unhex(JPEG))]
    } else {
        vec![
            if s(c, "reference_kind") == "webp" {
                reference("image/webp", unhex(WEBP))
            } else {
                png_ref()
            };
            n(c, "reference_count")
        ]
    };
    let mut request = ImageGenerationRequest {
        prompt: s(c, "prompt").into(),
        references,
    };
    match s(c, "invalid") {
        "empty-prompt" => {
            request.prompt = " \n".into();
            request.references.clear();
        }
        "six-references" => {
            request.prompt = "prompt".into();
            request.references = vec![png_ref(); 6];
        }
        "oversized-reference" => {
            request.references = vec![reference("image/png", vec![0; (10 << 20) + 1])]
        }
        "empty-reference" => request.references = vec![reference("image/png", vec![])],
        "wrong-media-type" => request.references = vec![reference("image/jpeg", png())],
        _ => {}
    }
    match s(c, "boundary") {
        "prompt-oversized" => {
            request.prompt = "a".repeat((80 << 20) + 1);
            request.references.clear();
        }
        "serialized-prompt-oversized" => {
            request.prompt = "é".repeat((80 << 20) / 6 + 1);
            request.references.clear();
        }
        "png-invalid" => {
            request.references = vec![reference("image/png", unhex("89504e470d0a1a0a"))]
        }
        "png-dimensions" => {
            let mut bytes = png();
            bytes[16..20].copy_from_slice(&8193u32.to_be_bytes());
            request.references = vec![reference("image/png", bytes)];
        }
        "png-frames" => {
            let mut bytes = png();
            let mut chunk = vec![0; 20];
            chunk[..4].copy_from_slice(&8u32.to_be_bytes());
            chunk[4..8].copy_from_slice(b"acTL");
            chunk[8..12].copy_from_slice(&2u32.to_be_bytes());
            bytes.splice(33..33, chunk);
            request.references = vec![reference("image/png", bytes)];
        }
        "reference-gif" => {
            request.references = vec![reference(
                "image/gif",
                unhex(
                    "47494638396101000100800000000000ffffff21f90401000000002c00000000010001000002024401003b",
                ),
            )]
        }
        _ => {}
    }
    request
}
async fn generation(c: &Value) -> Result<(Value, Value), Error> {
    let token = Arc::new(Token::default());
    let transport = Arc::new(Transport::new(c, "generation", token.clone()));
    let keys = Arc::new(Keys {
        missing: b(c, "missing"),
        ..Keys::default()
    });
    let store = Arc::new(KeyStore::default());
    let environment = Arc::new(Mutex::new(None::<String>));
    let source: Arc<dyn ApiKeySource> = if s(c, "provider") == "stored" {
        let env = environment.clone();
        Arc::new(StoredKeys(StoredApiKeySource::new(
            "configured",
            "synthetic-env",
            store.clone(),
            Arc::new(move |_| env.lock().unwrap().clone()),
        )))
    } else {
        keys.clone()
    };
    let provider: Arc<dyn Provider> = if s(c, "provider") == "unsupported" {
        Arc::new(Unsupported)
    } else if s(c, "provider") == "chatgpt" {
        Arc::new(ChatGptProvider::new_with_images(
            ChatGptOptions::new(Arc::new(Tokens {
                keys: keys.clone(),
                calls: AtomicUsize::new(0),
            })),
            Arc::new(NoHttp),
            transport.clone(),
            Arc::new(NoSocket),
        )?)
    } else {
        let mut options = CompatibleOptions::new("configured", s(c, "base_url"), source);
        options.headers = vec![("X-Tenant".into(), "tenant".into())];
        options.header_timeout_ms = 1;
        let compatible: Arc<dyn Provider> = Arc::new(OpenAICompatibleProvider::new_with_images(
            options,
            Arc::new(NoHttp),
            transport.clone(),
            Arc::new(NoSocket),
        )?);
        let inner = if matches!(s(c, "provider"), "kimi" | "opencode") {
            Arc::new(UsageReportingProvider::new(
                compatible,
                Arc::new(UsageBoundary),
            )) as Arc<dyn Provider>
        } else {
            compatible
        };
        if matches!(s(c, "provider"), "retry" | "kimi" | "opencode") {
            Arc::new(RetryingProvider::new(inner, 5))
        } else {
            inner
        }
    };
    if s(c, "cancel") == "before" {
        token.cancel();
    }
    let request = generation_request(c);
    let mut results = vec![];
    let mut errors = Errors::default();
    for operation in 1..=n(c, "operations") {
        if s(c, "provider") == "stored" {
            *store.value.lock().unwrap() =
                Some(Credential::for_api_key(format!("stored-{operation}")));
            if b(c, "environment_override") {
                *environment.lock().unwrap() = Some(format!("environment-{operation}"));
            }
        }
        let result = provider
            .generate_image(request.clone(), token.as_ref())
            .await;
        if let Ok(image) = &result {
            results.push(base64(&image.data));
        }
        errors.push(result.as_ref().err());
    }
    let requests = transport
        .requests
        .lock()
        .unwrap()
        .iter()
        .map(Capture::generation)
        .collect::<Vec<_>>();
    Ok((
        json!({"results":results,"errors":errors.raw,"semantic_errors":errors.selected,"credential_reads":keys.reads.load(Ordering::SeqCst),"stored_credential_reads":store.reads.load(Ordering::SeqCst),"requests":requests,"responses_disposed":transport.counters.disposed.load(Ordering::SeqCst),"response_bytes_read":transport.counters.read.load(Ordering::SeqCst)}),
        Value::Null,
    ))
}
fn signer(c: &Value) -> Result<(Value, Value), Error> {
    let bytes = unhex(s(c, "body_hex"));
    let hash = sha256(&bytes);
    let mut headers = vec![];
    for header in c["headers"].as_array().unwrap() {
        for value in header["values"].as_array().unwrap() {
            headers.push((
                s(header, "name").to_owned(),
                value.as_str().unwrap().to_owned(),
            ));
        }
    }
    let mut request = ImageHttpRequest {
        method: s(c, "method").into(),
        endpoint: Endpoint::parse(s(c, "url")).unwrap(),
        headers,
        content_type: None,
        body: bytes,
    };
    AwsV4Signer::new(
        s(c, "access_key").into(),
        s(c, "secret_key").into(),
        s(c, "region").into(),
    )
    .sign(&mut request, &hash, s(c, "amz_date"))?;
    let captured = Capture::from(request);
    Ok((
        json!({"method":captured.method,"url":captured.url,"headers":captured.headers,"payload_sha256":hash}),
        Value::Null,
    ))
}
struct Epoch;
impl Clock for Epoch {
    fn amz_date(&self) -> String {
        "19700101T000000Z".into()
    }
}
struct Store {
    mode: String,
    reads: AtomicUsize,
    released: Token,
    backend: Token,
    saw_cancellation: AtomicBool,
}
impl Store {
    fn new(mode: &str) -> Self {
        Self {
            mode: mode.into(),
            reads: AtomicUsize::new(0),
            released: Token::default(),
            backend: Token::default(),
            saw_cancellation: AtomicBool::new(false),
        }
    }
}
impl CredentialSource for Store {
    fn stored(&self) -> BoxFuture<'_, Result<Option<StoredCredential>, Error>> {
        Box::pin(async move {
            self.reads.fetch_add(1, Ordering::SeqCst);
            self.saw_cancellation
                .fetch_or(self.backend.is_cancelled(), Ordering::SeqCst);
            Ok(match self.mode.as_str() {
                "store-error" => {
                    return Err(Error::Authentication(AuthError::Message(
                        "synthetic store failure",
                    )));
                }
                "delayed" => {
                    self.released.cancelled().await;
                    Some(StoredCredential::S3 {
                        access_key: "async-access".into(),
                        secret_key: "async-secret".into(),
                    })
                }
                "stored" => Some(StoredCredential::S3 {
                    access_key: "stored-access".into(),
                    secret_key: "stored-secret".into(),
                }),
                "invalid-stored" => Some(StoredCredential::S3 {
                    access_key: " ".into(),
                    secret_key: "stored-secret".into(),
                }),
                "wrong-type" => Some(StoredCredential::Other),
                _ => None,
            })
        })
    }
    fn environment(&self, name: &str) -> Option<String> {
        match name {
            "PARROT_TEST_T34_S3_ACCESS" => Some("ak".into()),
            "PARROT_TEST_T34_S3_SECRET" => Some("sk".into()),
            _ => None,
        }
    }
}
fn make_bucket(
    c: &Value,
    transport: Arc<Transport>,
    store: Arc<Store>,
) -> Result<S3ImageBucket, Error> {
    let mut config = BucketConfig::new(s(c, "endpoint"), s(c, "bucket"));
    config.key_prefix = s(c, "prefix").into();
    config.region = s(c, "region").into();
    config.public_base_url = s(c, "public_base").into();
    config.access_key_env = s(c, "access_env").into();
    config.secret_key_env = s(c, "secret_env").into();
    S3ImageBucket::new(transport, config, Arc::new(Epoch), store)
}
async fn bucket(c: &Value) -> Result<(Value, Value), Error> {
    let token = Arc::new(Token::default());
    let transport = Arc::new(Transport::new(c, "bucket", token.clone()));
    let store = Arc::new(Store::new(s(c, "credentials")));
    let bucket = make_bucket(c, transport.clone(), store.clone())?;
    let bytes = unhex(s(c, "body_hex"));
    let mut results = vec![];
    let mut errors = Errors::default();
    let mut first = -1i64;
    let mut save = |result: Result<Value, Error>| {
        results.push(result.as_ref().cloned().unwrap_or(Value::Null));
        errors.push(result.as_ref().err());
    };
    match s(c, "mode") {
        "prepare" => {
            let result = async {
                bucket.prepare(token.as_ref()).await?;
                bucket.prepare(token.as_ref()).await?;
                Ok(Value::Null)
            }
            .await;
            save(result);
            first = transport.requests.lock().unwrap().len() as i64;
            bucket.forget_preparation();
            save(bucket.prepare(token.as_ref()).await.map(|_| Value::Null));
        }
        "prepare-retry" => {
            for _ in 0..2 {
                save(bucket.prepare(token.as_ref()).await.map(|_| Value::Null));
            }
        }
        "verify" => save(
            bucket
                .verify_public("https://cdn.example.com/b/parrot/x.png", token.as_ref())
                .await
                .map(Value::Bool),
        ),
        "concurrent" | "prepare-concurrent" => {
            let caller = Arc::new(Token::default());
            let first_bucket = bucket.clone();
            let first_token = caller.clone();
            let first_bytes = bytes.clone();
            let media = s(c, "media_type").to_owned();
            let prepare = s(c, "mode") == "prepare-concurrent";
            let abandoned = tokio::spawn(async move {
                if prepare {
                    first_bucket
                        .prepare(first_token.as_ref())
                        .await
                        .map(|_| json!("prepared"))
                } else {
                    first_bucket
                        .put_image(first_bytes, media, first_token.as_ref())
                        .await
                        .map(Value::String)
                }
            });
            while store.reads.load(Ordering::SeqCst) == 0 {
                tokio::task::yield_now().await;
            }
            let second_bucket = bucket.clone();
            let second_bytes = bytes.clone();
            let media = s(c, "media_type").to_owned();
            let alive = tokio::spawn(async move {
                let token = Token::default();
                if prepare {
                    second_bucket
                        .prepare(&token)
                        .await
                        .map(|_| json!("prepared"))
                } else {
                    second_bucket
                        .put_image(second_bytes, media, &token)
                        .await
                        .map(Value::String)
                }
            });
            tokio::task::yield_now().await;
            caller.cancel();
            save(abandoned.await.unwrap());
            store.released.cancel();
            save(alive.await.unwrap());
        }
        mode => {
            if mode == "cancel" {
                token.cancel();
            }
            save(
                bucket
                    .put_image(bytes, s(c, "media_type").into(), token.as_ref())
                    .await
                    .map(Value::String),
            );
        }
    }
    Ok((
        json!({"results":results,"errors":errors.raw,"semantic_errors":errors.selected,"credential_reads":store.reads.load(Ordering::SeqCst),"credential_saw_cancellation":store.saw_cancellation.load(Ordering::SeqCst),"first_preparation_request_count":first,"requests":transport.upload_records()}),
        Value::Null,
    ))
}
fn failure(kind: &str) -> Option<Error> {
    let http = |status, ty: &str, detail: &str| {
        Error::Http(HttpError::Response(HttpFailure {
            status,
            error_type: ty.into(),
            error_code: String::new(),
            detail: detail.into(),
            response_body: String::new(),
        }))
    };
    match kind {
        "visible" => Some(http(400, "", "late")),
        "timeout" => Some(Error::HeaderTimeout("timed out".into())),
        "permanent" => Some(Error::Provider("no key".into())),
        "overload" => Some(http(503, "", "busy")),
        "usage" => Some(Error::Response(ResponsesError::ProviderResponse {
            error_type: "usage_limit_reached".into(),
            error_code: String::new(),
            detail: "limit".into(),
            response_body: String::new(),
        })),
        "unauthorized" => Some(http(401, "", "no")),
        _ => None,
    }
}
fn error_pointer(error: &Error) -> Option<usize> {
    match error {
        Error::Provider(message) | Error::HeaderTimeout(message) => Some(message.as_ptr() as usize),
        Error::Http(HttpError::Response(error)) => Some(error.detail.as_ptr() as usize),
        Error::Response(ResponsesError::ProviderResponse { detail, .. }) => {
            Some(detail.as_ptr() as usize)
        }
        _ => None,
    }
}
struct Scripted {
    failure: Arc<Mutex<(Option<Error>, usize)>>,
    visible: bool,
    requests: Arc<Mutex<Vec<Request>>>,
    originals: Arc<Mutex<Vec<usize>>>,
    texts: Arc<Mutex<Vec<usize>>>,
    forwarded: Arc<Mutex<Vec<(bool, bool)>>>,
    failure_pointers: Arc<Mutex<Vec<usize>>>,
    disposals: Arc<AtomicUsize>,
    generation_calls: AtomicUsize,
    generation_enabled: AtomicBool,
}
impl Scripted {
    fn new(failure: Option<Error>, count: usize, visible: bool) -> Self {
        Self {
            failure: Arc::new(Mutex::new((failure, count))),
            visible,
            requests: Arc::new(Mutex::new(vec![])),
            originals: Arc::new(Mutex::new(vec![])),
            texts: Arc::new(Mutex::new(vec![])),
            forwarded: Arc::new(Mutex::new(vec![])),
            failure_pointers: Arc::new(Mutex::new(vec![])),
            disposals: Arc::new(AtomicUsize::new(0)),
            generation_calls: AtomicUsize::new(0),
            generation_enabled: AtomicBool::new(false),
        }
    }
    fn input(&self, template: &Request, text: bool) -> Request {
        let request = template.clone();
        let witness = request.messages.as_ptr() as usize;
        if text {
            self.texts.lock().unwrap().push(witness);
        } else {
            self.originals.lock().unwrap().push(witness);
        }
        request
    }
    fn scripted_call(&self, request: Request) -> Box<dyn EventStream> {
        let pointer = request.messages.as_ptr() as usize;
        self.forwarded.lock().unwrap().push((
            self.originals.lock().unwrap().contains(&pointer),
            self.texts.lock().unwrap().contains(&pointer),
        ));
        self.requests.lock().unwrap().push(request);
        let mut failures = self.failure.lock().unwrap();
        let error = if failures.1 > 0 {
            failures.1 -= 1;
            failures.0.clone()
        } else {
            None
        };
        if let Some(pointer) = error.as_ref().and_then(error_pointer) {
            self.failure_pointers.lock().unwrap().push(pointer);
        }
        let mut events = VecDeque::new();
        if error.is_some() {
            if self.visible {
                events.push_back(Event::TextDelta("partial".into()));
            }
        } else {
            events.push_back(Event::Completed(Completion {
                finish_reason: "stop".into(),
                input_tokens: 1,
                output_tokens: 1,
                assistant_text: "answer".into(),
                ..Completion::default()
            }));
        }
        Box::new(ScriptStream {
            events,
            error,
            disposals: self.disposals.clone(),
            closed: false,
        })
    }
}
impl Provider for Scripted {
    fn id(&self) -> &str {
        "scripted"
    }
    fn call(&self, request: Request) -> Result<Box<dyn EventStream>, Error> {
        Ok(self.scripted_call(request))
    }
    fn has_credential<'a>(&'a self, _: &'a dyn Cancellation) -> BoxFuture<'a, Result<bool, Error>> {
        Box::pin(async { Ok(true) })
    }
    fn seed_models(&self) -> Vec<Model> {
        vec![]
    }
    fn list_models<'a>(
        &'a self,
        _: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<Vec<Model>, Error>> {
        Box::pin(async { Ok(vec![]) })
    }
    fn calculate_image_tokens(&self, _: &Model, _: &Content) -> i64 {
        0
    }
    fn open_session(&self) -> Result<Arc<dyn Session>, Error> {
        Ok(Arc::new(ScriptSession {
            inner: Arc::new(Scripted {
                failure: self.failure.clone(),
                visible: self.visible,
                requests: self.requests.clone(),
                originals: self.originals.clone(),
                texts: self.texts.clone(),
                forwarded: self.forwarded.clone(),
                failure_pointers: self.failure_pointers.clone(),
                disposals: self.disposals.clone(),
                generation_calls: AtomicUsize::new(0),
                generation_enabled: AtomicBool::new(false),
            }),
        }))
    }
    fn generate_image<'a>(
        &'a self,
        _: ImageGenerationRequest,
        _: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<ImageGenerationResult, Error>> {
        Box::pin(async move {
            self.generation_calls.fetch_add(1, Ordering::SeqCst);
            if self.generation_enabled.load(Ordering::Acquire) {
                Ok(ImageGenerationResult {
                    data: vec![0, 1, 2],
                })
            } else {
                Err(Error::Provider(
                    "provider: image generation is not supported".into(),
                ))
            }
        })
    }
}
struct Unsupported;
impl Provider for Unsupported {
    fn id(&self) -> &str {
        "unsupported"
    }
    fn call(&self, _: Request) -> Result<Box<dyn EventStream>, Error> {
        panic!("unsupported generation must not call chat")
    }
    fn has_credential<'a>(&'a self, _: &'a dyn Cancellation) -> BoxFuture<'a, Result<bool, Error>> {
        Box::pin(async { Ok(false) })
    }
    fn seed_models(&self) -> Vec<Model> {
        vec![]
    }
    fn list_models<'a>(
        &'a self,
        _: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<Vec<Model>, Error>> {
        Box::pin(async { Ok(vec![]) })
    }
    fn calculate_image_tokens(&self, _: &Model, _: &Content) -> i64 {
        0
    }
    fn open_session(&self) -> Result<Arc<dyn Session>, Error> {
        panic!("unsupported generation must not open a session")
    }
}
struct ScriptSession {
    inner: Arc<Scripted>,
}
impl Session for ScriptSession {
    fn begin_turn(&self) -> BoxFuture<'_, Result<(), Error>> {
        Box::pin(async { Ok(()) })
    }
    fn call(&self, request: Request) -> Result<Box<dyn EventStream>, Error> {
        self.inner.call(request)
    }
    fn try_fall_back_to_http(&self) -> BoxFuture<'_, Result<bool, Error>> {
        Box::pin(async { Ok(false) })
    }
    fn close(&self) -> BoxFuture<'_, Result<(), Error>> {
        Box::pin(async { Ok(()) })
    }
}
struct ScriptStream {
    events: VecDeque<Event>,
    error: Option<Error>,
    disposals: Arc<AtomicUsize>,
    closed: bool,
}
impl EventStream for ScriptStream {
    fn next<'a>(
        &'a mut self,
        ct: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<Option<Event>, Error>> {
        Box::pin(async move {
            tokio::task::yield_now().await;
            if ct.is_cancelled() {
                return Err(Error::Cancelled);
            }
            if let Some(event) = self.events.pop_front() {
                Ok(Some(event))
            } else if let Some(error) = self.error.take() {
                Err(error)
            } else {
                Ok(None)
            }
        })
    }
    fn close(&mut self) -> BoxFuture<'_, Result<(), Error>> {
        Box::pin(async move {
            if !self.closed {
                self.closed = true;
                self.disposals.fetch_add(1, Ordering::SeqCst);
            }
            Ok(())
        })
    }
}
fn event(event: &Event) -> Value {
    let (kind, attempt, delay, input, cached, output, finish, text) = match event.base_event() {
        Event::Retry {
            attempt,
            retry_after,
            ..
        } => (
            "Retry",
            *attempt,
            retry_after.as_millis() as u64,
            0,
            0,
            0,
            "",
            "",
        ),
        Event::TextDelta(_) => ("TextDelta", 0, 0, 0, 0, 0, "", ""),
        Event::Completed(c) => (
            "Completed",
            0,
            0,
            c.input_tokens,
            c.cached_input_tokens,
            c.output_tokens,
            c.finish_reason.as_str(),
            c.assistant_text.as_str(),
        ),
        other => panic!("unexpected scripted event {other:?}"),
    };
    json!({"kind":kind,"text":event.text(),"attempt":attempt,"delay_ms":delay,"input_tokens":input,"cached_input_tokens":cached,"output_tokens":output,"finish_reason":finish,"assistant_text":text})
}
#[derive(Default)]
struct Collected {
    events: Vec<Value>,
    errors: Errors,
    same: Vec<bool>,
    streams: Vec<Box<dyn EventStream>>,
    failures: Vec<Error>,
}
impl Collected {
    async fn collect(
        &mut self,
        mut stream: Box<dyn EventStream>,
        token: &dyn Cancellation,
        scripted: &Scripted,
    ) -> Result<(), Error> {
        let mut events = vec![];
        let failure = loop {
            match stream.next(token).await {
                Ok(Some(value)) => events.push(event(&value)),
                Ok(None) => break None,
                Err(error) => break Some(error),
            }
        };
        stream.close().await?;
        self.events.push(json!(events));
        self.errors.push(failure.as_ref());
        self.same.push(
            failure
                .as_ref()
                .and_then(error_pointer)
                .is_some_and(|ptr| scripted.failure_pointers.lock().unwrap().contains(&ptr)),
        );
        if let Some(error) = failure {
            self.failures.push(error);
        }
        self.streams.push(stream);
        Ok(())
    }
}
async fn consume(
    collected: &mut Collected,
    provider: &ImageLinkingProvider,
    scripted: &Scripted,
    use_session: bool,
    request: Request,
    token: &dyn Cancellation,
) -> Result<(), Error> {
    let session = provider.open_session()?;
    let stream = if use_session {
        session.call(request)?
    } else {
        provider.call(request)?
    };
    collected.collect(stream, token, scripted).await?;
    session.close().await
}
async fn linking(c: &Value) -> Result<(Value, Value), Error> {
    let mode = s(c, "mode");
    let mut problem = failure(s(c, "failure"));
    let count = match mode {
        "missing" | "inline" | "failure" => 1,
        "second-rejection" => 2,
        _ => 0,
    };
    if matches!(mode, "missing" | "inline" | "second-rejection") {
        problem = Some(Error::Http(HttpError::Response(HttpFailure {
            status: 400,
            error_type: "invalid_request_error".into(),
            error_code: String::new(),
            detail: "bad image".into(),
            response_body: String::new(),
        })));
    }
    let scripted = Arc::new(Scripted::new(problem, count, s(c, "failure") == "visible"));
    let token = Arc::new(Token::default());
    let transport = Arc::new(Transport::new(c, "linking", token.clone()));
    let store = Arc::new(Store::new("environment"));
    let mut config = BucketConfig::new("https://minio.example.com", "b");
    config.access_key_env = "PARROT_TEST_T34_S3_ACCESS".into();
    config.secret_key_env = "PARROT_TEST_T34_S3_SECRET".into();
    let bucket = Arc::new(S3ImageBucket::new(
        transport.clone(),
        config,
        Arc::new(Epoch),
        store,
    )?);
    let provider = ImageLinkingProvider::new(scripted.clone(), bucket);
    let directory = std::env::var_os("TMPDIR")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| std::path::PathBuf::from(".tmp"));
    std::fs::create_dir_all(&directory).map_err(|error| Error::Io(error.to_string()))?;
    static NEXT_IMAGE: AtomicUsize = AtomicUsize::new(0);
    let path = directory.join(format!(
        "t34-rust-projector-{}-{}.png",
        std::process::id(),
        NEXT_IMAGE.fetch_add(1, Ordering::Relaxed)
    ));
    std::fs::write(&path, [0, 1, 2]).map_err(|error| Error::Io(error.to_string()))?;
    let path = path.to_str().unwrap().to_owned();
    let image = if mode == "existing-link" {
        Content::image_link(
            &path,
            "https://cdn.example.com/already.png",
            "image/png",
            2,
            1,
        )
    } else {
        Content::image_file(&path, "image/png", 2, 1)
    };
    let request = Request {
        model: "m".into(),
        messages: vec![Message::new(
            parrot_provider::sessions::Role::User,
            vec![Content::text("before"), image, Content::text("after")],
        )],
        ..Request::default()
    };
    let text_only = Request {
        model: "m".into(),
        messages: vec![Message::new(
            parrot_provider::sessions::Role::User,
            vec![Content::text("hello")],
        )],
        ..Request::default()
    };
    let mut collected = Collected::default();
    let mut generation_results = vec![];
    let mut after = -1i64;
    let mut immediate = -1i64;
    let mut settled = -1i64;
    match mode {
        "share" => {
            consume(
                &mut collected,
                &provider,
                &scripted,
                b(c, "session"),
                scripted.input(&request, false),
                token.as_ref(),
            )
            .await?;
            consume(
                &mut collected,
                &provider,
                &scripted,
                !b(c, "session"),
                scripted.input(&request, false),
                token.as_ref(),
            )
            .await?;
            consume(
                &mut collected,
                &provider,
                &scripted,
                b(c, "session"),
                scripted.input(&text_only, true),
                token.as_ref(),
            )
            .await?;
        }
        "inline" => {
            let rejected = provider.open_session()?;
            let other = provider.open_session()?;
            collected
                .collect(
                    rejected.call(scripted.input(&request, false))?,
                    token.as_ref(),
                    &scripted,
                )
                .await?;
            after = transport.requests.lock().unwrap().len() as i64;
            collected
                .collect(
                    rejected.call(scripted.input(&request, false))?,
                    token.as_ref(),
                    &scripted,
                )
                .await?;
            collected
                .collect(
                    other.call(scripted.input(&request, false))?,
                    token.as_ref(),
                    &scripted,
                )
                .await?;
            collected
                .collect(
                    provider.call(scripted.input(&request, false))?,
                    token.as_ref(),
                    &scripted,
                )
                .await?;
            rejected.close().await?;
            other.close().await?;
        }
        "generation-delegation" => {
            scripted.generation_enabled.store(true, Ordering::Release);
            let result = provider
                .generate_image(
                    ImageGenerationRequest {
                        prompt: "synthetic prompt".into(),
                        references: vec![],
                    },
                    token.as_ref(),
                )
                .await;
            if let Ok(value) = &result {
                generation_results.push(hex(&value.data));
            }
            collected.errors.push(result.as_ref().err());
        }
        _ => {
            if mode == "cancel-before" {
                token.cancel();
            }
            consume(
                &mut collected,
                &provider,
                &scripted,
                b(c, "session"),
                scripted.input(&request, false),
                token.as_ref(),
            )
            .await?;
            if mode == "cancel-before" {
                immediate = transport.requests.lock().unwrap().len() as i64;
                transport.release.notify_one();
                tokio::time::timeout(Duration::from_secs(5), transport.uploaded.notified())
                    .await
                    .map_err(|_| Error::Provider("detached upload did not settle".into()))?;
                settled = transport.requests.lock().unwrap().len() as i64;
            }
        }
    }
    let forwarded = scripted.forwarded.lock().unwrap().clone();
    let requests = scripted.requests.lock().unwrap();
    let contents=requests.iter().map(|request|request.messages.iter().map(|message|message.contents().iter().map(|content|json!({"kind":if matches!(content,Content::Text(_)){"Text"}else{"Image"},"text":content.text_value(),"image_path":if content.image_path()==path{"image-1"}else{content.image_path()},"image_url":content.image_url(),"media_type":content.media_type(),"width":content.image_width(),"height":content.image_height()})).collect::<Vec<_>>()).collect::<Vec<_>>()).collect::<Vec<_>>();
    let relation = json!({"representation":"NONIDENTICAL CLR reference identity versus moved Rust allocations","witness":"nonempty messages Vec allocation and original error String allocation","original_inputs":scripted.originals.lock().unwrap().len(),"text_inputs":scripted.texts.lock().unwrap().len(),"forwarded_original":forwarded.iter().map(|p|p.0).collect::<Vec<_>>(),"forwarded_text_only":forwarded.iter().map(|p|p.1).collect::<Vec<_>>(),"same_failure_reference":collected.same});
    let observation = json!({"cancellation_immediate_request_count":immediate,"cancellation_settled_request_count":settled,"events":collected.events,"errors":collected.errors.raw,"semantic_errors":collected.errors.selected,"same_failure_reference":collected.same,"forwarded_original":forwarded.iter().map(|p|p.0).collect::<Vec<_>>(),"forwarded_text_only":forwarded.iter().map(|p|p.1).collect::<Vec<_>>(),"forwarded_contents":contents,"inner_calls":requests.len(),"enumerator_disposals":scripted.disposals.load(Ordering::SeqCst),"generation_calls":scripted.generation_calls.load(Ordering::SeqCst),"generation_results":generation_results,"requests_after_fallback":after,"requests":transport.upload_records()});
    drop(requests);
    std::fs::remove_file(path).map_err(|error| Error::Io(error.to_string()))?;
    Ok((observation, relation))
}

/// Select only the explicitly approved runtime-language observations. Native
/// errors and ownership witnesses remain intact in the separate raw document.
pub fn selected(mut raw: Value) -> Value {
    raw.as_object_mut().unwrap().remove("native_errors");
    raw.as_object_mut().unwrap().remove("ownership_relation");
    for case in raw["cases"].as_array_mut().unwrap() {
        let observation = case["observation"].as_object_mut().unwrap();
        if let Some(error) = observation.remove("semantic_error") {
            observation.insert("error".into(), error);
        }
        if let Some(errors) = observation.remove("semantic_errors") {
            observation.insert("errors".into(), errors);
        }
    }
    raw
}

pub async fn project(input: &str) -> Result<String, Error> {
    let input: Value =
        serde_json::from_str(input).map_err(|error| Error::Encode(error.to_string()))?;
    let mut cases = vec![];
    let mut native_errors = vec![];
    let mut ownership_relation = vec![];
    for case in input["cases"]
        .as_array()
        .ok_or_else(|| Error::Encode("cases must be an array".into()))?
    {
        let (observation, relation) = match s(case, "family") {
            "generation" => generation(case).await?,
            "signer" => signer(case)?,
            "bucket" => bucket(case).await?,
            "linking" => linking(case).await?,
            "response_read_order" => review_projection::observe(case).await?,
            other => return Err(Error::Encode(format!("unknown family {other}"))),
        };
        if let Some(errors) = observation.get("errors") {
            native_errors.push(json!({"case_id":s(case,"case_id"),"errors":errors}));
        }
        if !relation.is_null() {
            ownership_relation.push(json!({"case_id":s(case,"case_id"),"relation":relation}));
        }
        cases.push(json!({"case_id":s(case,"case_id"),"family":s(case,"family"),"observation":observation}));
    }
    let contract = if cases
        .iter()
        .all(|case| case["family"] == "response_read_order")
    {
        "t34-images-review-v1"
    } else {
        "t34-images-source-v1"
    };
    serde_json::to_string(&json!({"source_commit":COMMIT,"contract":contract,"cases":cases,"native_errors":native_errors,"ownership_relation":ownership_relation})).map_err(|error|Error::Encode(error.to_string()))
}
