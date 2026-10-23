//! Session-owned single-drain orchestration. Persistence and the actual turn live in the backend.

#[path = "execution_recovery.rs"]
mod recovery;

use parrot_domain::protocol::messages::parrot::{Delivery, InputSource};
pub use parrot_domain::values::activity::{
    AgentExecution as Execution, AgentExecutionStatus as ExecutionStatus, DrainState,
};
use parrot_domain::{
    contracts::{
        agent_state::AgentStateStore,
        async_ops::{BoxFuture, Cancellation},
    },
    values::agent_state::{
        AgentStateUpdate, ExecutionCheckpoint, ExecutionInputPart, ExecutionPhase,
        ExecutionTerminal, NotificationCheckpoint,
    },
};
use parrot_storage::events::{Admission, ConversationPart};
use std::collections::VecDeque;
use std::fmt;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::sync::Notify;
use tokio::task::JoinHandle;

pub const MAX_AGENT_MESSAGE_BYTES: usize = 1024 * 1024;
pub const MAX_AGENT_RESULT_BYTES: usize = 1024 * 1024;

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum LifecycleError {
    Cancelled,
    ShuttingDown,
    Failed(String),
}
impl LifecycleError {
    pub fn is_cancelled(&self) -> bool {
        matches!(self, Self::Cancelled)
    }
}
impl fmt::Display for LifecycleError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Cancelled => f.write_str("operation cancelled"),
            Self::ShuttingDown => f.write_str("the user session is shutting down"),
            Self::Failed(message) => f.write_str(message),
        }
    }
}
impl std::error::Error for LifecycleError {}
fn canceled() -> LifecycleError {
    LifecycleError::Cancelled
}
fn shutting_down() -> LifecycleError {
    LifecycleError::ShuttingDown
}

fn bounded(mut execution: Execution) -> Execution {
    bound(&mut execution.output);
    bound(&mut execution.error);
    execution
}
fn bound(value: &mut String) {
    if value.len() > MAX_AGENT_RESULT_BYTES {
        let mut end = MAX_AGENT_RESULT_BYTES;
        while !value.is_char_boundary(end) {
            end -= 1;
        }
        value.truncate(end);
    }
}

/// One pass outcome and its result captured before the backend releases its state.
pub struct DrainPass {
    pub execution: Execution,
    pub result_reference: ExecutionResultReference,
}
#[derive(Clone, Copy)]
pub enum ExecutionResultReference {
    Missing,
    Captured(i64),
}

/// Turn code checks scheduled work at source's pass boundary and joins accepted
/// reminder mutations before deciding that a terminal result is ready.
pub trait DrainWork: Send + Sync {
    fn has_scheduled(&self) -> bool;
    /// Joins task lifetimes; invocation results belong only to their observers.
    fn join_mutations(&self) -> BoxFuture<'_, Result<(), LifecycleError>>;
}

/// No successful defaults: the concrete backend owns every durable/provider boundary.
pub trait DrainBackend: Send + Sync + 'static {
    type Selection: Clone + Send + Sync + 'static;
    fn admit(
        &self,
        parts: Vec<ConversationPart>,
        message_id: String,
        delivery: Delivery,
        source: InputSource,
    ) -> BoxFuture<'_, Result<Admission, LifecycleError>>;
    fn publish_admission<'a>(
        &'a self,
        admission: &'a Admission,
        cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<(), LifecycleError>>;
    fn has_pending_inputs(&self) -> BoxFuture<'_, Result<bool, LifecycleError>>;
    fn cancel_pending_input(&self, input_id: String) -> BoxFuture<'_, Result<(), LifecycleError>>;
    fn update_selection(&self, selection: Self::Selection) -> Result<(), LifecycleError>;
    /// Called synchronously under the lifecycle gate; implementations must not
    /// re-enter AgentSession.
    fn drain_state_changed(&self, state: DrainState);
    fn pass<'a>(
        &'a self,
        cancellation: &'a dyn Cancellation,
        work: &'a dyn DrainWork,
    ) -> BoxFuture<'a, Result<DrainPass, LifecycleError>>;
    fn prepare_scheduled<'a>(
        &'a self,
        cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<Self::Selection, LifecycleError>>;
    fn execution_started(&self) -> BoxFuture<'_, Result<(), LifecycleError>>;
    fn execution_finished<'a>(
        &'a self,
        execution: &'a Execution,
    ) -> BoxFuture<'a, Result<(), LifecycleError>>;
    /// Reconciles execution start under a stable durable identity.
    fn execution_started_correlated(
        &self,
        correlation: &str,
    ) -> BoxFuture<'_, Result<(), LifecycleError>>;
    /// Retries terminal publication and parent admission with stable identities.
    fn execution_finished_correlated<'a>(
        &'a self,
        execution: &'a Execution,
        correlation: &'a str,
    ) -> BoxFuture<'a, Result<(), LifecycleError>>;
    /// Cancels only the exact initial input named by a durable cancellation intent.
    fn cancel_execution_input(&self, message_id: &str)
    -> BoxFuture<'_, Result<(), LifecycleError>>;
    /// Captures the durable conversation boundary before a new prompt is admitted.
    fn execution_watermark(&self) -> BoxFuture<'_, Result<i64, LifecycleError>>;
    /// Resolves a committed terminal reference without invoking the provider.
    fn restore_execution<'a>(
        &'a self,
        terminal: &'a ExecutionTerminal,
    ) -> BoxFuture<'a, Result<Execution, LifecycleError>>;
    /// Captures the outcome only after normal drain readiness has settled.
    fn capture_execution<'a>(
        &'a self,
        checkpoint: &'a ExecutionCheckpoint,
        execution: &'a mut Execution,
        reference: ExecutionResultReference,
    ) -> BoxFuture<'a, Result<ExecutionTerminal, LifecycleError>>;
    /// Resource cancellation following suspension must not close the durable turn.
    fn suspend_execution(&self);
    fn close(&self) -> BoxFuture<'_, Result<(), LifecycleError>>;
}

/// An owned cancellation source; observation never transfers cancellation ownership.
#[derive(Default)]
pub struct Signal {
    canceled: AtomicBool,
    changed: Notify,
}
impl Signal {
    pub fn new() -> Self {
        Self::default()
    }
    pub fn cancel(&self) {
        self.canceled.store(true, Ordering::Release);
        self.changed.notify_waiters();
    }
}
impl Cancellation for Signal {
    fn is_cancelled(&self) -> bool {
        self.canceled.load(Ordering::Acquire)
    }
    fn cancelled(&self) -> BoxFuture<'_, ()> {
        Box::pin(async move {
            loop {
                let notified = self.changed.notified();
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
struct LinkedCancellation {
    lifetime: Arc<dyn Cancellation>,
    stop: Arc<Signal>,
    owner: Option<Arc<dyn Cancellation>>,
}
impl Cancellation for LinkedCancellation {
    fn is_cancelled(&self) -> bool {
        self.lifetime.is_cancelled()
            || self.stop.is_cancelled()
            || self
                .owner
                .as_ref()
                .is_some_and(|owner| owner.is_cancelled())
    }
    fn cancelled(&self) -> BoxFuture<'_, ()> {
        Box::pin(async move {
            let owner = async {
                match &self.owner {
                    Some(owner) => owner.cancelled().await,
                    None => std::future::pending().await,
                }
            };
            tokio::select! { _ = self.lifetime.cancelled() => {}, _ = self.stop.cancelled() => {}, _ = owner => {} }
        })
    }
}

struct Completion<T> {
    value: Mutex<Option<T>>,
    changed: Notify,
}
impl<T: Clone> Completion<T> {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            value: Mutex::new(None),
            changed: Notify::new(),
        })
    }
    fn ready(value: T) -> Arc<Self> {
        Arc::new(Self {
            value: Mutex::new(Some(value)),
            changed: Notify::new(),
        })
    }
    fn finish(&self, value: T) {
        let mut current = self.value.lock().expect("completion poisoned");
        if current.is_none() {
            *current = Some(value);
            self.changed.notify_waiters();
        }
    }
    fn is_finished(&self) -> bool {
        self.value.lock().expect("completion poisoned").is_some()
    }
    async fn wait(&self) -> T {
        loop {
            let notified = self.changed.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            if let Some(value) = self.value.lock().expect("completion poisoned").clone() {
                return value;
            }
            notified.await;
        }
    }
}

