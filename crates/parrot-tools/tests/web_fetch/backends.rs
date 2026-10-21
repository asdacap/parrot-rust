use super::*;
use parrot_tools::web_fetch::{FetchCookieStoreFactory, GzipStreamDecoder, ReqwestTransport};

// node:zlib gzipSync(Buffer.alloc(512, 97), { mtime: 0 }); verified by gunzipSync.
const GZIP_512_A: &[u8] = &[
    0x1f, 0x8b, 0x08, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x03, 0x4b, 0x4c, 0x1c, 0x05, 0x23, 0x19,
    0x00, 0x00, 0x51, 0x70, 0x51, 0xf9, 0x00, 0x02, 0x00, 0x00,
];

#[tokio::test]
async fn real_gzip_expansion_is_limited_to_decoded_body_bytes() {
    let fetcher = WebFetcher::new(
        Arc::new(PrivateAddressPolicy),
        Arc::new(FixedResolver::default()),
        Arc::new(ScriptedTransport::new(vec![response(
            200,
            None,
            Some("text/plain"),
            &["gzip"],
            GZIP_512_A,
        )])),
        Arc::new(GzipStreamDecoder),
        options(),
    );
    let result = fetcher
        .fetch(get("http://127.0.0.1/expanded"), &NeverCancel)
        .await
        .unwrap();
    assert_eq!(result.text, "a".repeat(256));
    assert!(result.truncated);
}

#[tokio::test]
async fn real_gzip_decoder_rejects_malformed_compressed_bytes() {
    let fetcher = WebFetcher::new(
        Arc::new(PrivateAddressPolicy),
        Arc::new(FixedResolver::default()),
        Arc::new(ScriptedTransport::new(vec![response(
            200,
            None,
            Some("text/plain"),
            &["gzip"],
            b"this is not a gzip stream",
        )])),
        Arc::new(GzipStreamDecoder),
        options(),
    );
    assert!(matches!(
        fetcher
            .fetch(get("http://127.0.0.1/malformed"), &NeverCancel)
            .await,
        Err(OperationError::Failed(_))
    ));
}

struct FailedCompressedBody;

impl ResponseBody for FailedCompressedBody {
    fn read<'a>(&'a mut self, _: &'a mut [u8]) -> BoxFuture<'a, Result<usize, FetchError>> {
        Box::pin(async {
            Err(FetchError::Rejected(
                "scripted transport failure".to_owned(),
            ))
        })
    }
}

#[tokio::test]
async fn real_gzip_preserves_underlying_transport_failure() {
    let mut decoded = GzipStreamDecoder
        .decode(Box::new(FailedCompressedBody))
        .unwrap();
    let error = decoded.read(&mut [0; 8]).await.unwrap_err();
    assert!(error.is_tool_error());
    assert_eq!(error.to_string(), "scripted transport failure");
}

#[tokio::test]
async fn real_gzip_decoder_reads_concatenated_members() {
    let compressed = [GZIP_512_A, GZIP_512_A].concat();
    let fetcher = WebFetcher::new(
        Arc::new(PrivateAddressPolicy),
        Arc::new(FixedResolver::default()),
        Arc::new(ScriptedTransport::new(vec![response(
            200,
            None,
            Some("text/plain"),
            &["gzip"],
            &compressed,
        )])),
        Arc::new(GzipStreamDecoder),
        Options {
            max_body_bytes: 2048,
            ..options()
        },
    );
    let result = fetcher
        .fetch(get("http://127.0.0.1/members"), &NeverCancel)
        .await
        .unwrap();
    assert_eq!(result.text, "a".repeat(1024));
    assert!(!result.truncated);
}

struct PendingGzipBody {
    polled: Arc<tokio::sync::Notify>,
    dropped: Arc<AtomicBool>,
}

impl ResponseBody for PendingGzipBody {
    fn read<'a>(&'a mut self, _: &'a mut [u8]) -> BoxFuture<'a, Result<usize, FetchError>> {
        Box::pin(async move {
            self.polled.notify_one();
            std::future::pending().await
        })
    }
}

impl Drop for PendingGzipBody {
    fn drop(&mut self) {
        self.dropped.store(true, Ordering::SeqCst);
    }
}

