use super::{HostedSession, Result, RpcCode, RpcError, SessionHost, SessionListener};
use parrot_domain::contracts::async_ops::{BoxFuture, Cancellation};
use std::{
    collections::HashMap,
    sync::{Arc, Mutex, OnceLock},
};
use tokio::{
    runtime::Handle,
    sync::{Mutex as AsyncMutex, oneshot, watch},
    task::JoinHandle,
};

type TaskJoin = AsyncMutex<Option<JoinHandle<()>>>;
type Failure = Arc<OnceLock<RpcError>>;

#[derive(Clone)]
struct Hosted {
    session: Arc<dyn HostedSession>,
    listener: Arc<dyn SessionListener>,
}
impl Hosted {
    async fn close(self) -> Result<()> {
        let listener = self.listener.close().await;
        let session = self.session.close().await;
        listener.and(session)
    }
}
struct PartialHost {
    _session: Arc<dyn HostedSession>,
    listener: Option<Arc<dyn SessionListener>>,
}
struct Opening {
    task: TaskJoin,
    // Unknown opening/host unwinds retain every capability actually returned.
    // They are not converted into successful supplier settlement or retried.
    partial: Mutex<Option<PartialHost>>,
}
struct Cleanup {
    hosted: Hosted,
    task: TaskJoin,
}
struct Shutdown {
    task: TaskJoin,
    openings: Vec<Arc<Opening>>,
    cleanups: Vec<Arc<Cleanup>>,
    work: watch::Sender<Option<Result<()>>>,
    result: OnceLock<Result<()>>,
}
struct State {
    accepting: bool,
    openings: Vec<Arc<Opening>>,
    sessions: HashMap<String, Hosted>,
    shutdown: Option<Arc<Shutdown>>,
}

