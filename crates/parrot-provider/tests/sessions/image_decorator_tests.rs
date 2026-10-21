use parrot_domain::contracts::async_ops::{BoxFuture, Cancellation};
use parrot_provider::{
    catalogs::Model,
    image_generation::{ImageGenerationReference, ImageGenerationRequest, ImageGenerationResult},
    sessions::{
        Content, Error, EventStream, Provider, Request, Session,
        decorators::{
            RetrySleeper, RetryingProvider, SubscriptionUsage, UsageReporter,
            UsageReportingProvider,
        },
    },
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
struct ImageSpy {
    request: Mutex<Option<ImageGenerationRequest>>,
    calls: AtomicUsize,
    token: Arc<Token>,
    result: Result<ImageGenerationResult, Error>,
}
impl Provider for ImageSpy {
    fn id(&self) -> &str {
        "image-spy"
    }
    fn call(&self, _: Request) -> Result<Box<dyn EventStream>, Error> {
        panic!("image call must not open streaming")
    }
    fn has_credential<'a>(&'a self, _: &'a dyn Cancellation) -> BoxFuture<'a, Result<bool, Error>> {
        panic!("image wrapper must not pre-resolve credentials")
    }
    fn seed_models(&self) -> Vec<Model> {
        vec![]
    }
    fn list_models<'a>(
        &'a self,
        _: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<Vec<Model>, Error>> {
        panic!("image wrapper must not query models")
    }
    fn calculate_image_tokens(&self, _: &Model, _: &Content) -> i64 {
        panic!("image wrapper must not estimate tokens")
    }
    fn open_session(&self) -> Result<Arc<dyn Session>, Error> {
        panic!("image wrapper must not open a retry session")
    }
    fn generate_image<'a>(
        &'a self,
        request: ImageGenerationRequest,
        cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<ImageGenerationResult, Error>> {
        Box::pin(async move {
            assert!(std::ptr::eq(
                cancellation,
                self.token.as_ref() as &dyn Cancellation
            ));
            self.calls.fetch_add(1, Ordering::SeqCst);
            *self.request.lock().unwrap() = Some(request);
            self.result.clone()
        })
    }
}
struct DefaultImageProvider;
impl Provider for DefaultImageProvider {
    fn id(&self) -> &str {
        "default-image"
    }
    fn call(&self, _: Request) -> Result<Box<dyn EventStream>, Error> {
        panic!("unsupported image must not open streaming")
    }
    fn has_credential<'a>(&'a self, _: &'a dyn Cancellation) -> BoxFuture<'a, Result<bool, Error>> {
        panic!("unsupported image must not resolve credentials")
    }
    fn seed_models(&self) -> Vec<Model> {
        vec![]
    }
    fn list_models<'a>(
        &'a self,
        _: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<Vec<Model>, Error>> {
        panic!("unsupported image must not query models")
    }
    fn calculate_image_tokens(&self, _: &Model, _: &Content) -> i64 {
        panic!("unsupported image must not estimate tokens")
    }
    fn open_session(&self) -> Result<Arc<dyn Session>, Error> {
        panic!("unsupported image must not open fallback session")
    }
}

#[tokio::test]
async fn default_image_capability_returns_exact_unsupported_without_io_even_if_cancelled() {
    for cancelled in [false, true] {
        let provider: Arc<dyn Provider> = Arc::new(DefaultImageProvider);
        assert_eq!(
            provider
                .generate_image(ImageGenerationRequest::default(), &Token(cancelled))
                .await,
            Err(Error::Provider(
                "provider: image generation is not supported".into()
            ))
        );
    }
}

struct NoWait;
impl RetrySleeper for NoWait {
    fn sleep<'a>(
        &'a self,
        _: Duration,
        _: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<(), Error>> {
        panic!("image generation must never schedule retry")
    }
}
struct NoUsage;
impl UsageReporter for NoUsage {
    fn usage<'a>(
        &'a self,
        _: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<SubscriptionUsage, Error>> {
        panic!("image generation must not fetch quota")
    }
}

#[tokio::test]
async fn retry_and_usage_forward_image_request_result_error_and_same_cancellation_once() {
    for retry_outer in [false, true] {
        for cancelled in [false, true] {
            for result in [
                Ok(ImageGenerationResult {
                    data: vec![0, 255, 42],
                }),
                Err(Error::Provider(
                    "provider: image generation failed (HTTP 503)".into(),
                )),
                Err(Error::Io("synthetic image transport failure".into())),
                Err(Error::Cancelled),
            ] {
                let token = Arc::new(Token(cancelled));
                let spy = Arc::new(ImageSpy {
                    request: Mutex::new(None),
                    calls: AtomicUsize::new(0),
                    token: token.clone(),
                    result: result.clone(),
                });
                let provider: Arc<dyn Provider> = if retry_outer {
                    Arc::new(RetryingProvider::with_sleeper(
                        Arc::new(UsageReportingProvider::new(spy.clone(), Arc::new(NoUsage))),
                        99,
                        Arc::new(NoWait),
                    ))
                } else {
                    Arc::new(UsageReportingProvider::new(
                        Arc::new(RetryingProvider::with_sleeper(
                            spy.clone(),
                            99,
                            Arc::new(NoWait),
                        )),
                        Arc::new(NoUsage),
                    ))
                };
                // Deliberately opaque input: transparent decorators must not validate,
                // rewrite the prompt, inspect references or substitute cancellation.
                let request = ImageGenerationRequest {
                    prompt: "  exact prompt\n".into(),
                    references: vec![ImageGenerationReference {
                        media_type: "synthetic/opaque".into(),
                        data: vec![4, 0, 3],
                    }],
                };
                assert_eq!(
                    provider
                        .generate_image(request.clone(), token.as_ref())
                        .await,
                    result
                );
                assert_eq!(*spy.request.lock().unwrap(), Some(request));
                assert_eq!(spy.calls.load(Ordering::SeqCst), 1);
            }
        }
    }
}
