mod backends;
mod boundaries;
mod session;
mod source_rows;

use parrot_domain::contracts::async_ops::{BoxFuture, Cancellation, OperationError};
use parrot_domain::contracts::paths::ResolvedRead;
use parrot_domain::contracts::security::{AccessSnapshot, InvocationContext};
use parrot_domain::contracts::tools::{ParallelSafety, Tool};
use parrot_domain::values::tools::ConfiguredToolDefinition;
use parrot_tools::web_fetch::{
    AddressPolicy, CookieStore, CookieStoreFactory, FetchError, GzipDecoder, Invocation, Method,
    Options, PrivateAddressPolicy, PublicAddressPolicy, Request, Resolver, Response, ResponseBody,
    Transport, TransportSession, WebFetchTool, WebFetcher, normalize_address,
};
use std::collections::VecDeque;
use std::io::{self, Read, Write};
use std::net::{IpAddr, SocketAddr, TcpListener};
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

struct NeverCancel;
impl Cancellation for NeverCancel {
    fn is_cancelled(&self) -> bool {
        false
    }
    fn cancelled(&self) -> BoxFuture<'_, ()> {
        Box::pin(std::future::pending())
    }
}

#[derive(Default)]
struct CancelSignal {
    cancelled: AtomicBool,
    notify: tokio::sync::Notify,
}
impl CancelSignal {
    fn cancel(&self) {
        self.cancelled.store(true, Ordering::SeqCst);
        self.notify.notify_waiters();
    }
}
impl Cancellation for CancelSignal {
    fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::SeqCst)
    }
    fn cancelled(&self) -> BoxFuture<'_, ()> {
        Box::pin(async move {
            loop {
                let notified = self.notify.notified();
                if self.is_cancelled() {
                    return;
                }
                notified.await;
            }
        })
    }
}

struct NoCookiesExpected;
impl CookieStoreFactory for NoCookiesExpected {
    fn create(&self) -> Arc<dyn CookieStore> {
        Arc::new(Self)
    }
}
impl CookieStore for NoCookiesExpected {
    fn request_header(&self, _: &url::Url) -> Option<String> {
        None
    }
    fn store_response(&self, _: &url::Url, cookies: &[String]) {
        assert!(
            cookies.is_empty(),
            "noncookie fixture must not require a cookie backend"
        );
    }
}

struct NoGzipExpected;
impl GzipDecoder for NoGzipExpected {
    fn decode(&self, _: Box<dyn ResponseBody>) -> Result<Box<dyn ResponseBody>, FetchError> {
        panic!("this nongzip test must not invoke a gzip decoder")
    }
}

struct BytesBody {
    bytes: Vec<u8>,
    position: usize,
    reads: Arc<Mutex<Vec<usize>>>,
}
impl BytesBody {
    fn new(bytes: &[u8]) -> Self {
        Self {
            bytes: bytes.to_vec(),
            position: 0,
            reads: Arc::default(),
        }
    }
}
impl ResponseBody for BytesBody {
    fn read<'a>(&'a mut self, output: &'a mut [u8]) -> BoxFuture<'a, Result<usize, FetchError>> {
        Box::pin(async move {
            self.reads.lock().unwrap().push(output.len());
            let count = output.len().min(self.bytes.len() - self.position);
            output[..count].copy_from_slice(&self.bytes[self.position..self.position + count]);
            self.position += count;
            Ok(count)
        })
    }
}

#[derive(Default)]
struct FixedResolver {
    hosts: Mutex<Vec<String>>,
    addresses: Vec<IpAddr>,
}
impl Resolver for FixedResolver {
    fn resolve<'a>(&'a self, host: &'a str) -> BoxFuture<'a, io::Result<Vec<IpAddr>>> {
        Box::pin(async move {
            self.hosts.lock().unwrap().push(host.to_owned());
            Ok(self.addresses.clone())
        })
    }
}

