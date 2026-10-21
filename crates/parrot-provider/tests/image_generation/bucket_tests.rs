use parrot_domain::contracts::async_ops::{BoxFuture, Cancellation};
use parrot_provider::{
    http_stream::{ByteStream, HttpError, HttpResponse},
    image_generation::{
        bucket::{
            BucketConfig, Clock, CredentialSource, ImageBucket, S3ImageBucket, StoredCredential,
        },
        signer::{AwsV4Signer, sha256},
        transport::{ImageHttpRequest, ImageTransport},
    },
    sessions::Error,
};
use std::{
    collections::VecDeque,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
};
use tokio::sync::Notify;
struct Never;
impl Cancellation for Never {
    fn is_cancelled(&self) -> bool {
        false
    }
    fn cancelled(&self) -> BoxFuture<'_, ()> {
        Box::pin(std::future::pending())
    }
}
#[derive(Default)]
struct Cancel {
    value: AtomicBool,
    notify: Notify,
}
impl Cancel {
    fn cancel(&self) {
        self.value.store(true, Ordering::SeqCst);
        self.notify.notify_waiters();
    }
}
impl Cancellation for Cancel {
    fn is_cancelled(&self) -> bool {
        self.value.load(Ordering::SeqCst)
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
struct Epoch;
impl Clock for Epoch {
    fn amz_date(&self) -> String {
        "19700101T000000Z".into()
    }
}
struct Credentials {
    mode: usize,
    reads: AtomicUsize,
    gate: Option<Arc<Notify>>,
}
impl CredentialSource for Credentials {
    fn stored(&self) -> BoxFuture<'_, Result<Option<StoredCredential>, Error>> {
        Box::pin(async move {
            self.reads.fetch_add(1, Ordering::SeqCst);
            if let Some(gate) = &self.gate {
                gate.notified().await;
            }
            match self.mode {
                1 => Ok(Some(StoredCredential::S3 {
                    access_key: "stored-access".into(),
                    secret_key: "stored-secret".into(),
                })),
                2 => Ok(Some(StoredCredential::Other)),
                3 => Ok(Some(StoredCredential::S3 {
                    access_key: " ".into(),
                    secret_key: "secret".into(),
                })),
                4 => Err(store_failure()),
                5 => Err(Error::Provider("unrelated provider failure".into())),
                6 => Err(Error::Cancellation("stored operation canceled".into())),
                _ => Ok(None),
            }
        })
    }
    fn environment(&self, name: &str) -> Option<String> {
        match name {
            "ACCESS" => Some("ak".into()),
            "SECRET" => Some("sk".into()),
            _ => None,
        }
    }
}
fn store_failure() -> Error {
    Error::Authentication(parrot_foundation::oauth::AuthError::Message(
        "synthetic store failure",
    ))
}
fn assert_cause(error: &Error, outer: &str, expected: &Error) {
    use std::error::Error as _;
    assert_eq!(error.to_string(), outer);
    let cause = error.source().unwrap().downcast_ref::<Error>().unwrap();
    assert_eq!(cause, expected);
    if let Error::Authentication(auth) = expected {
        assert_eq!(
            cause
                .source()
                .unwrap()
                .downcast_ref::<parrot_foundation::oauth::AuthError>(),
            Some(auth)
        );
    }
    assert!(!error.to_string().contains("synthetic store failure"));
    assert!(!error.to_string().contains("stored-secret"));
}
fn credentials(mode: usize) -> Arc<Credentials> {
    Arc::new(Credentials {
        mode,
        reads: AtomicUsize::new(0),
        gate: None,
    })
}
struct Bytes(Vec<u8>);
impl ByteStream for Bytes {
    fn read<'a>(
        &'a mut self,
        b: &'a mut [u8],
        _: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<usize, HttpError>> {
        Box::pin(async move {
            let n = b.len().min(self.0.len());
            b[..n].copy_from_slice(&self.0[..n]);
            self.0.drain(..n);
            Ok(n)
        })
    }
}
struct Transport {
    requests: Mutex<Vec<ImageHttpRequest>>,
    statuses: Mutex<VecDeque<i32>>,
    body: String,
}
impl Transport {
    fn new(statuses: &[i32]) -> Arc<Self> {
        Arc::new(Self {
            requests: Mutex::new(vec![]),
            statuses: Mutex::new(statuses.iter().copied().collect()),
            body: "denied".into(),
        })
    }
}
impl ImageTransport for Transport {
    fn send<'a>(
        &'a self,
        request: ImageHttpRequest,
        ct: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<HttpResponse, Error>> {
        Box::pin(async move {
            self.requests.lock().unwrap().push(request);
            if ct.is_cancelled() {
                return Err(Error::Cancelled);
            }
            let status = self.statuses.lock().unwrap().pop_front().unwrap_or(200);
            if status == -2 {
                return Err(Error::Cancellation(
                    "synthetic dispatch cancellation".into(),
                ));
            }
            if status < 0 {
                std::future::pending::<()>().await;
            }
            if status == 0 {
                return Err(Error::Http(HttpError::Transport("unreachable".into())));
            }
            Ok(HttpResponse {
                status: status as u16,
                reason: String::new(),
                headers: vec![],
                body: Box::new(Bytes(self.body.as_bytes().to_vec())),
            })
        })
    }
}
fn config() -> BucketConfig {
    let mut c = BucketConfig::new("https://minio.example.com:9000", "b");
    c.access_key_env = "ACCESS".into();
    c.secret_key_env = "SECRET".into();
    c
}
fn bucket(t: Arc<Transport>, c: BucketConfig, credentials: Arc<Credentials>) -> S3ImageBucket {
    S3ImageBucket::new(t, c, Arc::new(Epoch), credentials).unwrap()
}
fn header<'a>(r: &'a ImageHttpRequest, name: &str) -> &'a str {
    &r.headers
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case(name))
        .unwrap()
        .1
}
#[tokio::test]
async fn prepare_creates_a_missing_bucket_once_and_forget_repeats() {
    for exists in [true, false] {
        let statuses = if exists {
            vec![200, 200]
        } else {
            vec![404, 200, 404, 200]
        };
        let t = Transport::new(&statuses);
        let b = bucket(t.clone(), config(), credentials(0));
        b.prepare(&Never).await.unwrap();
        b.prepare(&Never).await.unwrap();
        let first = t
            .requests
            .lock()
            .unwrap()
            .iter()
            .map(|r| format!("{} {}", r.method, r.endpoint.as_url().path()))
            .collect::<Vec<_>>();
        assert_eq!(
            first,
            if exists {
                vec!["HEAD /b"]
            } else {
                vec!["HEAD /b", "PUT /b"]
            }
        );
        b.forget_preparation();
        b.prepare(&Never).await.unwrap();
        let requests = t.requests.lock().unwrap();
        assert_eq!(requests.len(), first.len() * 2);
        assert!(requests.iter().all(|r|header(r,"Authorization").starts_with("AWS4-HMAC-SHA256 Credential=ak/19700101/us-east-1/s3/aws4_request, SignedHeaders=host;")&&header(r,"x-amz-date")=="19700101T000000Z"));
    }
}
#[tokio::test]
async fn put_image_content_addressed_keys_and_public_base_all_four_tuples() {
    for (media, ext, base, expected) in [
        ("image/png", ".png", "", "https://minio.example.com:9000"),
        (
            "image/jpeg",
            ".jpg",
            "https://cdn.example.com/",
            "https://cdn.example.com",
        ),
        ("image/gif", ".gif", "", "https://minio.example.com:9000"),
        (
            "image/webp",
            ".webp",
            "https://cdn.example.com",
            "https://cdn.example.com",
        ),
    ] {
        let t = Transport::new(&[200]);
        let mut c = config();
        c.public_base_url = base.into();
        let b = bucket(t.clone(), c, credentials(0));
        let bytes = vec![0, 1, 2];
        let hash = sha256(&bytes);
        let url = b
            .put_image(bytes.clone(), media.into(), &Never)
            .await
            .unwrap();
        assert_eq!(url, format!("{expected}/b/parrot/{hash}{ext}"));
        let r = t.requests.lock().unwrap();
        assert_eq!(r.len(), 1);
        assert_eq!(r[0].method, "PUT");
        assert_eq!(
            r[0].endpoint.as_str(),
            format!("https://minio.example.com:9000/b/parrot/{hash}{ext}")
        );
        assert_eq!(r[0].body, bytes);
        assert_eq!(r[0].content_type.as_deref(), Some(media));
        assert_eq!(header(&r[0], "x-amz-content-sha256"), hash);
    }
}
#[tokio::test]
async fn failures_are_permanent_provider_errors_both_source_operations() {
    for upload in [false, true] {
        let t = Transport::new(&[403]);
        let b = bucket(t.clone(), config(), credentials(0));
        let error = if upload {
            b.put_image(vec![0, 1, 2], "image/png".into(), &Never)
                .await
                .map(|_| ())
        } else {
            b.prepare(&Never).await
        }
        .unwrap_err();
        assert!(matches!(error, Error::Provider(_)));
        let failing = if upload { "PUT /b/parrot/" } else { "HEAD /b" };
        assert!(
            error
                .to_string()
                .starts_with(&format!("image_upload: {failing}"))
        );
        assert!(error.to_string().ends_with("returned HTTP 403: denied"));
        assert_eq!(t.requests.lock().unwrap().len(), 1);
    }
}
#[tokio::test]
async fn missing_credentials_fail_before_any_request_exact_errors() {
    for (name, message) in [
        ("", "image_upload.access_key_env is not configured"),
        (
            "PARROT_TEST_S3_UNSET",
            "image_upload: environment variable PARROT_TEST_S3_UNSET is not set",
        ),
    ] {
        let t = Transport::new(&[]);
        let mut c = config();
        c.access_key_env = name.into();
        let b = bucket(t.clone(), c, credentials(0));
        assert_eq!(
            b.put_image(vec![0, 1, 2], "image/png".into(), &Never)
                .await
                .unwrap_err(),
            Error::Provider(message.into())
        );
        assert!(t.requests.lock().unwrap().is_empty());
    }
}
#[tokio::test]
async fn verify_public_is_unsigned_head_for_all_four_source_statuses() {
    for (status, expected) in [(200, true), (403, false), (404, false), (0, false)] {
        let t = Transport::new(&[status]);
        let creds = credentials(0);
        let b = bucket(t.clone(), config(), creds.clone());
        assert_eq!(
            b.verify_public("https://cdn.example.com/b/parrot/x.png", &Never)
                .await
                .unwrap(),
            expected
        );
        let requests = t.requests.lock().unwrap();
        assert_eq!(requests.len(), 1);
        assert_eq!(requests[0].method, "HEAD");
        assert_eq!(
            requests[0].endpoint.as_str(),
            "https://cdn.example.com/b/parrot/x.png"
        );
        assert!(requests[0].headers.is_empty());
        assert_eq!(creds.reads.load(Ordering::SeqCst), 0);
    }
}
#[tokio::test]
async fn stored_credentials_override_environment_and_are_cached() {
    let t = Transport::new(&[200, 200]);
    let creds = credentials(1);
    let b = bucket(t.clone(), config(), creds.clone());
    for _ in 0..2 {
        b.put_image(vec![0, 1, 2], "image/png".into(), &Never)
            .await
            .unwrap();
    }
    let requests = t.requests.lock().unwrap();
    let mut expected = ImageHttpRequest {
        method: "PUT".into(),
        endpoint: requests[0].endpoint.clone(),
        headers: vec![],
        content_type: None,
        body: vec![],
    };
    AwsV4Signer::new(
        "stored-access".into(),
        "stored-secret".into(),
        "us-east-1".into(),
    )
    .sign(&mut expected, &sha256(&[0, 1, 2]), "19700101T000000Z")
    .unwrap();
    assert_eq!(
        header(&requests[0], "Authorization"),
        header(&expected, "Authorization")
    );
    assert_eq!(creds.reads.load(Ordering::SeqCst), 1);
}
#[tokio::test]
async fn wrong_invalid_or_unreadable_credentials_never_use_environment() {
    for (mode, message) in [
        (
            2,
            "image_upload: stored credential must be an s3 credential",
        ),
        (3, "image_upload: invalid stored s3 credential"),
        (4, "image_upload: could not read stored credentials"),
    ] {
        let t = Transport::new(&[]);
        let b = bucket(t.clone(), config(), credentials(mode));
        let error = b
            .put_image(vec![0, 1, 2], "image/png".into(), &Never)
            .await
            .unwrap_err();
        match mode {
            2 => assert_eq!(error, Error::Provider(message.into())),
            3 => {
                let validation = parrot_foundation::credentials::Credential::for_s3(" ", "secret")
                    .validate()
                    .unwrap_err();
                assert_cause(
                    &error,
                    message,
                    &Error::Authentication(parrot_foundation::oauth::AuthError::Credential(
                        validation,
                    )),
                );
                assert!(!format!("{error:?}").contains("secret"));
            }
            4 => assert_cause(&error, message, &store_failure()),
            _ => unreachable!(),
        }
        assert!(t.requests.lock().unwrap().is_empty());
    }
}
#[tokio::test]
async fn concurrent_uploads_share_credential_read_not_caller_cancellation() {
    let gate = Arc::new(Notify::new());
    let creds = Arc::new(Credentials {
        mode: 1,
        reads: AtomicUsize::new(0),
        gate: Some(gate.clone()),
    });
    let t = Transport::new(&[200]);
    let b = bucket(t.clone(), config(), creds.clone());
    let ct = Arc::new(Cancel::default());
    let owner = b.clone();
    let canceled = ct.clone();
    let abandoned = tokio::spawn(async move {
        owner
            .put_image(vec![0, 1, 2], "image/png".into(), canceled.as_ref())
            .await
    });
    let owner = b.clone();
    let upload = tokio::spawn(async move {
        owner
            .put_image(vec![0, 1, 2], "image/png".into(), &Never)
            .await
    });
    while creds.reads.load(Ordering::SeqCst) == 0 {
        tokio::task::yield_now().await;
    }
    ct.cancel();
    assert_eq!(abandoned.await.unwrap().unwrap_err(), Error::Cancelled);
    gate.notify_one();
    upload.await.unwrap().unwrap();
    assert_eq!(creds.reads.load(Ordering::SeqCst), 1);
    assert_eq!(t.requests.lock().unwrap().len(), 1);
}
#[tokio::test]
async fn shared_preparation_survives_observer_cancellation_and_failure_retries() {
    let t = Transport::new(&[403, 200]);
    let b = bucket(t.clone(), config(), credentials(0));
    assert!(b.prepare(&Never).await.is_err());
    b.prepare(&Never).await.unwrap();
    b.prepare(&Never).await.unwrap();
    assert_eq!(t.requests.lock().unwrap().len(), 2);
    let gate = Arc::new(Notify::new());
    let creds = Arc::new(Credentials {
        mode: 1,
        reads: AtomicUsize::new(0),
        gate: Some(gate.clone()),
    });
    let t = Transport::new(&[404, 409]);
    let b = bucket(t.clone(), config(), creds.clone());
    let ct = Arc::new(Cancel::default());
    let owner = b.clone();
    let cancel = ct.clone();
    let observer = tokio::spawn(async move { owner.prepare(cancel.as_ref()).await });
    while creds.reads.load(Ordering::SeqCst) == 0 {
        tokio::task::yield_now().await;
    }
    ct.cancel();
    assert_eq!(observer.await.unwrap().unwrap_err(), Error::Cancelled);
    gate.notify_one();
    b.prepare(&Never).await.unwrap();
    assert_eq!(t.requests.lock().unwrap().len(), 2);
}
#[tokio::test(start_paused = true)]
async fn control_and_upload_timeouts_are_permanent_but_caller_cancel_is_not_timeout() {
    for upload in [false, true] {
        let t = Transport::new(&[-1]);
        let b = bucket(t, config(), credentials(0));
        let error = if upload {
            b.put_image(vec![0, 1, 2], "image/png".into(), &Never)
                .await
                .map(|_| ())
        } else {
            b.prepare(&Never).await
        }
        .unwrap_err();
        let operation = if upload {
            format!("PUT /b/parrot/{}.png", sha256(&[0, 1, 2]))
        } else {
            "HEAD /b".into()
        };
        assert_cause(
            &error,
            &format!("image_upload: {operation} timed out"),
            &Error::Cancelled,
        );
    }
    let t = Transport::new(&[-1]);
    let b = bucket(t, config(), credentials(0));
    let ct = Cancel::default();
    ct.cancel();
    assert_eq!(
        b.verify_public("https://cdn.example.com/a", &ct)
            .await
            .unwrap_err(),
        Error::Cancelled
    );
}
#[tokio::test]
async fn non_default_region_xml_and_conflict_and_unsupported_media() {
    let t = Transport::new(&[404, 409]);
    let mut c = config();
    c.region = "eu-west-1".into();
    let b = bucket(t.clone(), c, credentials(0));
    b.prepare(&Never).await.unwrap();
    {
        let requests = t.requests.lock().unwrap();
        assert_eq!(requests[1].body,b"<CreateBucketConfiguration><LocationConstraint>eu-west-1</LocationConstraint></CreateBucketConfiguration>");
        assert_eq!(
            requests[1].content_type.as_deref(),
            Some("application/xml; charset=utf-8")
        );
    }
    assert_eq!(
        b.put_image(vec![], "image/bmp".into(), &Never)
            .await
            .unwrap_err(),
        Error::Provider("image_upload: unsupported image media type \"image/bmp\"".into())
    );
    assert_eq!(t.requests.lock().unwrap().len(), 2);
}

