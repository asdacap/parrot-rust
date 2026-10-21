//! Owning, pull-driven bridge over T29's borrowed Responses session calls.
use super::diagnostics::{CallTrace, RequestTrace, TraceOutcome};
use super::{Error, Event, EventStream, Request, Session};
use crate::responses_websocket::{self, HttpFallback, WebSocketError, WebSocketRequest};
use parrot_domain::contracts::async_ops::{BoxFuture, Cancellation};
use std::sync::{
    Arc, Mutex as StdMutex, Weak,
    atomic::{AtomicBool, AtomicU64, Ordering},
};
use tokio::sync::{Mutex, Notify, mpsc, oneshot};

/// The exact prepared T27 request and provider-owned HTTP fallback for one call.
/// The error slot preserves original header-source and HTTP failures through
/// T29's socket error boundary and is shared with those provider adapters.
pub struct ResponsePrepared {
    pub prepare_websocket: Box<dyn FnOnce() -> Result<WebSocketRequest, WebSocketError> + Send>,
    pub http: Arc<dyn HttpFallback>,
    pub http_error: Arc<StdMutex<Option<Error>>>,
}
pub trait ResponseCallFactory: Send + Sync {
    fn prepare(
        &self,
        request: Request,
        trace: Option<Arc<CallTrace>>,
    ) -> Result<ResponsePrepared, Error>;
}

#[derive(Default)]
struct Stop {
    cancelled: AtomicBool,
    notify: Notify,
}
impl Stop {
    fn cancel(&self) {
        self.cancelled.store(true, Ordering::Release);
        self.notify.notify_waiters();
    }
}
impl Cancellation for Stop {
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

type PullResult = Result<Option<Event>, Error>;
struct Pull {
    reply: oneshot::Sender<PullResult>,
}
struct Actor {
    stop: Stop,
    pulls: mpsc::UnboundedSender<Pull>,
    task: Mutex<Option<tokio::task::JoinHandle<()>>>,
}
impl Actor {
    async fn join(&self) -> Result<(), Error> {
        // Keep the lock through the join: close and a terminal read must both
        // observe completion, rather than one taking the handle prematurely.
        let mut task = self.task.lock().await;
        if let Some(handle) = task.as_mut() {
            let result = handle.await;
            *task = None;
            result.map_err(|_| Error::Provider("provider session task failed".into()))?;
        }
        Ok(())
    }
}
#[derive(Default)]
struct Registry {
    disposed: bool,
    actors: Vec<Weak<Actor>>,
}
struct Inner {
    session: Mutex<responses_websocket::Session>,
    factory: Arc<dyn ResponseCallFactory>,
    registry: StdMutex<Registry>,
    close: Mutex<Option<Result<(), Error>>>,
}

/// One T29 session is shared by all its owning streams, with no prefetch.
#[derive(Clone)]
pub struct ResponseSession {
    inner: Arc<Inner>,
}
impl ResponseSession {
    pub fn new(
        session: responses_websocket::Session,
        factory: Arc<dyn ResponseCallFactory>,
    ) -> Self {
        Self {
            inner: Arc::new(Inner {
                session: Mutex::new(session),
                factory,
                registry: StdMutex::new(Registry::default()),
                close: Mutex::new(None),
            }),
        }
    }
    fn check_open(&self) -> Result<(), Error> {
        if self
            .inner
            .registry
            .lock()
            .expect("session registry")
            .disposed
        {
            Err(Error::Disposed)
        } else {
            Ok(())
        }
    }
}
impl Session for ResponseSession {
    fn begin_turn(&self) -> BoxFuture<'_, Result<(), Error>> {
        Box::pin(async move {
            let mut session = self.inner.session.lock().await;
            self.check_open()?;
            session.begin_turn();
            Ok(())
        })
    }
    fn call(&self, request: Request) -> Result<Box<dyn EventStream>, Error> {
        self.open_stream(request, None)
    }
    fn call_with_trace(
        &self,
        request: Request,
        trace: Arc<CallTrace>,
    ) -> Result<Box<dyn EventStream>, Error> {
        self.open_stream(request, Some(trace))
    }
    fn try_fall_back_to_http(&self) -> BoxFuture<'_, Result<bool, Error>> {
        Box::pin(async move {
            let mut session = self.inner.session.lock().await;
            self.check_open()?;
            session.try_fall_back_to_http().map_err(Error::Socket)
        })
    }
    fn close(&self) -> BoxFuture<'_, Result<(), Error>> {
        Box::pin(async move {
            let mut closed = self.inner.close.lock().await;
            if let Some(result) = closed.as_ref() {
                return result.clone();
            }
            let actors: Vec<_> = {
                let mut registry = self.inner.registry.lock().expect("session registry");
                registry.disposed = true;
                registry.actors.iter().filter_map(Weak::upgrade).collect()
            };
            for actor in &actors {
                actor.stop.cancel();
            }
            let mut result = Ok(());
            for actor in actors {
                if let Err(error) = actor.join().await {
                    if result.is_ok() {
                        result = Err(error);
                    }
                }
            }
            self.inner.session.lock().await.dispose();
            *closed = Some(result.clone());
            result
        })
    }
}
impl ResponseSession {
    fn open_stream(
        &self,
        request: Request,
        trace: Option<Arc<CallTrace>>,
    ) -> Result<Box<dyn EventStream>, Error> {
        self.check_open()?;
        Ok(Box::new(ResponseStream {
            inner: self.inner.clone(),
            request: Some(request),
            trace,
            actor: None,
            pending: None,
            done: false,
        }))
    }
}

