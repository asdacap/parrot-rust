use parrot_domain::contracts::async_ops::{BoxFuture, Cancellation};
use parrot_provider::{
    http_stream::{ByteStream, Endpoint, HttpError, HttpResponse},
    image_generation::{
        ImageHttpRequest, ImageTransport,
        client::{
            ImageGenerationClient, ImageGenerationReference, ImageGenerationRequest, ImageHeaders,
            MAXIMUM_RESPONSE_BYTES, decode_response, prepare,
        },
    },
    sessions::Error,
};
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicUsize, Ordering},
};

const PNG: &str = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR4nGP4z8DwHwAFAAH/iZk9HQAAAABJRU5ErkJggg==";
fn success() -> Vec<u8> {
    format!("{{\"data\":[{{\"b64_json\":\"{PNG}\"}}]}}").into_bytes()
}
struct Never;
impl Cancellation for Never {
    fn is_cancelled(&self) -> bool {
        false
    }
    fn cancelled(&self) -> BoxFuture<'_, ()> {
        Box::pin(std::future::pending())
    }
}
struct Cancelled;
impl Cancellation for Cancelled {
    fn is_cancelled(&self) -> bool {
        true
    }
    fn cancelled(&self) -> BoxFuture<'_, ()> {
        Box::pin(async {})
    }
}
struct Bytes(Vec<u8>, usize);
impl ByteStream for Bytes {
    fn read<'a>(
        &'a mut self,
        output: &'a mut [u8],
        ct: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<usize, HttpError>> {
        Box::pin(async move {
            if ct.is_cancelled() {
                return Err(HttpError::Cancelled);
            }
            let count = output.len().min(self.0.len() - self.1);
            output[..count].copy_from_slice(&self.0[self.1..self.1 + count]);
            self.1 += count;
            Ok(count)
        })
    }
}
#[derive(Default)]
struct Headers(AtomicUsize);
impl ImageHeaders for Headers {
    fn resolve<'a>(
        &'a self,
        _: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<Vec<(String, String)>, Error>> {
        Box::pin(async move {
            let n = self.0.fetch_add(1, Ordering::SeqCst) + 1;
            Ok(vec![("Authorization".into(), format!("Bearer key-{n}"))])
        })
    }
}
struct Transport {
    requests: Mutex<Vec<ImageHttpRequest>>,
    status: u16,
    body: Vec<u8>,
    length: Option<usize>,
}
impl ImageTransport for Transport {
    fn send<'a>(
        &'a self,
        request: ImageHttpRequest,
        ct: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<HttpResponse, Error>> {
        Box::pin(async move {
            if ct.is_cancelled() {
                return Err(Error::Cancelled);
            }
            self.requests.lock().unwrap().push(request);
            Ok(HttpResponse {
                status: self.status,
                reason: String::new(),
                headers: self
                    .length
                    .map(|n| vec![("Content-Length".into(), n.to_string())])
                    .unwrap_or_default(),
                body: Box::new(Bytes(self.body.clone(), 0)),
            })
        })
    }
}
fn fixture(
    status: u16,
    body: Vec<u8>,
    length: Option<usize>,
) -> (ImageGenerationClient, Arc<Transport>, Arc<Headers>) {
    let transport = Arc::new(Transport {
        requests: Mutex::new(Vec::new()),
        status,
        body,
        length,
    });
    let headers = Arc::new(Headers::default());
    (
        ImageGenerationClient::new(
            transport.clone(),
            Endpoint::parse("https://example.test/v1/images/generations").unwrap(),
            Endpoint::parse("https://example.test/custom/v1/images/edits").unwrap(),
            headers.clone(),
        ),
        transport,
        headers,
    )
}
fn request() -> ImageGenerationRequest {
    ImageGenerationRequest {
        prompt: "  Keep this prompt verbatim.\nSecond line.  ".into(),
        references: vec![],
    }
}
fn png() -> Vec<u8> {
    decode_response(&success()).unwrap().data
}

