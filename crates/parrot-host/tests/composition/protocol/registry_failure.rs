//! Public-registry lifecycle regressions. The construction owner retains its
//! runtime until actual registry close publication; observer runtimes are not
//! ownership roots. No dropped task implies supplier settlement.
use parrot_domain::contracts::async_ops::{BoxFuture, Cancellation};
use parrot_host::composition::protocol::{
    HostedSession, Result, RpcCode, RpcError, SessionHost, SessionListener, UserSessionRegistry,
};
use parrot_storage::events::NeverCancelled;
use std::{
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};
use tokio::{
    runtime::{Builder, Runtime},
    sync::oneshot,
};

struct UnusedHost;
impl SessionHost for UnusedHost {
    fn host(
        &self,
        _: Arc<dyn HostedSession>,
        _: Arc<dyn Cancellation>,
    ) -> BoxFuture<'_, Result<Arc<dyn SessionListener>>> {
        panic!("the deliberately refused opening must never reach the listener")
    }
}
struct Dropped(Arc<AtomicBool>);
impl Drop for Dropped {
    fn drop(&mut self) {
        self.0.store(true, Ordering::SeqCst);
    }
}
fn runtime() -> Runtime {
    Builder::new_current_thread().enable_all().build().unwrap()
}

// Test harness ownership only: production construction and shutdown are T63.
struct OwnerRuntime {
    handle: tokio::runtime::Handle,
    stop: Option<oneshot::Sender<()>>,
    thread: Option<std::thread::JoinHandle<()>>,
}
impl OwnerRuntime {
    fn new() -> Self {
        let (ready_send, ready) = std::sync::mpsc::sync_channel(1);
        let (stop, stopped) = oneshot::channel();
        let thread = std::thread::spawn(move || {
            let runtime = runtime();
            ready_send.send(runtime.handle().clone()).unwrap();
            runtime.block_on(async {
                let _ = stopped.await;
            });
        });
        Self {
            handle: ready.recv().unwrap(),
            stop: Some(stop),
            thread: Some(thread),
        }
    }
}
impl Drop for OwnerRuntime {
    fn drop(&mut self) {
        let _ = self.stop.take().unwrap().send(());
        self.thread.take().unwrap().join().unwrap();
    }
}

#[test]
fn registry_open_owner_survives_caller_runtime_loss_and_does_not_strand_close() {
    let owner = OwnerRuntime::new();
    let registry = UserSessionRegistry::new(owner.handle.clone());
    let caller = runtime();
    let (entered_send, entered) = oneshot::channel();
    let (release, released) = oneshot::channel();
    let dropped = Arc::new(AtomicBool::new(false));
    let owner_drop = dropped.clone();
    let opening_registry = registry.clone();
    let observer = caller.spawn(async move {
        opening_registry
            .host(
                Box::pin(async move {
                    let _dropped = Dropped(owner_drop);
                    entered_send.send(()).unwrap();
                    released.await.unwrap();
                    Err(RpcError::new(
                        RpcCode::Unavailable,
                        "opening refused after release",
                    ))
                }),
                Arc::new(UnusedHost),
                Arc::new(NeverCancelled),
            )
            .await
    });
    caller.block_on(entered).unwrap();
    drop(observer);
    drop(caller);
    let abandoned_before_release = dropped.load(Ordering::SeqCst);
    let release_delivered = release.send(()).is_ok();
    let surviving_caller = runtime();
    let closing = surviving_caller.block_on(async {
        tokio::time::timeout(Duration::from_millis(200), registry.close()).await
    });
    drop(surviving_caller);
    eprintln!(
        "P1 runtime-loss: abandoned_before_release={abandoned_before_release}; release_delivered={release_delivered}; close_completed={}",
        closing.is_ok()
    );
    assert!(
        !abandoned_before_release,
        "registry owner was dropped with its caller runtime"
    );
    assert!(
        release_delivered,
        "the admitted opening lost its real receiver"
    );
    assert!(
        closing.is_ok(),
        "a later live caller cannot join registry shutdown"
    );
}

