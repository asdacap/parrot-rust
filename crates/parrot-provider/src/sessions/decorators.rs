//! Transparent provider capabilities and pull-driven retry orchestration.
use super::{
    Error, Event, EventStream, Provider, Request, Session,
    retry::{Failure, RetryPolicy},
};
use crate::{
    http_stream::HttpError, responses::ResponsesError, responses_websocket::WebSocketError,
};
use parrot_domain::contracts::async_ops::{BoxFuture, Cancellation};
use std::{sync::Arc, time::Duration};

#[derive(Clone, Debug, Default, PartialEq)]
pub struct SubscriptionUsage {
    pub plan_type: String,
    pub primary_window: Option<UsageWindow>,
    pub secondary_window: Option<UsageWindow>,
    pub credits: Option<UsageCredits>,
}
#[derive(Clone, Debug, PartialEq)]
pub struct UsageWindow {
    pub used_percent: f64,
    /// Source DateTimeOffset text, retained without timezone/precision conversion.
    pub reset_at: String,
    pub limit_window_seconds: i64,
}
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct UsageCredits {
    pub has_credits: bool,
    pub balance: String,
}
/// Optional account quota capability, not a completed-token event observer.
pub trait UsageReporter: Send + Sync {
    fn usage<'a>(
        &'a self,
        cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<SubscriptionUsage, Error>>;
}

pub struct UsageReportingProvider {
    inner: Arc<dyn Provider>,
    reporter: Arc<dyn UsageReporter>,
}
impl UsageReportingProvider {
    pub fn new(inner: Arc<dyn Provider>, reporter: Arc<dyn UsageReporter>) -> Self {
        Self { inner, reporter }
    }
}
impl Provider for UsageReportingProvider {
    fn id(&self) -> &str {
        self.inner.id()
    }
    fn usage_reporter(&self) -> Option<Arc<dyn UsageReporter>> {
        Some(self.reporter.clone())
    }
    fn call(&self, request: Request) -> Result<Box<dyn EventStream>, Error> {
        self.inner.call(request)
    }
    fn generate_image<'a>(
        &'a self,
        request: crate::image_generation::ImageGenerationRequest,
        cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<crate::image_generation::ImageGenerationResult, Error>> {
        self.inner.generate_image(request, cancellation)
    }
    fn has_credential<'a>(
        &'a self,
        cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<bool, Error>> {
        self.inner.has_credential(cancellation)
    }
    fn seed_models(&self) -> Vec<crate::catalogs::Model> {
        self.inner.seed_models()
    }
    fn list_models<'a>(
        &'a self,
        cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<Vec<crate::catalogs::Model>, Error>> {
        self.inner.list_models(cancellation)
    }
    fn calculate_image_tokens(
        &self,
        model: &crate::catalogs::Model,
        content: &super::Content,
    ) -> i64 {
        self.inner.calculate_image_tokens(model, content)
    }
    fn open_session(&self) -> Result<Arc<dyn Session>, Error> {
        self.inner.open_session()
    }
}

/// Injectable timer boundary equivalent to the source retry TimeProvider.
/// The retry stream invokes it only for nonzero, noncancelled waits.
pub trait RetrySleeper: Send + Sync {
    fn sleep<'a>(
        &'a self,
        delay: Duration,
        cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<(), Error>>;
}
#[derive(Default)]
pub struct TokioRetrySleeper;
impl RetrySleeper for TokioRetrySleeper {
    fn sleep<'a>(
        &'a self,
        delay: Duration,
        cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<(), Error>> {
        Box::pin(async move {
            if cancellation.is_cancelled() {
                return Err(Error::Cancelled);
            }
            if delay.is_zero() {
                return Ok(());
            }
            tokio::select! {
                biased;
                _ = cancellation.cancelled() => Err(Error::Cancelled),
                _ = tokio::time::sleep(delay) => Ok(()),
            }
        })
    }
}

