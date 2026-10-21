//! Source-shaped diagnostic session: tracing begins on the first stream pull.
use super::{
    Event, Request,
    diagnostics::{
        CallContext, CallTrace, DiagnosticClock, DiagnosticSink, FailureKind, LastRequestDumper,
        RequestTrace, TraceOutcome,
    },
    runtime::{Error, EventStream, Session},
};
use crate::{
    http_stream::HttpError,
    responses::ResponsesError,
    responses_websocket::{SocketErrorCode, WebSocketError},
};
use parrot_domain::contracts::async_ops::{BoxFuture, Cancellation};
use std::sync::{
    Arc,
    atomic::{AtomicU64, Ordering},
};
use tokio::task::JoinHandle;

pub struct DiagnosticSession {
    inner: Arc<dyn Session>,
    sink: Arc<dyn DiagnosticSink>,
    clock: Arc<dyn DiagnosticClock>,
    agent_session_id: String,
    provider_id: String,
    dumper: Option<Arc<LastRequestDumper>>,
}

/// The source implementation's name.
pub type DiagnosticProviderSession = DiagnosticSession;

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

    /// Owns the source call token until the stream terminates or is dropped.
    /// Unlike `Session::call`, cancellation is observed even while the consumer
    /// is suspended between pulls. The watcher starts lazily with the call.
    pub fn call_cancellable(
        &self,
        request: Request,
        cancellation: Arc<dyn Cancellation>,
    ) -> Result<Box<dyn EventStream>, Error> {
        Ok(self.stream(request, Some(cancellation)))
    }

    fn stream(
        &self,
        request: Request,
        cancellation: Option<Arc<dyn Cancellation>>,
    ) -> Box<dyn EventStream> {
        Box::new(DiagnosticStream {
            session: self.inner.clone(),
            request: Some(request),
            sink: self.sink.clone(),
            clock: self.clock.clone(),
            agent_session_id: self.agent_session_id.clone(),
            provider_id: self.provider_id.clone(),
            dumper: self.dumper.clone(),
            cancellation,
            trace: None,
            inner: None,
            watcher: None,
            done: false,
        })
    }
}
impl Session for DiagnosticSession {
    fn begin_turn(&self) -> BoxFuture<'_, Result<(), Error>> {
        self.inner.begin_turn()
    }
    /// Without an owning call token only the current pull's cancellation can be
    /// observed. Use `call_cancellable` for cancellation between pulls.
    fn call(&self, request: Request) -> Result<Box<dyn EventStream>, Error> {
        Ok(self.stream(request, None))
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
    cancellation: Option<Arc<dyn Cancellation>>,
    trace: Option<Arc<CallTrace>>,
    inner: Option<Box<dyn EventStream>>,
    watcher: Option<JoinHandle<()>>,
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
        if let Some(cancellation) = self.cancellation.clone() {
            trace.observe_cancellation(cancellation.is_cancelled());
            let trace = trace.clone();
            self.watcher = Some(tokio::spawn(async move {
                cancellation.cancelled().await;
                trace.cancel();
            }));
        }
        self.inner = Some(self.session.call_with_trace(request, trace)?);
        Ok(())
    }

    async fn finish(&mut self, outcome: TraceOutcome, failure: Option<Error>) -> Result<(), Error> {
        if let Some(trace) = &self.trace {
            if self
                .cancellation
                .as_ref()
                .is_some_and(|token| token.is_cancelled())
            {
                trace.cancel();
            }
        }
        // Source finally semantics: disposal failure replaces an earlier read
        // failure, but cannot replace a terminal already recorded by cancellation.
        let dispose_failure = if let Some(mut inner) = self.inner.take() {
            inner.close().await.err()
        } else {
            None
        };
        let failure = dispose_failure.or(failure);
        if let Some(trace) = &self.trace {
            if self
                .cancellation
                .as_ref()
                .is_some_and(|token| token.is_cancelled())
            {
                trace.cancel();
            }
            trace.finish(
                failure
                    .as_ref()
                    .map_or(outcome, |error| TraceOutcome::Failed(failure_kind(error))),
            );
        }
        if let Some(watcher) = self.watcher.take() {
            watcher.abort();
            let _ = watcher.await;
        }
        self.cancellation = None;
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
            let combined = PullCancellation {
                caller: cancellation,
                owner: self.cancellation.as_deref(),
            };
            let result = if combined.is_cancelled() {
                Err(Error::Cancelled)
            } else {
                let inner = self.inner.as_mut().expect("started diagnostic stream");
                inner.next(&combined).await
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
                    if combined.is_cancelled() {
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
        if let Some(watcher) = self.watcher.take() {
            watcher.abort();
        }
        if !self.done
            && self
                .cancellation
                .as_ref()
                .is_some_and(|token| token.is_cancelled())
        {
            if let Some(trace) = &self.trace {
                trace.cancel();
            }
        }
        // Releasing the inner stream first preserves request-before-call order.
        self.inner = None;
        if !self.done {
            if let Some(trace) = &self.trace {
                trace.finish(TraceOutcome::Disposed);
            }
        }
    }
}

/// Traces one generic provider attempt, including non-HTTP synthetic providers.
/// The enclosing call's `CallTrace::cancel` also cancels this attempt while the
/// consumer is suspended. Transport-specific byte hooks remain with adapters.
pub struct RequestTraceStream {
    inner: Option<Box<dyn EventStream>>,
    call_trace: Arc<CallTrace>,
    transport: String,
    request_id: String,
    trace: Option<RequestTrace>,
    done: bool,
}
impl RequestTraceStream {
    pub fn new(
        inner: Box<dyn EventStream>,
        call_trace: Arc<CallTrace>,
        transport: String,
        request_id: String,
    ) -> Self {
        Self {
            inner: Some(inner),
            call_trace,
            transport,
            request_id,
            trace: None,
            done: false,
        }
    }

    async fn finish(&mut self, outcome: TraceOutcome, failure: Option<Error>) -> Result<(), Error> {
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
                    .map_or(outcome, |error| TraceOutcome::Failed(failure_kind(error))),
            );
        }
        self.done = true;
        failure.map_or(Ok(()), Err)
    }
}
impl EventStream for RequestTraceStream {
    fn next<'a>(
        &'a mut self,
        cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<Option<Event>, Error>> {
        Box::pin(async move {
            if self.done {
                return Ok(None);
            }
            let trace = self.trace.get_or_insert_with(|| {
                self.call_trace
                    .begin_request(&self.transport, &self.request_id)
            });
            let result = if cancellation.is_cancelled() {
                Err(Error::Cancelled)
            } else {
                let inner = self.inner.as_mut().expect("open request stream");
                inner.next(cancellation).await
            };
            match result {
                Ok(Some(event)) => Ok(Some(event)),
                Ok(None) => self
                    .finish(TraceOutcome::Completed, None)
                    .await
                    .map(|()| None),
                Err(error) => {
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
impl Drop for RequestTraceStream {
    fn drop(&mut self) {
        self.inner = None;
        if !self.done {
            if let Some(trace) = &self.trace {
                trace.finish(TraceOutcome::Disposed);
            }
        }
    }
}

struct PullCancellation<'a> {
    caller: &'a dyn Cancellation,
    owner: Option<&'a dyn Cancellation>,
}
impl Cancellation for PullCancellation<'_> {
    fn is_cancelled(&self) -> bool {
        self.caller.is_cancelled() || self.owner.is_some_and(Cancellation::is_cancelled)
    }
    fn cancelled(&self) -> BoxFuture<'_, ()> {
        Box::pin(async move {
            if let Some(owner) = self.owner {
                tokio::select! { () = self.caller.cancelled() => {}, () = owner.cancelled() => {} }
            } else {
                self.caller.cancelled().await;
            }
        })
    }
}

pub fn failure_kind(error: &Error) -> FailureKind {
    match error {
        Error::WebSocketTransport { cause, .. } => {
            FailureKind::websocket_transport(Some(failure_kind(cause)))
        }
        Error::Authentication(error) => match error {
            parrot_foundation::oauth::AuthError::Transport
            | parrot_foundation::oauth::AuthError::TransportWithMessage(_) => FailureKind::Http,
            parrot_foundation::oauth::AuthError::Io
            | parrot_foundation::oauth::AuthError::IoWithMessage(_) => FailureKind::Io,
            parrot_foundation::oauth::AuthError::InvalidOperation(_) => {
                FailureKind::InvalidOperation
            }
            parrot_foundation::oauth::AuthError::ArgumentOutOfRange(_)
            | parrot_foundation::oauth::AuthError::ArgumentOutOfRangeWithParameter { .. } => {
                FailureKind::Argument
            }
            _ => FailureKind::Unexpected,
        },
        Error::Cancelled
        | Error::Cancellation(_)
        | Error::Http(HttpError::Cancelled | HttpError::RequestCancelled(_)) => {
            FailureKind::Cancelled
        }
        Error::Http(HttpError::ProviderHttp(_)) => FailureKind::Unexpected,
        Error::Io(_) => FailureKind::Io,
        Error::InvalidOperation(_) | Error::Disposed | Error::Http(HttpError::Disposed) => {
            FailureKind::InvalidOperation
        }
        Error::Wire(_)
        | Error::Http(HttpError::Wire(_))
        | Error::Response(ResponsesError::WireProtocol(_)) => FailureKind::WireProtocol,
        Error::Http(HttpError::Transport(_) | HttpError::Http(_)) => FailureKind::Http,
        Error::Http(HttpError::RequestTooLarge(_)) => FailureKind::RequestTooLarge,
        Error::Socket(error) => socket_failure_kind(error),
        _ => FailureKind::Unexpected,
    }
}
pub(super) fn socket_failure_kind(error: &WebSocketError) -> FailureKind {
    match error {
        WebSocketError::Cancelled => FailureKind::Cancelled,
        WebSocketError::Upgrade { .. } => FailureKind::WebSocketUpgrade,
        WebSocketError::TransportFailure { cause, .. } => socket_failure_kind(cause),
        WebSocketError::Transport(_) => FailureKind::WebSocketTransport,
        WebSocketError::Socket {
            code: SocketErrorCode::ConnectionClosedPrematurely,
            ..
        } => FailureKind::WebSocketClosed,
        WebSocketError::Socket { .. } => FailureKind::WebSocket,
        WebSocketError::Wire(_) | WebSocketError::Response(ResponsesError::WireProtocol(_)) => {
            FailureKind::WireProtocol
        }
        WebSocketError::RequestTooLarge { .. } => FailureKind::RequestTooLarge,
        WebSocketError::MessageTooLarge => FailureKind::WebSocketMessageTooLarge,
        WebSocketError::InvalidRequest(_) => FailureKind::Argument,
        WebSocketError::InvalidOperation(_) | WebSocketError::Disposed => {
            FailureKind::InvalidOperation
        }
        _ => FailureKind::Unexpected,
    }
}