type SentRequests = Vec<(String, Method, Vec<SocketAddr>)>;
struct ScriptedTransport {
    responses: Mutex<VecDeque<Response>>,
    sent: Mutex<SentRequests>,
}
impl ScriptedTransport {
    fn new(responses: Vec<Response>) -> Self {
        Self {
            responses: Mutex::new(responses.into()),
            sent: Mutex::new(Vec::new()),
        }
    }
}
impl Transport for ScriptedTransport {
    fn session(&self) -> Result<Box<dyn TransportSession + '_>, FetchError> {
        Ok(Box::new(self))
    }
}
impl TransportSession for &ScriptedTransport {
    fn send<'a>(
        &'a mut self,
        request: &'a Request,
        addresses: &'a [SocketAddr],
        _: &'a Options,
    ) -> BoxFuture<'a, Result<Response, FetchError>> {
        Box::pin(async move {
            self.sent.lock().unwrap().push((
                request.address.to_string(),
                request.method,
                addresses.to_vec(),
            ));
            Ok(self
                .responses
                .lock()
                .unwrap()
                .pop_front()
                .expect("unexpected extra HTTP request"))
        })
    }
}

fn response(
    status: u16,
    location: Option<&str>,
    mime: Option<&str>,
    encodings: &[&str],
    bytes: &[u8],
) -> Response {
    Response {
        status_code: status,
        location: location.map(str::to_owned),
        content_type: mime.map(str::to_owned),
        content_encodings: encodings.iter().map(|value| (*value).to_owned()).collect(),
        body: Box::new(BytesBody::new(bytes)),
    }
}
fn options() -> Options {
    Options {
        max_redirects: 5,
        max_body_bytes: 256,
        timeout: Duration::from_secs(2),
        user_agent: "test-agent".to_owned(),
    }
}
fn fetcher(
    policy: Arc<dyn AddressPolicy>,
    resolver: Arc<dyn Resolver>,
    transport: Arc<dyn Transport>,
    options: Options,
) -> WebFetcher {
    WebFetcher::new(
        policy,
        resolver,
        transport,
        Arc::new(NoGzipExpected),
        options,
    )
}
fn get(address: &str) -> Request {
    Request {
        address: normalize_address(address).unwrap(),
        method: Method::Get,
    }
}
fn failed(
    result: Result<parrot_tools::web_fetch::FetchResult, OperationError<FetchError>>,
) -> String {
    match result {
        Err(OperationError::Failed(error)) => error.to_string(),
        other => panic!("expected fetch failure, got {other:?}"),
    }
}

#[test]
fn original_eighteen_public_address_rows() {
    for (address, expected) in [
        ("0.0.0.1", false),
        ("10.0.0.1", false),
        ("100.64.0.1", false),
        ("127.0.0.1", false),
        ("169.254.0.1", false),
        ("172.16.0.1", false),
        ("192.0.2.1", false),
        ("192.168.0.1", false),
        ("198.18.0.1", false),
        ("198.51.100.1", false),
        ("203.0.113.1", false),
        ("240.0.0.1", false),
        ("64:ff9b:1::1", false),
        ("100::1", false),
        ("2001:db8::1", false),
        ("8.8.8.8", true),
        ("1.1.1.1", true),
        ("2606:4700:4700::1111", true),
    ] {
        assert_eq!(
            PublicAddressPolicy.allows(address.parse().unwrap()),
            expected,
            "{address}"
        );
    }
}

#[test]
fn original_host_and_private_optin_rules() {
    assert!(!PublicAddressPolicy.allows_host("localhost"));
    assert!(!PublicAddressPolicy.allows_host("api.localhost"));
    assert!(PublicAddressPolicy.allows_host("example.com"));
    assert!(PrivateAddressPolicy.allows_host("localhost"));
    assert!(PrivateAddressPolicy.allows("127.0.0.1".parse().unwrap()));
    assert!(!PrivateAddressPolicy.allows("0.0.0.0".parse().unwrap()));
    assert!(!PrivateAddressPolicy.allows("224.0.0.1".parse().unwrap()));
}

#[test]
fn supplemental_address_ranges_and_ipv4_mapped_ipv6() {
    for address in [
        "::",
        "::1",
        "fe80::1",
        "fec0::1",
        "ff00::1",
        "fc00::1",
        "fd00::1",
        "2001:2::1",
        "::ffff:127.0.0.1",
        "::ffff:192.168.0.1",
        "192.0.0.1",
        "198.19.0.1",
    ] {
        assert!(
            !PublicAddressPolicy.allows(address.parse().unwrap()),
            "{address}"
        );
    }
    assert!(PublicAddressPolicy.allows("::ffff:8.8.8.8".parse().unwrap()));
    for address in [
        "100.63.255.255",
        "100.128.0.0",
        "172.15.255.255",
        "172.32.0.0",
        "198.20.0.0",
    ] {
        assert!(
            PublicAddressPolicy.allows(address.parse().unwrap()),
            "{address}"
        );
    }
}