#[tokio::test]
async fn real_gzip_cancellation_drops_pending_compressed_body() {
    let polled = Arc::new(tokio::sync::Notify::new());
    let dropped = Arc::new(AtomicBool::new(false));
    let fetcher = WebFetcher::new(
        Arc::new(PrivateAddressPolicy),
        Arc::new(FixedResolver::default()),
        Arc::new(ScriptedTransport::new(vec![Response {
            status_code: 200,
            location: None,
            content_type: Some("text/plain".to_owned()),
            content_encodings: vec!["gzip".to_owned()],
            body: Box::new(PendingGzipBody {
                polled: polled.clone(),
                dropped: dropped.clone(),
            }),
        }])),
        Arc::new(GzipStreamDecoder),
        options(),
    );
    let cancellation = CancelSignal::default();
    let fetch = fetcher.fetch(get("http://127.0.0.1/pending-gzip"), &cancellation);
    tokio::pin!(fetch);
    tokio::select! {
        () = polled.notified() => {}
        result = &mut fetch => panic!("fetch completed before polling gzip body: {result:?}"),
    }
    assert!(!dropped.load(Ordering::SeqCst));
    cancellation.cancel();
    assert!(matches!(fetch.await, Err(OperationError::Cancelled)));
    assert!(dropped.load(Ordering::SeqCst));
}

#[tokio::test]
async fn actual_loopback_gzip_uses_real_decoder_and_decoded_limit() {
    let mut wire = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nContent-Encoding: gzip\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        GZIP_512_A.len()
    )
    .into_bytes();
    wire.extend_from_slice(GZIP_512_A);
    let (port, serving) = local_server(vec![wire]);
    let resolver = Arc::new(FixedResolver {
        hosts: Mutex::default(),
        addresses: vec!["127.0.0.1".parse().unwrap()],
    });
    let fetcher = WebFetcher::new(
        Arc::new(PrivateAddressPolicy),
        resolver.clone(),
        Arc::new(ReqwestTransport::new(Arc::new(FetchCookieStoreFactory))),
        Arc::new(GzipStreamDecoder),
        options(),
    );
    let result = fetcher
        .fetch(
            get(&format!("http://fixture.invalid:{port}/compressed")),
            &NeverCancel,
        )
        .await
        .unwrap();
    assert_eq!(result.text, "a".repeat(256));
    assert!(result.truncated);
    let requests = serving.join().unwrap();
    assert_eq!(requests.len(), 1);
    assert!(requests[0].starts_with("GET /compressed HTTP/1.1\r\n"));
    assert!(
        requests[0]
            .to_ascii_lowercase()
            .contains("accept-encoding: gzip\r\n")
    );
    assert_eq!(*resolver.hosts.lock().unwrap(), ["fixture.invalid"]);
}

#[tokio::test]
async fn production_constructor_supplies_real_backends() {
    let mut wire = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nContent-Encoding: gzip\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        GZIP_512_A.len()
    )
    .into_bytes();
    wire.extend_from_slice(GZIP_512_A);
    let (port, serving) = local_server(vec![wire]);
    let result = WebFetcher::production(Arc::new(PrivateAddressPolicy))
        .fetch(
            get(&format!("http://127.0.0.1:{port}/production")),
            &NeverCancel,
        )
        .await
        .unwrap();
    assert_eq!(result.text, "a".repeat(512));
    assert!(!result.truncated);
    assert_eq!(serving.join().unwrap().len(), 1);
}

struct GzipPrefixBody {
    prefix: Option<&'static [u8]>,
    reads: Arc<AtomicUsize>,
}

impl ResponseBody for GzipPrefixBody {
    fn read<'a>(&'a mut self, output: &'a mut [u8]) -> BoxFuture<'a, Result<usize, FetchError>> {
        Box::pin(async move {
            self.reads.fetch_add(1, Ordering::SeqCst);
            match self.prefix.take() {
                Some(prefix) => {
                    output[..prefix.len()].copy_from_slice(prefix);
                    Ok(prefix.len())
                }
                None => std::future::pending().await,
            }
        })
    }
}

#[tokio::test]
async fn decoded_limit_finishes_without_reading_the_gzip_tail() {
    let reads = Arc::new(AtomicUsize::new(0));
    let fetcher = WebFetcher::new(
        Arc::new(PrivateAddressPolicy),
        Arc::new(FixedResolver::default()),
        Arc::new(ScriptedTransport::new(vec![Response {
            status_code: 200,
            location: None,
            content_type: Some("text/plain".to_owned()),
            content_encodings: vec!["gzip".to_owned()],
            body: Box::new(GzipPrefixBody {
                prefix: Some(&GZIP_512_A[..18]),
                reads: reads.clone(),
            }),
        }])),
        Arc::new(GzipStreamDecoder),
        options(),
    );
    let result = fetcher
        .fetch(get("http://127.0.0.1/partial"), &NeverCancel)
        .await
        .unwrap();
    assert_eq!(result.text, "a".repeat(256));
    assert!(result.truncated);
    assert_eq!(reads.load(Ordering::SeqCst), 1);
}

