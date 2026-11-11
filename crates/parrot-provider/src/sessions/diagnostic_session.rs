//! Source-shaped diagnostic session: tracing begins on the first stream pull.
use super::{
    Event, Request,
    diagnostics::{
        CallContext, CallTrace, DiagnosticClock, DiagnosticSink, LastRequestDumper, TraceOutcome,
    },
    runtime::{Error, EventStream, Session},
};
use crate::{
    http_stream::HttpError,
    responses::ResponsesError,
    responses_websocket::{SocketErrorCode, WebSocketError},
};
use parrot_core::ErrorCode;
use parrot_domain::contracts::async_ops::{BoxFuture, Cancellation};
use std::sync::{
    Arc,
    atomic::{AtomicU64, Ordering},
};

pub struct DiagnosticSession {
    inner: Arc<dyn Session>,
    sink: Arc<dyn DiagnosticSink>,
    clock: Arc<dyn DiagnosticClock>,
    agent_session_id: String,
    provider_id: String,
    dumper: Option<Arc<LastRequestDumper>>,
}

impl DiagnosticSession {
    pub fn new(
        inner: Arc<dyn Session>,
        sink: Arc<dyn DiagnosticSink>,
        clock: Arc<dyn DiagnosticClock>,
        agent_session_id: String,
        provider_id: String,
        dumper: Option<Arc<LastRequestDumper>>,
    ) -> Self {
        Self {
            inner,
            sink,
            clock,
            agent_session_id,
            provider_id,
            dumper,
        }
    }
}
impl Session for DiagnosticSession {
    fn begin_turn(&self) -> BoxFuture<'_, Result<(), Error>> {
        self.inner.begin_turn()
    }
    fn call(&self, request: Request) -> Result<Box<dyn EventStream>, Error> {
        Ok(Box::new(DiagnosticStream {
            session: self.inner.clone(),
            request: Some(request),
            sink: self.sink.clone(),
            clock: self.clock.clone(),
            agent_session_id: self.agent_session_id.clone(),
            provider_id: self.provider_id.clone(),
            dumper: self.dumper.clone(),
            trace: None,
            inner: None,
            done: false,
        }))
    }
    fn try_fall_back_to_http(&self) -> BoxFuture<'_, Result<bool, Error>> {
        self.inner.try_fall_back_to_http()
    }
    fn close(&self) -> BoxFuture<'_, Result<(), Error>> {
        self.inner.close()
    }
}

struct DiagnosticStream {
    session: Arc<dyn Session>,
    request: Option<Request>,
    sink: Arc<dyn DiagnosticSink>,
    clock: Arc<dyn DiagnosticClock>,
    agent_session_id: String,
    provider_id: String,
    dumper: Option<Arc<LastRequestDumper>>,
    trace: Option<Arc<CallTrace>>,
    inner: Option<Box<dyn EventStream>>,
    done: bool,
}
impl DiagnosticStream {
    fn start(&mut self) -> Result<(), Error> {
        let Some(request) = self.request.take() else {
            return Ok(());
        };
        static NEXT_CALL: AtomicU64 = AtomicU64::new(0);
        let trace = Arc::new(CallTrace::start(
            self.sink.clone(),
            self.clock.clone(),
            CallContext {
                agent_session_id: self.agent_session_id.clone(),
                correlation_id: format!(
                    "{:016x}{:016x}",
                    std::process::id(),
                    NEXT_CALL.fetch_add(1, Ordering::Relaxed)
                ),
                provider_id: self.provider_id.clone(),
                model_id: request.model.clone(),
            },
            self.dumper.clone(),
        ));
        self.trace = Some(trace.clone());
        self.inner = Some(self.session.call_with_trace(request, trace)?);
        Ok(())
    }