/// A retained completion. Dropping or canceling an observer never drops its task.
pub struct OperationHandle<T: Clone> {
    completion: Arc<Completion<Result<T, LifecycleError>>>,
}
impl<T: Clone> Clone for OperationHandle<T> {
    fn clone(&self) -> Self {
        Self {
            completion: self.completion.clone(),
        }
    }
}
impl<T: Clone> OperationHandle<T> {
    pub fn is_finished(&self) -> bool {
        self.completion.is_finished()
    }
    pub async fn wait(&self, cancellation: &dyn Cancellation) -> Result<T, LifecycleError> {
        if cancellation.is_cancelled() {
            return Err(canceled());
        }
        tokio::select! { result = self.completion.wait() => result, _ = cancellation.cancelled() => Err(canceled()) }
    }
    pub async fn join(&self) -> Result<T, LifecycleError> {
        self.completion.wait().await
    }
}
pub type ExecutionHandle = OperationHandle<Execution>;
pub type AdmissionCallback = Arc<dyn Fn(&Admission) + Send + Sync>;

struct MutationCompletion<T: Clone> {
    operation: Arc<Completion<Result<T, LifecycleError>>>,
    lifetime: Arc<Completion<Result<(), LifecycleError>>>,
    settled: bool,
}
impl<T: Clone> MutationCompletion<T> {
    fn finish(mut self, result: Result<T, LifecycleError>) {
        self.operation.finish(result);
        // An invocation failure is not an owned-task cleanup failure.
        self.lifetime.finish(Ok(()));
        self.settled = true;
    }
}
impl<T: Clone> Drop for MutationCompletion<T> {
    fn drop(&mut self) {
        if !self.settled {
            let error =
                LifecycleError::Failed("owned agent mutation task ended before completion".into());
            self.operation.finish(Err(error.clone()));
            self.lifetime.finish(Err(error));
        }
    }
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct IncomingActivity {
    pub name: String,
    pub cause: Option<String>,
    pub source: InputSource,
}
#[derive(Clone)]
pub struct SendResult {
    pub admission: Admission,
    pub follow_up: bool,
    pub execution: ExecutionHandle,
}
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WaitResult {
    pub yielded: bool,
    pub elapsed_milliseconds: u64,
    pub execution: Option<Execution>,
}
type TaskJoin = Arc<tokio::sync::Mutex<Option<JoinHandle<()>>>>;
type Scheduled<B> = Box<
    dyn FnOnce(
            <B as DrainBackend>::Selection,
            Arc<dyn Cancellation>,
        ) -> BoxFuture<'static, Result<(), LifecycleError>>
        + Send,
>;
struct WorkRequest<B: DrainBackend> {
    execute: Scheduled<B>,
    cancellation: Arc<dyn Cancellation>,
    completion: Arc<Completion<Result<(), LifecycleError>>>,
}
struct OwnedReservation {
    admission: Mutex<Option<Admission>>,
    cancellation: Arc<dyn Cancellation>,
    startup: Arc<Completion<Result<bool, LifecycleError>>>,
    result_reference: Mutex<ExecutionResultReference>,
}
impl OwnedReservation {
    fn capture_result_reference(&self) -> ExecutionResultReference {
        *self
            .result_reference
            .lock()
            .expect("execution result reference poisoned")
    }
}
struct DrainSlot {
    id: u64,
    cancellation: Arc<Signal>,
    execution: ExecutionHandle,
    join: TaskJoin,
}
struct IncomingWait {
    id: u64,
    completion: Arc<Completion<IncomingActivity>>,
}
struct State<B: DrainBackend> {
    disposing: bool,
    drain_state: DrainState,
    next_id: u64,
    wake: bool,
    drain: Option<DrainSlot>,
    last_drain: ExecutionHandle,
    started: bool,
    execution: ExecutionHandle,
    tail: Arc<Completion<()>>,
    incoming: Option<IncomingWait>,
    scheduled: VecDeque<WorkRequest<B>>,
    tasks: Vec<TaskJoin>,
    task_failure: Option<LifecycleError>,
    mutations: Vec<OperationHandle<()>>,
    closed: Option<OperationHandle<()>>,
}
struct Inner<B: DrainBackend> {
    backend: Arc<B>,
    lifetime: Arc<dyn Cancellation>,
    closing: Arc<Signal>,
    state: Mutex<State<B>>,
    close_join: tokio::sync::Mutex<Option<JoinHandle<()>>>,
    recovery: Option<Arc<dyn AgentStateStore>>,
    checkpoint_gate: tokio::sync::Mutex<()>,
    suspending: AtomicBool,
    interrupting: AtomicBool,
}
pub struct AgentSession<B: DrainBackend> {
    inner: Arc<Inner<B>>,
}
impl<B: DrainBackend> Clone for AgentSession<B> {
    fn clone(&self) -> Self {
        Self {
            inner: self.inner.clone(),
        }
    }
}

fn reap_tasks<B: DrainBackend>(state: &mut State<B>) {
    // Once close snapshots the handles it owns their joins, including failures.
    if state.disposing {
        return;
    }
    let mut failure = None;
    let mut context = std::task::Context::from_waker(std::task::Waker::noop());
    state.tasks.retain(|task| {
        let Ok(mut slot) = task.try_lock() else {
            return true;
        };
        let Some(handle) = slot.as_mut() else {
            return false;
        };
        if !handle.is_finished() {
            return true;
        }
        match std::future::Future::poll(std::pin::Pin::new(handle), &mut context) {
            std::task::Poll::Pending => true,
            std::task::Poll::Ready(result) => {
                *slot = None;
                if let Err(error) = result {
                    if failure.is_none() {
                        failure = Some(LifecycleError::Failed(format!(
                            "owned agent operation failed: {error}"
                        )));
                    }
                }
                false
            }
        }
    });
    if state.task_failure.is_none() {
        state.task_failure = failure;
    }
}

fn spawn_tracked<B: DrainBackend>(
    state: &mut State<B>,
    future: impl std::future::Future<Output = ()> + Send + 'static,
) -> TaskJoin {
    reap_tasks(state);
    let task = Arc::new(tokio::sync::Mutex::new(Some(tokio::spawn(future))));
    state.tasks.push(task.clone());
    task
}
async fn join_task(task: &TaskJoin) -> Result<(), LifecycleError> {
    let mut slot = task.lock().await;
    let result = match slot.as_mut() {
        Some(task) => task.await.map_err(|error| {
            LifecycleError::Failed(format!("owned agent operation failed: {error}"))
        }),
        None => Ok(()),
    };
    *slot = None;
    result
}
fn check_message(parts: &[ConversationPart]) -> Result<(), LifecycleError> {
    let mut bytes = 0;
    let mut text_only = true;
    let mut nonempty = false;
    for part in parts {
        match part {
            ConversationPart::Text(text) => {
                bytes += text.len();
                nonempty |= !text.trim().is_empty();
            }
            ConversationPart::ImageArtifact { .. } => text_only = false,
        }
    }
    if text_only && !nonempty {
        return Err(LifecycleError::Failed("no message given".into()));
    }
    if bytes > MAX_AGENT_MESSAGE_BYTES {
        return Err(LifecycleError::Failed(
            "agent message exceeds 1048576 bytes".into(),
        ));
    }
    Ok(())
}

impl<B: DrainBackend> AgentSession<B> {
    pub fn new(backend: Arc<B>, lifetime: Arc<dyn Cancellation>) -> Self {
        Self::construct(backend, lifetime, None)
    }
    /// Binds the same agent-local store shared by the scope's other services.
    pub fn with_state(
        backend: Arc<B>,
        lifetime: Arc<dyn Cancellation>,
        store: Arc<dyn AgentStateStore>,
    ) -> Self {
        Self::construct(backend, lifetime, Some(store))
    }
    fn construct(
        backend: Arc<B>,
        lifetime: Arc<dyn Cancellation>,
        recovery: Option<Arc<dyn AgentStateStore>>,
    ) -> Self {
        let initial = OperationHandle {
            completion: Completion::ready(Ok(Execution::succeeded(""))),
        };
        Self {
            inner: Arc::new(Inner {
                backend,
                lifetime,
                closing: Arc::new(Signal::new()),
                state: Mutex::new(State {
                    disposing: false,
                    drain_state: DrainState::Idle,
                    next_id: 0,
                    wake: false,
                    drain: None,
                    last_drain: initial.clone(),
                    started: false,
                    execution: initial,
                    tail: Completion::ready(()),
                    incoming: None,
                    scheduled: VecDeque::new(),
                    tasks: Vec::new(),
                    task_failure: None,
                    mutations: Vec::new(),
                    closed: None,
                }),
                close_join: tokio::sync::Mutex::new(None),
                recovery,
                checkpoint_gate: tokio::sync::Mutex::new(()),
                suspending: AtomicBool::new(false),
                interrupting: AtomicBool::new(false),
            }),
        }
    }
    pub fn backend(&self) -> &Arc<B> {
        &self.inner.backend
    }
    pub fn state(&self) -> DrainState {
        self.inner
            .state
            .lock()
            .expect("session poisoned")
            .drain_state
    }
    pub fn is_idle(&self) -> bool {
        self.state() == DrainState::Idle
    }
    pub fn is_active(&self) -> bool {
        let state = self.inner.state.lock().expect("session poisoned");
        state.drain_state != DrainState::Idle
            || (state.started && !state.execution.is_finished())
            || state
                .mutations
                .iter()
                .any(|mutation| !mutation.is_finished())
    }
    fn accepting(&self, state: &State<B>) -> Result<(), LifecycleError> {
        if state.disposing || self.inner.lifetime.is_cancelled() {
            Err(shutting_down())
        } else {
            Ok(())
        }
    }
    fn wake_locked(
        &self,
        state: &mut State<B>,
        activity: Option<IncomingActivity>,
        owned: Option<Arc<OwnedReservation>>,
    ) -> (bool, ExecutionHandle) {
        if state.disposing {
            return (false, state.last_drain.clone());
        }
        if let Some(activity) = activity {
            if let Some(wait) = &state.incoming {
                wait.completion.finish(activity);
            }
        }
        if let Some(drain) = &state.drain {
            state.wake = true;
            return (false, drain.execution.clone());
        }
        let owned = if owned.is_none() && self.inner.recovery.is_some() && state.started {
            let predecessor = state.execution.clone();
            let reservation = Arc::new(OwnedReservation {
                admission: Mutex::new(None),
                cancellation: Arc::new(Signal::new()),
                startup: Completion::new(),
                result_reference: Mutex::new(ExecutionResultReference::Missing),
            });
            let ready = reservation.clone();
            let closing = self.inner.closing.clone();
            let lifetime = self.inner.lifetime.clone();
            let session = self.clone();
            spawn_tracked(state, async move {
                let start = tokio::select! {
                    _ = predecessor.join() => {
                        match session.execution_checkpoint().await {
                            Ok(Some(checkpoint)) if checkpoint.terminal.is_some() => Ok(true),
                            Ok(_) => Err(LifecycleError::Failed("previous execution has no durable terminal; recover it before starting more work".into())),
                            Err(error) => Err(error),
                        }
                    },
                    _ = closing.cancelled() => Ok(false),
                    _ = lifetime.cancelled() => Ok(false),
                };
                ready.startup.finish(start);
            });
            Some(reservation)
        } else {
            owned
        };
        state.next_id += 1;
        let id = state.next_id;
        let cancellation = Arc::new(Signal::new());
        let linked: Arc<dyn Cancellation> = Arc::new(LinkedCancellation {
            lifetime: self.inner.lifetime.clone(),
            stop: cancellation.clone(),
            owner: owned
                .as_ref()
                .map(|reservation| reservation.cancellation.clone()),
        });
        let completion = Completion::new();
        let execution = OperationHandle {
            completion: completion.clone(),
        };
        let session = self.clone();
        let join = spawn_tracked(state, async move {
            tokio::task::yield_now().await;
            let result = session.drain(id, owned, linked).await;
            completion.finish(result);
        });
        state.drain_state = DrainState::Running;
        self.inner.backend.drain_state_changed(DrainState::Running);
        state.last_drain = execution.clone();
        state.drain = Some(DrainSlot {
            id,
            cancellation,
            execution: execution.clone(),
            join,
        });
        (true, execution)
    }
    pub fn wake(&self, activity: Option<IncomingActivity>) -> (bool, ExecutionHandle) {
        let mut state = self.inner.state.lock().expect("session poisoned");
        self.wake_locked(&mut state, activity, None)
    }
    pub fn recover(&self) -> ExecutionHandle {
        self.wake(None).1
    }

