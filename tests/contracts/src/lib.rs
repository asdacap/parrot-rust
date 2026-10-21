//! Deterministic test models only: no database, provider HTTP, or OS process.

use std::collections::{BTreeMap, VecDeque};
use std::future::{Future, poll_fn};
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll, Wake, Waker};
use std::time::{Duration, Instant, SystemTime};

pub use parrot_domain::contracts::async_ops::{BoxFuture, Cancellation, Clock, OperationError};
use parrot_domain::contracts::async_ops::{Cancelled, OperationResult};
use parrot_domain::contracts::paths::{ResolvedRead, RootOrigin, SelectedSessionRoot};
use parrot_domain::contracts::providers::{ProviderSendResult, ProviderSession, ProviderStream};
use parrot_domain::contracts::security::{AccessSnapshot, InvocationContext, SecurityProvider};
use parrot_domain::contracts::storage::{
    AdmissionError, CommitReceipt, CommitTicket, Committed, SessionTransactions,
    TransactionCommand, TransactionTicket,
};
use parrot_domain::contracts::tools::{ParallelSafety, Tool};

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Fault {
    Unconfigured,
    Conflict,
    Rollback,
    TurnClosed,
    Denied,
    Closed,
    IdleTimeout,
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
pub struct Command {
    pub key: u64,
    pub turn: u64,
    pub payload: Vec<u8>,
    pub delivery: Delivery,
    pub usage: u64,
    pub terminal: bool,
    pub fail_projection: bool,
}
impl TransactionCommand for Command {
    type Key = u64;
    type Output = Event;
    fn key(&self) -> &u64 {
        &self.key
    }
}
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Event {
    pub key: u64,
    pub turn: u64,
    pub payload: Vec<u8>,
    pub delivery: Delivery,
    pub terminal: bool,
}
#[derive(Clone, Default)]
pub struct Durable {
    pub events: Vec<Event>,
    pub projection: BTreeMap<u64, u64>,
    pub closed: BTreeMap<u64, bool>,
    commands: BTreeMap<u64, Command>,
    receipts: BTreeMap<u64, Committed<CommitReceipt<u64, Event>>>,
}
type Completion = Result<Committed<CommitReceipt<u64, Event>>, Fault>;
#[derive(Default)]
struct ActorState {
    durable: Durable,
    queue: VecDeque<Command>,
    admitted: BTreeMap<u64, Command>,
    completions: BTreeMap<u64, Completion>,
}
/// Explicit scheduler-owned actor. Neither admission nor ticket polling executes
/// commands. drive_one is the sole application point, independent of observers.
pub struct Store {
    root: SelectedSessionRoot,
    actor: Arc<Mutex<ActorState>>,
}
impl Store {
    pub fn new(root: SelectedSessionRoot) -> Self {
        Self::restart(root, Durable::default())
    }
    pub fn restart(root: SelectedSessionRoot, durable: Durable) -> Self {
        Self {
            root,
            actor: Arc::new(Mutex::new(ActorState {
                durable,
                ..ActorState::default()
            })),
        }
    }
    pub fn durable(&self) -> Durable {
        self.actor.lock().unwrap().durable.clone()
    }
    pub fn pending(&self) -> usize {
        self.actor.lock().unwrap().queue.len()
    }
    pub fn drive_one(&self) -> Result<(), Fault> {
        let mut state = self.actor.lock().unwrap();
        let command = state.queue.pop_front().ok_or(Fault::Unconfigured)?;
        // Stage the whole transaction, then replace atomically. Injected failure
        // occurs after staging the event, before applying its projection.
        let mut staged = state.durable.clone();
        let result = if staged.closed.get(&command.turn) == Some(&true) {
            Err(Fault::TurnClosed)
        } else {
            let event = Event {
                key: command.key,
                turn: command.turn,
                payload: command.payload.clone(),
                delivery: command.delivery,
                terminal: command.terminal,
            };
            staged.events.push(event.clone());
            if command.fail_projection {
                Err(Fault::Rollback)
            } else {
                staged.projection.insert(command.turn, command.usage);
                if command.terminal {
                    staged.closed.insert(command.turn, true);
                }
                let receipt = Committed::after_commit(CommitReceipt::new(command.key, event));
                staged.commands.insert(command.key, command.clone());
                staged.receipts.insert(command.key, receipt.clone());
                state.durable = staged;
                Ok(receipt)
            }
        };
        state.completions.insert(command.key, result.clone());
        result.map(|_| ())
    }
}
impl SessionTransactions for Store {
    type Command = Command;
    type Error = Fault;
    fn selected_root(&self) -> &SelectedSessionRoot {
        &self.root
    }
    fn admit<'a>(
        &'a self,
        command: Command,
        cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<TransactionTicket<Self>, AdmissionError<Fault>>> {
        Box::pin(async move {
            cancellation
                .check()
                .map_err(|_| AdmissionError::Cancelled)?;
            let key = command.key;
            {
                let mut state = self.actor.lock().unwrap();
                let prior = state
                    .admitted
                    .get(&key)
                    .or_else(|| state.durable.commands.get(&key));
                match prior {
                    Some(prior) if prior != &command => {
                        return Err(AdmissionError::Rejected(Fault::Conflict));
                    }
                    Some(_) => {}
                    None => {
                        state.admitted.insert(key, command.clone());
                        state.queue.push_back(command);
                    }
                }
            }
            let actor = self.actor.clone();
            Ok(CommitTicket::new(
                key,
                Box::pin(poll_fn(move |_| {
                    let state = actor.lock().unwrap();
                    if let Some(result) = state.completions.get(&key) {
                        Poll::Ready(result.clone())
                    } else if let Some(receipt) = state.durable.receipts.get(&key) {
                        Poll::Ready(Ok(receipt.clone()))
                    } else {
                        Poll::Pending
                    }
                })),
            ))
        })
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Content {
    pub known: String,
    pub unknown: Vec<u8>,
}
type ConfiguredRoute = (Content, Vec<(Duration, Content)>);

/// Inputs must exactly match a configured complete request, including opaque data.
pub struct Provider {
    clock: FakeClock,
    idle: Duration,
    routes: Mutex<Vec<ConfiguredRoute>>,
    closed: AtomicBool,
}
impl Provider {
    pub fn new(clock: FakeClock, idle: Duration) -> Self {
        Self {
            clock,
            idle,
            routes: Mutex::new(Vec::new()),
            closed: AtomicBool::new(false),
        }
    }
    pub fn configure(&self, request: Content, chunks: Vec<(Duration, Content)>) {
        self.routes.lock().unwrap().push((request, chunks));
    }
}
struct Stream {
    clock: FakeClock,
    idle: Duration,
    deadline: Mutex<Instant>,
    chunks: Mutex<VecDeque<(Instant, Content)>>,
}
impl ProviderStream for Stream {
    type Event = Content;
    type Error = Fault;
    fn next<'a>(
        &'a self,
        cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, OperationResult<Option<Content>, Fault>> {
        Box::pin(poll_fn(move |_| {
            if cancellation.is_cancelled() {
                return Poll::Ready(Err(OperationError::Cancelled));
            }
            let now = self.clock.now();
            let mut deadline = self.deadline.lock().unwrap();
            let mut chunks = self.chunks.lock().unwrap();
            // Consume modelled wire activity at its scheduled time, not when a
            // delayed observer happens to poll. Expiry before that read wins.
            match chunks.front() {
                Some((at, _)) if *at <= now && *at < *deadline => {
                    let (at, content) = chunks.pop_front().unwrap();
                    *deadline = at + self.idle;
                    Poll::Ready(Ok(Some(content)))
                }
                None => Poll::Ready(Ok(None)),
                _ if now >= *deadline => {
                    Poll::Ready(Err(OperationError::Failed(Fault::IdleTimeout)))
                }
                _ => Poll::Pending,
            }
        }))
    }
}
impl ProviderSession for Provider {
    type Request = Content;
    type Event = Content;
    type Error = Fault;
    fn send<'a>(
        &'a self,
        request: Content,
        cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, ProviderSendResult<Content, Fault>> {
        Box::pin(async move {
            cancellation
                .check()
                .map_err(|_| OperationError::Cancelled)?;
            if self.closed.load(Ordering::SeqCst) {
                return Err(OperationError::Failed(Fault::Closed));
            }
            let chunks = self
                .routes
                .lock()
                .unwrap()
                .iter()
                .find(|(configured, _)| configured == &request)
                .map(|(_, chunks)| chunks.clone())
                .ok_or(OperationError::Failed(Fault::Unconfigured))?;
            let start = self.clock.now();
            let stream = Stream {
                clock: self.clock.clone(),
                idle: self.idle,
                deadline: Mutex::new(start + self.idle),
                chunks: Mutex::new(
                    chunks
                        .into_iter()
                        .map(|(delay, content)| (start + delay, content))
                        .collect(),
                ),
            };
            Ok(Box::new(stream) as _)
        })
    }
    fn close(&self) -> BoxFuture<'_, Result<(), Fault>> {
        Box::pin(async move {
            self.closed.store(true, Ordering::SeqCst);
            Ok(())
        })
    }
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
pub struct Bindings<S: SessionTransactions, P: ProviderSession, T: Tool, C: Clock> {
    pub storage: S,
    pub provider: P,
    pub tool: T,
    pub clock: C,
}
impl<S: SessionTransactions, P: ProviderSession, T: Tool, C: Clock> Bindings<S, P, T, C> {
    /// A generic borrowed consumer receives exactly the supplied service.
    pub fn storage(&self) -> &S {
        &self.storage
    }
}

pub fn components() -> (Store, Provider, FakeTool, FakeClock) {
    let clock = FakeClock::default();
    (
        Store::new(
            SelectedSessionRoot::new(PathBuf::from("/synthetic/session"), RootOrigin::Current)
                .unwrap(),
        ),
        Provider::new(clock.clone(), Duration::from_secs(10)),
        FakeTool::default(),
        clock,
    )
}