#[tokio::test]
async fn client_generates_and_edits_with_ordered_json_and_fresh_headers() {
    for edit in [false, true] {
        let (client, transport, headers) = fixture(200, success(), None);
        let mut request = request();
        if edit {
            request.references = vec![ImageGenerationReference {
                media_type: "image/png".into(),
                data: png(),
            }];
        }
        for operation in 1..=2 {
            assert_eq!(
                client.generate(request.clone(), &Never).await.unwrap().data,
                png()
            );
            let requests = transport.requests.lock().unwrap();
            let sent = &requests[operation - 1];
            assert_eq!(sent.method, "POST");
            assert_eq!(sent.content_type.as_deref(), Some("application/json"));
            assert_eq!(
                sent.endpoint.as_str(),
                if edit {
                    "https://example.test/custom/v1/images/edits"
                } else {
                    "https://example.test/v1/images/generations"
                }
            );
            assert_eq!(
                sent.headers,
                vec![("Authorization".into(), format!("Bearer key-{operation}"))]
            );
            let value: serde_json::Value = serde_json::from_slice(&sent.body).unwrap();
            assert_eq!(value["prompt"], request.prompt);
            assert_eq!(value["model"], "gpt-image-2");
            for key in ["background", "quality", "size"] {
                assert_eq!(value[key], "auto");
            }
            assert!(value.get("stream").is_none());
            assert_eq!(value.get("images").is_some(), edit);
            assert!(
                std::str::from_utf8(&sent.body)
                    .unwrap()
                    .starts_with("{\"prompt\":")
            );
            if edit {
                assert_eq!(
                    value["images"][0]["image_url"],
                    format!("data:image/png;base64,{PNG}")
                );
            }
        }
        assert_eq!(headers.0.load(Ordering::SeqCst), 2);
    }
}
#[tokio::test]
async fn generation_dispatch_uses_source_uri_spelling_without_changing_authority() {
    let (_, transport, headers) = fixture(200, success(), None);
    let original = Endpoint::parse("https://s3.example.test/b/%7E/%41%61%30%2D%5F%2E").unwrap();
    let client = ImageGenerationClient::new(
        transport.clone(),
        original.clone(),
        original.clone(),
        headers,
    );
    assert_eq!(
        client.generate(request(), &Never).await.unwrap().data,
        png()
    );
    let requests = transport.requests.lock().unwrap();
    assert_eq!(requests.len(), 1);
    assert_eq!(
        requests[0].endpoint.as_url().as_str(),
        "https://s3.example.test/b/~/Aa0-_."
    );
    assert_eq!(requests[0].endpoint.authority(), original.authority());
}
#[tokio::test]
async fn invalid_requests_fail_before_credentials_and_http() {
    for kind in [
        "empty-prompt",
        "six-references",
        "oversized-reference",
        "empty-reference",
        "wrong-media-type",
    ] {
        let (client, transport, headers) = fixture(200, success(), None);
        let reference = ImageGenerationReference {
            media_type: "image/png".into(),
            data: png(),
        };
        let mut req = request();
        match kind {
            "empty-prompt" => req.prompt = " \n".into(),
            "six-references" => req.references = vec![reference; 6],
            "oversized-reference" => {
                req.references = vec![ImageGenerationReference {
                    media_type: "image/png".into(),
                    data: vec![0; (10 << 20) + 1],
                }]
            }
            "empty-reference" => {
                req.references = vec![ImageGenerationReference {
                    media_type: "image/png".into(),
                    data: vec![],
                }]
            }
            _ => {
                req.references = vec![ImageGenerationReference {
                    media_type: "image/jpeg".into(),
                    data: reference.data,
                }]
            }
        }
        assert!(
            matches!(client.generate(req, &Never).await, Err(Error::Provider(_))),
            "{kind}"
        );
        assert_eq!(headers.0.load(Ordering::SeqCst), 0);
        assert!(transport.requests.lock().unwrap().is_empty());
    }
}
#[tokio::test]
async fn paid_failures_are_not_retried_and_error_body_is_not_exposed() {
    for status in [401, 429, 404, 503] {
        let (client, transport, _) = fixture(
            status,
            b"secret key-1 data:image/png;base64,sensitive".to_vec(),
            None,
        );
        let error = client.generate(request(), &Never).await.unwrap_err();
        assert_eq!(
            error,
            Error::Provider(format!("provider: image generation failed (HTTP {status})"))
        );
        assert_eq!(transport.requests.lock().unwrap().len(), 1);
    }
}
#[tokio::test]
async fn invalid_responses_do_not_follow_urls_or_send_followup_requests() {
    for body in [
        "not JSON",
        "null",
        "{}",
        "{\"data\":[]}",
        "{\"data\":[null]}",
        "{\"data\":[{}]}",
        "{\"data\":[{\"b64_json\":\"\"}]}",
        "{\"data\":[{\"b64_json\":\"%%%\"}]}",
        "{\"data\":[{\"b64_json\":\"aGVsbG8=\"}]}",
        "{\"data\":[{\"url\":\"https://example.test/not-downloaded.png\"}]}",
        &format!("{{\"data\":[{{\"b64_json\":\"{PNG}\"}},{{\"b64_json\":\"{PNG}\"}}]}}"),
    ] {
        let (client, transport, _) = fixture(200, body.as_bytes().to_vec(), None);
        assert!(
            matches!(
                client.generate(request(), &Never).await,
                Err(Error::Provider(_))
            ),
            "{body}"
        );
        assert_eq!(transport.requests.lock().unwrap().len(), 1);
    }
}
#[tokio::test]
async fn jpeg_output_is_rejected_without_retry_or_followup() {
    let jpeg = "/9j/4AAQSkZJRgABAQAAAQABAAD/2wBDABALDA4MChAODQ4SERATGCgaGBYWGDEjJR0oOjM9PDkzODdASFxOQERXRTc4UG1RV19iZ2hnPk1xeXBkeFxlZ2P/wAALCAACAAMBAREA/8QAFAABAAAAAAAAAAAAAAAAAAAAAP/EABQQAQAAAAAAAAAAAAAAAAAAAAD/2gAIAQEAAD8AP//Z";
    let body = format!("{{\"data\":[{{\"b64_json\":\"{jpeg}\"}}]}}");
    let (client, transport, _) = fixture(200, body.into_bytes(), None);
    assert_eq!(
        client.generate(request(), &Never).await.unwrap_err(),
        Error::Provider("provider: unsupported image format".into())
    );
    assert_eq!(transport.requests.lock().unwrap().len(), 1);
}
#[tokio::test]
async fn advertised_oversized_response_is_rejected_and_precancel_stops_before_credentials() {
    let (client, transport, headers) = fixture(200, success(), Some(MAXIMUM_RESPONSE_BYTES + 1));
    assert_eq!(
        client.generate(request(), &Never).await.unwrap_err(),
        Error::Provider("provider: image response exceeds byte limit".into())
    );
    assert_eq!(transport.requests.lock().unwrap().len(), 1);
    assert_eq!(
        client.generate(request(), &Cancelled).await.unwrap_err(),
        Error::Cancelled
    );
    assert_eq!(headers.0.load(Ordering::SeqCst), 1);
    assert_eq!(transport.requests.lock().unwrap().len(), 1);
}
#[test]
fn source_encoder_escaping_and_response_duplicate_last_assignment_are_preserved() {
    let mut req = request();
    req.prompt = "\"'<>+&`é😀".into();
    let body = String::from_utf8(prepare(&req).unwrap()).unwrap();
    assert!(
        body.contains("\\u0022\\u0027\\u003C\\u003E\\u002B\\u0026\\u0060\\u00E9\\uD83D\\uDE00")
    );
    let response = format!(
        "{{\"data\":[],\"data\":[{{\"b64_json\":\"bad\",\"b64_json\":\"{PNG}\",\"ignored\":1e999}}]}}"
    );
    assert_eq!(decode_response(response.as_bytes()).unwrap().data, png());
}

