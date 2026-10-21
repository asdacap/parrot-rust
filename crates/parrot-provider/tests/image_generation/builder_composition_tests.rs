//! T34 composition proof: real T32 builder, T30 providers/retry and T34 S3/linking.
use parrot_domain::contracts::async_ops::{BoxFuture, Cancellation};
use parrot_foundation::config::{Configuration, runtime::ImageUploadConfig};
use parrot_provider::{
    http_stream::{ByteStream, HttpError, HttpMethod, HttpRequest, HttpResponse, HttpTransport},
    image_generation::{
        ImageGenerationRequest, ImageHttpRequest, ImageTransport,
        bucket::{Clock, CredentialSource, StoredCredential},
    },
    responses_websocket::{ConnectedSocket, Connector, Headers, WebSocketError},
    routing::{
        auth::CredentialSources,
        builder::{
            BuildServices, HttpClients, ProviderRegistryBuilder, S3ImageBucketFactory,
            SelectedHttpClient,
        },
        models_dev::ModelsDevInformationProvider,
    },
    sessions::{
        Content, Error, Event, Message, Provider, Request, Role,
        provider::{ApiKeySource, OAuthAccess, OAuthTokenSource},
    },
};
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeMap,
    path::PathBuf,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    time::Duration,
};
use tokio::sync::Notify;

