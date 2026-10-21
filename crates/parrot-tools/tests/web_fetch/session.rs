use super::*;

type CookieEvents = Arc<Mutex<Vec<(usize, String, Vec<String>)>>>;

#[derive(Default)]
struct CookieSpyFactory {
    created: AtomicUsize,
    observed: CookieEvents,
}
impl CookieStoreFactory for CookieSpyFactory {
    fn create(&self) -> Arc<dyn CookieStore> {
        let id = self.created.fetch_add(1, Ordering::SeqCst);
        Arc::new(CookieSpy {
            id,
            saw_fixture: AtomicBool::new(false),
            observed: self.observed.clone(),
        })
    }
}
struct CookieSpy {
    id: usize,
    saw_fixture: AtomicBool,
    observed: CookieEvents,
}
impl CookieStore for CookieSpy {
    fn request_header(&self, _: &url::Url) -> Option<String> {
        self.saw_fixture
            .load(Ordering::SeqCst)
            .then(|| "fixture=supplied".to_owned())
    }
    fn store_response(&self, address: &url::Url, set_cookie: &[String]) {
        self.observed.lock().unwrap().push((
            self.id,
            address.path().to_owned(),
            set_cookie.to_vec(),
        ));
        if !set_cookie.is_empty() {
            assert_eq!(set_cookie, ["fixture=supplied; Path=/"]);
            self.saw_fixture.store(true, Ordering::SeqCst);
        }
    }
}

#[tokio::test]
async fn actual_loopback_content_type_decodes_latin1_obs_text_before_validation() {
    let mut wire = b"HTTP/1.1 200 OK\r\nContent-Type: text/plain; foo=\"a".to_vec();
    wire.push(0xe9);
    wire.extend_from_slice(b"b\"\r\nContent-Length: 7\r\nConnection: close\r\n\r\nfetched");
    let (port, serving) = local_server(vec![wire]);
    let fetcher = WebFetcher::new(
        Arc::new(PrivateAddressPolicy),
        Arc::new(FixedResolver::default()),
        Arc::new(parrot_tools::web_fetch::ReqwestTransport::new(Arc::new(
            NoCookiesExpected,
        ))),
        Arc::new(NoGzipExpected),
        options(),
    );
    let result = fetcher
        .fetch(
            get(&format!("http://127.0.0.1:{port}/latin1")),
            &NeverCancel,
        )
        .await
        .unwrap();
    assert_eq!(result.content_type, "text/plain");
    assert_eq!(result.text, "fetched");
    assert_eq!(serving.join().unwrap().len(), 1);
}

#[tokio::test]
async fn injected_cookie_backend_call_order_and_fetch_scope_are_not_cookie_parsing_proof() {
    let (port, serving) = local_server(vec![
        wire_response(
            "302 Found",
            "text/plain",
            "Location: /next\r\nSet-Cookie: fixture=supplied; Path=/\r\n",
            "",
        ),
        wire_response("200 OK", "text/plain", "", "first"),
        wire_response("200 OK", "text/plain", "", "second"),
    ]);
    let factory = Arc::new(CookieSpyFactory::default());
    let resolver = Arc::new(FixedResolver {
        hosts: Mutex::default(),
        addresses: vec!["127.0.0.1".parse().unwrap()],
    });
    let fetcher = WebFetcher::new(
        Arc::new(PrivateAddressPolicy),
        resolver,
        Arc::new(parrot_tools::web_fetch::ReqwestTransport::new(
            factory.clone(),
        )),
        Arc::new(NoGzipExpected),
        options(),
    );
    let base = format!("http://fixture.invalid:{port}");
    assert_eq!(
        fetcher
            .fetch(get(&format!("{base}/start")), &NeverCancel)
            .await
            .unwrap()
            .text,
        "first"
    );
    assert_eq!(
        fetcher
            .fetch(get(&format!("{base}/new-fetch")), &NeverCancel)
            .await
            .unwrap()
            .text,
        "second"
    );
    let requests = serving.join().unwrap();
    assert!(!requests[0].to_ascii_lowercase().contains("\r\ncookie:"));
    assert!(
        requests[1]
            .to_ascii_lowercase()
            .contains("\r\ncookie: fixture=supplied\r\n")
    );
    assert!(!requests[2].to_ascii_lowercase().contains("\r\ncookie:"));
    assert_eq!(factory.created.load(Ordering::SeqCst), 2);
    assert_eq!(
        *factory.observed.lock().unwrap(),
        [
            (
                0,
                "/start".to_owned(),
                vec!["fixture=supplied; Path=/".to_owned()]
            ),
            (0, "/next".to_owned(), Vec::new()),
            (1, "/new-fetch".to_owned(), Vec::new()),
        ]
    );
}