#[test]
fn original_request_normalization_get_head_and_invalid_roots() {
    let request = Request::parse(r#"{"url":"example.com/path#part"}"#).unwrap();
    assert_eq!(request.address.as_str(), "https://example.com/path");
    assert_eq!(request.method, Method::Get);
    assert_eq!(
        Request::parse(r#"{"url":"https://example.com","method":" head "}"#)
            .unwrap()
            .method,
        Method::Head
    );
    assert_eq!(
        Request::parse("[]").unwrap_err().to_string(),
        "web fetch URL is required"
    );
    assert_eq!(
        Request::parse(r#"{"url":"https://example.com","method":"POST"}"#)
            .unwrap_err()
            .to_string(),
        "web fetch supports only GET and HEAD"
    );
    assert_eq!(
        Request::parse(r#"{"url":"bad","url":"example.com","ignored":true}"#)
            .unwrap()
            .address
            .as_str(),
        "https://example.com/"
    );
    for address in [
        "file:///tmp/a",
        "http://user:password@example.com",
        "",
        "http://",
    ] {
        assert!(normalize_address(address).is_err(), "{address}");
    }
}

#[tokio::test]
async fn public_policy_denies_loopback_before_transport_or_dns_and_canonicalizes_localhost() {
    let resolver = Arc::new(FixedResolver::default());
    let transport = Arc::new(ScriptedTransport::new(Vec::new()));
    let fetcher = fetcher(
        Arc::new(PublicAddressPolicy),
        resolver.clone(),
        transport.clone(),
        options(),
    );
    assert_eq!(
        failed(
            fetcher
                .fetch(get("http://127.0.0.1/blocked"), &NeverCancel)
                .await
        ),
        "web fetch host resolves only to forbidden addresses"
    );
    assert_eq!(
        failed(
            fetcher
                .fetch(get("http://LOCALHOST./blocked"), &NeverCancel)
                .await
        ),
        "web fetch local or empty host is forbidden"
    );
    assert_eq!(
        failed(
            fetcher
                .fetch(get("http://api.LOCALHOST./blocked"), &NeverCancel)
                .await
        ),
        "web fetch local or empty host is forbidden"
    );
    assert!(resolver.hosts.lock().unwrap().is_empty());
    assert!(transport.sent.lock().unwrap().is_empty());
}

#[tokio::test]
async fn resolves_once_filters_deduplicates_orders_and_pins_redirect_addresses() {
    let resolver = Arc::new(FixedResolver {
        hosts: Mutex::default(),
        addresses: [
            "::ffff:8.8.8.8",
            "127.0.0.1",
            "2606:4700:4700::1111",
            "1.1.1.1",
            "8.8.8.8",
        ]
        .map(|value| value.parse().unwrap())
        .to_vec(),
    });
    let transport = Arc::new(ScriptedTransport::new(vec![
        response(302, Some("/next#gone"), None, &[], b""),
        response(200, None, Some("text/plain"), &[], b"fetched"),
    ]));
    let fetcher = fetcher(
        Arc::new(PublicAddressPolicy),
        resolver.clone(),
        transport.clone(),
        options(),
    );
    let result = fetcher
        .fetch(get("http://Example.COM.:8080/start"), &NeverCancel)
        .await
        .unwrap();
    assert_eq!(
        result.final_address.as_str(),
        "http://example.com.:8080/next"
    );
    assert_eq!(result.text, "fetched");
    assert_eq!(*resolver.hosts.lock().unwrap(), ["example.com"]);
    let sent = transport.sent.lock().unwrap();
    assert_eq!(sent.len(), 2);
    assert_eq!(sent[0].2, sent[1].2);
    assert_eq!(
        sent[0].2,
        [
            "1.1.1.1:8080",
            "8.8.8.8:8080",
            "[2606:4700:4700::1111]:8080"
        ]
        .map(|value| value.parse().unwrap())
    );
}

#[tokio::test]
async fn redirects_revalidate_new_hosts_and_reject_forbidden_schemes_credentials_and_limit() {
    for (location, expected) in [
        (
            "file:///etc/passwd",
            "web fetch redirect used a forbidden address",
        ),
        (
            "http://user:secret@example.com",
            "web fetch redirect used a forbidden address",
        ),
        (
            "http://localhost/",
            "web fetch local or empty host is forbidden",
        ),
        (
            "http://127.0.0.1/",
            "web fetch host resolves only to forbidden addresses",
        ),
    ] {
        let resolver = Arc::new(FixedResolver {
            hosts: Mutex::default(),
            addresses: vec!["8.8.8.8".parse().unwrap()],
        });
        let transport = Arc::new(ScriptedTransport::new(vec![response(
            302,
            Some(location),
            None,
            &[],
            b"",
        )]));
        let fetcher = fetcher(
            Arc::new(PublicAddressPolicy),
            resolver,
            transport.clone(),
            options(),
        );
        assert_eq!(
            failed(
                fetcher
                    .fetch(get("http://example.com/"), &NeverCancel)
                    .await
            ),
            expected
        );
        assert_eq!(transport.sent.lock().unwrap().len(), 1);
    }
    let transport = Arc::new(ScriptedTransport::new(vec![response(
        302,
        Some("/again"),
        None,
        &[],
        b"",
    )]));
    let fetcher = fetcher(
        Arc::new(PrivateAddressPolicy),
        Arc::new(FixedResolver::default()),
        transport,
        Options {
            max_redirects: 0,
            ..options()
        },
    );
    assert_eq!(
        failed(fetcher.fetch(get("http://127.0.0.1/"), &NeverCancel).await),
        "web fetch followed too many redirects"
    );
}

#[tokio::test]
async fn html_plaintext_exact_bounds_and_utf8_controls() {
    let cases = [
        (
            "text/html; charset=utf-8",
            "<html><style>secret</style><body><h1>Hello &amp; bye</h1><script>alert(1)</script><p>Text\u{1}</p></body></html>",
            256,
            "Hello & bye\n\nText",
            false,
        ),
        (
            "text/html",
            "<h1>Title</h1><!-- comment > still comment --><p>A&nbsp; B</p><script>bad</script><div title=\">\">C</div>",
            256,
            "Title\n\nA B\n\nC",
            false,
        ),
        ("text/plain", "ab界", 4, "ab", true),
        (
            "text/plain",
            "a\u{1}b\u{fffd}\n\r\t",
            256,
            "ab\n\r\t",
            false,
        ),
        ("APPLICATION/JSON; charset=latin1", "{}", 2, "{}", false),
    ];
    for (mime, input, maximum, expected, truncated) in cases {
        let transport = Arc::new(ScriptedTransport::new(vec![response(
            200,
            None,
            Some(mime),
            &[],
            input.as_bytes(),
        )]));
        let fetcher = fetcher(
            Arc::new(PrivateAddressPolicy),
            Arc::new(FixedResolver::default()),
            transport,
            Options {
                max_body_bytes: maximum,
                ..options()
            },
        );
        let result = fetcher
            .fetch(get("http://127.0.0.1/"), &NeverCancel)
            .await
            .unwrap();
        assert_eq!(result.text, expected);
        assert_eq!(result.truncated, truncated);
    }
}

#[tokio::test]
async fn unsupported_mime_and_encodings_reject_without_decoder() {
    for (mime, encodings, expected) in [
        (
            None,
            vec![],
            "web fetch response used an unsupported content type",
        ),
        (
            Some("application/octet-stream"),
            vec![],
            "web fetch response used an unsupported content type",
        ),
        (
            Some("text/plain"),
            vec!["br"],
            "web fetch response used an unsupported content encoding",
        ),
        (
            Some("text/plain"),
            vec!["gzip", "identity"],
            "web fetch response used an unsupported content encoding",
        ),
    ] {
        let transport = Arc::new(ScriptedTransport::new(vec![response(
            200, None, mime, &encodings, b"body",
        )]));
        let fetcher = fetcher(
            Arc::new(PrivateAddressPolicy),
            Arc::new(FixedResolver::default()),
            transport,
            options(),
        );
        assert_eq!(
            failed(fetcher.fetch(get("http://127.0.0.1/"), &NeverCancel).await),
            expected
        );
    }
}

struct DecodedFixture {
    calls: AtomicUsize,
    reads: Arc<Mutex<Vec<usize>>>,
}
impl GzipDecoder for DecodedFixture {
    fn decode(
        &self,
        _compressed: Box<dyn ResponseBody>,
    ) -> Result<Box<dyn ResponseBody>, FetchError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Ok(Box::new(BytesBody {
            bytes: vec![b'a'; 300],
            position: 0,
            reads: self.reads.clone(),
        }))
    }
}

#[tokio::test]
async fn injected_decoder_output_bound_is_not_a_real_gzip_implementation_claim() {
    let decoder = Arc::new(DecodedFixture {
        calls: AtomicUsize::new(0),
        reads: Arc::default(),
    });
    let transport = Arc::new(ScriptedTransport::new(vec![response(
        200,
        None,
        Some("text/plain"),
        &["GZIP"],
        b"not-a-real-gzip-fixture",
    )]));
    let fetcher = WebFetcher::new(
        Arc::new(PrivateAddressPolicy),
        Arc::new(FixedResolver::default()),
        transport,
        decoder.clone(),
        options(),
    );
    let result = fetcher
        .fetch(get("http://127.0.0.1/"), &NeverCancel)
        .await
        .unwrap();
    assert_eq!(result.text, "a".repeat(256));
    assert!(result.truncated);
    assert_eq!(decoder.calls.load(Ordering::SeqCst), 1);
    assert_eq!(*decoder.reads.lock().unwrap(), [257]);
}

struct PendingTransport;
impl Transport for PendingTransport {
    fn session(&self) -> Result<Box<dyn TransportSession + '_>, FetchError> {
        Ok(Box::new(self))
    }
}
impl TransportSession for &PendingTransport {
    fn send<'a>(
        &'a mut self,
        _: &'a Request,
        _: &'a [SocketAddr],
        _: &'a Options,
    ) -> BoxFuture<'a, Result<Response, FetchError>> {
        Box::pin(std::future::pending())
    }
}