struct ResponseStream {
    inner: Arc<Inner>,
    request: Option<Request>,
    trace: Option<Arc<CallTrace>>,
    actor: Option<Arc<Actor>>,
    // Keep the pending response in the stream, so dropping a next() observer
    // does not lose its event or issue a second pull on the next observation.
    pending: Option<oneshot::Receiver<PullResult>>,
    done: bool,
}
impl ResponseStream {
    fn start(&mut self) -> Result<(), Error> {
        let mut registry = self.inner.registry.lock().expect("session registry");
        if registry.disposed {
            return Err(Error::Disposed);
        }
        let request = self.request.take().expect("unstarted call request");
        let (pulls, receiver) = mpsc::unbounded_channel();
        let actor = Arc::new(Actor {
            stop: Stop::default(),
            pulls,
            task: Mutex::new(None),
        });
        let inner = self.inner.clone();
        let trace = self.trace.take();
        let running = actor.clone();
        let task = tokio::spawn(async move {
            run(inner, running, receiver, request, trace).await;
        });
        // No task can be joined before this synchronous registration completes.
        *actor.task.try_lock().expect("new task lock") = Some(task);
        registry.actors.retain(|actor| actor.strong_count() != 0);
        registry.actors.push(Arc::downgrade(&actor));
        self.actor = Some(actor);
        Ok(())
    }
    async fn settle(&mut self) -> Result<(), Error> {
        self.done = true;
        self.pending = None;
        if let Some(actor) = &self.actor {
            actor.stop.cancel();
            actor.join().await?;
        }
        Ok(())
    }
}
impl EventStream for ResponseStream {
    fn next<'a>(&'a mut self, cancellation: &'a dyn Cancellation) -> BoxFuture<'a, PullResult> {
        Box::pin(async move {
            if self.done {
                return Ok(None);
            }
            if cancellation.is_cancelled() {
                self.settle().await?;
                return Err(Error::Cancelled);
            }
            if self.actor.is_none() {
                self.start()?;
            }
            let actor = self.actor.as_ref().expect("started actor").clone();
            if self.pending.is_none() {
                let (reply, response) = oneshot::channel();
                if actor.pulls.send(Pull { reply }).is_err() {
                    self.settle().await?;
                    return Err(Error::Disposed);
                }
                self.pending = Some(response);
            }
            let response = self.pending.as_mut().expect("pending pull");
            let result = tokio::select! {
                biased;
                result = &mut *response => result.unwrap_or(Err(Error::Disposed)),
                _ = cancellation.cancelled() => {
                    actor.stop.cancel();
                    response.await.unwrap_or(Err(Error::Cancelled))
                }
            };
            self.pending = None;
            if !matches!(&result, Ok(Some(_))) {
                self.settle().await?;
            }
            result
        })
    }
    fn close(&mut self) -> BoxFuture<'_, Result<(), Error>> {
        Box::pin(self.settle())
    }
}
impl Drop for ResponseStream {
    fn drop(&mut self) {
        if let Some(actor) = &self.actor {
            actor.stop.cancel();
        }
    }
}