    async fn admit_and_wake(
        &self,
        parts: Vec<ConversationPart>,
        message_id: String,
        delivery: Delivery,
        activity: IncomingActivity,
        publication: &dyn Cancellation,
    ) -> Result<SendResult, LifecycleError> {
        let admission = self
            .inner
            .backend
            .admit(parts, message_id, delivery, activity.source)
            .await?;
        // Publication is a separate boundary. Even a canceled/failed publication
        // cannot strand the admission that the actor has already committed.
        let published = self
            .inner
            .backend
            .publish_admission(&admission, publication)
            .await;
        let pending = if admission.created() {
            Ok(true)
        } else {
            self.inner.backend.has_pending_inputs().await
        };
        let incoming =
            (admission.created() || pending.as_ref().copied().unwrap_or(false)).then_some(activity);
        let (follow_up, execution) = {
            let mut state = self.inner.state.lock().expect("session poisoned");
            self.wake_locked(&mut state, incoming, None)
        };
        published?;
        pending?;
        Ok(SendResult {
            admission,
            follow_up,
            execution,
        })
    }

    pub fn send_handle(
        &self,
        parts: Vec<ConversationPart>,
        message_id: String,
        delivery: Delivery,
        activity: IncomingActivity,
        caller: Arc<dyn Cancellation>,
    ) -> Result<OperationHandle<SendResult>, LifecycleError> {
        let mut state = self.inner.state.lock().expect("session poisoned");
        self.accepting(&state)?;
        let completion = Completion::new();
        let handle = OperationHandle {
            completion: completion.clone(),
        };
        let session = self.clone();
        spawn_tracked(&mut state, async move {
            completion.finish(
                session
                    .admit_and_wake(parts, message_id, delivery, activity, caller.as_ref())
                    .await,
            );
        });
        Ok(handle)
    }
    pub async fn send(
        &self,
        parts: Vec<ConversationPart>,
        message_id: String,
        delivery: Delivery,
        activity: IncomingActivity,
        caller: Arc<dyn Cancellation>,
    ) -> Result<SendResult, LifecycleError> {
        self.send_handle(parts, message_id, delivery, activity, caller.clone())?
            .wait(caller.as_ref())
            .await
    }
    pub fn record_handle(
        &self,
        parts: Vec<ConversationPart>,
        message_id: String,
        delivery: Delivery,
        caller: Arc<dyn Cancellation>,
    ) -> Result<OperationHandle<Admission>, LifecycleError> {
        if message_id.trim().is_empty() {
            return Err(LifecycleError::Failed(
                "message id must not be empty".into(),
            ));
        }
        let mut state = self.inner.state.lock().expect("session poisoned");
        self.accepting(&state)?;
        let completion = Completion::new();
        let handle = OperationHandle {
            completion: completion.clone(),
        };
        let backend = self.inner.backend.clone();
        spawn_tracked(&mut state, async move {
            let result = async {
                let admission = backend
                    .admit(
                        parts,
                        message_id,
                        delivery,
                        InputSource::INPUT_SOURCE_UNSPECIFIED,
                    )
                    .await?;
                backend
                    .publish_admission(&admission, caller.as_ref())
                    .await?;
                Ok(admission)
            }
            .await;
            completion.finish(result);
        });
        Ok(handle)
    }
    pub async fn record(
        &self,
        parts: Vec<ConversationPart>,
        message_id: String,
        delivery: Delivery,
        caller: Arc<dyn Cancellation>,
    ) -> Result<Admission, LifecycleError> {
        self.record_handle(parts, message_id, delivery, caller.clone())?
            .wait(caller.as_ref())
            .await
    }