pub struct RetryingProvider {
    inner: Arc<dyn Provider>,
    header_max_retries: u32,
    sleeper: Arc<dyn RetrySleeper>,
}
impl RetryingProvider {
    pub fn new(inner: Arc<dyn Provider>, header_max_retries: u32) -> Self {
        Self::with_sleeper(inner, header_max_retries, Arc::new(TokioRetrySleeper))
    }
    pub fn with_sleeper(
        inner: Arc<dyn Provider>,
        header_max_retries: u32,
        sleeper: Arc<dyn RetrySleeper>,
    ) -> Self {
        Self {
            inner,
            header_max_retries,
            sleeper,
        }
    }
}
impl Provider for RetryingProvider {
    fn id(&self) -> &str {
        self.inner.id()
    }
    fn usage_reporter(&self) -> Option<Arc<dyn UsageReporter>> {
        self.inner.usage_reporter()
    }
    fn call(&self, request: Request) -> Result<Box<dyn EventStream>, Error> {
        Ok(Box::new(RetryStream::new(
            RetryTarget::Provider(self.inner.clone()),
            request,
            self.header_max_retries,
            self.sleeper.clone(),
        )))
    }
    fn generate_image<'a>(
        &'a self,
        request: crate::image_generation::ImageGenerationRequest,
        cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<crate::image_generation::ImageGenerationResult, Error>> {
        self.inner.generate_image(request, cancellation)
    }
    fn has_credential<'a>(
        &'a self,
        cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<bool, Error>> {
        self.inner.has_credential(cancellation)
    }
    fn seed_models(&self) -> Vec<crate::catalogs::Model> {
        self.inner.seed_models()
    }
    fn list_models<'a>(
        &'a self,
        cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<Vec<crate::catalogs::Model>, Error>> {
        self.inner.list_models(cancellation)
    }
    fn calculate_image_tokens(
        &self,
        model: &crate::catalogs::Model,
        content: &super::Content,
    ) -> i64 {
        self.inner.calculate_image_tokens(model, content)
    }
    fn open_session(&self) -> Result<Arc<dyn Session>, Error> {
        Ok(Arc::new(RetryingSession {
            inner: self.inner.open_session()?,
            header_max_retries: self.header_max_retries,
            sleeper: self.sleeper.clone(),
        }))
    }
}
struct RetryingSession {
    inner: Arc<dyn Session>,
    header_max_retries: u32,
    sleeper: Arc<dyn RetrySleeper>,
}
impl Session for RetryingSession {
    fn begin_turn(&self) -> BoxFuture<'_, Result<(), Error>> {
        self.inner.begin_turn()
    }
    fn try_fall_back_to_http(&self) -> BoxFuture<'_, Result<bool, Error>> {
        self.inner.try_fall_back_to_http()
    }
    fn close(&self) -> BoxFuture<'_, Result<(), Error>> {
        self.inner.close()
    }
    fn call(&self, request: Request) -> Result<Box<dyn EventStream>, Error> {
        Ok(Box::new(RetryStream::new(
            RetryTarget::Session(self.inner.clone()),
            request,
            self.header_max_retries,
            self.sleeper.clone(),
        )))
    }
    fn call_with_trace(
        &self,
        request: Request,
        trace: Arc<super::diagnostics::CallTrace>,
    ) -> Result<Box<dyn EventStream>, Error> {
        let mut stream = RetryStream::new(
            RetryTarget::Session(self.inner.clone()),
            request,
            self.header_max_retries,
            self.sleeper.clone(),
        );
        stream.trace = Some(trace);
        Ok(Box::new(stream))
    }
}
enum RetryTarget {
    Provider(Arc<dyn Provider>),
    Session(Arc<dyn Session>),
}
struct RetryStream {
    target: RetryTarget,
    trace: Option<Arc<super::diagnostics::CallTrace>>,
    request: Request,
    policy: RetryPolicy,
    sleeper: Arc<dyn RetrySleeper>,
    attempt: Option<Box<dyn EventStream>>,
    delay: Option<Duration>,
    done: bool,
}
impl RetryStream {
    fn new(
        target: RetryTarget,
        request: Request,
        header_max_retries: u32,
        sleeper: Arc<dyn RetrySleeper>,
    ) -> Self {
        Self {
            target,
            sleeper,
            trace: None,
            policy: RetryPolicy::new(request.max_output_tokens, header_max_retries),
            request,
            attempt: None,
            delay: None,
            done: false,
        }
    }
    fn call_attempt(&self) -> Result<Box<dyn EventStream>, Error> {
        match &self.target {
            RetryTarget::Provider(provider) => provider.call(self.request.clone()),
            RetryTarget::Session(session) => match &self.trace {
                Some(trace) => session.call_with_trace(self.request.clone(), trace.clone()),
                None => session.call(self.request.clone()),
            },
        }
    }
    async fn close_attempt(&mut self) -> Result<(), Error> {
        if let Some(mut attempt) = self.attempt.take() {
            attempt.close().await?;
        }
        Ok(())
    }
    async fn advance(&mut self, cancellation: &dyn Cancellation) -> Result<Option<Event>, Error> {
        if self.done {
            return Ok(None);
        }
        if let Some(delay) = self.delay {
            if cancellation.is_cancelled() {
                return Err(Error::Cancelled);
            }
            // Task.Delay returns synchronously for zero and creates no timer.
            if !delay.is_zero() {
                self.sleeper.sleep(delay, cancellation).await?;
            }
            self.delay = None;
        }
        let result = match self.attempt.as_mut() {
            Some(attempt) => attempt.next(cancellation).await,
            None => match self.call_attempt() {
                Ok(attempt) => {
                    self.attempt = Some(attempt);
                    self.attempt
                        .as_mut()
                        .expect("attempt was just assigned")
                        .next(cancellation)
                        .await
                }
                Err(error) => Err(error),
            },
        };
        match result {
            Ok(Some(event)) => {
                if event.is_visible_output() {
                    self.policy.mark_visible();
                }
                Ok(Some(event))
            }
            Ok(None) => {
                self.done = true;
                self.close_attempt().await?;
                Ok(None)
            }
            Err(original) => {
                let decision = self
                    .policy
                    .classify(&classify_failure(&original), cancellation.is_cancelled());
                self.close_attempt().await?;
                let action = match decision {
                    Ok(action) => action,
                    Err(Failure::HeaderRetryLimit {
                        maximum_retries,
                        attempts,
                    }) => {
                        return Err(Error::HeaderRetryLimit {
                            maximum_retries,
                            attempts,
                            cause: Box::new(original),
                        });
                    }
                    Err(_) => return Err(original),
                };
                if action.fallback_http {
                    match &self.target {
                        RetryTarget::Provider(_) => {
                            return Err(Error::InvalidOperation(
                                "A stateless provider cannot select HTTP fallback.".into(),
                            ));
                        }
                        RetryTarget::Session(session) => {
                            if !session.try_fall_back_to_http().await? {
                                return Err(Error::InvalidOperation("A provider call requested HTTP fallback without supporting it.".into()));
                            }
                        }
                    }
                    self.policy.reset_stream();
                }
                self.request.max_output_tokens = action.max_output_tokens;
                self.delay = Some(action.delay);
                Ok(Some(Event::Retry {
                    attempt: action.attempt as i32,
                    retry_after: action.delay,
                    reason: action.reason,
                }))
            }
        }
    }
}
impl EventStream for RetryStream {
    fn next<'a>(
        &'a mut self,
        cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<Option<Event>, Error>> {
        Box::pin(async move {
            let result = self.advance(cancellation).await;
            if result.is_err() {
                self.done = true;
            }
            result
        })
    }
    fn close(&mut self) -> BoxFuture<'_, Result<(), Error>> {
        Box::pin(async move {
            self.done = true;
            self.delay = None;
            self.close_attempt().await
        })
    }
}