#[tokio::test]
async fn completed_preparation_beats_cancelled_observer_and_failed_signer_retries() {
    let t = Transport::new(&[200]);
    let b = bucket(t.clone(), config(), credentials(0));
    b.prepare(&Never).await.unwrap();
    let ct = Cancel::default();
    ct.cancel();
    b.prepare(&ct).await.unwrap();
    assert_eq!(t.requests.lock().unwrap().len(), 1);
    assert_eq!(
        b.put_image(vec![0, 1, 2], "image/png".into(), &ct)
            .await
            .unwrap_err(),
        Error::Cancelled
    );
    assert_eq!(t.requests.lock().unwrap().len(), 2);
    let creds = credentials(4);
    let t = Transport::new(&[]);
    let b = bucket(t.clone(), config(), creds.clone());
    for _ in 0..2 {
        assert_eq!(
            b.put_image(vec![0, 1, 2], "image/png".into(), &Never)
                .await
                .unwrap_err(),
            Error::ProviderWithCause {
                message: "image_upload: could not read stored credentials".into(),
                cause: Box::new(store_failure()),
            }
        );
    }
    assert_eq!(creds.reads.load(Ordering::SeqCst), 2);
    assert!(t.requests.lock().unwrap().is_empty());
}

#[tokio::test]
async fn ready_credential_load_dispatches_even_with_precancelled_caller() {
    let t = Transport::new(&[]);
    let creds = credentials(0);
    let b = bucket(t.clone(), config(), creds.clone());
    let ct = Cancel::default();
    ct.cancel();
    assert_eq!(
        b.put_image(vec![0, 1, 2], "image/png".into(), &ct)
            .await
            .unwrap_err(),
        Error::Cancelled
    );
    assert_eq!(creds.reads.load(Ordering::SeqCst), 1);
    assert_eq!(t.requests.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn non_authentication_store_failures_propagate_without_wrapping_or_fallback() {
    for (mode, expected) in [
        (5, Error::Provider("unrelated provider failure".into())),
        (6, Error::Cancellation("stored operation canceled".into())),
    ] {
        let transport = Transport::new(&[]);
        let b = bucket(transport.clone(), config(), credentials(mode));
        assert_eq!(
            b.put_image(vec![0, 1, 2], "image/png".into(), &Never)
                .await
                .unwrap_err(),
            expected
        );
        assert!(transport.requests.lock().unwrap().is_empty());
    }
}

#[tokio::test]
async fn shared_store_failure_retains_actual_typed_cause_and_retries_failed_cache() {
    let gate = Arc::new(Notify::new());
    let creds = Arc::new(Credentials {
        mode: 4,
        reads: AtomicUsize::new(0),
        gate: Some(gate.clone()),
    });
    let transport = Transport::new(&[]);
    let b = bucket(transport.clone(), config(), creds.clone());
    let mut first = b.put_image(vec![0, 1, 2], "image/png".into(), &Never);
    let mut second = b.put_image(vec![0, 1, 2], "image/png".into(), &Never);
    std::future::poll_fn(|cx| {
        assert!(first.as_mut().poll(cx).is_pending());
        assert!(second.as_mut().poll(cx).is_pending());
        std::task::Poll::Ready(())
    })
    .await;
    assert_eq!(creds.reads.load(Ordering::SeqCst), 1);
    gate.notify_one();
    let (first, second) = tokio::join!(first, second);
    for error in [first.unwrap_err(), second.unwrap_err()] {
        assert_cause(
            &error,
            "image_upload: could not read stored credentials",
            &store_failure(),
        );
    }
    assert_eq!(creds.reads.load(Ordering::SeqCst), 1);
    gate.notify_one();
    let error = b
        .put_image(vec![0, 1, 2], "image/png".into(), &Never)
        .await
        .unwrap_err();
    assert_cause(
        &error,
        "image_upload: could not read stored credentials",
        &store_failure(),
    );
    assert_eq!(creds.reads.load(Ordering::SeqCst), 2);
    assert!(transport.requests.lock().unwrap().is_empty());
}

#[tokio::test]
async fn scripted_dispatch_cancellation_keeps_exact_original_cause_only_without_caller_cancel() {
    for upload in [false, true] {
        let transport = Transport::new(&[-2]);
        let b = bucket(transport.clone(), config(), credentials(0));
        let error = if upload {
            b.put_image(vec![0, 1, 2], "image/png".into(), &Never)
                .await
                .map(|_| ())
        } else {
            b.prepare(&Never).await
        }
        .unwrap_err();
        let path = if upload {
            format!("PUT /b/parrot/{}.png", sha256(&[0, 1, 2]))
        } else {
            "HEAD /b".into()
        };
        assert_cause(
            &error,
            &format!("image_upload: {path} timed out"),
            &Error::Cancellation("synthetic dispatch cancellation".into()),
        );
        assert_eq!(transport.requests.lock().unwrap().len(), 1);
    }
    let transport = Transport::new(&[-2]);
    let b = bucket(transport.clone(), config(), credentials(0));
    let ct = Cancel::default();
    ct.cancel();
    assert_eq!(
        b.put_image(vec![0, 1, 2], "image/png".into(), &ct)
            .await
            .unwrap_err(),
        Error::Cancelled
    );
    assert_eq!(transport.requests.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn unsigned_public_head_uses_the_same_source_uri_normalization_without_credentials() {
    let inputs: serde_json::Value =
        serde_json::from_str(include_str!("fixtures/source/uri-cases-v1/inputs.json")).unwrap();
    let oracle: serde_json::Value =
        serde_json::from_str(include_str!("fixtures/source/uri-cases-v1/expected.json")).unwrap();
    let transport = Transport::new(&[]);
    let creds = credentials(0);
    let b = bucket(transport.clone(), config(), creds.clone());
    for input in inputs["cases"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|case| case["family"] == "uri_signing")
    {
        let id = input["case_id"].as_str().unwrap();
        let url = input["url"].as_str().unwrap();
        assert!(b.verify_public(url, &Never).await.unwrap(), "{id}");
        let expected = &oracle["cases"]
            .as_array()
            .unwrap()
            .iter()
            .find(|case| case["case_id"] == id)
            .unwrap()["observation"]["sent_uri"]["absolute_uri"];
        let requests = transport.requests.lock().unwrap();
        let actual = requests.last().unwrap();
        assert_eq!(actual.endpoint.as_str(), expected.as_str().unwrap(), "{id}");
        assert_eq!(actual.method, "HEAD");
        assert!(actual.headers.is_empty());
    }
    assert_eq!(transport.requests.lock().unwrap().len(), 32);
    assert_eq!(creds.reads.load(Ordering::SeqCst), 0);
}