#[test]
fn real_cookie_store_host_only_cookie_excludes_subdomains() {
    let jar = FetchCookieStoreFactory.create();
    let address = url::Url::parse("https://fixture.invalid/start").unwrap();
    jar.store_response(&address, &["host=only; Path=/".to_owned()]);
    assert_eq!(jar.request_header(&address).as_deref(), Some("host=only"));
    assert_eq!(
        jar.request_header(&url::Url::parse("https://sub.fixture.invalid/").unwrap()),
        None
    );
    assert_eq!(
        jar.request_header(&url::Url::parse("https://unrelated.invalid/").unwrap()),
        None
    );
}

#[test]
fn real_cookie_store_domain_matches_parent_and_subdomains_but_rejects_unrelated() {
    let jar = FetchCookieStoreFactory.create();
    let address = url::Url::parse("https://api.fixture.invalid/start").unwrap();
    jar.store_response(
        &address,
        &[
            "domain=shared; Domain=fixture.invalid; Path=/".to_owned(),
            "rejected=unrelated; Domain=unrelated.invalid; Path=/".to_owned(),
        ],
    );
    for address in [
        "https://fixture.invalid/",
        "https://api.fixture.invalid/",
        "https://other.fixture.invalid/",
    ] {
        assert_eq!(
            jar.request_header(&url::Url::parse(address).unwrap())
                .as_deref(),
            Some("domain=shared"),
            "{address}"
        );
    }
    for address in ["https://unrelated.invalid/", "https://notfixture.invalid/"] {
        assert_eq!(
            jar.request_header(&url::Url::parse(address).unwrap()),
            None,
            "{address}"
        );
    }
}

#[test]
fn real_cookie_store_explicit_path_requires_a_path_boundary() {
    let jar = FetchCookieStoreFactory.create();
    jar.store_response(
        &url::Url::parse("https://fixture.invalid/start").unwrap(),
        &["scoped=value; Path=/docs".to_owned()],
    );
    for path in ["/docs", "/docs/", "/docs/child"] {
        assert_eq!(
            jar.request_header(
                &url::Url::parse(&format!("https://fixture.invalid{path}")).unwrap()
            )
            .as_deref(),
            Some("scoped=value"),
            "{path}"
        );
    }
    for path in ["/", "/doc", "/docs-other"] {
        assert_eq!(
            jar.request_header(
                &url::Url::parse(&format!("https://fixture.invalid{path}")).unwrap()
            ),
            None,
            "{path}"
        );
    }
}

#[test]
fn real_cookie_store_default_path_is_the_response_directory() {
    let jar = FetchCookieStoreFactory.create();
    jar.store_response(
        &url::Url::parse("https://fixture.invalid/docs/start").unwrap(),
        &["directory=value".to_owned()],
    );
    for path in ["/docs", "/docs/next"] {
        assert_eq!(
            jar.request_header(
                &url::Url::parse(&format!("https://fixture.invalid{path}")).unwrap()
            )
            .as_deref(),
            Some("directory=value"),
            "{path}"
        );
    }
    for path in ["/", "/docs-other/next"] {
        assert_eq!(
            jar.request_header(
                &url::Url::parse(&format!("https://fixture.invalid{path}")).unwrap()
            ),
            None,
            "{path}"
        );
    }
}

#[test]
fn real_cookie_store_secure_cookie_is_sent_only_over_https() {
    let jar = FetchCookieStoreFactory.create();
    let secure = url::Url::parse("https://fixture.invalid/").unwrap();
    jar.store_response(&secure, &["secure=value; Secure; Path=/".to_owned()]);
    assert_eq!(jar.request_header(&secure).as_deref(), Some("secure=value"));
    assert_eq!(
        jar.request_header(&url::Url::parse("http://fixture.invalid/").unwrap()),
        None
    );
}

#[test]
fn real_cookie_store_replaces_same_name_domain_and_path() {
    let jar = FetchCookieStoreFactory.create();
    let address = url::Url::parse("https://fixture.invalid/").unwrap();
    jar.store_response(&address, &["session=old; Path=/".to_owned()]);
    jar.store_response(&address, &["session=new; Path=/".to_owned()]);
    assert_eq!(jar.request_header(&address).as_deref(), Some("session=new"));
}