#[test]
fn registry_open_unwind_is_fenced_without_stranding_shutdown() {
    let owner = OwnerRuntime::new();
    let registry = UserSessionRegistry::new(owner.handle.clone());
    let caller = runtime();
    let error = caller
        .block_on(registry.host(
            Box::pin(async { panic!("intentional injected opening unwind before any resources") }),
            Arc::new(UnusedHost),
            Arc::new(NeverCancelled),
        ))
        .err()
        .expect("the failed opening cannot be admitted");
    assert_eq!(error.code, RpcCode::Internal);
    let closing = caller.block_on(async {
        tokio::time::timeout(Duration::from_millis(200), registry.close()).await
    });
    drop(caller);
    eprintln!(
        "P1 unwind: opening_code={:?}; close_completed={}",
        error.code,
        closing.is_ok()
    );
    let closed = closing
        .expect("failed owner must produce a terminal fenced result, not an unjoinable count")
        .unwrap_err();
    assert_eq!(closed, error);
    let observer = runtime();
    assert_eq!(observer.block_on(registry.close()).unwrap_err(), error);
    assert_eq!(registry.find("absent").err().unwrap(), error);
}

#[test]
fn lost_owner_runtime_is_unknown_and_repeated_close_is_terminal() {
    let owner = OwnerRuntime::new();
    let registry = UserSessionRegistry::new(owner.handle.clone());
    let caller = runtime();
    let (entered_send, entered) = oneshot::channel();
    let (release, released) = oneshot::channel::<()>();
    let observing_registry = registry.clone();
    let observing = caller.spawn(async move {
        observing_registry
            .host(
                Box::pin(async move {
                    entered_send.send(()).unwrap();
                    released.await.unwrap();
                    Err(RpcError::new(RpcCode::Unavailable, "not admitted"))
                }),
                Arc::new(UnusedHost),
                Arc::new(NeverCancelled),
            )
            .await
    });
    caller.block_on(entered).unwrap();
    // Explicit failure simulation: violating the construction-owner obligation
    // must fence UNKNOWN, never claim successful supplier settlement.
    drop(owner);
    assert!(release.send(()).is_err());
    let error = caller.block_on(observing).unwrap().err().unwrap();
    assert_eq!(error.code, RpcCode::Internal);
    for _ in 0..2 {
        let closed = caller.block_on(async {
            tokio::time::timeout(Duration::from_secs(2), registry.close())
                .await
                .unwrap()
                .unwrap_err()
        });
        assert_eq!(closed, error);
    }
}

struct PanicHost;
impl SessionHost for PanicHost {
    fn host(
        &self,
        _: Arc<dyn HostedSession>,
        _: Arc<dyn Cancellation>,
    ) -> BoxFuture<'_, Result<Arc<dyn SessionListener>>> {
        Box::pin(async { panic!("injected listener construction unwind after session return") })
    }
}

#[tokio::test]
async fn opening_unwind_retains_returned_session_without_claiming_cleanup() {
    let fixture = super::service::support::Fixture::new();
    let session = fixture.unhosted().await;
    let before = Arc::strong_count(&session);
    let registry = UserSessionRegistry::new(tokio::runtime::Handle::current());
    let opening = session.clone();
    let error = registry
        .host(
            Box::pin(async move { Ok(opening as Arc<dyn HostedSession>) }),
            Arc::new(PanicHost),
            Arc::new(NeverCancelled),
        )
        .await
        .err()
        .unwrap();
    assert_eq!(error.code, RpcCode::Internal);
    assert_eq!(registry.close().await.unwrap_err(), error);
    assert_eq!(registry.close().await.unwrap_err(), error);
    assert!(!session.closed.load(Ordering::SeqCst));
    assert_eq!(Arc::strong_count(&session), before + 1);
    // Test teardown performs actual supplier cleanup explicitly; registry loss
    // did not close the returned capability or infer lease settlement.
    session.close().await.unwrap();
    fixture.close().await;
}

struct PendingListener {
    entered: std::sync::Mutex<Option<oneshot::Sender<()>>>,
    released: std::sync::Mutex<Option<oneshot::Receiver<()>>>,
    dropped: Arc<AtomicBool>,
}
impl SessionListener for PendingListener {
    fn close(&self) -> BoxFuture<'_, Result<()>> {
        let entered = self
            .entered
            .lock()
            .unwrap()
            .take()
            .expect("one cleanup attempt");
        let released = self.released.lock().unwrap().take().unwrap();
        Box::pin(async move {
            let _dropped = Dropped(self.dropped.clone());
            entered.send(()).unwrap();
            released.await.unwrap();
            Ok(())
        })
    }
}
struct FixedHost(Arc<dyn SessionListener>);
impl SessionHost for FixedHost {
    fn host(
        &self,
        _: Arc<dyn HostedSession>,
        _: Arc<dyn Cancellation>,
    ) -> BoxFuture<'_, Result<Arc<dyn SessionListener>>> {
        Box::pin(async { Ok(self.0.clone()) })
    }
}