    async fn finish(&mut self, outcome: TraceOutcome, failure: Option<Error>) -> Result<(), Error> {
        // Source finally semantics: disposal failure replaces an earlier read
        // failure, but cannot replace a terminal already recorded by cancellation.
        let dispose_failure = if let Some(mut inner) = self.inner.take() {
            inner.close().await.err()
        } else {
            None
        };
        let failure = dispose_failure.or(failure);
        if let Some(trace) = &self.trace {
            trace.finish(
                failure
                    .as_ref()
                    .map_or(outcome, |error| TraceOutcome::Failed(failure_code(error))),
            );
        }
        self.done = true;
        failure.map_or(Ok(()), Err)
    }
}
impl EventStream for DiagnosticStream {
    fn next<'a>(
        &'a mut self,
        cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<Option<Event>, Error>> {
        Box::pin(async move {
            if self.done {
                return Ok(None);
            }
            if let Err(error) = self.start() {
                return self
                    .finish(TraceOutcome::Disposed, Some(error))
                    .await
                    .map(|()| None);
            }
            let result = if cancellation.is_cancelled() {
                Err(Error::Cancelled)
            } else {
                let inner = self.inner.as_mut().expect("started diagnostic stream");
                inner.next(cancellation).await
            };
            let trace = self.trace.as_ref().expect("started diagnostic trace");
            match result {
                Ok(Some(event)) => {
                    match event.base_event() {
                        Event::Retry {
                            attempt,
                            retry_after,
                            ..
                        } => trace.record_retry(*attempt, *retry_after),
                        Event::Completed(completion) => trace.record_tokens(
                            i64::from(completion.input_tokens),
                            i64::from(completion.cached_input_tokens),
                            i64::from(completion.output_tokens),
                        ),
                        _ => {}
                    }
                    Ok(Some(event))
                }
                Ok(None) => self
                    .finish(TraceOutcome::Completed, None)
                    .await
                    .map(|()| None),
                Err(error) => {
                    // Only an observed caller cancellation wins the race with
                    // disposal; an inner cancellation error can be superseded.
                    if cancellation.is_cancelled() {
                        trace.cancel();
                    }
                    self.finish(TraceOutcome::Disposed, Some(error))
                        .await
                        .map(|()| None)
                }
            }
        })
    }
    fn close(&mut self) -> BoxFuture<'_, Result<(), Error>> {
        Box::pin(async move {
            if self.done {
                return Ok(());
            }
            self.finish(TraceOutcome::Disposed, None).await
        })
    }
}
impl Drop for DiagnosticStream {
    fn drop(&mut self) {
        // Releasing the inner stream first preserves request-before-call order.
        self.inner = None;
        if !self.done
            && let Some(trace) = &self.trace
        {
            trace.finish(TraceOutcome::Disposed);
        }
    }
}

pub fn failure_code(error: &Error) -> ErrorCode {
    match error {
        Error::WebSocketTransport { cause, .. } => failure_code(cause),
        Error::Authentication(error) => match error {
            parrot_foundation::oauth::AuthError::Transport
            | parrot_foundation::oauth::AuthError::TransportWithMessage(_) => ErrorCode::Http,
            parrot_foundation::oauth::AuthError::Io
            | parrot_foundation::oauth::AuthError::IoWithMessage(_) => ErrorCode::Io,
            parrot_foundation::oauth::AuthError::InvalidOperation(_) => ErrorCode::InvalidOperation,
            parrot_foundation::oauth::AuthError::ArgumentOutOfRange(_)
            | parrot_foundation::oauth::AuthError::ArgumentOutOfRangeWithParameter { .. } => {
                ErrorCode::Argument
            }
            _ => ErrorCode::Unexpected,
        },
        Error::Cancelled | Error::Http(HttpError::Cancelled) => ErrorCode::Cancelled,
        Error::Http(HttpError::ProviderHttp(_)) => ErrorCode::Unexpected,
        Error::Io(_) => ErrorCode::Io,
        Error::InvalidOperation(_) | Error::Disposed | Error::Http(HttpError::Disposed) => {
            ErrorCode::InvalidOperation
        }
        Error::Wire(_)
        | Error::Http(HttpError::Wire(_))
        | Error::Response(ResponsesError::WireProtocol(_)) => ErrorCode::WireProtocol,
        Error::Http(HttpError::Transport(_)) => ErrorCode::Http,
        Error::Http(HttpError::RequestTooLarge(_)) => ErrorCode::RequestTooLarge,
        Error::Socket(error) => socket_failure_code(error),
        _ => ErrorCode::Unexpected,
    }
}
pub(super) fn socket_failure_code(error: &WebSocketError) -> ErrorCode {
    match error {
        WebSocketError::Cancelled => ErrorCode::Cancelled,
        WebSocketError::Upgrade { .. } => ErrorCode::WebSocketUpgrade,
        WebSocketError::TransportFailure { cause, .. } => socket_failure_code(cause),
        WebSocketError::Transport(_) => ErrorCode::WebSocketTransport,
        WebSocketError::Socket {
            code: SocketErrorCode::ConnectionClosedPrematurely,
            ..
        } => ErrorCode::WebSocketClosed,
        WebSocketError::Socket { .. } => ErrorCode::WebSocket,
        WebSocketError::Wire(_) | WebSocketError::Response(ResponsesError::WireProtocol(_)) => {
            ErrorCode::WireProtocol
        }
        WebSocketError::RequestTooLarge { .. } => ErrorCode::RequestTooLarge,
        WebSocketError::MessageTooLarge => ErrorCode::WebSocketMessageTooLarge,
        WebSocketError::InvalidRequest(_) => ErrorCode::Argument,
        WebSocketError::Disposed => ErrorCode::InvalidOperation,
        _ => ErrorCode::Unexpected,
    }
}
