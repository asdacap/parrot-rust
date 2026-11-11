//! Deterministic test models only: no database, provider HTTP, or OS process.

use std::future::{Future, poll_fn};
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll, Wake, Waker};
use std::time::{Duration, Instant, SystemTime};

use parrot_domain::contracts::async_ops::Cancelled;
pub use parrot_domain::contracts::async_ops::{BoxFuture, Cancellation, Clock, OperationError};
use parrot_domain::contracts::paths::ResolvedRead;
use parrot_domain::contracts::security::{AccessSnapshot, InvocationContext, SecurityProvider};
use parrot_domain::contracts::tools::{ParallelSafety, Tool};

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Fault {
    Denied,
    Closed,
}

struct NoWake;
impl Wake for NoWake {
    fn wake(self: Arc<Self>) {}
}

/// Single deterministic scheduler step. Tests advance actor/clock explicitly.
pub fn step<F: Future + ?Sized>(future: Pin<&mut F>) -> Poll<F::Output> {
    let waker = Waker::from(Arc::new(NoWake));
    future.poll(&mut Context::from_waker(&waker))
}

/// Only for operations expected to be immediately ready in this model.
pub fn ready<F: Future>(future: F) -> F::Output {
    let mut future = std::pin::pin!(future);
    match step(future.as_mut()) {
        Poll::Ready(output) => output,
        Poll::Pending => panic!("operation unexpectedly pending; advance its deterministic owner"),
    }
}

#[derive(Default)]
pub struct Cancel {
    cancelled: AtomicBool,
    waiters: Mutex<Vec<Waker>>,
}
impl Cancel {
    pub fn cancel(&self) {
        self.cancelled.store(true, Ordering::SeqCst);
        for waiter in self.waiters.lock().unwrap().drain(..) {
            waiter.wake();
        }
    }
}
impl Cancellation for Cancel {
    fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::SeqCst)
    }
    fn cancelled(&self) -> BoxFuture<'_, ()> {
        Box::pin(poll_fn(move |cx| {
            let mut waiters = self.waiters.lock().unwrap();
            if self.is_cancelled() {
                Poll::Ready(())
            } else {
                waiters.push(cx.waker().clone());
                Poll::Pending
            }
        }))
    }
}

#[derive(Clone)]
pub struct FakeClock {
    start: Instant,
    utc_start: SystemTime,
    elapsed: Arc<Mutex<Duration>>,
}
impl Default for FakeClock {
    fn default() -> Self {
        Self {
            start: Instant::now(),
            utc_start: SystemTime::UNIX_EPOCH,
            elapsed: Arc::new(Mutex::new(Duration::ZERO)),
        }
    }
}
impl FakeClock {
    pub fn advance(&self, duration: Duration) {
        *self.elapsed.lock().unwrap() += duration;
    }
}
impl Clock for FakeClock {
    fn now(&self) -> Instant {
        self.start + *self.elapsed.lock().unwrap()
    }
    fn utc_now(&self) -> SystemTime {
        self.utc_start + *self.elapsed.lock().unwrap()
    }
    fn sleep_until<'a>(
        &'a self,
        deadline: Instant,
        cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<(), Cancelled>> {
        Box::pin(poll_fn(move |_| {
            if cancellation.is_cancelled() {
                Poll::Ready(Err(Cancelled))
            } else if self.now() >= deadline {
                Poll::Ready(Ok(()))
            } else {
                Poll::Pending
            }
        }))
    }
}

/// Fully owned immutable security snapshot. No sharing of mutable policy state.
#[derive(Clone)]
pub struct Snapshot(pub Vec<PathBuf>);
impl AccessSnapshot for Snapshot {
    type Error = Fault;
    fn check_read(&self, read: &ResolvedRead) -> Result<(), Fault> {
        self.check_write(read.actual_path())
    }
    fn check_write(&self, target: &Path) -> Result<(), Fault> {
        if self.0.iter().any(|allowed| target.starts_with(allowed)) {
            Ok(())
        } else {
            Err(Fault::Denied)
        }
    }
}
#[derive(Default)]
pub struct Policy(pub Mutex<Vec<PathBuf>>);
impl SecurityProvider for Policy {
    type Snapshot = Snapshot;
    fn capture(&self) -> Snapshot {
        Snapshot(self.0.lock().unwrap().clone())
    }
}

/// Abstract delivery distinction for exact-payload conflict checks; not a wire DTO.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Delivery {
    Foreground,
    Background,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Event {
    pub key: u64,
    pub turn: u64,
    pub payload: Vec<u8>,
    pub delivery: Delivery,
    pub terminal: bool,
}
#[derive(Clone)]
pub enum Invocation {
    Read(ResolvedRead),
    Write(PathBuf),
}
pub struct FakeTool {
    pub definition: String,
    pub invocations: Mutex<Vec<(u64, PathBuf)>>,
}
impl Default for FakeTool {
    fn default() -> Self {
        Self {
            definition: "checked-file-model".into(),
            invocations: Mutex::new(Vec::new()),
        }
    }
}
impl Tool for FakeTool {
    type Definition = String;
    type Invocation = Invocation;
    type Selection = u64;
    type Security = Snapshot;
    type Output = PathBuf;
    type Error = Fault;
    fn definition(&self) -> &String {
        &self.definition
    }
    fn parallel_safety(&self, invocation: &Invocation) -> ParallelSafety {
        match invocation {
            Invocation::Read(_) => ParallelSafety::ParallelSafe,
            Invocation::Write(_) => ParallelSafety::Sequential,
        }
    }
    fn enabled_after_interruption(&self) -> bool {
        false
    }
    fn invoke<'a>(
        &'a self,
        invocation: Invocation,
        context: InvocationContext<'a, Snapshot, u64>,
    ) -> BoxFuture<'a, Result<PathBuf, OperationError<Fault>>> {
        Box::pin(async move {
            context
                .cancellation()
                .check()
                .map_err(|_| OperationError::Cancelled)?;
            let path = match invocation {
                Invocation::Read(read) => {
                    context
                        .security()
                        .check_read(&read)
                        .map_err(OperationError::Failed)?;
                    read.actual_path().to_path_buf()
                }
                Invocation::Write(path) => {
                    context
                        .security()
                        .check_write(&path)
                        .map_err(OperationError::Failed)?;
                    path
                }
            };
            self.invocations
                .lock()
                .unwrap()
                .push((*context.selection(), path.clone()));
            Ok(path)
        })
    }
}

/// Direct typed composition replaces dynamic registry lookup. Non-optional fields
/// are actual contract implementors, not unrelated marker values.
pub struct Bindings<T: Tool, C: Clock> {
    pub tool: T,
    pub clock: C,
}

pub fn components() -> (FakeTool, FakeClock) {
    (FakeTool::default(), FakeClock::default())
}