#[test]
fn owner_runtime_loss_during_actual_partial_cleanup_is_joined_and_fenced() {
    let caller = runtime();
    let (fixture, session) = caller.block_on(async {
        let fixture = super::service::support::Fixture::new();
        let session = fixture.unhosted().await;
        (fixture, session)
    });
    let owner = OwnerRuntime::new();
    let registry = UserSessionRegistry::new(owner.handle.clone());
    let (entered_send, entered) = oneshot::channel();
    let (release, released) = oneshot::channel();
    let dropped = Arc::new(AtomicBool::new(false));
    let listener = Arc::new(PendingListener {
        entered: std::sync::Mutex::new(Some(entered_send)),
        released: std::sync::Mutex::new(Some(released)),
        dropped: dropped.clone(),
    });
    let retained = Arc::downgrade(&listener);
    let opening = session.clone();
    caller
        .block_on(registry.host(
            Box::pin(async move { Ok(opening as Arc<dyn HostedSession>) }),
            Arc::new(FixedHost(listener)),
            Arc::new(NeverCancelled),
        ))
        .unwrap();
    let closing_registry = registry.clone();
    let closing = caller.spawn(async move { closing_registry.close().await });
    caller.block_on(entered).unwrap();
    assert!(!dropped.load(Ordering::SeqCst));
    drop(owner);
    assert!(release.send(()).is_err());
    let error = caller.block_on(async {
        tokio::time::timeout(Duration::from_secs(2), closing)
            .await
            .unwrap()
            .unwrap()
            .unwrap_err()
    });
    assert_eq!(error.code, RpcCode::Internal);
    assert!(dropped.load(Ordering::SeqCst));
    assert!(retained.upgrade().is_some());
    assert!(!session.closed.load(Ordering::SeqCst));
    assert_eq!(caller.block_on(registry.close()).unwrap_err(), error);
    caller.block_on(async {
        session.close().await.unwrap();
        fixture.close().await;
    });
}

struct PanicListener {
    calls: Arc<std::sync::atomic::AtomicUsize>,
}
impl SessionListener for PanicListener {
    fn close(&self) -> BoxFuture<'_, Result<()>> {
        Box::pin(async move {
            self.calls.fetch_add(1, Ordering::SeqCst);
            panic!("injected actual listener cleanup unwind");
        })
    }
}
struct PanicCleanupHost {
    calls: Arc<std::sync::atomic::AtomicUsize>,
    listener: Arc<std::sync::Mutex<Option<std::sync::Weak<PanicListener>>>>,
}
impl SessionHost for PanicCleanupHost {
    fn host(
        &self,
        _: Arc<dyn HostedSession>,
        _: Arc<dyn Cancellation>,
    ) -> BoxFuture<'_, Result<Arc<dyn SessionListener>>> {
        Box::pin(async move {
            let listener = Arc::new(PanicListener {
                calls: self.calls.clone(),
            });
            *self.listener.lock().unwrap() = Some(Arc::downgrade(&listener));
            Ok(listener as Arc<dyn SessionListener>)
        })
    }
}

#[tokio::test]
async fn cleanup_unwind_retains_actual_capabilities_and_never_retries_unknown_work() {
    let fixture = super::service::support::Fixture::new();
    let session = fixture.unhosted().await;
    let registry = UserSessionRegistry::new(tokio::runtime::Handle::current());
    let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let listener = Arc::new(std::sync::Mutex::new(None));
    let opening = session.clone();
    registry
        .host(
            Box::pin(async move { Ok(opening as Arc<dyn HostedSession>) }),
            Arc::new(PanicCleanupHost {
                calls: calls.clone(),
                listener: listener.clone(),
            }),
            Arc::new(NeverCancelled),
        )
        .await
        .unwrap();
    let error = tokio::time::timeout(Duration::from_secs(2), registry.close())
        .await
        .unwrap()
        .unwrap_err();
    assert_eq!(error.code, RpcCode::Internal);
    assert_eq!(registry.close().await.unwrap_err(), error);
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    assert!(
        listener
            .lock()
            .unwrap()
            .as_ref()
            .unwrap()
            .upgrade()
            .is_some()
    );
    assert!(!session.closed.load(Ordering::SeqCst));
    session.close().await.unwrap();
    fixture.close().await;
}