#[tokio::test]
async fn timeout_and_caller_cancellation_remain_distinct() {
    let fetcher = fetcher(
        Arc::new(PrivateAddressPolicy),
        Arc::new(FixedResolver::default()),
        Arc::new(PendingTransport),
        Options {
            timeout: Duration::from_millis(15),
            ..options()
        },
    );
    assert_eq!(
        failed(fetcher.fetch(get("http://127.0.0.1/"), &NeverCancel).await),
        "web fetch timed out"
    );
    let cancellation = Arc::new(CancelSignal::default());
    let signal = cancellation.clone();
    let cancel = async move {
        tokio::time::sleep(Duration::from_millis(2)).await;
        signal.cancel();
    };
    let fetch = fetcher.fetch(get("http://127.0.0.1/"), cancellation.as_ref());
    let (result, ()) = tokio::join!(fetch, cancel);
    assert!(matches!(result, Err(OperationError::Cancelled)));
}

fn local_server(responses: Vec<Vec<u8>>) -> (u16, std::thread::JoinHandle<Vec<String>>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    listener.set_nonblocking(true).unwrap();
    let serving = std::thread::spawn(move || {
        let mut requests = Vec::new();
        for response in responses {
            let deadline = Instant::now() + Duration::from_secs(4);
            let mut socket = loop {
                match listener.accept() {
                    Ok((socket, _)) => break socket,
                    Err(error)
                        if error.kind() == io::ErrorKind::WouldBlock
                            && Instant::now() < deadline =>
                    {
                        std::thread::yield_now()
                    }
                    error => panic!("loopback accept: {error:?}"),
                }
            };
            socket
                .set_read_timeout(Some(Duration::from_secs(2)))
                .unwrap();
            let mut request = Vec::new();
            while !request.ends_with(b"\r\n\r\n") {
                let mut byte = [0];
                assert_eq!(socket.read(&mut byte).unwrap(), 1);
                request.push(byte[0]);
                assert!(request.len() < 16384);
            }
            requests.push(String::from_utf8(request).unwrap());
            socket.write_all(&response).unwrap();
        }
        requests
    });
    (port, serving)
}
fn wire_response(status: &str, content_type: &str, extra: &str, body: &str) -> Vec<u8> {
    format!("HTTP/1.1 {status}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n{extra}\r\n{body}", body.len()).into_bytes()
}

