use parrot_domain::contracts::async_ops::{BoxFuture, Cancellation};
use parrot_provider::http_stream::*;
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
struct Cancel(tokio::sync::watch::Sender<bool>);
impl Cancel {
    fn new() -> Self {
        Self(tokio::sync::watch::channel(false).0)
    }
    fn cancel(&self) {
        self.0.send_replace(true);
    }
}
impl Cancellation for Cancel {
    fn is_cancelled(&self) -> bool {
        *self.0.borrow()
    }
    fn cancelled(&self) -> BoxFuture<'_, ()> {
        Box::pin(async move {
            let mut receiver = self.0.subscribe();
            while !*receiver.borrow_and_update() {
                if receiver.changed().await.is_err() {
                    return;
                }
            }
        })
    }
}
struct BytesBody {
    bytes: Vec<u8>,
    offset: usize,
    position: Arc<AtomicUsize>,
    disposed: Arc<AtomicBool>,
}
impl BytesBody {
    fn new(bytes: Vec<u8>) -> Self {
        Self {
            bytes,
            offset: 0,
            position: Arc::default(),
            disposed: Arc::default(),
        }
    }
}
impl Drop for BytesBody {
    fn drop(&mut self) {
        self.disposed.store(true, Ordering::SeqCst);
    }
}
impl ByteStream for BytesBody {
    fn read<'a>(
        &'a mut self,
        buffer: &'a mut [u8],
        cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<usize, HttpError>> {
        Box::pin(async move {
            if cancellation.is_cancelled() {
                return Err(HttpError::Cancelled);
            }
            let count = buffer.len().min(self.bytes.len() - self.offset);
            buffer[..count].copy_from_slice(&self.bytes[self.offset..self.offset + count]);
            self.offset += count;
            self.position.store(self.offset, Ordering::SeqCst);
            Ok(count)
        })
    }
}
struct ChannelBody(tokio::sync::mpsc::UnboundedReceiver<u8>, Arc<AtomicBool>);
impl Drop for ChannelBody {
    fn drop(&mut self) {
        self.1.store(true, Ordering::SeqCst);
    }
}
impl ByteStream for ChannelBody {
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
fn channel() -> (tokio::sync::mpsc::UnboundedSender<u8>, ChannelBody) {
    let (send, receive) = tokio::sync::mpsc::unbounded_channel();
    (send, ChannelBody(receive, Arc::default()))
}
struct FakeTransport {
    body: Mutex<Option<Box<dyn ByteStream>>>,
    requests: Mutex<Vec<HttpRequest>>,
    status: u16,
    delay: Duration,
}
impl FakeTransport {
    fn body(body: impl ByteStream + 'static, status: u16) -> Self {
        Self {
            body: Mutex::new(Some(Box::new(body))),
            requests: Mutex::new(Vec::new()),
            status,
            delay: Duration::ZERO,
        }
    }
}
impl HttpTransport for FakeTransport {
    fn send<'a>(
        &'a self,
        request: HttpRequest,
        cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<HttpResponse, HttpError>> {
        Box::pin(async move {
            self.requests.lock().unwrap().push(request);
            if !self.delay.is_zero() {
                tokio::select! { biased;
                    _ = cancellation.cancelled() => return Err(HttpError::Cancelled),
                    _ = tokio::time::sleep(self.delay) => {},
                }
            }
            Ok(HttpResponse {
                status: self.status,
                reason: "Bad Request".into(),
                headers: vec![("X-Codex-Turn-State".into(), "state".into())],
                body: self.body.lock().unwrap().take().unwrap(),
            })
        })
    }
}
fn endpoint() -> Url {
    Url::parse("https://example.test/v1/responses").unwrap()
}
fn options() -> StreamOptions {
    StreamOptions {
        header_timeout: Duration::from_secs(1),
        idle_timeout: Duration::from_secs(300),
        maximum_request_bytes: 10,
    }
}

#[test]
fn catalog_scopes_reuses_and_disposes_only_its_clients() {
    let standard = ProviderHttpClient::standard(reqwest::Client::new());
    let mut catalog = ProviderHttpClientCatalog::new(standard.clone());
    assert!(
        catalog
            .resolve("https://strict.example/v1", false)
            .unwrap()
            .same_client(&standard)
    );
    assert!(
        catalog
            .resolve("http://insecure.example/v1", true)
            .unwrap()
            .same_client(&standard)
    );
    let relaxed = catalog
        .resolve("https://insecure.example/v1", true)
        .unwrap();
    assert!(!relaxed.same_client(&standard));
    assert!(
        relaxed.same_client(
            &catalog
                .resolve("https://INSECURE.example:443/other", true)
                .unwrap()
        )
    );
    assert_eq!(catalog.insecure_client_count(), 1);
    assert!(has_authority(
        Url::parse("https://insecure.example/v1/models").unwrap(),
        "https://insecure.example"
    ));
    assert!(!has_authority(
        Url::parse("https://other.example/v1/models").unwrap(),
        "https://insecure.example"
    ));
    catalog.dispose();
    assert_eq!(catalog.insecure_client_count(), 0);
    assert!(relaxed.is_disposed());
    assert!(!standard.is_disposed());
}

#[tokio::test]
async fn bounded_counts_bytes_and_exact_limit_probe() {
    for exceeds in [false, true] {
        let body = BytesBody::new(if exceeds { "éé!" } else { "éé" }.as_bytes().to_vec());
        let position = body.position.clone();
        let disposed = body.disposed.clone();
        let counter = ResponseByteCounter::default();
        let mut stream = BoundedStream::new(body, 4, counter.clone());
        let result = stream.read(&mut [0; 8], &Never).await;
        if exceeds {
            assert_eq!(
                result.unwrap_err().to_string(),
                "provider: response stream exceeds byte limit"
            );
        } else {
            assert_eq!(result.unwrap(), 4);
        }
        assert_eq!(counter.get(), if exceeds { 5 } else { 4 });
        assert_eq!(position.load(Ordering::SeqCst), if exceeds { 5 } else { 4 });
        drop(stream);
        assert!(disposed.load(Ordering::SeqCst));
    }
}

#[tokio::test(start_paused = true)]
async fn first_read_idle_expires_without_a_byte() {
    let (_send, source) = channel();
    let mut stream = IdleTimeoutStream::new(source, Duration::from_secs(300));
    let error = stream.read(&mut [0; 1], &Never).await.unwrap_err();
    assert_eq!(
        error,
        HttpError::Wire("provider: idle timeout waiting for HTTP/SSE response bytes".into())
    );
}

#[tokio::test(start_paused = true)]
async fn each_read_resets_and_limit_probe_has_its_own_idle_bound() {
    let (send, source) = channel();
    let mut bounded = BoundedStream::new(
        IdleTimeoutStream::new(source, Duration::from_secs(300)),
        3,
        ResponseByteCounter::default(),
    );
    for _ in 0..2 {
        let sender = send.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_secs(240)).await;
            sender.send(b':').unwrap();
        });
        assert_eq!(bounded.read(&mut [0; 1], &Never).await.unwrap(), 1);
    }
    send.send(b'\n').unwrap();
    assert!(matches!(
        bounded.read(&mut [0; 1], &Never).await,
        Err(HttpError::Wire(_))
    ));
    assert_eq!(bounded.counter().get(), 3);
}