    /// Selects the source public execution when idle; otherwise steers into the
    /// current drain. A unary observer's token never becomes that drain's owner.
    pub fn send_and_select_execution(
        &self,
        parts: Vec<ConversationPart>,
        message_id: String,
        caller: Arc<dyn Cancellation>,
    ) -> Result<OperationHandle<SendResult>, LifecycleError> {
        check_message(&parts)?;
        let mut state = self.inner.state.lock().expect("session poisoned");
        self.accepting(&state)?;
        if state.tail.is_finished() && (!state.started || state.execution.is_finished()) {
            let follow_up = state.started;
            self.inner.interrupting.store(false, Ordering::Release);
            state.started = true;
            let result = Completion::new();
            let handle = OperationHandle {
                completion: result.clone(),
            };
            let completed = Completion::new();
            state.execution = OperationHandle {
                completion: completed.clone(),
            };
            let reservation = self.inner.recovery.as_ref().map(|_| {
                let owned = Arc::new(OwnedReservation {
                    admission: Mutex::new(None),
                    cancellation: Arc::new(Signal::new()),
                    startup: Completion::new(),
                    result_reference: Mutex::new(ExecutionResultReference::Missing),
                });
                let selected = self.wake_locked(&mut state, None, Some(owned.clone())).1;
                (owned, selected)
            });
            let session = self.clone();
            spawn_tracked(&mut state, async move {
                tokio::task::yield_now().await;
                let mut owns_intent = false;
                let execution = async {
                    session.begin_execution(&parts, &message_id).await?;
                    owns_intent = true;
                    let checkpoint = session.execution_checkpoint().await?;
                    if checkpoint.is_none() {
                        session.inner.backend.execution_started().await?;
                    }
                    let uncanceled = Signal::new();
                    let publication: &dyn Cancellation = if follow_up && reservation.is_none() {
                        caller.as_ref()
                    } else {
                        &uncanceled
                    };
                    let mut sent = session
                        .admit_and_wake(
                            parts,
                            message_id,
                            Delivery::DELIVERY_STEER,
                            IncomingActivity::default(),
                            publication,
                        )
                        .await?;
                    if let (Some(checkpoint), Some((owned, _))) = (&checkpoint, &reservation) {
                        let active = session
                            .activate_execution(checkpoint, owned, &sent.admission)
                            .await?;
                        owned.startup.finish(Ok(active));
                    }
                    sent.follow_up = follow_up;
                    let drain = sent.execution.clone();
                    // Fresh sends expose the public execution, whose completion
                    // includes terminal callbacks, rather than its raw drain.
                    sent.execution = OperationHandle {
                        completion: completed.clone(),
                    };
                    result.finish(Ok(sent));
                    drain.join().await
                }
                .await;
                if let Some((owned, selected)) = &reservation {
                    owned.startup.finish(Ok(false));
                    let _ = selected.join().await;
                }
                let execution = if owns_intent {
                    let reference = reservation
                        .as_ref()
                        .map_or(ExecutionResultReference::Missing, |(owned, _)| {
                            owned.capture_result_reference()
                        });
                    session.finish_execution(execution, reference).await
                } else {
                    bounded(match execution {
                        Ok(execution) => execution,
                        Err(error) => Execution::failed(error.to_string()),
                    })
                };
                if !result.is_finished() {
                    result.finish(Err(if execution.status == ExecutionStatus::Canceled {
                        canceled()
                    } else {
                        LifecycleError::Failed(execution.error.clone())
                    }));
                }
                completed.finish(Ok(execution));
            });
            Ok(handle)
        } else {
            drop(state);
            self.send_handle(
                parts,
                message_id,
                Delivery::DELIVERY_STEER,
                IncomingActivity::default(),
                caller,
            )
        }
    }
    pub async fn send_text_message(
        &self,
        message: String,
        caller: Arc<dyn Cancellation>,
    ) -> Result<SendResult, LifecycleError> {
        let id = parrot_domain::values::identifiers::Identifier::message_id()
            .map_err(|error| LifecycleError::Failed(error.to_string()))?;
        self.send_and_select_execution(vec![ConversationPart::text(message)], id, caller.clone())?
            .wait(caller.as_ref())
            .await
    }