struct SignalCancellation {
    cancelled: std::sync::atomic::AtomicBool,
    notify: tokio::sync::Notify,
}
impl SignalCancellation {
    fn new() -> Self {
        Self {
            cancelled: std::sync::atomic::AtomicBool::new(false),
            notify: tokio::sync::Notify::new(),
        }
    }
    fn cancel(&self) {
        self.cancelled.store(true, Ordering::Release);
        self.notify.notify_waiters();
    }
}
impl Cancellation for SignalCancellation {
    fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::Acquire)
    }
    fn cancelled(&self) -> BoxFuture<'_, ()> {
        Box::pin(async move {
            let notified = self.notify.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            if !self.is_cancelled() {
                notified.await;
            }
        })
    }
}

#[derive(Clone, Copy)]
enum Tail {
    Io,
    Cancel,
    Block,
}
struct PrefixBody {
    prefix: Vec<u8>,
    offset: usize,
    tail: Tail,
    reads: Arc<AtomicUsize>,
    drops: Arc<AtomicUsize>,
}
impl Drop for PrefixBody {
    fn drop(&mut self) {
        self.drops.fetch_add(1, Ordering::SeqCst);
    }
}
impl ByteStream for PrefixBody {
    fn read<'a>(
        &'a mut self,
        output: &'a mut [u8],
        _: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<usize, HttpError>> {
        Box::pin(async move {
            self.reads.fetch_add(1, Ordering::SeqCst);
            if self.offset < self.prefix.len() {
                let count = output.len().min(self.prefix.len() - self.offset);
                output[..count].copy_from_slice(&self.prefix[self.offset..self.offset + count]);
                self.offset += count;
                return Ok(count);
            }
            match self.tail {
                Tail::Io => Err(HttpError::Transport("synthetic later I/O".into())),
                Tail::Cancel => Err(HttpError::Cancelled),
                Tail::Block => std::future::pending().await,
            }
        })
    }
}
struct PrefixTransport {
    prefix: Vec<u8>,
    tail: Tail,
    reads: Arc<AtomicUsize>,
    drops: Arc<AtomicUsize>,
}
impl ImageTransport for PrefixTransport {
    fn send<'a>(
        &'a self,
        _: ImageHttpRequest,
        _: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<HttpResponse, Error>> {
        Box::pin(async move {
            Ok(HttpResponse {
                status: 200,
                reason: String::new(),
                headers: Vec::new(),
                body: Box::new(PrefixBody {
                    prefix: self.prefix.clone(),
                    offset: 0,
                    tail: self.tail,
                    reads: self.reads.clone(),
                    drops: self.drops.clone(),
                }),
            })
        })
    }
}
fn prefix_fixture(
    prefix: Vec<u8>,
    tail: Tail,
) -> (ImageGenerationClient, Arc<AtomicUsize>, Arc<AtomicUsize>) {
    let reads = Arc::new(AtomicUsize::new(0));
    let drops = Arc::new(AtomicUsize::new(0));
    let transport = Arc::new(PrefixTransport {
        prefix,
        tail,
        reads: reads.clone(),
        drops: drops.clone(),
    });
    (
        ImageGenerationClient::new(
            transport,
            Endpoint::parse("https://example.test/images/generations").unwrap(),
            Endpoint::parse("https://example.test/images/edits").unwrap(),
            Arc::new(Headers::default()),
        ),
        reads,
        drops,
    )
}
#[tokio::test]
async fn syntax_and_schema_failures_stop_after_first_buffer_and_release_response() {
    let invalid_data = format!("{{\"data\":123,\"data\":[{{\"b64_json\":\"{PNG}\"}}]}}");
    let invalid_image = format!("{{\"data\":[{{\"b64_json\":123,\"b64_json\":\"{PNG}\"}}]}}");
    for (text, tail) in [
        ("!", Tail::Io),
        ("!", Tail::Cancel),
        ("!", Tail::Block),
        (invalid_data.as_str(), Tail::Io),
        (invalid_image.as_str(), Tail::Io),
        ("{\"data\":123,", Tail::Io),
    ] {
        let mut prefix = text.as_bytes().to_vec();
        prefix.resize(16384, b' ');
        let (client, reads, drops) = prefix_fixture(prefix, tail);
        let result = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            client.generate(request(), &Never),
        )
        .await
        .expect("invalid JSON must not wait for a later body read");
        assert_eq!(
            result.unwrap_err(),
            Error::Provider("provider: invalid image response JSON".into())
        );
        assert_eq!(reads.load(Ordering::SeqCst), 1);
        assert_eq!(drops.load(Ordering::SeqCst), 1);
    }
}
#[tokio::test]
async fn potentially_valid_prefix_preserves_later_transport_failure_and_cancellation() {
    for tail in [Tail::Io, Tail::Cancel] {
        let mut prefix = b"{\"ignored\":\"".to_vec();
        prefix.resize(16384, b'a');
        let (client, reads, drops) = prefix_fixture(prefix, tail);
        let error = client.generate(request(), &Never).await.unwrap_err();
        assert_eq!(
            error,
            match tail {
                Tail::Io => Error::Http(HttpError::Transport("synthetic later I/O".into())),
                _ => Error::Http(HttpError::Cancelled),
            }
        );
        assert_eq!(reads.load(Ordering::SeqCst), 2);
        assert_eq!(drops.load(Ordering::SeqCst), 1);
    }
}