const SUCCESS: &str = "data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"answer\"},\"finish_reason\":\"stop\"}],\"usage\":{\"prompt_tokens\":1,\"completion_tokens\":1}}\n\ndata: [DONE]\n\n";
const PNG: &str = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR4nGP4z8DwHwAFAAH/iZk9HQAAAABJRU5ErkJggg==";
struct Token {
    cancelled: AtomicBool,
    notify: Notify,
}
impl Token {
    fn new(cancelled: bool) -> Self {
        Self {
            cancelled: AtomicBool::new(cancelled),
            notify: Notify::new(),
        }
    }
}
impl Cancellation for Token {
    fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::Acquire)
    }
    fn cancelled(&self) -> BoxFuture<'_, ()> {
        Box::pin(async move {
            loop {
                let notified = self.notify.notified();
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
struct Body {
    bytes: Vec<u8>,
    offset: usize,
}
impl ByteStream for Body {
    fn read<'a>(
        &'a mut self,
        output: &'a mut [u8],
        ct: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<usize, HttpError>> {
        Box::pin(async move {
            if ct.is_cancelled() {
                return Err(HttpError::Cancelled);
            }
            let count = output.len().min(self.bytes.len() - self.offset);
            output[..count].copy_from_slice(&self.bytes[self.offset..self.offset + count]);
            self.offset += count;
            Ok(count)
        })
    }
}
fn response(status: u16, bytes: impl Into<Vec<u8>>) -> HttpResponse {
    HttpResponse {
        status,
        reason: String::new(),
        headers: vec![],
        body: Box::new(Body {
            bytes: bytes.into(),
            offset: 0,
        }),
    }
}
#[derive(Clone, Copy, PartialEq)]
enum Behavior {
    Success,
    RejectUrl,
    MissingObject,
    GatedPrepare,
}
#[derive(Clone)]
struct Recorded {
    kind: &'static str,
    method: String,
    url: String,
    headers: Vec<(String, String)>,
    content_type: Option<String>,
    body: Vec<u8>,
}
impl Recorded {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }
}
struct PairedHttp {
    requests: Mutex<Vec<Recorded>>,
    resolutions: Mutex<Vec<(String, bool)>>,
    chat_calls: AtomicUsize,
    behavior: Behavior,
    generation_status: u16,
    release_prepare: AtomicBool,
    prepare_notify: Notify,
}
impl PairedHttp {
    fn new(behavior: Behavior, generation_status: u16) -> Arc<Self> {
        Arc::new(Self {
            requests: Mutex::new(vec![]),
            resolutions: Mutex::new(vec![]),
            chat_calls: AtomicUsize::new(0),
            behavior,
            generation_status,
            release_prepare: AtomicBool::new(false),
            prepare_notify: Notify::new(),
        })
    }
    fn image_requests(&self) -> Vec<Recorded> {
        self.requests
            .lock()
            .unwrap()
            .iter()
            .filter(|r| r.kind == "image")
            .cloned()
            .collect()
    }
    fn chat_bodies(&self) -> Vec<serde_json::Value> {
        self.requests
            .lock()
            .unwrap()
            .iter()
            .filter(|r| r.kind == "stream" && r.method == "POST")
            .map(|r| serde_json::from_slice(&r.body).unwrap())
            .collect()
    }
    fn release(&self) {
        self.release_prepare.store(true, Ordering::Release);
        self.prepare_notify.notify_waiters();
    }
}
// Both selected views below use this SAME Arc and recorder. They are not two
// independent mocks whose behavior merely happens to agree.
impl HttpTransport for PairedHttp {
    fn send<'a>(
        &'a self,
        request: HttpRequest,
        ct: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<HttpResponse, HttpError>> {
        Box::pin(async move {
            let method = match request.method {
                HttpMethod::Get => "GET",
                HttpMethod::Post => "POST",
            };
            let post = method == "POST";
            let url = request.endpoint.as_str().to_owned();
            self.requests.lock().unwrap().push(Recorded {
                kind: "stream",
                method: method.into(),
                url: url.clone(),
                headers: request.headers,
                content_type: None,
                body: request.body,
            });
            if ct.is_cancelled() {
                return Err(HttpError::Cancelled);
            }
            if post {
                let call = self.chat_calls.fetch_add(1, Ordering::SeqCst);
                if call == 0
                    && matches!(self.behavior, Behavior::RejectUrl | Behavior::MissingObject)
                {
                    return Ok(response(
                        400,
                        br#"{"error":{"type":"invalid_request_error","message":"bad image"}}"#
                            .to_vec(),
                    ));
                }
                Ok(response(200, SUCCESS.as_bytes().to_vec()))
            } else if url == "https://models.dev/api.json" {
                Ok(response(200, b"{}".to_vec()))
            } else if url.ends_with("/model/info") {
                Ok(response(200, br#"{"data":[]}"#.to_vec()))
            } else {
                Ok(response(200, br#"{"data":[{"id":"m"}]}"#.to_vec()))
            }
        })
    }
}
impl ImageTransport for PairedHttp {
    fn send<'a>(
        &'a self,
        request: ImageHttpRequest,
        ct: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<HttpResponse, Error>> {
        Box::pin(async move {
            let path = request.endpoint.as_url().path().to_owned();
            let method = request.method.clone();
            let generation = path.contains("/images/");
            self.requests.lock().unwrap().push(Recorded {
                kind: "image",
                method: method.clone(),
                url: request.endpoint.as_str().to_owned(),
                headers: request.headers,
                content_type: request.content_type,
                body: request.body,
            });
            if self.behavior == Behavior::GatedPrepare && method == "HEAD" && path == "/b" {
                loop {
                    let notified = self.prepare_notify.notified();
                    tokio::pin!(notified);
                    notified.as_mut().enable();
                    if self.release_prepare.load(Ordering::Acquire) {
                        break;
                    }
                    notified.await;
                }
            }
            if ct.is_cancelled() {
                return Err(Error::Cancelled);
            }
            if generation {
                let body = if self.generation_status == 200 {
                    format!("{{\"data\":[{{\"b64_json\":\"{PNG}\"}}]}}")
                } else {
                    "private-body key-sensitive data:image/png;base64,sensitive".into()
                };
                return Ok(response(self.generation_status, body.into_bytes()));
            }
            if self.behavior == Behavior::MissingObject
                && method == "HEAD"
                && path.ends_with(".png")
            {
                return Ok(response(404, vec![]));
            }
            Ok(response(200, vec![]))
        })
    }
}
struct Clients(Arc<PairedHttp>);
impl HttpClients for Clients {
    fn resolve(&self, base: &str, invalid: bool) -> Result<SelectedHttpClient, Error> {
        self.0
            .resolutions
            .lock()
            .unwrap()
            .push((base.into(), invalid));
        let streaming: Arc<dyn HttpTransport> = self.0.clone();
        let images: Arc<dyn ImageTransport> = self.0.clone();
        assert_eq!(
            Arc::as_ptr(&streaming) as *const (),
            Arc::as_ptr(&images) as *const ()
        );
        Ok(SelectedHttpClient { streaming, images })
    }
}
#[derive(Default)]
struct Keys {
    reads: AtomicUsize,
}
impl ApiKeySource for Keys {
    fn api_key<'a>(&'a self, _: &'a dyn Cancellation) -> BoxFuture<'a, Result<String, Error>> {
        Box::pin(async move {
            Ok(format!(
                "key-{}",
                self.reads.fetch_add(1, Ordering::SeqCst) + 1
            ))
        })
    }
    fn has_credential<'a>(&'a self, _: &'a dyn Cancellation) -> BoxFuture<'a, Result<bool, Error>> {
        Box::pin(async { Ok(true) })
    }
}
impl OAuthTokenSource for Keys {
    fn token<'a>(&'a self, _: &'a dyn Cancellation) -> BoxFuture<'a, Result<OAuthAccess, Error>> {
        Box::pin(async { panic!("compatible fixture must not resolve OAuth") })
    }
    fn has_credential<'a>(&'a self, _: &'a dyn Cancellation) -> BoxFuture<'a, Result<bool, Error>> {
        Box::pin(async { Ok(true) })
    }
}
struct Sources(Arc<Keys>);
impl CredentialSources for Sources {
    fn api_key(&self, _: &str, _: &str) -> Arc<dyn ApiKeySource> {
        self.0.clone()
    }
    fn oauth(&self, _: &str) -> Arc<dyn OAuthTokenSource> {
        self.0.clone()
    }
}
struct BucketCredentials;
impl CredentialSource for BucketCredentials {
    fn stored(&self) -> BoxFuture<'_, Result<Option<StoredCredential>, Error>> {
        Box::pin(async {
            Ok(Some(StoredCredential::S3 {
                access_key: "s3-access".into(),
                secret_key: "s3-secret".into(),
            }))
        })
    }
    fn environment(&self, _: &str) -> Option<String> {
        panic!("stored S3 credentials must take precedence")
    }
}
struct Epoch;
impl Clock for Epoch {
    fn amz_date(&self) -> String {
        "19700101T000000Z".into()
    }
}
struct NoSocket;
impl Connector for NoSocket {
    fn connect<'a>(
        &'a self,
        _: &'a str,
        _: &'a Headers,
        _: Option<Duration>,
        _: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<ConnectedSocket, WebSocketError>> {
        Box::pin(async { panic!("chat-completions fixture must not connect WebSocket") })
    }
}
struct Fixture {
    directory: PathBuf,
    request: Request,
    http: Arc<PairedHttp>,
    keys: Arc<Keys>,
    provider: Arc<dyn Provider>,
}
impl Fixture {
    async fn build(
        upload: bool,
        image_urls: bool,
        behavior: Behavior,
        generation_status: u16,
    ) -> Self {
        Self::build_with_bucket(
            upload,
            image_urls,
            behavior,
            generation_status,
            "https://bucket.test",
            "b",
        )
        .await
    }
    async fn build_with_bucket(
        upload: bool,
        image_urls: bool,
        behavior: Behavior,
        generation_status: u16,
        endpoint: &str,
        bucket: &str,
    ) -> Self {
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        let directory = std::env::current_dir().unwrap().join(".tmp").join(format!(
            "t34-real-builder-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::SeqCst)
        ));
        std::fs::create_dir_all(&directory).unwrap();
        let image = directory.join("input.png");
        std::fs::write(&image, [0, 1, 2]).unwrap();
        let yaml = format!(
            "providers:\n  configured:\n    base_url: https://provider.test/v1\n    protocol: chat-completions\n    image_urls: {image_urls}\n    disable_websocket: true\n    header_timeout_max_retries: 0\n    models:\n      m: {{}}\n"
        );
        let environment: BTreeMap<_, _> = [
            "HOME",
            "XDG_CONFIG_HOME",
            "XDG_DATA_HOME",
            "XDG_STATE_HOME",
            "WORKDIR",
        ]
        .into_iter()
        .map(|name| {
            (
                name.into(),
                directory.join(name).to_string_lossy().into_owned(),
            )
        })
        .collect();
        let mut configuration =
            Configuration::from_content(&yaml, &environment, |_| Ok(())).unwrap();
        configuration
            .models
            .providers
            .retain(|name, _| name == "configured");
        configuration.runtime.image_upload = upload.then(|| ImageUploadConfig {
            endpoint: endpoint.into(),
            bucket: bucket.into(),
            region: "us-east-1".into(),
            key_prefix: "parrot/".into(),
            public_base_url: String::new(),
            access_key_env: String::new(),
            secret_key_env: String::new(),
        });
        let http = PairedHttp::new(behavior, generation_status);
        let keys = Arc::new(Keys::default());
        let services = BuildServices {
            clients: Arc::new(Clients(http.clone())),
            credentials: Arc::new(Sources(keys.clone())),
            connector: Arc::new(NoSocket),
            models_dev: ModelsDevInformationProvider::new(http.clone()),
            image_buckets: Arc::new(S3ImageBucketFactory::new(
                Arc::new(Epoch),
                Arc::new(BucketCredentials),
            )),
        };
        let registry = ProviderRegistryBuilder::new(configuration, services)
            .build(&Token::new(false))
            .await
            .unwrap();
        let provider = registry
            .list()
            .into_iter()
            .find(|p| p.id() == "configured")
            .unwrap();
        let request = Request {
            model: "m".into(),
            messages: vec![Message::new(
                Role::User,
                vec![
                    Content::text("before"),
                    Content::image_file(image.to_string_lossy(), "image/png", 2, 1),
                    Content::text("after"),
                ],
            )],
            ..Request::default()
        };
        Self {
            directory,
            request,
            http,
            keys,
            provider,
        }
    }
    fn files(&self) -> usize {
        std::fs::read_dir(&self.directory).unwrap().count()
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        std::fs::remove_dir_all(&self.directory).unwrap();
    }
}
async fn collect(
    mut stream: Box<dyn parrot_provider::sessions::EventStream>,
    ct: &dyn Cancellation,
) -> Result<Vec<Event>, Error> {
    let mut result = vec![];
    let outcome = loop {
        match stream.next(ct).await {
            Ok(Some(event)) => result.push(event),
            Ok(None) => break Ok(result),
            Err(error) => break Err(error),
        }
    };
    stream.close().await?;
    outcome
}
fn image_url(body: &serde_json::Value) -> &str {
    body["messages"][0]["content"][1]["image_url"]["url"]
        .as_str()
        .unwrap()
}
fn linked_url() -> String {
    format!(
        "https://bucket.test/b/parrot/{:x}.png",
        Sha256::digest([0, 1, 2])
    )
}
fn assert_signed_uploads(requests: &[Recorded]) {
    for request in requests {
        if request.url.starts_with("https://bucket.test/")
            && !(request.method == "HEAD" && request.url.ends_with(".png"))
        {
            assert!(request.header("Authorization").unwrap().starts_with("AWS4-HMAC-SHA256 Credential=s3-access/19700101/us-east-1/s3/aws4_request, SignedHeaders=host;"));
            assert_eq!(request.header("x-amz-date"), Some("19700101T000000Z"));
        }
        if request.method == "PUT" {
            assert_eq!(request.body, [0, 1, 2]);
            assert_eq!(request.content_type.as_deref(), Some("image/png"));
            assert_eq!(
                request.header("x-amz-content-sha256"),
                Some(format!("{:x}", Sha256::digest([0, 1, 2])).as_str())
            );
        }
    }
}

#[tokio::test]
async fn actual_builder_links_only_when_upload_and_image_urls_are_both_enabled() {
    for upload in [false, true] {
        for image_urls in [false, true] {
            let fixture = Fixture::build(upload, image_urls, Behavior::Success, 200).await;
            let active = Token::new(false);
            let events = collect(
                fixture.provider.call(fixture.request.clone()).unwrap(),
                &active,
            )
            .await
            .unwrap();
            assert!(
                events
                    .iter()
                    .any(|e| matches!(e.base_event(), Event::Completed(_)))
            );
            let bodies = fixture.http.chat_bodies();
            assert_eq!(bodies.len(), 1);
            let contents = &bodies[0]["messages"][0]["content"];
            assert_eq!(contents[0]["text"], "before");
            assert_eq!(contents[2]["text"], "after");
            let requests = fixture.http.image_requests();
            if upload && image_urls {
                assert_eq!(image_url(&bodies[0]), linked_url());
                assert_eq!(
                    requests
                        .iter()
                        .map(|r| r.method.as_str())
                        .collect::<Vec<_>>(),
                    ["HEAD", "PUT"]
                );
                assert_signed_uploads(&requests);
            } else {
                assert_eq!(image_url(&bodies[0]), "data:image/png;base64,AAEC");
                assert!(requests.is_empty());
            }
            let resolutions = fixture.http.resolutions.lock().unwrap();
            assert_eq!(
                resolutions
                    .iter()
                    .filter(|(url, _)| url == "https://bucket.test")
                    .count(),
                usize::from(upload)
            );
            assert!(
                resolutions
                    .iter()
                    .filter(|(url, _)| url == "https://bucket.test")
                    .all(|(_, invalid)| !*invalid)
            );
            assert_eq!(fixture.files(), 1);
        }
    }
    let fixture = Fixture::build_with_bucket(
        true,
        true,
        Behavior::Success,
        200,
        "https://minio.example.com",
        "parrot-images",
    )
    .await;
    collect(
        fixture.provider.call(fixture.request.clone()).unwrap(),
        &Token::new(false),
    )
    .await
    .unwrap();
    let bodies = fixture.http.chat_bodies();
    assert_eq!(bodies.len(), 1);
    assert!(image_url(&bodies[0]).starts_with("https://minio.example.com/parrot-images/parrot/"));
}

#[tokio::test]
async fn actual_builder_url_repair_precedes_outer_retry_and_stays_session_local() {
    let fixture = Fixture::build(true, true, Behavior::RejectUrl, 200).await;
    let active = Token::new(false);
    let rejected = fixture.provider.open_session().unwrap();
    let other = fixture.provider.open_session().unwrap();
    let events = collect(rejected.call(fixture.request.clone()).unwrap(), &active)
        .await
        .unwrap();
    let retries: Vec<_> = events
        .iter()
        .filter_map(|e| match e.base_event() {
            Event::Retry {
                attempt,
                retry_after,
                reason,
            } => Some((*attempt, *retry_after, reason.as_str())),
            _ => None,
        })
        .collect();
    assert_eq!(
        retries,
        [(
            1,
            Duration::ZERO,
            "Provider rejected image URLs. Retrying with inline image data."
        )]
    );
    collect(rejected.call(fixture.request.clone()).unwrap(), &active)
        .await
        .unwrap();
    collect(other.call(fixture.request.clone()).unwrap(), &active)
        .await
        .unwrap();
    rejected.close().await.unwrap();
    other.close().await.unwrap();
    let bodies = fixture.http.chat_bodies();
    assert_eq!(bodies.len(), 4);
    assert_eq!(image_url(&bodies[0]), linked_url());
    assert!(image_url(&bodies[1]).starts_with("data:image/png;base64,"));
    assert!(image_url(&bodies[2]).starts_with("data:image/png;base64,"));
    assert_eq!(image_url(&bodies[3]), linked_url());
    let requests = fixture.http.image_requests();
    assert_eq!(
        requests
            .iter()
            .map(|r| r.method.as_str())
            .collect::<Vec<_>>(),
        ["HEAD", "PUT", "HEAD"]
    );
    assert!(requests.last().unwrap().headers.is_empty());
    assert_signed_uploads(&requests);
    assert_eq!(fixture.files(), 1);
}

#[tokio::test]
async fn actual_builder_reuploads_missing_object_once_with_real_bucket_preparation() {
    let fixture = Fixture::build(true, true, Behavior::MissingObject, 200).await;
    let active = Token::new(false);
    let events = collect(
        fixture.provider.call(fixture.request.clone()).unwrap(),
        &active,
    )
    .await
    .unwrap();
    let reasons: Vec<_> = events
        .iter()
        .filter_map(|e| match e.base_event() {
            Event::Retry {
                retry_after,
                reason,
                ..
            } => Some((*retry_after, reason.as_str())),
            _ => None,
        })
        .collect();
    assert_eq!(
        reasons,
        [(
            Duration::ZERO,
            "Re-uploaded missing image objects. Retrying."
        )]
    );
    let bodies = fixture.http.chat_bodies();
    assert_eq!(bodies.len(), 2);
    assert!(bodies.iter().all(|b| image_url(b) == linked_url()));
    let requests = fixture.http.image_requests();
    assert_eq!(requests.iter().filter(|r| r.method == "PUT").count(), 2);
    assert_eq!(
        requests
            .iter()
            .filter(|r| r.method == "HEAD" && r.url == "https://bucket.test/b")
            .count(),
        2
    );
    assert_eq!(
        requests
            .iter()
            .filter(|r| r.method == "HEAD" && r.url.ends_with(".png"))
            .count(),
        1
    );
    assert_signed_uploads(&requests);
}

#[tokio::test]
async fn actual_builder_generation_through_linking_and_retry_is_one_paid_request() {
    for status in [200, 503] {
        let fixture = Fixture::build(true, true, Behavior::Success, status).await;
        let active = Token::new(false);
        let before = fixture.keys.reads.load(Ordering::SeqCst);
        let files = fixture.files();
        let result = fixture
            .provider
            .generate_image(
                ImageGenerationRequest {
                    prompt: "preserve image prompt".into(),
                    references: vec![],
                },
                &active,
            )
            .await;
        if status == 200 {
            let data = result.unwrap().data;
            assert_eq!(data.len(), 70);
            assert_eq!(&data[..8], b"\x89PNG\r\n\x1a\n");
        } else {
            let error = result.unwrap_err();
            assert_eq!(
                error,
                Error::Provider("provider: image generation failed (HTTP 503)".into())
            );
            assert!(!error.to_string().contains("key-sensitive"));
            assert!(!error.to_string().contains("private-body"));
        }
        let requests = fixture.http.image_requests();
        assert_eq!(requests.len(), 1);
        assert_eq!(requests[0].method, "POST");
        assert_eq!(
            requests[0].url,
            "https://provider.test/v1/images/generations"
        );
        assert_eq!(
            requests[0].content_type.as_deref(),
            Some("application/json")
        );
        assert_eq!(
            requests[0].header("Authorization"),
            Some(format!("Bearer key-{}", before + 1).as_str())
        );
        assert_eq!(
            serde_json::from_slice::<serde_json::Value>(&requests[0].body).unwrap()["prompt"],
            "preserve image prompt"
        );
        assert_eq!(fixture.keys.reads.load(Ordering::SeqCst), before + 1);
        assert!(fixture.http.chat_bodies().is_empty());
        assert_eq!(fixture.files(), files);
    }
}

#[tokio::test]
async fn actual_builder_cancelled_observer_leaves_shared_signed_upload_alive_and_settled() {
    let fixture = Fixture::build(true, true, Behavior::GatedPrepare, 200).await;
    let cancelled = Token::new(true);
    let mut abandoned = fixture.provider.call(fixture.request.clone()).unwrap();
    assert!(matches!(
        abandoned.next(&cancelled).await,
        Err(Error::Cancelled)
    ));
    abandoned.close().await.unwrap();
    let before = fixture.http.image_requests();
    assert_eq!(before.len(), 1);
    assert_eq!(before[0].method, "HEAD");
    assert!(fixture.http.chat_bodies().is_empty());
    fixture.http.release();
    let active = Token::new(false);
    collect(
        fixture.provider.call(fixture.request.clone()).unwrap(),
        &active,
    )
    .await
    .unwrap();
    let after = fixture.http.image_requests();
    assert_eq!(
        after.iter().map(|r| r.method.as_str()).collect::<Vec<_>>(),
        ["HEAD", "PUT"]
    );
    assert_signed_uploads(&after);
    assert_eq!(fixture.http.chat_bodies().len(), 1);
    assert_eq!(fixture.files(), 1);
}

#[test]
fn source_image_token_rows_use_actual_factory_build_and_retry_composition() {
    use parrot_provider::{
        catalogs::Model,
        routing::factory::{ProviderBuildContext, build_provider},
        sessions::decorators::RetryingProvider,
    };
    let directory = std::env::current_dir()
        .unwrap()
        .join(".tmp/t34-token-context");
    let environment = [
        "HOME",
        "XDG_CONFIG_HOME",
        "XDG_DATA_HOME",
        "XDG_STATE_HOME",
        "WORKDIR",
    ]
    .into_iter()
    .map(|name| (name.into(), directory.to_string_lossy().into_owned()))
    .collect();
    let configuration = Configuration::from_content(
        "providers:\n  token-source:\n    base_url: https://example.test/v1\n",
        &environment,
        |_| Ok(()),
    )
    .unwrap();
    let config = &configuration.models.providers["token-source"];
    let http = PairedHttp::new(Behavior::Success, 200);
    let keys = Arc::new(Keys::default());
    let credentials = Sources(keys.clone());
    for (provider_id, expected) in [
        ("openai", 203),
        ("chatgpt", 203),
        ("generic", 4096),
        ("openrouter", 4096),
    ] {
        let built = build_provider(ProviderBuildContext {
            id: provider_id,
            config,
            credentials: &credentials,
            transport: http.clone(),
            images: http.clone(),
            connector: Arc::new(NoSocket),
            external_models: &[],
            maximum_request_bytes: 64 << 20,
        })
        .unwrap();
        let retry = RetryingProvider::new(built.provider, 5);
        let provider: &dyn Provider = &retry;
        let actual = provider.calculate_image_tokens(
            &Model::new("gpt-6-astra", provider_id),
            &Content::image_file("unused.png", "image/png", 400, 400),
        );
        println!(
            "T34_TOKEN_DELEGATE {}",
            serde_json::json!({"provider_id":provider_id,"model_id":"gpt-6-astra","width":400,"height":400,"tokens":actual})
        );
        assert_eq!(actual, expected, "source tuple {provider_id}");
    }
    assert!(http.requests.lock().unwrap().is_empty());
    assert_eq!(keys.reads.load(Ordering::SeqCst), 0);
}

#[test]
fn unknown_provider_uses_actual_trait_default_for_source_invalid_image() {
    use parrot_provider::{
        catalogs::Model,
        sessions::{EventStream, Session},
    };
    struct UnusedProvider;
    impl Provider for UnusedProvider {
        fn id(&self) -> &str {
            "unused"
        }
        fn call(&self, _: Request) -> Result<Box<dyn EventStream>, Error> {
            panic!("token default must not call the provider")
        }
        fn has_credential<'a>(
            &'a self,
            _: &'a dyn Cancellation,
        ) -> BoxFuture<'a, Result<bool, Error>> {
            panic!("token default must not resolve credentials")
        }
        fn seed_models(&self) -> Vec<Model> {
            panic!("token default must not read models")
        }
        fn list_models<'a>(
            &'a self,
            _: &'a dyn Cancellation,
        ) -> BoxFuture<'a, Result<Vec<Model>, Error>> {
            panic!("token default must not fetch models")
        }
        fn open_session(&self) -> Result<Arc<dyn Session>, Error> {
            panic!("token default must not open a session")
        }
    }
    let provider: &dyn Provider = &UnusedProvider;
    let actual = provider.calculate_image_tokens(
        &Model::new("gpt-6-astra", "openai"),
        &Content::image_file("unused.png", "image/png", 0, 0),
    );
    println!(
        "T34_TOKEN_DELEGATE {}",
        serde_json::json!({"operation":"unknown_default","model_id":"gpt-6-astra","model_provider":"openai","width":0,"height":0,"tokens":actual})
    );
    assert_eq!(actual, 4096);
}