    pub fn enqueue_execution(
        &self,
        parts: Vec<ConversationPart>,
        message_id: String,
        selection: Option<B::Selection>,
        admitted: Option<AdmissionCallback>,
        caller: Arc<dyn Cancellation>,
    ) -> Result<ExecutionHandle, LifecycleError> {
        check_message(&parts)?;
        let mut state = self.inner.state.lock().expect("session poisoned");
        self.accepting(&state)?;
        let predecessor = if state.tail.is_finished() {
            state.execution.completion.clone()
        } else {
            // A tail has no execution result, but its predecessor barrier remains
            // live even after the reservation's visible cancellation completes.
            let tail = state.tail.clone();
            let barrier = Completion::new();
            let done = barrier.clone();
            spawn_tracked(&mut state, async move {
                tail.wait().await;
                done.finish(Ok(Execution::succeeded("")));
            });
            barrier
        };
        state.started = true;
        let completion = Completion::new();
        let handle = OperationHandle {
            completion: completion.clone(),
        };
        let tail = Completion::new();
        state.tail = tail.clone();
        let session = self.clone();
        spawn_tracked(&mut state, async move {
            let waiting = async {
                tokio::select! {
                    _ = predecessor.wait() => Ok(()),
                    _ = caller.cancelled() => Err(canceled()),
                    _ = session.inner.closing.cancelled() => Err(canceled()),
                    _ = session.inner.lifetime.cancelled() => Err(canceled()),
                }
            }
            .await;
            if let Err(error) = waiting {
                completion.finish(Err(error));
                // This barrier, not the canceled observer's completion, owns its
                // FIFO position. No later reservation may overtake it.
                let _ = predecessor.wait().await;
                tail.finish(());
                return;
            }
            let result = session
                .execute_after(
                    parts,
                    message_id,
                    selection,
                    admitted,
                    caller,
                    completion.clone(),
                )
                .await;
            completion.finish(result);
            tail.finish(());
        });
        Ok(handle)
    }
    pub async fn send_and_wait(
        &self,
        parts: Vec<ConversationPart>,
        message_id: String,
        selection: Option<B::Selection>,
        admitted: Option<AdmissionCallback>,
        caller: Arc<dyn Cancellation>,
    ) -> Result<String, LifecycleError> {
        let result = self
            .enqueue_execution(parts, message_id, selection, admitted, caller.clone())?
            .wait(caller.as_ref())
            .await?;
        if caller.is_cancelled() {
            return Err(canceled());
        }
        match result.status {
            ExecutionStatus::Succeeded => Ok(result.output),
            ExecutionStatus::Failed => Err(LifecycleError::Failed(result.error)),
            ExecutionStatus::Canceled => Err(canceled()),
        }
    }

    async fn execute_after(
        &self,
        parts: Vec<ConversationPart>,
        message_id: String,
        selection: Option<B::Selection>,
        admitted: Option<AdmissionCallback>,
        caller: Arc<dyn Cancellation>,
        completion: Arc<Completion<Result<Execution, LifecycleError>>>,
    ) -> Result<Execution, LifecycleError> {
        loop {
            if caller.is_cancelled() {
                return Err(canceled());
            }
            let (active, steer, owned) = {
                let mut state = self.inner.state.lock().expect("session poisoned");
                if state.disposing || self.inner.lifetime.is_cancelled() {
                    return Ok(Execution::canceled());
                }
                let active = if !state.execution.is_finished() {
                    Some(state.execution.clone())
                } else {
                    state.drain.as_ref().map(|drain| drain.execution.clone())
                };
                let steer = active.is_some() && state.incoming.is_some();
                let owned = if active.is_none() {
                    if let Some(selection) = selection.clone() {
                        self.inner.backend.update_selection(selection)?;
                    }
                    self.inner.interrupting.store(false, Ordering::Release);
                    state.started = true;
                    state.execution = OperationHandle {
                        completion: completion.clone(),
                    };
                    // Reserve the slot before the first asynchronous durable
                    // admission. Raw wakes must reuse this startup-gated drain,
                    // not create an unowned drain while the actor commits.
                    let owned = Arc::new(OwnedReservation {
                        admission: Mutex::new(None),
                        cancellation: caller.clone(),
                        startup: Completion::new(),
                        result_reference: Mutex::new(ExecutionResultReference::Missing),
                    });
                    let selected = self
                        .wake_locked(
                            &mut state,
                            Some(IncomingActivity::default()),
                            Some(owned.clone()),
                        )
                        .1;
                    Some((owned, selected))
                } else {
                    None
                };
                (active, steer, owned)
            };
            if let Some((owned, selected)) = owned {
                return self
                    .execute_owned(parts, message_id, admitted, owned, selected)
                    .await;
            }
            let active = active.expect("active execution selected");
            if steer {
                let sent = self
                    .admit_and_wake(
                        parts,
                        message_id,
                        Delivery::DELIVERY_STEER,
                        IncomingActivity::default(),
                        caller.as_ref(),
                    )
                    .await?;
                if let Some(admitted) = admitted {
                    admitted(&sent.admission);
                }
                return sent.execution.wait(caller.as_ref()).await;
            }
            tokio::select! {
                _ = active.join() => {},
                _ = caller.cancelled() => return Err(canceled()),
                _ = self.inner.closing.cancelled() => return Ok(Execution::canceled()),
                _ = self.inner.lifetime.cancelled() => return Ok(Execution::canceled()),
            }
        }
    }
    async fn execute_owned(
        &self,
        parts: Vec<ConversationPart>,
        message_id: String,
        admitted: Option<AdmissionCallback>,
        owned: Arc<OwnedReservation>,
        selected: ExecutionHandle,
    ) -> Result<Execution, LifecycleError> {
        if let Err(error) = self.begin_execution(&parts, &message_id).await {
            owned.startup.finish(Ok(false));
            let _ = selected.join().await;
            return Err(error);
        }
        let admission = match self
            .inner
            .backend
            .admit(
                parts,
                message_id,
                Delivery::DELIVERY_STEER,
                InputSource::INPUT_SOURCE_UNSPECIFIED,
            )
            .await
        {
            Ok(admission) => admission,
            Err(error) => {
                owned.startup.finish(Ok(false));
                let _ = selected.join().await;
                return Err(error);
            }
        };
        *owned.admission.lock().expect("reservation poisoned") = Some(admission.clone());
        if let Some(admitted) = admitted {
            admitted(&admission);
        };
        let result = async {
            let checkpoint = self.execution_checkpoint().await?;
            let active = match &checkpoint {
                Some(checkpoint) => {
                    self.activate_execution(checkpoint, &owned, &admission)
                        .await?
                }
                None => {
                    self.inner.backend.execution_started().await?;
                    true
                }
            };
            self.inner
                .backend
                .publish_admission(&admission, &Signal::new())
                .await?;
            owned.startup.finish(Ok(active));
            selected.join().await
        }
        .await;
        owned.startup.finish(Ok(false));
        // Startup failure must still release and join the reserved drain before
        // terminal callbacks and before a successor can run.
        let _ = selected.join().await;
        Ok(self
            .finish_execution(result, owned.capture_result_reference())
            .await)
    }
    async fn finish_execution(
        &self,
        result: Result<Execution, LifecycleError>,
        reference: ExecutionResultReference,
    ) -> Execution {
        let mut execution = bounded(match result {
            Ok(execution) => execution,
            Err(error) if error.is_cancelled() => Execution::canceled(),
            Err(error) => Execution::failed(error.to_string()),
        });
        if self.inner.suspending.load(Ordering::Acquire)
            && execution.status == ExecutionStatus::Canceled
        {
            return execution;
        }
        match self
            .finish_durable_execution(&mut execution, reference)
            .await
        {
            Ok(true) => return execution,
            Ok(false) => {}
            Err(error) => return bounded(Execution::failed(error.to_string())),
        }
        if let Err(error) = self.inner.backend.execution_finished(&execution).await {
            execution = bounded(Execution::failed(error.to_string()));
        }
        execution
    }