#[tokio::test(start_paused = true)]
async fn caller_cancellation_wins_including_disabled_idle_and_empty_reads() {
    for disabled in [false, true] {
        let (_send, source) = channel();
        let disposed = source.1.clone();
        let token = Arc::new(Cancel::new());
        let mut stream = IdleTimeoutStream::new(
            source,
            if disabled {
                Duration::ZERO
            } else {
                Duration::from_secs(300)
            },
        );
        let caller = token.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_secs(if disabled { 86400 } else { 100 })).await;
            caller.cancel();
        });
        assert_eq!(
            stream.read(&mut [0; 1], &*token).await.unwrap_err(),
            HttpError::Cancelled
        );
        assert_eq!(
            stream.read(&mut [], &*token).await.unwrap_err(),
            HttpError::Cancelled
        );
        drop(stream);
        assert!(disposed.load(Ordering::SeqCst));
    }
}

#[tokio::test(start_paused = true)]
async fn empty_reads_eof_and_drop_do_not_wait() {
    let source = BytesBody::new(Vec::new());
    let disposed = source.disposed.clone();
    let mut stream = IdleTimeoutStream::new(source, Duration::from_secs(300));
    let start = tokio::time::Instant::now();
    assert_eq!(stream.read(&mut [], &Never).await.unwrap(), 0);
    assert_eq!(stream.read(&mut [0; 1], &Never).await.unwrap(), 0);
    assert_eq!(tokio::time::Instant::now(), start);
    drop(stream);
    assert!(disposed.load(Ordering::SeqCst));
}