#[test]
fn real_cookie_store_max_age_zero_removes_existing_cookie() {
    let jar = FetchCookieStoreFactory.create();
    let address = url::Url::parse("https://fixture.invalid/").unwrap();
    jar.store_response(&address, &["session=live; Path=/".to_owned()]);
    assert_eq!(
        jar.request_header(&address).as_deref(),
        Some("session=live")
    );
    jar.store_response(&address, &["session=deleted; Path=/; Max-Age=0".to_owned()]);
    assert_eq!(jar.request_header(&address), None);
}

#[test]
fn real_cookie_store_past_expires_removes_existing_cookie() {
    let jar = FetchCookieStoreFactory.create();
    let address = url::Url::parse("https://fixture.invalid/").unwrap();
    jar.store_response(&address, &["session=live; Path=/".to_owned()]);
    assert_eq!(
        jar.request_header(&address).as_deref(),
        Some("session=live")
    );
    jar.store_response(
        &address,
        &["session=deleted; Path=/; Expires=Wed, 01 Jan 1997 00:00:00 GMT".to_owned()],
    );
    assert_eq!(jar.request_header(&address), None);
}

#[test]
fn real_cookie_factory_creates_isolated_jars() {
    let factory = FetchCookieStoreFactory;
    let first = factory.create();
    let second = factory.create();
    let address = url::Url::parse("https://fixture.invalid/").unwrap();
    first.store_response(&address, &["session=first; Path=/".to_owned()]);
    assert_eq!(
        first.request_header(&address).as_deref(),
        Some("session=first")
    );
    assert_eq!(second.request_header(&address), None);
    second.store_response(&address, &["session=second; Path=/".to_owned()]);
    assert_eq!(
        second.request_header(&address).as_deref(),
        Some("session=second")
    );
    assert_eq!(
        first.request_header(&address).as_deref(),
        Some("session=first")
    );
    assert_eq!(factory.create().request_header(&address), None);
}

#[tokio::test]
async fn actual_loopback_real_cookie_jar_keeps_redirect_cookie_but_not_next_fetch() {
    let (port, serving) = local_server(vec![
        wire_response(
            "302 Found",
            "text/plain",
            "Location: /next\r\nSet-Cookie: fixture=retained; Path=/\r\nSet-Cookie: rejected=value; Domain=unrelated.invalid; Path=/\r\nSet-Cookie: not-a-cookie\r\n",
            "",
        ),
        wire_response("200 OK", "text/plain", "", "first"),
        wire_response("200 OK", "text/plain", "", "second"),
    ]);
    let resolver = Arc::new(FixedResolver {
        hosts: Mutex::default(),
        addresses: vec!["127.0.0.1".parse().unwrap()],
    });
    let fetcher = WebFetcher::new(
        Arc::new(PrivateAddressPolicy),
        resolver.clone(),
        Arc::new(ReqwestTransport::new(Arc::new(FetchCookieStoreFactory))),
        Arc::new(GzipStreamDecoder),
        options(),
    );
    let base = format!("http://fixture.invalid:{port}");
    let first = fetcher
        .fetch(get(&format!("{base}/start")), &NeverCancel)
        .await
        .unwrap();
    assert_eq!(first.final_address.path(), "/next");
    assert_eq!(first.text, "first");
    assert_eq!(
        fetcher
            .fetch(get(&format!("{base}/new-fetch")), &NeverCancel)
            .await
            .unwrap()
            .text,
        "second"
    );
    let requests = serving.join().unwrap();
    assert!(requests[0].starts_with("GET /start HTTP/1.1\r\n"));
    assert!(requests[1].starts_with("GET /next HTTP/1.1\r\n"));
    assert!(requests[2].starts_with("GET /new-fetch HTTP/1.1\r\n"));
    assert!(!requests[0].to_ascii_lowercase().contains("\r\ncookie:"));
    assert!(
        requests[1]
            .to_ascii_lowercase()
            .contains("\r\ncookie: fixture=retained\r\n")
    );
    assert!(!requests[1].contains("rejected="));
    assert!(!requests[1].contains("not-a-cookie"));
    assert!(!requests[2].to_ascii_lowercase().contains("\r\ncookie:"));
    assert_eq!(
        *resolver.hosts.lock().unwrap(),
        ["fixture.invalid", "fixture.invalid"]
    );
}