/// User sessions and listeners are admitted atomically after hosting succeeds.
/// The construction owner must keep `runtime` alive through `close`'s actual
/// owner join, not merely until an RPC observer or work notification completes.
/// A Handle alone does not keep a Runtime alive; premature owner loss is fenced.
pub struct UserSessionRegistry {
    state: Mutex<State>,
    runtime: Handle,
    failure: Failure,
}
impl UserSessionRegistry {
    pub fn new(runtime: Handle) -> Arc<Self> {
        Arc::new(Self {
            state: Mutex::new(State {
                accepting: true,
                openings: Vec::new(),
                sessions: HashMap::new(),
                shutdown: None,
            }),
            runtime,
            failure: Arc::new(OnceLock::new()),
        })
    }
    pub fn find(&self, id: &str) -> Result<Arc<dyn HostedSession>> {
        let state = self.state.lock().expect("session registry poisoned");
        if let Some(error) = self.failure.get() {
            return Err(error.clone());
        }
        if !state.accepting {
            return Err(shutting_down());
        }
        state
            .sessions
            .get(id)
            .map(|entry| entry.session.clone())
            .ok_or_else(|| RpcError::new(RpcCode::NotFound, format!("no user session {id}")))
    }
    pub fn contains(&self, id: &str) -> bool {
        let state = self.state.lock().expect("session registry poisoned");
        self.failure.get().is_none() && state.accepting && state.sessions.contains_key(id)
    }
    pub async fn host(
        self: &Arc<Self>,
        open: BoxFuture<'static, Result<Arc<dyn HostedSession>>>,
        host: Arc<dyn SessionHost>,
        cancellation: Arc<dyn Cancellation>,
    ) -> Result<Arc<dyn HostedSession>> {
        let (send, receive) = oneshot::channel();
        {
            let mut state = self.state.lock().expect("session registry poisoned");
            if let Some(error) = self.failure.get() {
                return Err(error.clone());
            }
            if !state.accepting {
                return Err(shutting_down());
            }
            let opening = Arc::new(Opening {
                task: AsyncMutex::new(None),
                partial: Mutex::new(None),
            });
            state.openings.push(opening.clone());
            let registry = self.clone();
            let owned = opening.clone();
            // Construct the guard before spawning: even a never-polled task on
            // an already stopped owner runtime must publish its failure fence.
            let owner = OpeningOwner {
                failure: self.failure.clone(),
                send: Some(send),
                finished: false,
            };
            let task = self.runtime.spawn(async move {
                let mut owner = owner;
                let result = registry.finish_host(open, host, cancellation, owned).await;
                owner.finish(result);
            });
            // The admission gate publishes the actual handle before any close
            // can capture the admitted operation set. No gate crosses an await.
            *opening.task.try_lock().expect("unpublished opening join") = Some(task);
        }
        receive.await.map_err(|_| {
            self.failure
                .get()
                .cloned()
                .unwrap_or_else(|| RpcError::new(RpcCode::Internal, "session opening task failed"))
        })?
    }
    async fn finish_host(
        &self,
        open: BoxFuture<'static, Result<Arc<dyn HostedSession>>>,
        host: Arc<dyn SessionHost>,
        cancellation: Arc<dyn Cancellation>,
        opening: Arc<Opening>,
    ) -> Result<Arc<dyn HostedSession>> {
        let session = open.await?;
        *opening.partial.lock().expect("partial host poisoned") = Some(PartialHost {
            _session: session.clone(),
            listener: None,
        });
        let listener = match host.host(session.clone(), cancellation).await {
            Ok(listener) => listener,
            Err(error) => {
                let cancelled = error.code == RpcCode::Cancelled;
                let mut event = parrot_storage::diagnostics::DiagnosticEvent::new(
                    "transport",
                    "host_failure",
                    if cancelled {
                        parrot_storage::diagnostics::DiagnosticSeverity::Information
                    } else {
                        parrot_storage::diagnostics::DiagnosticSeverity::Error
                    },
                );
                event.error_code = Some(if cancelled { "cancelled" } else { "unexpected" }.into());
                session.write_diagnostic(&event);
                if let Err(cleanup) = session.close().await {
                    return Err(remember(&self.failure, cleanup));
                }
                opening
                    .partial
                    .lock()
                    .expect("partial host poisoned")
                    .take();
                return Err(error);
            }
        };
        opening
            .partial
            .lock()
            .expect("partial host poisoned")
            .as_mut()
            .expect("returned session retained")
            .listener = Some(listener.clone());
        let created = Hosted { session, listener };
        // Do not invoke a supplier callback while holding the admission gate.
        let id = created.session.snapshot().id;
        let refusal = {
            let mut state = self.state.lock().expect("session registry poisoned");
            if let Some(error) = self.failure.get() {
                Some(error.clone())
            } else if !state.accepting {
                Some(shutting_down())
            } else if let std::collections::hash_map::Entry::Vacant(entry) =
                state.sessions.entry(id)
            {
                entry.insert(created.clone());
                None
            } else {
                Some(RpcError::new(
                    RpcCode::AlreadyExists,
                    "the user session is already hosted",
                ))
            }
        };
        if let Some(refusal) = refusal {
            if let Err(error) = created.close().await {
                return Err(remember(&self.failure, error));
            }
            opening
                .partial
                .lock()
                .expect("partial host poisoned")
                .take();
            return Err(refusal);
        }
        opening
            .partial
            .lock()
            .expect("partial host poisoned")
            .take();
        Ok(created.session)
    }
    pub async fn close(self: &Arc<Self>) -> Result<()> {
        let shutdown = {
            let mut state = self.state.lock().expect("session registry poisoned");
            if let Some(shutdown) = &state.shutdown {
                shutdown.clone()
            } else {
                state.accepting = false;
                let cleanups = state
                    .sessions
                    .drain()
                    .map(|(_, hosted)| {
                        Arc::new(Cleanup {
                            hosted,
                            task: AsyncMutex::new(None),
                        })
                    })
                    .collect();
                let (work, _) = watch::channel(None);
                let shutdown = Arc::new(Shutdown {
                    task: AsyncMutex::new(None),
                    openings: state.openings.clone(),
                    cleanups,
                    work,
                    result: OnceLock::new(),
                });
                state.shutdown = Some(shutdown.clone());
                let owner = CloseOwner {
                    failure: self.failure.clone(),
                    shutdown: shutdown.clone(),
                    finished: false,
                };
                let registry = self.clone();
                let owned = shutdown.clone();
                let task = self.runtime.spawn(async move {
                    let mut owner = owner;
                    let result = registry.finish_close(owned).await;
                    owner.finish(result);
                });
                *shutdown.task.try_lock().expect("unpublished close join") = Some(task);
                shutdown
            }
        };
        if let Some(result) = shutdown.result.get() {
            return result.clone();
        }
        let mut work = shutdown.work.subscribe();
        let result = loop {
            if let Some(result) = work.borrow_and_update().clone() {
                break result;
            }
            work.changed().await.map_err(|_| {
                remember(&self.failure, unknown("registry close notification failed"))
            })?;
        };
        // Publication above is not the owner join. A cancelled observer leaves
        // the handle in its slot, so a later close still joins the actual owner.
        if let Err(error) = join(&shutdown.task).await {
            remember(&self.failure, error);
        }
        // A lost close owner may not have reached its child-join loops. Join the
        // retained admitted work before publishing even a terminal UNKNOWN.
        for opening in &shutdown.openings {
            if let Err(error) = join(&opening.task).await {
                remember(&self.failure, error);
            }
        }
        for cleanup in &shutdown.cleanups {
            if let Err(error) = join(&cleanup.task).await {
                remember(&self.failure, error);
            }
        }
        let result = self.failure.get().cloned().map_or(result, Err);
        shutdown.result.get_or_init(|| result).clone()
    }
    async fn finish_close(&self, shutdown: Arc<Shutdown>) -> Result<()> {
        for cleanup in &shutdown.cleanups {
            let owned = cleanup.clone();
            let fence = CleanupOwner {
                failure: self.failure.clone(),
                finished: false,
            };
            let task = self.runtime.spawn(async move {
                let mut fence = fence;
                if let Err(error) = owned.hosted.clone().close().await {
                    remember(&fence.failure, error);
                }
                fence.finished = true;
            });
            *cleanup.task.try_lock().expect("unpublished cleanup join") = Some(task);
        }
        for opening in &shutdown.openings {
            if let Err(error) = join(&opening.task).await {
                remember(&self.failure, error);
            }
        }
        for cleanup in &shutdown.cleanups {
            if let Err(error) = join(&cleanup.task).await {
                remember(&self.failure, error);
            }
        }
        self.failure.get().cloned().map_or(Ok(()), Err)
    }
}
async fn join(slot: &TaskJoin) -> Result<()> {
    let mut slot = slot.lock().await;
    let result = match slot.as_mut() {
        Some(task) => task
            .await
            .map_err(|error| unknown(format!("registry owner join failed: {error}"))),
        None => Ok(()),
    };
    *slot = None;
    result
}
fn unknown(message: impl Into<String>) -> RpcError {
    RpcError::new(RpcCode::Internal, message)
}
fn remember(failure: &Failure, error: RpcError) -> RpcError {
    failure.get_or_init(|| error).clone()
}
struct OpeningOwner {
    failure: Failure,
    send: Option<oneshot::Sender<Result<Arc<dyn HostedSession>>>>,
    finished: bool,
}
impl OpeningOwner {
    fn finish(&mut self, result: Result<Arc<dyn HostedSession>>) {
        self.finished = true;
        let _ = self.send.take().expect("opening observer").send(result);
    }
}
impl Drop for OpeningOwner {
    fn drop(&mut self) {
        if !self.finished {
            let error = remember(
                &self.failure,
                unknown("session opening owner lost; settlement is unknown"),
            );
            if let Some(send) = self.send.take() {
                let _ = send.send(Err(error));
            }
        }
    }
}
struct CleanupOwner {
    failure: Failure,
    finished: bool,
}
impl Drop for CleanupOwner {
    fn drop(&mut self) {
        if !self.finished {
            remember(
                &self.failure,
                unknown("session cleanup owner lost; settlement is unknown"),
            );
        }
    }
}
struct CloseOwner {
    failure: Failure,
    shutdown: Arc<Shutdown>,
    finished: bool,
}
impl CloseOwner {
    fn finish(&mut self, result: Result<()>) {
        self.finished = true;
        self.shutdown.work.send_replace(Some(result));
    }
}
impl Drop for CloseOwner {
    fn drop(&mut self) {
        if !self.finished {
            let error = remember(
                &self.failure,
                unknown("registry close owner lost; settlement is unknown"),
            );
            self.shutdown.work.send_replace(Some(Err(error)));
        }
    }
}
fn shutting_down() -> RpcError {
    RpcError::new(RpcCode::Unavailable, "the service is shutting down")
}