#[tokio::test]
async fn actual_loopback_transport_get_redirect_headers_html_and_head() {
    let (port, serving) = local_server(vec![
        wire_response("302 Found", "text/plain", "Location: /html\r\n", ""),
        wire_response(
            "200 OK",
            "text/html; charset=utf-8",
            "",
            "<h1>Hello &amp; bye</h1><p>Text</p>",
        ),
        wire_response("200 OK", "text/plain", "", ""),
    ]);
    let resolver = Arc::new(FixedResolver {
        hosts: Mutex::default(),
        addresses: vec!["127.0.0.1".parse().unwrap()],
    });
    let fetcher = WebFetcher::new(
        Arc::new(PrivateAddressPolicy),
        resolver.clone(),
        Arc::new(parrot_tools::web_fetch::ReqwestTransport::new(Arc::new(
            NoCookiesExpected,
        ))),
        Arc::new(NoGzipExpected),
        options(),
    );
    let address = format!("http://t56.invalid:{port}/start#fragment");
    let result = fetcher.fetch(get(&address), &NeverCancel).await.unwrap();
    assert_eq!(result.final_address.path(), "/html");
    assert_eq!(result.text, "Hello & bye\n\nText");
    let mut head = get(&format!("http://t56.invalid:{port}/head"));
    head.method = Method::Head;
    assert_eq!(fetcher.fetch(head, &NeverCancel).await.unwrap().text, "");
    let requests = serving.join().unwrap();
    assert!(requests[0].starts_with("GET /start HTTP/1.1\r\n"));
    assert!(requests[1].starts_with("GET /html HTTP/1.1\r\n"));
    assert!(requests[2].starts_with("HEAD /head HTTP/1.1\r\n"));
    let headers = requests[0].to_ascii_lowercase();
    assert!(headers.contains("accept-encoding: gzip\r\n"));
    assert!(headers.contains("user-agent: test-agent\r\n"));
    assert!(headers.contains(&format!("host: t56.invalid:{port}\r\n")));
    assert_eq!(
        *resolver.hosts.lock().unwrap(),
        ["t56.invalid", "t56.invalid"]
    );
}