    pub async fn wait(
        &self,
        yield_after_milliseconds: u64,
        caller: &dyn Cancellation,
    ) -> Result<WaitResult, LifecycleError> {
        let execution = {
            let state = self.inner.state.lock().expect("session poisoned");
            if !state.started {
                return Err(LifecycleError::Failed("agent has not started".into()));
            }
            state.execution.clone()
        };
        let start = Instant::now();
        let result = if yield_after_milliseconds == 0 {
            Some(execution.wait(caller).await?)
        } else {
            tokio::select! {
                biased;
                result = execution.wait(caller) => Some(result?),
                _ = tokio::time::sleep(Duration::from_millis(yield_after_milliseconds)) => {
                    if caller.is_cancelled() { return Err(canceled()); }
                    None
                },
            }
        };
        Ok(WaitResult {
            yielded: result.is_none(),
            elapsed_milliseconds: start.elapsed().as_millis().min(u64::MAX as u128) as u64,
            execution: result,
        })
    }

    pub async fn wait_for_incoming_input(
        &self,
        duration: Duration,
        caller: &dyn Cancellation,
    ) -> Result<Option<IncomingActivity>, LifecycleError> {
        if caller.is_cancelled() {
            return Err(canceled());
        }
        let completion = Completion::new();
        let pending = Completion::new();
        let id = {
            let mut state = self.inner.state.lock().expect("session poisoned");
            self.accepting(&state)?;
            if state.incoming.is_some() {
                return Err(LifecycleError::Failed(
                    "An incoming-input wait is already active.".into(),
                ));
            }
            state.next_id += 1;
            let id = state.next_id;
            state.incoming = Some(IncomingWait {
                id,
                completion: completion.clone(),
            });
            // The durable look is owned too: dropping a reader cannot leave a
            // borrowed storage operation racing backend.close.
            let backend = self.inner.backend.clone();
            let done = pending.clone();
            spawn_tracked(&mut state, async move {
                done.finish(backend.has_pending_inputs().await);
            });
            id
        };
        let _registration = IncomingRegistration {
            session: self.clone(),
            id,
        };
        let has_pending = tokio::select! {
            pending = pending.wait() => pending?,
            _ = caller.cancelled() => return Err(canceled()),
            _ = self.inner.closing.cancelled() => return Err(canceled()),
            _ = self.inner.lifetime.cancelled() => return Err(canceled()),
        };
        if has_pending {
            completion.finish(IncomingActivity::default());
        }
        let activity = tokio::select! {
            biased;
            activity = completion.wait() => Some(activity),
            _ = tokio::time::sleep(duration) => None,
            _ = caller.cancelled() => return Err(canceled()),
            _ = self.inner.closing.cancelled() => return Err(canceled()),
            _ = self.inner.lifetime.cancelled() => return Err(canceled()),
        };
        if caller.is_cancelled() {
            return Err(canceled());
        }
        Ok(activity)
    }

    pub fn schedule_work<F>(
        &self,
        execute: F,
        caller: Arc<dyn Cancellation>,
    ) -> Result<OperationHandle<()>, LifecycleError>
    where
        F: FnOnce(
                B::Selection,
                Arc<dyn Cancellation>,
            ) -> BoxFuture<'static, Result<(), LifecycleError>>
            + Send
            + 'static,
    {
        if caller.is_cancelled() {
            return Err(canceled());
        }
        let completion = Completion::new();
        let handle = OperationHandle {
            completion: completion.clone(),
        };
        let mut state = self.inner.state.lock().expect("session poisoned");
        self.accepting(&state)?;
        state.scheduled.push_back(WorkRequest {
            execute: Box::new(execute),
            cancellation: caller,
            completion,
        });
        self.wake_locked(&mut state, None, None);
        Ok(handle)
    }

    /// Admission to this gate transfers mutation ownership to the session. The
    /// closure is never selected against caller cancellation after acceptance.
    pub fn owned_mutation<T, F>(
        &self,
        execute: F,
        caller: Arc<dyn Cancellation>,
    ) -> Result<OperationHandle<T>, LifecycleError>
    where
        T: Clone + Send + 'static,
        F: FnOnce(Arc<B>, Arc<dyn Cancellation>) -> BoxFuture<'static, Result<T, LifecycleError>>
            + Send
            + 'static,
    {
        if caller.is_cancelled() {
            return Err(canceled());
        }
        let mut state = self.inner.state.lock().expect("session poisoned");
        self.accepting(&state)?;
        let completion = Completion::new();
        let handle = OperationHandle {
            completion: completion.clone(),
        };
        let barrier = Completion::new();
        state.mutations.push(OperationHandle {
            completion: barrier.clone(),
        });
        if state.drain.is_some() {
            state.wake = true;
        }
        let backend = self.inner.backend.clone();
        let lifetime = self.inner.lifetime.clone();
        let completion = MutationCompletion {
            operation: completion,
            lifetime: barrier,
            settled: false,
        };
        spawn_tracked(&mut state, async move {
            let result = execute(backend, lifetime).await;
            completion.finish(result);
        });
        Ok(handle)
    }

    pub fn interrupt_handle(&self) -> Result<OperationHandle<()>, LifecycleError> {
        let completion = Completion::new();
        let handle = OperationHandle {
            completion: completion.clone(),
        };
        let mut state = self.inner.state.lock().expect("session poisoned");
        if state.disposing {
            return Err(shutting_down());
        }
        let Some(drain) = &state.drain else {
            completion.finish(Ok(()));
            return Ok(handle);
        };
        self.inner.interrupting.store(true, Ordering::Release);
        let stop = drain.cancellation.clone();
        let execution = drain.execution.clone();
        let join = drain.join.clone();
        state.wake = false;
        if state.drain_state != DrainState::Interrupting {
            state.drain_state = DrainState::Interrupting;
            self.inner
                .backend
                .drain_state_changed(DrainState::Interrupting);
        }
        let session = self.clone();
        spawn_tracked(&mut state, async move {
            let recorded = session.persist_cancellation().await;
            stop.cancel();
            let result = execution.join().await.map(|_| ());
            let joined = join_task(&join).await;
            let pending = session.inner.backend.has_pending_inputs().await;
            if pending.as_ref().copied().unwrap_or(false) {
                let mut state = session.inner.state.lock().expect("session poisoned");
                if !state.disposing {
                    session.wake_locked(&mut state, Some(IncomingActivity::default()), None);
                }
            }
            completion.finish(recorded.and(result).and(joined).and(pending.map(|_| ())));
        });
        Ok(handle)
    }
    pub async fn interrupt(&self, caller: &dyn Cancellation) -> Result<(), LifecycleError> {
        self.interrupt_handle()?.wait(caller).await
    }

    /// Suspends unfinished durable work before cancellation joins resources.
    /// Integration must call this before canceling the user-session lifetime.
    pub fn suspend_handle(&self) -> OperationHandle<()> {
        if self.inner.recovery.is_some() && !self.inner.interrupting.load(Ordering::Acquire) {
            self.inner.suspending.store(true, Ordering::Release);
            self.inner.backend.suspend_execution();
        }
        self.close_handle()
    }
    pub async fn suspend(&self) -> Result<(), LifecycleError> {
        self.suspend_handle().join().await?;
        self.close().await
    }