async fn run(
    inner: Arc<Inner>,
    actor: Arc<Actor>,
    mut pulls: mpsc::UnboundedReceiver<Pull>,
    request: Request,
    trace: Option<Arc<CallTrace>>,
) {
    // Waiting for the session lock is cancellable too: queued calls must not
    // delay close until the currently enumerated call finishes naturally.
    let mut session = tokio::select! {
        biased;
        _ = actor.stop.cancelled() => return,
        session = inner.session.lock() => session,
    };
    let prepared = inner.factory.prepare(request, trace.clone());
    let prepared = match prepared {
        Ok(prepared) => prepared,
        Err(error) => {
            if let Some(pull) = pulls.recv().await {
                let _ = pull.reply.send(Err(error));
            }
            return;
        }
    };
    let mut call = match session.call_lazy(prepared.prepare_websocket, prepared.http.as_ref()) {
        Ok(call) => call,
        Err(error) => {
            if let Some(pull) = pulls.recv().await {
                let _ = pull.reply.send(Err(Error::Socket(error)));
            }
            return;
        }
    };
    if let Some(trace) = trace {
        call = call.with_websocket_observer(Arc::new(WebSocketTraceObserver {
            trace,
            original_error: prepared.http_error.clone(),
        }));
    }
    loop {
        let pull = tokio::select! {
            biased;
            _ = actor.stop.cancelled() => break,
            pull = pulls.recv() => match pull { Some(pull) => pull, None => break },
        };
        let result = call
            .next(&actor.stop)
            .await
            .map(|event| event.map(Event::from))
            .map_err(|error| {
                let original = prepared.http_error.lock().expect("HTTP error slot").take();
                if actor.stop.is_cancelled()
                    && original.is_none()
                    && error == WebSocketError::Cancelled
                {
                    Error::Cancelled
                } else {
                    restore_original_error(error, original)
                }
            });
        let terminal = !matches!(&result, Ok(Some(_)));
        let _ = pull.reply.send(result);
        if terminal {
            break;
        }
    }
    // Dropping SessionCall performs T29's exact incomplete-exchange poisoning
    // while the exclusive session lock is still held.
}

fn restore_original_error(error: WebSocketError, original: Option<Error>) -> Error {
    let Some(original) = original else {
        return Error::Socket(error);
    };
    if let WebSocketError::TransportFailure { message, .. } = error {
        let preserved = matches!(
            &original,
            Error::Cancelled
                | Error::Cancellation(_)
                | Error::Provider(_)
                | Error::ProviderWithCause { .. }
                | Error::WebSocketTransport { .. }
                | Error::ChatProvider(_)
                | Error::Response(crate::responses::ResponsesError::ProviderResponse { .. })
                | Error::Http(
                    crate::http_stream::HttpError::Cancelled
                        | crate::http_stream::HttpError::RequestCancelled(_)
                        | crate::http_stream::HttpError::Provider(_)
                        | crate::http_stream::HttpError::RequestTooLarge(_)
                )
                | Error::Socket(
                    WebSocketError::Cancelled
                        | WebSocketError::RequestTooLarge { .. }
                        | WebSocketError::TransportFailure { .. }
                        | WebSocketError::Transport(_)
                        | WebSocketError::Upgrade { .. }
                        | WebSocketError::Response(
                            crate::responses::ResponsesError::ProviderResponse { .. }
                        )
                )
        );
        if !preserved {
            return Error::WebSocketTransport {
                message,
                cause: Box::new(original),
            };
        }
    }
    original
}

struct WebSocketTraceObserver {
    trace: Arc<CallTrace>,
    original_error: Arc<StdMutex<Option<Error>>>,
}
impl responses_websocket::WebSocketObserver for WebSocketTraceObserver {
    fn start(&self) -> Box<dyn responses_websocket::WebSocketAttemptObserver> {
        static NEXT_REQUEST: AtomicU64 = AtomicU64::new(0);
        let request_id = format!(
            "{:016x}{:016x}",
            std::process::id(),
            NEXT_REQUEST.fetch_add(1, Ordering::Relaxed)
        );
        Box::new(WebSocketTraceAttempt {
            request: self.trace.begin_request("websocket", &request_id),
            call: self.trace.clone(),
            original_error: self.original_error.clone(),
        })
    }
}
struct WebSocketTraceAttempt {
    request: RequestTrace,
    call: Arc<CallTrace>,
    original_error: Arc<StdMutex<Option<Error>>>,
}
impl responses_websocket::WebSocketAttemptObserver for WebSocketTraceAttempt {
    fn dump_request(&mut self, body: &[u8]) {
        self.call.dump_prepared_body(body);
    }
    fn record_request_bytes(&mut self, bytes: u64) {
        self.request.record_request_bytes(bytes);
    }
    fn record_response_bytes(&mut self, bytes: u64) {
        self.request.record_response_bytes(bytes);
    }
    fn record_close_status(&mut self, status: u16) {
        self.request.record_close_status(i32::from(status));
    }
    fn finish(&mut self, outcome: responses_websocket::WebSocketAttemptOutcome<'_>) {
        self.request.finish(match outcome {
            responses_websocket::WebSocketAttemptOutcome::Completed => TraceOutcome::Completed,
            responses_websocket::WebSocketAttemptOutcome::Disposed => TraceOutcome::Disposed,
            responses_websocket::WebSocketAttemptOutcome::Failed(error) => {
                let kind = self
                    .original_error
                    .lock()
                    .expect("original error slot")
                    .as_ref()
                    .map_or_else(
                        || super::diagnostic_session::socket_failure_kind(error),
                        super::diagnostic_session::failure_kind,
                    );
                TraceOutcome::Failed(kind)
            }
        });
    }
}