#[tokio::test]
async fn consumed_body_only_counts_read_bytes_and_error_prefix_is_bounded() {
    for (error, oversized) in [(false, false), (true, false), (true, true)] {
        let bytes = if oversized {
            "é".repeat(MAX_ERROR_BYTES).into_bytes()
        } else {
            "éé!".as_bytes().to_vec()
        };
        let source = BytesBody::new(bytes);
        let position = source.position.clone();
        let disposed = source.disposed.clone();
        let transport = FakeTransport::body(source, if error { 400 } else { 200 });
        let counter = ResponseByteCounter::default();
        let result = open_stream(
            &transport,
            endpoint(),
            vec![],
            vec![],
            options(),
            counter.clone(),
            &Never,
        )
        .await;
        if error {
            let Err(HttpError::Response(error)) = result else {
                panic!("expected HTTP error");
            };
            assert!(error.response_body.len() <= MAX_ERROR_BYTES);
            if oversized {
                assert!(error.response_body.ends_with("\n… [truncated]"));
            }
        } else {
            let mut response = result.unwrap();
            assert_eq!(response.turn_state(), Some("state"));
            assert_eq!(counter.get(), 0);
            assert_eq!(response.content.read(&mut [0; 2], &Never).await.unwrap(), 2);
        }
        let expected = if error {
            if oversized { MAX_ERROR_BYTES + 4 } else { 5 }
        } else {
            2
        };
        assert_eq!(counter.get(), expected as u64);
        assert_eq!(position.load(Ordering::SeqCst), expected);
        assert!(disposed.load(Ordering::SeqCst));
    }
}

#[tokio::test(start_paused = true)]
async fn header_deadline_ends_at_headers_and_get_deadline_includes_body() {
    let (_send, source) = channel();
    let mut slow = FakeTransport::body(source, 200);
    slow.delay = Duration::from_secs(2);
    assert!(matches!(
        open_stream(
            &slow,
            endpoint(),
            vec![],
            vec![],
            options(),
            ResponseByteCounter::default(),
            &Never
        )
        .await,
        Err(HttpError::HeaderTimeout(_))
    ));
    let (_send, source) = channel();
    let ready = FakeTransport::body(source, 200);
    let mut response = open_stream(
        &ready,
        endpoint(),
        vec![],
        vec![],
        options(),
        ResponseByteCounter::default(),
        &Never,
    )
    .await
    .unwrap();
    tokio::time::advance(Duration::from_secs(10)).await;
    let start = tokio::time::Instant::now();
    assert!(matches!(
        response.content.read(&mut [0; 1], &Never).await,
        Err(HttpError::Wire(_))
    ));
    assert_eq!(
        tokio::time::Instant::now() - start,
        Duration::from_secs(300)
    );
    let (_send, source) = channel();
    let ready = FakeTransport::body(source, 200);
    assert_eq!(
        get(
            &ready,
            endpoint(),
            vec![],
            Duration::from_secs(5),
            10,
            &Never
        )
        .await
        .unwrap_err(),
        HttpError::ProviderHttp("provider: request timed out".into())
    );
}

#[tokio::test]
async fn request_limit_rejects_before_send_and_get_enforces_exact_byte_limit() {
    let transport = FakeTransport::body(BytesBody::new(Vec::new()), 200);
    assert!(matches!(
        open_stream(
            &transport,
            endpoint(),
            vec![0; 11],
            vec![],
            options(),
            ResponseByteCounter::default(),
            &Never
        )
        .await,
        Err(HttpError::RequestTooLarge(10))
    ));
    assert!(transport.requests.lock().unwrap().is_empty());
    for bytes in ["éé", "éé!"] {
        let transport = FakeTransport::body(BytesBody::new(bytes.as_bytes().to_vec()), 200);
        let result = get(&transport, endpoint(), vec![], Duration::ZERO, 4, &Never).await;
        if bytes.len() == 4 {
            assert_eq!(result.unwrap(), bytes);
        } else {
            assert_eq!(
                result.unwrap_err().to_string(),
                "provider: response exceeds 4 bytes"
            );
        }
    }
}