struct NoFilesystemAccess;
impl AccessSnapshot for NoFilesystemAccess {
    type Error = String;
    fn check_read(&self, _: &ResolvedRead) -> Result<(), String> {
        panic!("web fetch does not read files")
    }
    fn check_write(&self, _: &Path) -> Result<(), String> {
        panic!("web fetch does not write files")
    }
}

#[tokio::test]
async fn actual_tool_uses_definition_parallel_default_plain_result_and_errors() {
    let transport = Arc::new(ScriptedTransport::new(vec![response(
        200,
        None,
        Some("text/plain"),
        &[],
        b"fetched",
    )]));
    let fetcher = Arc::new(fetcher(
        Arc::new(PrivateAddressPolicy),
        Arc::new(FixedResolver::default()),
        transport,
        options(),
    ));
    let definition = ConfiguredToolDefinition::new("configured web fetch", "{\"type\":\"object\"}");
    let tool = WebFetchTool::<NoFilesystemAccess, ()>::new(fetcher, definition.clone());
    let invocation = |arguments: &str| Invocation {
        arguments_json: arguments.to_owned(),
        prompt_templates: None,
    };
    assert_eq!(tool.definition(), &definition);
    assert_eq!(
        tool.parallel_safety(&invocation("[]")),
        ParallelSafety::ParallelSafe
    );
    assert!(!tool.enabled_after_interruption());
    assert_eq!(
        tool.invoke(
            invocation("[]"),
            InvocationContext::new(NoFilesystemAccess, (), &NeverCancel)
        )
        .await
        .unwrap()
        .text,
        "error: web fetch URL is required"
    );
    assert_eq!(
        tool.invoke(
            invocation(r#"{"url":"https://example.com","method":"POST"}"#),
            InvocationContext::new(NoFilesystemAccess, (), &NeverCancel)
        )
        .await
        .unwrap()
        .text,
        "error: web fetch supports only GET and HEAD"
    );
    assert_eq!(
        tool.invoke(
            invocation(r#"{"url":"http://127.0.0.1/tool","method":"get"}"#),
            InvocationContext::new(NoFilesystemAccess, (), &NeverCancel)
        )
        .await
        .unwrap()
        .text,
        "fetched"
    );
}