/// Classification reads the actual typed adapter failure, never its formatted text.
pub fn classify_failure(error: &Error) -> Failure {
    match error {
        Error::WebSocketTransport { .. } => Failure::WebSocketTransport,
        Error::Authentication(
            parrot_foundation::oauth::AuthError::Transport
            | parrot_foundation::oauth::AuthError::TransportWithMessage(_)
            | parrot_foundation::oauth::AuthError::Io
            | parrot_foundation::oauth::AuthError::IoWithMessage(_),
        ) => Failure::Network,
        Error::Authentication(_) => Failure::Protocol,
        Error::HeaderRetryLimit { .. } | Error::HeaderTimeout(_) => Failure::HeaderTimeout,
        Error::Http(error) => match error {
            HttpError::Cancelled | HttpError::RequestCancelled(_) => Failure::Cancelled,
            HttpError::ProviderHttp(_) => Failure::Http {
                status: 0,
                error_type: String::new(),
                code: String::new(),
                detail: String::new(),
            },
            HttpError::HeaderTimeout(_) => Failure::HeaderTimeout,
            HttpError::Response(error) => Failure::Http {
                status: i32::from(error.status),
                error_type: error.error_type.clone(),
                code: error.error_code.clone(),
                detail: error.detail.clone(),
            },
            HttpError::RequestTooLarge(_) => Failure::RequestTooLarge,
            HttpError::Provider(_) => Failure::Configuration,
            HttpError::Wire(_) | HttpError::InvalidJsonString(_) => Failure::Protocol,
            _ => Failure::Network,
        },
        Error::Response(error) => response_failure(error),
        Error::ChatProvider(error) => Failure::Response {
            error_type: error.error_type.clone(),
            code: error.error_code.clone(),
            detail: error.message.clone(),
        },
        Error::Socket(error) => match error {
            WebSocketError::Cancelled => Failure::Cancelled,
            WebSocketError::Upgrade { status, .. } => Failure::Upgrade {
                status: i32::from(*status),
            },
            WebSocketError::Transport(_) | WebSocketError::TransportFailure { .. } => {
                Failure::WebSocketTransport
            }
            WebSocketError::Response(error) => response_failure(error),
            WebSocketError::RequestTooLarge { .. } => Failure::RequestTooLarge,
            _ => Failure::Protocol,
        },
        Error::Cancelled | Error::Cancellation(_) => Failure::Cancelled,
        Error::Io(_) => Failure::Network,
        Error::Provider(_) | Error::ProviderWithCause { .. } => Failure::Configuration,
        Error::Wire(_) | Error::Encode(_) | Error::InvalidOperation(_) | Error::Disposed => {
            Failure::Protocol
        }
    }
}
fn response_failure(error: &ResponsesError) -> Failure {
    match error {
        ResponsesError::ProviderResponse {
            error_type,
            error_code,
            detail,
            ..
        } => Failure::Response {
            error_type: error_type.clone(),
            code: error_code.clone(),
            detail: detail.clone(),
        },
        _ => Failure::Protocol,
    }
}