#[tokio::test]
async fn structured_error_preserves_big_numeric_code_and_body() {
    let body = br#"{"error":{"type":" bad\u0000type ","code":123456789012345678901234567890,"message":" bad\nnews "},"opaque":true}"#;
    let transport = FakeTransport::body(BytesBody::new(body.to_vec()), 429);
    let Err(HttpError::Response(failure)) = open_stream(
        &transport,
        endpoint(),
        vec![],
        vec![],
        options(),
        ResponseByteCounter::default(),
        &Never,
    )
    .await
    else {
        panic!("expected structured response");
    };
    assert_eq!(failure.error_type, "badtype");
    assert_eq!(failure.error_code, "123456789012345678901234567890");
    assert_eq!(failure.detail, "badnews");
    assert_eq!(failure.response_body.as_bytes(), body);
    assert_eq!(
        failure.to_string(),
        "provider request failed with HTTP 429 (badtype) [123456789012345678901234567890]: badnews"
    );
}

#[test]
fn endpoints_and_configured_header_security_are_explicit() {
    assert_eq!(
        endpoint_url("https://example.test/v1/", "models", false, false)
            .unwrap()
            .as_str(),
        "https://example.test/v1/models"
    );
    for host in ["localhost", "127.0.0.2", "[::1]"] {
        assert!(endpoint_url(&format!("http://{host}/v1"), "models", true, false).is_ok());
    }
    assert!(endpoint_url("http://remote.test", "models", true, false).is_err());
    assert!(endpoint_url("http://remote.test", "models", false, true).is_ok());
    for base in [
        "https://name:password@example.test",
        "https://example.test?x",
        "https://example.test#x",
        "ftp://example.test",
        "not a URL",
    ] {
        assert!(endpoint_url(base, "models", true, true).is_err());
    }
    for name in [
        "Authorization",
        "cookie",
        "HOST",
        "proxy-authorization",
        " X-Header",
        "",
        "é",
    ] {
        assert!(validate_headers(&[(name.into(), "test".into())]).is_err());
    }
    assert!(validate_headers(&[("X-Test".into(), "a\r\nb".into())]).is_err());
    assert_eq!(
        validate_headers(&[("X-Test".into(), "a".into()), ("x-test".into(), "b".into())]).unwrap(),
        vec![("X-Test".into(), "b".into())]
    );
    assert!(bearer_headers("test", "").is_err());
    assert_eq!(
        bearer_headers("test", " key ").unwrap()[0].1,
        "Bearer  key "
    );
}

#[tokio::test]
async fn unicode_tls_scope_cannot_be_spoofed_by_path_userinfo_delimiters() {
    let standard =
        ProviderHttpClient::standard(reqwest::Client::builder().no_proxy().build().unwrap());
    let mut catalog = ProviderHttpClientCatalog::new(standard);
    let scoped = catalog.resolve("https://bücher.example", true).unwrap();
    for raw in [
        "https://other.example/path@bücher.example",
        "https://other.example/?@bücher.example",
        "https://other.example/#@bücher.example",
    ] {
        let endpoint = Endpoint::parse(raw).unwrap();
        assert_eq!(endpoint.as_url().host_str(), Some("other.example"));
        assert!(!scoped.allows_invalid_tls_for(&endpoint), "{raw}");
    }
    assert!(Endpoint::parse("https://other.example\\@bücher.example/path").is_err());
    assert!(scoped.allows_invalid_tls_for(Endpoint::parse("https://bücher.example/path").unwrap()));
    assert!(
        !scoped
            .allows_invalid_tls_for(Endpoint::parse("https://xn--bcher-kva.example/path").unwrap())
    );
}

#[test]
fn provider_policy_errors_are_distinct_from_native_http_errors() {
    for error in [
        endpoint_url("", "models", false, false).unwrap_err(),
        validate_headers(&[("Authorization".into(), "synthetic".into())]).unwrap_err(),
    ] {
        assert!(
            !matches!(error, HttpError::Http(_) | HttpError::Transport(_)),
            "provider policy must retain its typed origin: {error:?}"
        );
    }
}

#[tokio::test]
async fn bounded_get_polls_ready_transport_before_precancelled_caller() {
    let caller = Cancel::new();
    caller.cancel();
    let transport = FakeTransport::body(BytesBody::new(vec![1]), 200);
    assert!(matches!(
        get(
            &transport,
            endpoint(),
            vec![],
            Duration::from_secs(1),
            10,
            &caller
        )
        .await,
        Err(HttpError::Cancelled)
    ));
    assert_eq!(transport.requests.lock().unwrap().len(), 1);
}