    pub fn close_handle(&self) -> OperationHandle<()> {
        let mut state = self.inner.state.lock().expect("session poisoned");
        if let Some(closed) = &state.closed {
            return closed.clone();
        }
        reap_tasks(&mut state);
        state.disposing = true;
        state.wake = false;
        self.inner.closing.cancel();
        if state.drain.is_some() {
            if state.drain_state != DrainState::Interrupting {
                state.drain_state = DrainState::Interrupting;
                self.inner
                    .backend
                    .drain_state_changed(DrainState::Interrupting);
            }
            state.drain.as_ref().unwrap().cancellation.cancel();
        }
        while let Some(request) = state.scheduled.pop_front() {
            request.completion.finish(Err(canceled()));
        }
        let completion = Completion::new();
        let handle = OperationHandle {
            completion: completion.clone(),
        };
        state.closed = Some(handle.clone());
        let tasks = state.tasks.clone();
        let task_failure = state.task_failure.take();
        let backend = self.inner.backend.clone();
        let session = self.clone();
        let task = tokio::spawn(async move {
            let mut result = task_failure.map_or(Ok(()), Err);
            if session.inner.suspending.load(Ordering::Acquire) {
                if let Err(error) = session.persist_suspension().await {
                    result = Err(error);
                }
            }
            for task in tasks {
                if let Err(error) = join_task(&task).await {
                    if result.is_ok() {
                        result = Err(error);
                    }
                }
            }
            if let Err(error) = backend.close().await {
                if result.is_ok() {
                    result = Err(error);
                }
            }
            completion.finish(result);
        });
        *self
            .inner
            .close_join
            .try_lock()
            .expect("new close task cannot already be joining") = Some(task);
        handle
    }
    pub async fn close(&self) -> Result<(), LifecycleError> {
        let result = self.close_handle().join().await;
        // Keep the handle in its slot while awaiting so dropping this observer
        // cannot detach even the close task itself.
        let mut slot = self.inner.close_join.lock().await;
        let joined = match slot.as_mut() {
            Some(task) => task
                .await
                .map_err(|error| LifecycleError::Failed(format!("agent close failed: {error}"))),
            None => Ok(()),
        };
        *slot = None;
        result.and(joined)
    }

    async fn run_scheduled(&self, drain_cancellation: Arc<dyn Cancellation>) {
        loop {
            let request = self
                .inner
                .state
                .lock()
                .expect("session poisoned")
                .scheduled
                .pop_front();
            let Some(request) = request else {
                return;
            };
            let cancellation: Arc<dyn Cancellation> = Arc::new(LinkedCancellation {
                lifetime: drain_cancellation.clone(),
                stop: Arc::new(Signal::new()),
                owner: Some(request.cancellation),
            });
            let result = async {
                if cancellation.is_cancelled() {
                    return Err(canceled());
                }
                let selection = self
                    .inner
                    .backend
                    .prepare_scheduled(cancellation.as_ref())
                    .await?;
                (request.execute)(selection, cancellation).await
            }
            .await;
            request.completion.finish(result);
        }
    }
    async fn drain(
        &self,
        id: u64,
        owned: Option<Arc<OwnedReservation>>,
        cancellation: Arc<dyn Cancellation>,
    ) -> Result<Execution, LifecycleError> {
        let result = self.drain_passes(id, owned, cancellation).await;
        if result.is_ok() {
            return result;
        }
        // Persistence/backend failures still own every mutation accepted before
        // settlement. Keep the primary failure while joining repeated snapshots.
        loop {
            let _ = self.join_mutations().await;
            let mut state = self.inner.state.lock().expect("session poisoned");
            if !state.mutations.is_empty() {
                continue;
            }
            reap_tasks(&mut state);
            if state.drain.as_ref().is_some_and(|drain| drain.id == id) {
                state.drain = None;
                state.drain_state = DrainState::Idle;
                self.inner.backend.drain_state_changed(DrainState::Idle);
                while let Some(request) = state.scheduled.pop_front() {
                    request.completion.finish(Err(canceled()));
                }
            }
            return result;
        }
    }
    async fn drain_passes(
        &self,
        id: u64,
        owned: Option<Arc<OwnedReservation>>,
        cancellation: Arc<dyn Cancellation>,
    ) -> Result<Execution, LifecycleError> {
        let started = match &owned {
            Some(owned) => owned.startup.wait().await?,
            None => true,
        };
        let mut completed = Execution::succeeded("");
        loop {
            let pass = async {
                if !started || cancellation.is_cancelled() {
                    return Ok(DrainPass {
                        execution: Execution::canceled(),
                        result_reference: ExecutionResultReference::Missing,
                    });
                }
                if owned
                    .as_ref()
                    .is_some_and(|owned| owned.cancellation.is_cancelled())
                {
                    return Ok(DrainPass {
                        execution: Execution::canceled(),
                        result_reference: ExecutionResultReference::Missing,
                    });
                }
                self.run_scheduled(cancellation.clone()).await;
                self.inner.backend.pass(cancellation.as_ref(), self).await
            }
            .await;
            let mut pass = match (pass, &owned) {
                (Ok(pass), _) => {
                    if let Some(owned) = &owned {
                        if let ExecutionResultReference::Captured(_) = pass.result_reference {
                            *owned
                                .result_reference
                                .lock()
                                .expect("execution result reference poisoned") =
                                pass.result_reference;
                        }
                    }
                    pass.execution
                }
                (Err(error), Some(_)) if error.is_cancelled() => Execution::canceled(),
                (Err(error), Some(_)) => Execution::failed(error.to_string()),
                (Err(error), None) => return Err(error),
            };
            if let Some(owned) = &owned {
                if (!started
                    || owned.cancellation.is_cancelled()
                    || self.inner.interrupting.load(Ordering::Acquire))
                    && !self.inner.suspending.load(Ordering::Acquire)
                {
                    let input_id = owned
                        .admission
                        .lock()
                        .expect("reservation poisoned")
                        .as_ref()
                        .map(|admission| admission.input.id.clone());
                    if let Some(input_id) = input_id {
                        if let Err(error) = self.inner.backend.cancel_pending_input(input_id).await
                        {
                            pass = Execution::failed(error.to_string());
                        }
                    }
                }
            }
            if pass.status != ExecutionStatus::Succeeded || !pass.output.is_empty() {
                completed = pass.clone();
            }
            loop {
                if let Err(error) = self.join_mutations().await {
                    if pass.status == ExecutionStatus::Succeeded {
                        pass = Execution::failed(error.to_string());
                        completed = pass.clone();
                    }
                }
                // Recheck durable input after the mutation await. A send may
                // have woken this drain while its terminal barrier was waiting.
                let pending = self.inner.backend.has_pending_inputs().await?;
                let mut state = self.inner.state.lock().expect("session poisoned");
                // This is the mutation admission gate too. A snapshot accepted
                // while joining the previous one must finish before settlement;
                // admission after this final check belongs to the next boundary.
                if !state.mutations.is_empty() {
                    continue;
                }
                if pass.status == ExecutionStatus::Succeeded
                    && !cancellation.is_cancelled()
                    && (!state.scheduled.is_empty() || state.wake || pending)
                {
                    state.wake = false;
                    break;
                }
                reap_tasks(&mut state);
                if state.drain.as_ref().is_some_and(|drain| drain.id == id) {
                    state.drain = None;
                    state.drain_state = DrainState::Idle;
                    self.inner.backend.drain_state_changed(DrainState::Idle);
                    while let Some(request) = state.scheduled.pop_front() {
                        request.completion.finish(Err(canceled()));
                    }
                }
                return Ok(completed);
            }
        }
    }
}

impl<B: DrainBackend> DrainWork for AgentSession<B> {
    fn has_scheduled(&self) -> bool {
        !self
            .inner
            .state
            .lock()
            .expect("session poisoned")
            .scheduled
            .is_empty()
    }
    fn join_mutations(&self) -> BoxFuture<'_, Result<(), LifecycleError>> {
        // The snapshot is linearized against admission. Mutations accepted
        // later belong to the next boundary and set wake on an active drain.
        let snapshot = self
            .inner
            .state
            .lock()
            .expect("session poisoned")
            .mutations
            .clone();
        Box::pin(async move {
            let mut result = Ok(());
            for mutation in &snapshot {
                if let Err(error) = mutation.join().await {
                    if result.is_ok() {
                        result = Err(error);
                    }
                }
            }
            let mut state = self.inner.state.lock().expect("session poisoned");
            state.mutations.retain(|mutation| {
                !snapshot
                    .iter()
                    .any(|taken| Arc::ptr_eq(&taken.completion, &mutation.completion))
            });
            result
        })
    }
}
struct IncomingRegistration<B: DrainBackend> {
    session: AgentSession<B>,
    id: u64,
}
impl<B: DrainBackend> Drop for IncomingRegistration<B> {
    fn drop(&mut self) {
        let mut state = self.session.inner.state.lock().expect("session poisoned");
        if state
            .incoming
            .as_ref()
            .is_some_and(|wait| wait.id == self.id)
        {
            state.incoming = None;
        }
    }
}

