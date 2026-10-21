use parrot_domain::contracts::async_ops::{BoxFuture, Cancellation};
use parrot_provider::{
    http_stream::{ByteStream, HttpError, HttpMethod, HttpRequest, HttpResponse, HttpTransport},
    routing::models_dev::ModelsDevInformationProvider,
    sessions::Error,
};
use std::{
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};

struct Token(bool);
impl Cancellation for Token {
    fn is_cancelled(&self) -> bool {
        self.0
    }
    fn cancelled(&self) -> BoxFuture<'_, ()> {
        Box::pin(async move {
            if !self.0 {
                std::future::pending::<()>().await;
            }
        })
    }
}
const ACTIVE: Token = Token(false);
struct Body {
    bytes: Vec<u8>,
    offset: usize,
    stalled: bool,
    drops: Arc<AtomicUsize>,
}
impl Drop for Body {
    fn drop(&mut self) {
        self.drops.fetch_add(1, Ordering::SeqCst);
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
            if self.stalled {
                cancellation.cancelled().await;
                return Err(HttpError::Cancelled);
            }
            let count = buffer.len().min(self.bytes.len() - self.offset);
            buffer[..count].copy_from_slice(&self.bytes[self.offset..self.offset + count]);
            self.offset += count;
            Ok(count)
        })
    }
}
struct Transport {
    body: Vec<u8>,
    status: u16,
    failure: Option<HttpError>,
    stalled: bool,
    requests: Mutex<Vec<HttpRequest>>,
    drops: Arc<AtomicUsize>,
}
impl Transport {
    fn body(bytes: impl Into<Vec<u8>>) -> Self {
        Self {
            body: bytes.into(),
            status: 200,
            failure: None,
            stalled: false,
            requests: Mutex::new(Vec::new()),
            drops: Arc::new(AtomicUsize::new(0)),
        }
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
            if cancellation.is_cancelled() {
                return Err(HttpError::Cancelled);
            }
            if let Some(error) = &self.failure {
                return Err(error.clone());
            }
            Ok(HttpResponse {
                status: self.status,
                reason: "Service Unavailable".into(),
                headers: Vec::new(),
                body: Box::new(Body {
                    bytes: self.body.clone(),
                    offset: 0,
                    stalled: self.stalled,
                    drops: self.drops.clone(),
                }),
            })
        })
    }
}

#[tokio::test]
async fn fetch_uses_fixed_unauthenticated_endpoint_and_decodes_catalogue() {
    let transport = Arc::new(Transport::body(br#"{"openai":{"id":"openai","models":{"external":{"id":"external-model","name":"External"}}}}"#.to_vec()));
    let catalogue = ModelsDevInformationProvider::new(transport.clone())
        .fetch(&ACTIVE)
        .await
        .unwrap();
    let requests = transport.requests.lock().unwrap();
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].method, HttpMethod::Get);
    assert_eq!(
        requests[0].endpoint.as_url().as_str(),
        "https://models.dev/api.json"
    );
    assert!(
        !requests[0]
            .headers
            .iter()
            .any(|(name, _)| name.eq_ignore_ascii_case("authorization"))
    );
    assert!(
        !requests[0]
            .headers
            .iter()
            .any(|(name, _)| name.eq_ignore_ascii_case("x-provider"))
    );
    assert_eq!(catalogue["openai"].len(), 1);
    assert_eq!(catalogue["openai"][0].id, "external-model");
    assert_eq!(transport.drops.load(Ordering::SeqCst), 1);
}

async fn expected_failure(failure: &str) {
    let mut transport = match failure {
        "json" => Transport::body(b"not-json".to_vec()),
        "schema" => Transport::body(b"[]".to_vec()),
        "oversized" => Transport::body(vec![0; (16 << 20) + 1]),
        _ => Transport::body(Vec::new()),
    };
    if failure == "http" {
        transport.status = 503;
    }
    if failure == "transport" {
        transport.failure = Some(HttpError::Transport("unavailable".into()));
    }
    let transport = Arc::new(transport);
    let catalogue = ModelsDevInformationProvider::new(transport.clone())
        .fetch(&ACTIVE)
        .await
        .unwrap();
    assert!(catalogue.is_empty(), "{failure}");
    assert_eq!(transport.requests.lock().unwrap().len(), 1);
    assert_eq!(
        transport.drops.load(Ordering::SeqCst),
        usize::from(failure != "transport")
    );
}
#[tokio::test]
async fn fetch_treats_http_failure_as_an_empty_catalogue() {
    expected_failure("http").await;
}
#[tokio::test]
async fn fetch_treats_json_failure_as_an_empty_catalogue() {
    expected_failure("json").await;
}
#[tokio::test]
async fn fetch_treats_schema_failure_as_an_empty_catalogue() {
    expected_failure("schema").await;
}
#[tokio::test]
async fn fetch_treats_oversized_failure_as_an_empty_catalogue() {
    expected_failure("oversized").await;
}
#[tokio::test]
async fn fetch_treats_transport_failure_as_an_empty_catalogue() {
    expected_failure("transport").await;
}

#[tokio::test]
async fn fetch_propagates_caller_cancellation() {
    let transport = Arc::new(Transport::body(Vec::new()));
    let error = ModelsDevInformationProvider::new(transport)
        .fetch(&Token(true))
        .await
        .unwrap_err();
    assert!(matches!(
        error,
        Error::Http(HttpError::Cancelled) | Error::Cancelled
    ));
}

#[tokio::test(start_paused = true)]
async fn fetch_times_out_while_reading_the_response_body() {
    let mut transport = Transport::body(Vec::new());
    transport.stalled = true;
    let transport = Arc::new(transport);
    let provider = ModelsDevInformationProvider::new(transport.clone());
    let started = tokio::time::Instant::now();
    let catalogue = provider.fetch(&ACTIVE).await.unwrap();
    assert!(catalogue.is_empty());
    assert!(started.elapsed() < Duration::from_secs(10));
    assert_eq!(started.elapsed(), Duration::from_secs(5));
    assert_eq!(transport.drops.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn fetch_swallows_uncancelled_transport_cancellation_without_suppressing_native_protocol_errors()
 {
    for failure in [
        HttpError::Cancelled,
        HttpError::RequestCancelled("The operation was canceled.".into()),
        HttpError::ProviderHttp("provider: response limit".into()),
    ] {
        let mut transport = Transport::body(Vec::new());
        transport.failure = Some(failure);
        assert!(
            ModelsDevInformationProvider::new(Arc::new(transport))
                .fetch(&ACTIVE)
                .await
                .unwrap()
                .is_empty()
        );
    }
    let mut transport = Transport::body(Vec::new());
    transport.failure = Some(HttpError::Wire("unexpected wire failure".into()));
    assert_eq!(
        ModelsDevInformationProvider::new(Arc::new(transport))
            .fetch(&ACTIVE)
            .await
            .unwrap_err(),
        Error::Http(HttpError::Wire("unexpected wire failure".into()))
    );
}