#[tokio::test]
async fn a_short_invalid_chunk_is_filled_before_parsing_and_keeps_later_failures() {
    for tail in [Tail::Io, Tail::Cancel] {
        let (client, reads, drops) = prefix_fixture(b"!".to_vec(), tail);
        let error = client.generate(request(), &Never).await.unwrap_err();
        assert_eq!(
            error,
            match tail {
                Tail::Io => Error::Http(HttpError::Transport("synthetic later I/O".into())),
                _ => Error::Http(HttpError::Cancelled),
            }
        );
        assert_eq!(reads.load(Ordering::SeqCst), 2);
        assert_eq!(drops.load(Ordering::SeqCst), 1);
    }
}
#[tokio::test]
async fn cancellation_wakes_a_blocked_fill_after_a_short_invalid_chunk() {
    let (client, reads, drops) = prefix_fixture(b"!".to_vec(), Tail::Block);
    let token = Arc::new(SignalCancellation::new());
    let observed = token.clone();
    let operation =
        tokio::spawn(async move { client.generate(request(), observed.as_ref()).await });
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        while reads.load(Ordering::SeqCst) < 2 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    token.cancel();
    let result = tokio::time::timeout(std::time::Duration::from_secs(5), operation)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(result.unwrap_err(), Error::Http(HttpError::Cancelled));
    assert_eq!(reads.load(Ordering::SeqCst), 2);
    assert_eq!(drops.load(Ordering::SeqCst), 1);
}
#[test]
fn cancellation_aborts_a_queued_parser_without_waiting_for_an_unrelated_blocker() {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .max_blocking_threads(1)
        .enable_all()
        .build()
        .unwrap();
    runtime.block_on(async {
        let (entered, started) = tokio::sync::oneshot::channel();
        let (release, hold) = std::sync::mpsc::channel();
        let blocker = tokio::task::spawn_blocking(move || {
            let _ = entered.send(());
            let _ = hold.recv();
        });
        started.await.unwrap();
        let (client, reads, drops) = prefix_fixture(success(), Tail::Block);
        let token = SignalCancellation::new();
        let mut operation = client.generate(request(), &token);
        std::future::poll_fn(|cx| {
            assert!(std::future::Future::poll(operation.as_mut(), cx).is_pending());
            std::task::Poll::Ready(())
        })
        .await;
        token.cancel();
        let result = tokio::time::timeout(std::time::Duration::from_millis(100), operation).await;
        let read_count = reads.load(Ordering::SeqCst);
        let disposed = drops.load(Ordering::SeqCst);
        release.send(()).unwrap();
        blocker.await.unwrap();
        assert_eq!(
            result
                .expect("cancellation must not depend on a free blocking thread")
                .unwrap_err(),
            Error::Cancelled
        );
        assert_eq!(read_count, 0);
        assert_eq!(disposed, 1);
    });
}