#[cfg(test)]
mod task_reaper_tests {
    use super::*;

    #[derive(Default)]
    struct Backend {
        closed: AtomicBool,
    }

    impl DrainBackend for Backend {
        type Selection = ();
        fn admit(
            &self,
            _: Vec<ConversationPart>,
            _: String,
            _: Delivery,
            _: InputSource,
        ) -> BoxFuture<'_, Result<Admission, LifecycleError>> {
            unreachable!()
        }
        fn publish_admission<'a>(
            &'a self,
            _: &'a Admission,
            _: &'a dyn Cancellation,
        ) -> BoxFuture<'a, Result<(), LifecycleError>> {
            unreachable!()
        }
        fn has_pending_inputs(&self) -> BoxFuture<'_, Result<bool, LifecycleError>> {
            unreachable!()
        }
        fn cancel_pending_input(&self, _: String) -> BoxFuture<'_, Result<(), LifecycleError>> {
            unreachable!()
        }
        fn update_selection(&self, _: ()) -> Result<(), LifecycleError> {
            unreachable!()
        }
        fn drain_state_changed(&self, _: DrainState) {
            unreachable!()
        }
        fn pass<'a>(
            &'a self,
            _: &'a dyn Cancellation,
            _: &'a dyn DrainWork,
        ) -> BoxFuture<'a, Result<DrainPass, LifecycleError>> {
            unreachable!()
        }
        fn prepare_scheduled<'a>(
            &'a self,
            _: &'a dyn Cancellation,
        ) -> BoxFuture<'a, Result<(), LifecycleError>> {
            unreachable!()
        }
        fn execution_started(&self) -> BoxFuture<'_, Result<(), LifecycleError>> {
            unreachable!()
        }
        fn execution_finished<'a>(
            &'a self,
            _: &'a Execution,
        ) -> BoxFuture<'a, Result<(), LifecycleError>> {
            unreachable!()
        }
        fn execution_started_correlated(
            &self,
            _: &str,
        ) -> BoxFuture<'_, Result<(), LifecycleError>> {
            Box::pin(async { panic!("not a persistent backend") })
        }
        fn execution_finished_correlated<'a>(
            &'a self,
            _: &'a Execution,
            _: &'a str,
        ) -> BoxFuture<'a, Result<(), LifecycleError>> {
            Box::pin(async { panic!("not a persistent backend") })
        }
        fn cancel_execution_input(&self, _: &str) -> BoxFuture<'_, Result<(), LifecycleError>> {
            Box::pin(async { panic!("not a persistent backend") })
        }
        fn execution_watermark(&self) -> BoxFuture<'_, Result<i64, LifecycleError>> {
            Box::pin(async { panic!("not a persistent backend") })
        }
        fn restore_execution<'a>(
            &'a self,
            _: &'a parrot_domain::values::agent_state::ExecutionTerminal,
        ) -> BoxFuture<'a, Result<Execution, LifecycleError>> {
            Box::pin(async { panic!("not a persistent backend") })
        }
        fn capture_execution<'a>(
            &'a self,
            _: &'a parrot_domain::values::agent_state::ExecutionCheckpoint,
            _: &'a mut Execution,
            _: ExecutionResultReference,
        ) -> BoxFuture<
            'a,
            Result<parrot_domain::values::agent_state::ExecutionTerminal, LifecycleError>,
        > {
            Box::pin(async { panic!("not a persistent backend") })
        }
        fn suspend_execution(&self) {
            panic!("not a persistent backend")
        }
        fn close(&self) -> BoxFuture<'_, Result<(), LifecycleError>> {
            self.closed.store(true, Ordering::SeqCst);
            Box::pin(async { Ok(()) })
        }
    }

    #[tokio::test]
    async fn unpolled_mutation_task_drop_settles_both_completions() {
        let operation = Completion::new();
        let lifetime = Completion::new();
        let completion = MutationCompletion {
            operation: operation.clone(),
            lifetime: lifetime.clone(),
            settled: false,
        };
        let task = async move { completion.finish(Ok(())) };
        drop(task);
        let error =
            LifecycleError::Failed("owned agent mutation task ended before completion".into());
        assert_eq!(operation.wait().await, Err(error.clone()));
        assert_eq!(lifetime.wait().await, Err(error));
    }

    #[tokio::test]
    async fn completed_operations_are_joined_and_reaped_on_next_admission() {
        let session = AgentSession::new(Arc::new(Backend::default()), Arc::new(Signal::new()));
        for _ in 0..128 {
            let (done, completion) = tokio::sync::oneshot::channel();
            {
                let mut state = session.inner.state.lock().unwrap();
                spawn_tracked(&mut state, async move {
                    done.send(()).unwrap();
                });
                assert_eq!(state.tasks.len(), 1);
            }
            completion.await.unwrap();
        }
        {
            let mut state = session.inner.state.lock().unwrap();
            reap_tasks(&mut state);
            assert!(state.tasks.is_empty());
            assert!(state.task_failure.is_none());
        }
        session.close().await.unwrap();
    }

    #[tokio::test]
    async fn reaped_task_failure_is_retained_until_close_and_backend_still_closes() {
        let backend = Arc::new(Backend::default());
        let session = AgentSession::new(backend.clone(), Arc::new(Signal::new()));
        let task = {
            let mut state = session.inner.state.lock().unwrap();
            spawn_tracked(&mut state, async { panic!("tracked test panic") })
        };
        // The task itself is the completion barrier; do not join it here, since
        // the synchronous reaper must observe and retain its JoinError.
        while !task.lock().await.as_ref().unwrap().is_finished() {
            tokio::task::yield_now().await;
        }
        {
            let mut state = session.inner.state.lock().unwrap();
            reap_tasks(&mut state);
            assert!(state.tasks.is_empty());
            assert!(
                state
                    .task_failure
                    .as_ref()
                    .unwrap()
                    .to_string()
                    .contains("tracked test panic")
            );
        }
        let error = session.close().await.unwrap_err();
        assert!(error.to_string().contains("tracked test panic"));
        assert!(backend.closed.load(Ordering::SeqCst));
    }
}
