use super::{
    signer::{AwsV4Signer, EMPTY_PAYLOAD_SHA256, sha256},
    transport::{ImageHttpRequest, ImageTransport},
};
use crate::{
    http_stream::{Endpoint, HttpError, HttpResponse},
    sessions::Error,
};
use chrono::{DateTime, Datelike, Utc};
use parrot_domain::contracts::async_ops::{BoxFuture, Cancellation};
use std::{
    sync::{Arc, Mutex},
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use tokio::sync::watch;

pub trait ImageBucket: Send + Sync {
    fn prepare<'a>(
        &'a self,
        cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<(), Error>>;
    fn put_image<'a>(
        &'a self,
        bytes: Vec<u8>,
        media_type: String,
        cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<String, Error>>;
    fn verify_public<'a>(
        &'a self,
        url: &'a str,
        cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<bool, Error>>;
    fn forget_preparation(&self);
}
#[derive(Clone)]
pub struct BucketConfig {
    pub endpoint: String,
    pub bucket: String,
    pub region: String,
    pub key_prefix: String,
    pub public_base_url: String,
    pub access_key_env: String,
    pub secret_key_env: String,
}
impl BucketConfig {
    pub fn new(endpoint: impl Into<String>, bucket: impl Into<String>) -> Self {
        Self {
            endpoint: endpoint.into(),
            bucket: bucket.into(),
            region: "us-east-1".into(),
            key_prefix: "parrot/".into(),
            public_base_url: String::new(),
            access_key_env: String::new(),
            secret_key_env: String::new(),
        }
    }
}
pub enum StoredCredential {
    S3 {
        access_key: String,
        secret_key: String,
    },
    Other,
}
/// Source loading is detached from caller cancellation. Implementations own their
/// credential store/environment capability; the bucket never reads process state.
/// Typed store authentication failures retain their cause beneath the source's
/// safe outer message. Other failures, including cancellation, propagate.
pub trait CredentialSource: Send + Sync {
    fn stored(&self) -> BoxFuture<'_, Result<Option<StoredCredential>, Error>>;
    fn environment(&self, name: &str) -> Option<String>;
}
pub trait Clock: Send + Sync {
    fn amz_date(&self) -> String;
}
pub struct SystemClock;
impl Clock for SystemClock {
    fn amz_date(&self) -> String {
        format_amz_date(SystemTime::now())
    }
}
fn format_amz_date(time: SystemTime) -> String {
    let seconds = time
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    i64::try_from(seconds)
        .ok()
        .and_then(|seconds| DateTime::<Utc>::from_timestamp(seconds, 0))
        .filter(|date| date.year() <= 9999)
        .map(|date| date.format("%Y%m%dT%H%M%SZ").to_string())
        // AWS requires a four-digit year. An unsupported time returns an empty
        // timestamp, which AwsV4Signer rejects before signing or dispatching.
        .unwrap_or_default()
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
struct Work<T: Clone> {
    result: watch::Receiver<Option<Result<T, Error>>>,
}
impl<T: Clone + Send + Sync + 'static> Work<T> {
    fn start(mut future: BoxFuture<'static, Result<T, Error>>) -> Arc<Self> {
        // Source async methods run synchronously until their first pending await.
        // Preserve the completed-task fast path before observing caller cancellation.
        struct Noop;
        impl std::task::Wake for Noop {
            fn wake(self: Arc<Self>) {}
        }
        let waker = std::task::Waker::from(Arc::new(Noop));
        let mut context = std::task::Context::from_waker(&waker);
        let immediate = match future.as_mut().poll(&mut context) {
            std::task::Poll::Ready(result) => Some(result),
            std::task::Poll::Pending => None,
        };
        let ready = immediate.is_some();
        let (sender, result) = watch::channel(immediate);
        if !ready {
            tokio::spawn(async move {
                let result = future.await;
                let _ = sender.send(Some(result));
            });
        }
        Arc::new(Self { result })
    }
    fn failed(&self) -> bool {
        self.result.borrow().as_ref().is_some_and(Result::is_err)
    }
    async fn observe(&self, cancellation: &dyn Cancellation) -> Result<T, Error> {
        let mut receiver = self.result.clone();
        loop {
            if let Some(result) = receiver.borrow().clone() {
                return result;
            }
            tokio::select! { biased;
                result=receiver.changed()=> { if result.is_err() { return Err(Error::InvalidOperation("image upload task ended without a result".into())); } },
                _=cancellation.cancelled()=> return Err(Error::Cancelled),
            }
        }
    }
}
struct State {
    transport: Arc<dyn ImageTransport>,
    config: BucketConfig,
    endpoint: url::Url,
    public_base: String,
    clock: Arc<dyn Clock>,
    credentials: Arc<dyn CredentialSource>,
    signer: Mutex<Option<Arc<Work<AwsV4Signer>>>>,
    prepared: Mutex<Option<Arc<Work<()>>>>,
}
#[derive(Clone)]
pub struct S3ImageBucket(Arc<State>);
impl S3ImageBucket {
    pub fn new(
        transport: Arc<dyn ImageTransport>,
        config: BucketConfig,
        clock: Arc<dyn Clock>,
        credentials: Arc<dyn CredentialSource>,
    ) -> Result<Self, Error> {
        let endpoint = url::Url::parse(&(config.endpoint.trim_end_matches('/').to_owned() + "/"))
            .map_err(|e| Error::Provider(e.to_string()))?;
        let public_base = if config.public_base_url.is_empty() {
            &config.endpoint
        } else {
            &config.public_base_url
        }
        .trim_end_matches('/')
        .to_owned();
        Ok(Self(Arc::new(State {
            transport,
            config,
            endpoint,
            public_base,
            clock,
            credentials,
            signer: Mutex::new(None),
            prepared: Mutex::new(None),
        })))
    }
    async fn resolve_signer(&self, cancellation: &dyn Cancellation) -> Result<AwsV4Signer, Error> {
        let work = {
            let mut slot = self.0.signer.lock().unwrap();
            if slot.as_ref().is_none_or(|work| work.failed()) {
                let owner = self.clone();
                *slot = Some(Work::start(Box::pin(
                    async move { owner.load_signer().await },
                )));
            }
            slot.as_ref().unwrap().clone()
        };
        work.observe(cancellation).await
    }
    async fn load_signer(&self) -> Result<AwsV4Signer, Error> {
        let credential = self
            .0
            .credentials
            .stored()
            .await
            .map_err(|error| match error {
                cause @ Error::Authentication(_) => Error::ProviderWithCause {
                    message: "image_upload: could not read stored credentials".into(),
                    cause: Box::new(cause),
                },
                other => other,
            })?;
        let (access_key, secret_key) = match credential {
            None => (
                self.variable("access_key_env", &self.0.config.access_key_env)?,
                self.variable("secret_key_env", &self.0.config.secret_key_env)?,
            ),
            Some(StoredCredential::Other) => {
                return Err(Error::Provider(
                    "image_upload: stored credential must be an s3 credential".into(),
                ));
            }
            Some(StoredCredential::S3 {
                access_key,
                secret_key,
            }) => {
                parrot_foundation::credentials::Credential::for_s3(&access_key, &secret_key)
                    .validate()
                    .map_err(|cause| Error::ProviderWithCause {
                        message: "image_upload: invalid stored s3 credential".into(),
                        cause: Box::new(Error::Authentication(
                            parrot_foundation::oauth::AuthError::Credential(cause),
                        )),
                    })?;
                (access_key, secret_key)
            }
        };
        Ok(AwsV4Signer::new(
            access_key,
            secret_key,
            self.0.config.region.clone(),
        ))
    }
    fn variable(&self, field: &str, name: &str) -> Result<String, Error> {
        if name.is_empty() {
            return Err(Error::Provider(format!(
                "image_upload.{field} is not configured"
            )));
        }
        self.0
            .credentials
            .environment(name)
            .filter(|value| !value.is_empty())
            .ok_or_else(|| {
                Error::Provider(format!(
                    "image_upload: environment variable {name} is not set"
                ))
            })
    }
    fn request(
        &self,
        method: &str,
        relative: &str,
        body: Vec<u8>,
        content_type: Option<String>,
    ) -> Result<ImageHttpRequest, Error> {
        let endpoint = self
            .0
            .endpoint
            .join(relative)
            .map_err(|e| Error::Provider(e.to_string()))?;
        Ok(ImageHttpRequest {
            method: method.into(),
            endpoint: super::uri::normalize_endpoint(&Endpoint::from(endpoint))?,
            headers: Vec::new(),
            body,
            content_type,
        })
    }
    async fn send(
        &self,
        mut request: ImageHttpRequest,
        hash: &str,
        timeout: Duration,
        cancellation: &dyn Cancellation,
    ) -> Result<HttpResponse, Error> {
        self.resolve_signer(cancellation).await?.sign(
            &mut request,
            hash,
            &self.0.clock.amz_date(),
        )?;
        self.dispatch(request, timeout, cancellation).await
    }
    async fn dispatch(
        &self,
        request: ImageHttpRequest,
        timeout: Duration,
        cancellation: &dyn Cancellation,
    ) -> Result<HttpResponse, Error> {
        let method = request.method.clone();
        let uri = request.endpoint.as_url();
        let path = format!(
            "{}{}",
            uri.path(),
            uri.query().map(|q| format!("?{q}")).unwrap_or_default()
        );
        let failure = |cause| Error::ProviderWithCause {
            message: format!("image_upload: {method} {path} timed out"),
            cause: Box::new(cause),
        };
        let result = tokio::select! { biased;
            result=self.0.transport.send(request,cancellation)=>result,
            _=cancellation.cancelled()=>return Err(Error::Cancelled),
            _=tokio::time::sleep(timeout)=>return Err(failure(Error::Cancelled)),
        };
        match result {
            Err(
                cause @ (Error::Cancelled
                | Error::Cancellation(_)
                | Error::Http(HttpError::Cancelled)),
            ) if !cancellation.is_cancelled() => Err(failure(cause)),
            other => other,
        }
    }
    async fn ensure_success(
        mut response: HttpResponse,
        method: &str,
        endpoint: &Endpoint,
    ) -> Result<(), Error> {
        if (200..300).contains(&response.status) {
            return Ok(());
        }
        let mut bytes = Vec::new();
        let mut buffer = [0; 8192];
        loop {
            let count = response.body.read(&mut buffer, &Never).await?;
            if count == 0 {
                break;
            }
            bytes.extend_from_slice(&buffer[..count]);
        }
        let mut body = String::from_utf8_lossy(&bytes).into_owned();
        const LIMIT: usize = 64 << 10;
        const MARKER: &str = "\n… [truncated]";
        if body.len() > LIMIT {
            let mut end = LIMIT - MARKER.len();
            while !body.is_char_boundary(end) {
                end -= 1;
            }
            body.truncate(end);
            body.push_str(MARKER);
        }
        let uri = endpoint.as_url();
        let query = uri.query().map(|q| format!("?{q}")).unwrap_or_default();
        Err(Error::Provider(format!(
            "image_upload: {method} {}{query} returned HTTP {}: {body}",
            uri.path(),
            response.status
        )))
    }
    async fn prepare_bucket(&self) -> Result<(), Error> {
        let request = self.request("HEAD", &self.0.config.bucket, Vec::new(), None)?;
        let endpoint = request.endpoint.clone();
        let response = self
            .send(
                request,
                EMPTY_PAYLOAD_SHA256,
                Duration::from_secs(30),
                &Never,
            )
            .await?;
        if response.status == 404 {
            drop(response);
            let body = if self.0.config.region == "us-east-1" {
                String::new()
            } else {
                format!(
                    "<CreateBucketConfiguration><LocationConstraint>{}</LocationConstraint></CreateBucketConfiguration>",
                    self.0.config.region
                )
            };
            let request = self.request(
                "PUT",
                &self.0.config.bucket,
                body.as_bytes().to_vec(),
                Some("application/xml; charset=utf-8".into()),
            )?;
            let endpoint = request.endpoint.clone();
            let response = self
                .send(
                    request,
                    &sha256(body.as_bytes()),
                    Duration::from_secs(30),
                    &Never,
                )
                .await?;
            if response.status != 409 {
                Self::ensure_success(response, "PUT", &endpoint).await?;
            }
            Ok(())
        } else {
            Self::ensure_success(response, "HEAD", &endpoint).await
        }
    }
}
impl ImageBucket for S3ImageBucket {
    fn prepare<'a>(
        &'a self,
        cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<(), Error>> {
        Box::pin(async move {
            let work = {
                let mut slot = self.0.prepared.lock().unwrap();
                if slot.as_ref().is_none_or(|work| work.failed()) {
                    let owner = self.clone();
                    *slot = Some(Work::start(Box::pin(async move {
                        owner.prepare_bucket().await
                    })));
                }
                slot.as_ref().unwrap().clone()
            };
            work.observe(cancellation).await
        })
    }
    fn forget_preparation(&self) {
        *self.0.prepared.lock().unwrap() = None;
    }
    fn put_image<'a>(
        &'a self,
        bytes: Vec<u8>,
        media_type: String,
        cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<String, Error>> {
        Box::pin(async move {
            let extension = match media_type.as_str() {
                "image/png" => ".png",
                "image/jpeg" => ".jpg",
                "image/gif" => ".gif",
                "image/webp" => ".webp",
                _ => {
                    return Err(Error::Provider(format!(
                        "image_upload: unsupported image media type \"{media_type}\""
                    )));
                }
            };
            let hash = sha256(&bytes);
            let key = format!("{}{hash}{extension}", self.0.config.key_prefix);
            let request = self.request(
                "PUT",
                &format!("{}/{key}", self.0.config.bucket),
                bytes,
                Some(media_type),
            )?;
            let endpoint = request.endpoint.clone();
            let response = self
                .send(request, &hash, Duration::from_secs(120), cancellation)
                .await?;
            Self::ensure_success(response, "PUT", &endpoint).await?;
            Ok(format!(
                "{}/{}/{key}",
                self.0.public_base, self.0.config.bucket
            ))
        })
    }
    fn verify_public<'a>(
        &'a self,
        url: &'a str,
        cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<bool, Error>> {
        Box::pin(async move {
            let request = ImageHttpRequest {
                method: "HEAD".into(),
                endpoint: super::uri::normalize_endpoint(
                    &Endpoint::parse(url).map_err(|e| Error::Provider(e.to_string()))?,
                )?,
                headers: Vec::new(),
                content_type: None,
                body: Vec::new(),
            };
            match self
                .dispatch(request, Duration::from_secs(30), cancellation)
                .await
            {
                Ok(response) => Ok((200..300).contains(&response.status)),
                Err(Error::Http(HttpError::Transport(_))) => Ok(false),
                Err(error) => Err(error),
            }
        })
    }
}

#[cfg(test)]
mod clock_tests {
    use super::*;

    #[test]
    fn formats_aws_timestamps_at_calendar_boundaries() {
        for (seconds, expected) in [
            (0, "19700101T000000Z"),
            (1, "19700101T000001Z"),
            (86_399, "19700101T235959Z"),
            (86_400, "19700102T000000Z"),
            (946_684_799, "19991231T235959Z"),
            (946_684_800, "20000101T000000Z"),
            (951_782_399, "20000228T235959Z"),
            (951_782_400, "20000229T000000Z"),
            (951_868_800, "20000301T000000Z"),
            (4_107_542_399, "21000228T235959Z"),
            (4_107_542_400, "21000301T000000Z"),
            (253_402_300_799, "99991231T235959Z"),
        ] {
            assert_eq!(
                format_amz_date(UNIX_EPOCH + Duration::from_secs(seconds)),
                expected
            );
        }
    }

    #[test]
    fn clamps_pre_epoch_times_and_discards_subseconds() {
        assert_eq!(
            format_amz_date(UNIX_EPOCH - Duration::from_nanos(1)),
            "19700101T000000Z"
        );
        assert_eq!(
            format_amz_date(UNIX_EPOCH - Duration::from_secs(86_400)),
            "19700101T000000Z"
        );
        assert_eq!(
            format_amz_date(UNIX_EPOCH + Duration::from_nanos(1_999_999_999)),
            "19700101T000001Z"
        );
    }

    #[test]
    fn unsupported_timestamps_are_rejected_by_signing() {
        for seconds in [253_402_300_800, 10_000_000_000_000, i64::MAX as u64] {
            let time = UNIX_EPOCH
                .checked_add(Duration::from_secs(seconds))
                .unwrap();
            let timestamp = format_amz_date(time);
            assert!(timestamp.is_empty());
            let mut request = ImageHttpRequest {
                method: "HEAD".into(),
                endpoint: Endpoint::parse("https://example.com/bucket").unwrap(),
                headers: Vec::new(),
                content_type: None,
                body: Vec::new(),
            };
            assert_eq!(
                AwsV4Signer::new("access".into(), "secret".into(), "us-east-1".into()).sign(
                    &mut request,
                    EMPTY_PAYLOAD_SHA256,
                    &timestamp
                ),
                Err(Error::Provider("invalid AWS timestamp".into()))
            );
            assert!(request.headers.is_empty());
        }
    }
}
