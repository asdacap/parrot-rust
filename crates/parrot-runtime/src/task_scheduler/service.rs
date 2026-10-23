use super::{
    ForkHistoryMode, SchedulerConfig, TaskDiagnostic, TaskRunner, TaskSession,
    apply_visibility_changes, has_same_definition, has_same_visibility,
    notifier::{Notification, TaskNotifier},
    ports::{
        CapturedSelection, RunCancellation, TaskRunProgress, TaskWorkerLease, render, state_error,
    },
    status_name,
};
use crate::{
    agent_turn::{lifecycle::Signal, services::TurnSelection},
    children::{
        ChildError, ChildResult, CompletionDelivery, HistorySource, LaunchRequest, Operation, Scope,
    },
};
use parrot_domain::{
    contracts::{
        agent_tasks::TaskArtifactParser,
        async_ops::{BoxFuture, Cancellation, OwnedWork},
        security::RestrictableAccessSnapshot,
        storage::Committed,
    },
    protocol::messages::parrot::{
        AgentTaskProgressNode, AgentTaskProgressSnapshot, AgentTaskProgressStatus, Delivery, Event,
        ToolSnapshot,
    },
    values::{
        agent_state::{
            AcceptedTaskCall, AgentStateUpdate, ForkBoundarySnapshot, NotificationCheckpoint,
            SelectionSnapshot, TaskEntrySnapshot, TaskForkModeSnapshot, TaskGraphSnapshot,
            TaskNotificationSnapshot, TaskResultClaim, TaskRunnerCheckpoint, TaskStopCheckpoint,
        },
        agent_tasks::{ExecutionStatus, Task, TaskCallIdentity},
        identifiers::Identifier,
    },
};
use parrot_storage::{
    artifacts::output_blobs::is_tool_output_oversized,
    conversation::{HistoryForkBoundary, HistoryForkSelection},
    events::{ConversationPart, NeverCancelled},
};
use protobuf::Message;
use std::{
    collections::{HashMap, HashSet, VecDeque},
    sync::{Arc, Mutex, Weak},
    time::Instant,
};

pub const PROGRESS_KIND: &str = "agent_task_progress";
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TaskDetail {
    pub task: Task,
    pub agent_name: Option<String>,
}
struct Entry<S: RestrictableAccessSnapshot + 'static> {
    task: Task,
    diagnostic_id: String,
    selection: CapturedSelection,
    boundary: HistoryForkBoundary,
    fork_mode: ForkHistoryMode,
    worker_session_id: Option<String>,
    runner: Option<TaskRunnerCheckpoint>,
    pending_stop: Option<TaskStopCheckpoint>,
    terminal_notification: Option<NotificationCheckpoint>,
    scope: Option<Arc<Scope<S>>>,
    run: Option<Arc<Run>>,
    last_execution: Option<Operation<()>>,
    stopping_run: Option<Arc<Run>>,
}
struct Run {
    cancellation: Arc<RunCancellation>,
    started: Instant,
    generation_id: String,
    ready: Arc<Readiness>,
}
struct State<S: RestrictableAccessSnapshot + 'static> {
    entries: Vec<Entry<S>>,
    executions: Vec<Operation<()>>,
    execution_failure: Option<ChildError>,
    revision: u64,
    complete: bool,
    all_resolved: bool,
    settling: bool,
    suspended: bool,
    execution_active: bool,
    notifications_active: bool,
    persistence_failure: Option<ChildError>,
    notifications: Vec<TaskNotificationSnapshot>,
    in_flight: HashSet<String>,
    stopping: HashSet<String>,
    completion_notification: Option<NotificationCheckpoint>,
    stop_readiness: HashMap<String, Arc<Readiness>>,
    reservations: Vec<Arc<Readiness>>,
    accepted_calls: Vec<AcceptedTaskCall>,
}
#[derive(Default)]
struct Readiness {
    signal: Signal,
    result: Mutex<Option<ChildResult<()>>>,
}
impl Readiness {
    fn finish(&self, result: ChildResult<()>) {
        let mut saved = self.result.lock().expect("task readiness poisoned");
        if saved.is_none() {
            *saved = Some(result);
            self.signal.cancel();
        }
    }
    fn is_ready(&self) -> bool {
        self.result
            .lock()
            .expect("task readiness poisoned")
            .is_some()
    }
    async fn wait(&self) -> ChildResult<()> {
        self.signal.cancelled().await;
        self.result
            .lock()
            .expect("task readiness poisoned")
            .clone()
            .expect("ready result")
    }
}
#[derive(Default)]
struct WorkerGate {
    gate: Arc<tokio::sync::Mutex<()>>,
    changed: Arc<tokio::sync::Notify>,
}
struct RunProgress<S: RestrictableAccessSnapshot + 'static, P: TaskArtifactParser + 'static> {
    scheduler: Weak<TaskScheduler<S, P>>,
    index: usize,
    run: Arc<Run>,
    owner_session_id: String,
}
impl<S: RestrictableAccessSnapshot + 'static, P: TaskArtifactParser + 'static> TaskRunProgress
    for RunProgress<S, P>
{
    fn claim(&self, prompt_id: &str) -> TaskResultClaim {
        TaskResultClaim {
            owner_session_id: self.owner_session_id.clone(),
            claim_id: prompt_id.to_owned(),
        }
    }
    fn acquire<'a>(
        &'a self,
        checkpoint: &'a TaskRunnerCheckpoint,
        session: &'a dyn TaskSession,
    ) -> BoxFuture<'a, ChildResult<TaskWorkerLease>> {
        Box::pin(async move {
            let scheduler = self.scheduler.upgrade().ok_or(ChildError::Cancelled)?;
            let slot = scheduler.worker_gate(&session.identity().session.session_id);
            scheduler
                .reserve_worker(slot, checkpoint, session, &self.run)
                .await
        })
    }
    fn save(&self, checkpoint: TaskRunnerCheckpoint) -> BoxFuture<'_, ChildResult<()>> {
        Box::pin(async move {
            let scheduler = self.scheduler.upgrade().ok_or(ChildError::Cancelled)?;
            let mut state = scheduler.gate.lock().await;
            scheduler.ensure_healthy(&state)?;
            if state.suspended
                || !state.entries[self.index]
                    .run
                    .as_ref()
                    .is_some_and(|run| Arc::ptr_eq(run, &self.run))
                || checkpoint.generation_id != self.run.generation_id
            {
                return Err(ChildError::Cancelled);
            }
            state.entries[self.index].runner = Some(checkpoint);
            scheduler.save_graph(&mut state).await
        })
    }
    fn reserve(&self) {
        self.run.ready.finish(Ok(()));
    }
}
enum UpdateOrigin {
    Direct,
    Tool(TaskCallIdentity),
}
#[derive(Default)]
struct Admission {
    tail: Option<Operation<()>>,
    settlement: Option<Operation<()>>,
    freeze: Option<Operation<()>>,
    suspension: Option<Operation<()>>,
}
/// One graph per actual T48 scope. Composition registers this exact Arc as a service
/// and work owner; it never stores a strong reference back to its owning scope.
pub struct TaskScheduler<S: RestrictableAccessSnapshot + 'static, P: TaskArtifactParser + 'static> {
    config: SchedulerConfig<S, P>,
    runner: TaskRunner<P>,
    notifier: Arc<TaskNotifier>,
    stop: Arc<Signal>,
    lifetime: Arc<dyn Cancellation>,
    gate: tokio::sync::Mutex<State<S>>,
    admission: Mutex<Admission>,
    worker_gates: Mutex<HashMap<String, Arc<WorkerGate>>>,
}
impl<S: RestrictableAccessSnapshot + 'static, P: TaskArtifactParser + 'static> TaskScheduler<S, P> {
    pub fn work_owner(self: &Arc<Self>) -> Arc<dyn OwnedWork<Error = ChildError>> {
        Arc::new(SchedulerWork(self.clone()))
    }
    pub async fn new(config: SchedulerConfig<S, P>) -> ChildResult<Arc<Self>> {
        if !Arc::ptr_eq(&config.owner_identity, config.owner_session.identity())
            || config.history.agent_session_id() != config.owner_identity.session.session_id
        {
            return Err(ChildError::registry(
                "task scheduler session/history does not match owner identity",
            ));
        }
        let saved = config
            .state
            .load()
            .await
            .map_err(state_error)?
            .ok_or_else(|| ChildError::registry("task scheduler state is not initialized"))?;
        if saved.owner_session_id != config.owner_identity.session.session_id {
            return Err(ChildError::registry(
                "task scheduler snapshot does not match owner identity",
            ));
        }
        if !saved.tasks.entries.is_empty() {
            config
                .parser
                .validate_merged_graph(
                    &saved
                        .tasks
                        .entries
                        .iter()
                        .map(|entry| entry.task.clone())
                        .collect::<Vec<_>>(),
                    "tasks",
                )
                .map_err(|error| ChildError::failure("argument", error.to_string()))?;
        }
        let mut notice_ids = HashSet::new();
        for notice in &saved.tasks.notifications {
            if notice.message_id.is_empty() || !notice_ids.insert(&notice.message_id) {
                return Err(ChildError::registry(
                    "task notification identities are invalid",
                ));
            }
        }
        let mut accepted_ids = HashSet::new();
        let accepted_batch = saved
            .tasks
            .accepted_calls
            .first()
            .map(|receipt| receipt.identity.assistant_sequence);
        for receipt in &saved.tasks.accepted_calls {
            if Some(receipt.identity.assistant_sequence) != accepted_batch
                || !accepted_ids.insert(&receipt.identity.call_id)
            {
                return Err(ChildError::registry(
                    "accepted task call receipts do not belong to one unique batch",
                ));
            }
        }
        let mut entries = Vec::new();
        for entry in saved.tasks.entries {
            if entry.task.state == ExecutionStatus::Running && entry.runner.is_none() {
                return Err(ChildError::registry(
                    "running task has no saved runner checkpoint",
                ));
            }
            if let Some(runner) = &entry.runner {
                if runner.original_task.name != entry.task.name
                    || runner.current_task.name != entry.task.name
                    || runner.generation_id.is_empty()
                    || runner.prompt_message_id.is_empty()
                    || runner.attempt == 0
                    || runner
                        .outcome
                        .as_ref()
                        .is_some_and(|outcome| outcome.name != entry.task.name)
                {
                    return Err(ChildError::registry(
                        "task runner checkpoint does not match its task",
                    ));
                }
            }
            if let Some(reference) = entry
                .runner
                .as_ref()
                .and_then(|runner| runner.response_reference.as_ref())
            {
                if entry.worker_session_id.as_deref() != Some(reference.worker_session_id.as_str())
                    || entry.runner.as_ref().is_some_and(|runner| {
                        runner.prompt_message_id != reference.initial_message_id
                    })
                {
                    return Err(ChildError::registry(
                        "task response reference does not match its worker prompt",
                    ));
                }
            }
            if let Some(stop) = &entry.pending_stop {
                if entry.worker_session_id.as_deref() != Some(stop.worker_session_id.as_str())
                    || !saved.tasks.notifications.iter().any(|notice| {
                        notice.message_id == stop.notice_message_id
                            && notice.target_session_id == stop.worker_session_id
                            && !notice.wake
                    })
                {
                    return Err(ChildError::registry(
                        "task stop intent has no matching worker notification",
                    ));
                }
            }
            entries.push(Entry {
                task: entry.task,
                diagnostic_id: entry.diagnostic_id,
                selection: CapturedSelection {
                    profile_id: entry.selection.profile_id,
                    model: config.router.resolve(&entry.selection.model_selector)?,
                },
                boundary: match entry.fork_boundary {
                    ForkBoundarySnapshot::BeforeToolBatch {
                        assistant_sequence,
                        tool_call_id,
                    } => HistoryForkBoundary::BeforeToolBatch {
                        assistant_sequence,
                        tool_call_id,
                    },
                    ForkBoundarySnapshot::AfterCompletedHistory => {
                        HistoryForkBoundary::AfterCompletedHistory
                    }
                    ForkBoundarySnapshot::AfterSafeHistoryPrefix => {
                        HistoryForkBoundary::AfterSafeHistoryPrefix
                    }
                },
                fork_mode: match entry.fork_mode {
                    TaskForkModeSnapshot::Dependency => ForkHistoryMode::Dependency,
                    TaskForkModeSnapshot::Parent => ForkHistoryMode::Parent,
                    TaskForkModeSnapshot::Empty => ForkHistoryMode::Empty,
                },
                worker_session_id: entry.worker_session_id,
                runner: entry.runner,
                pending_stop: entry.pending_stop,
                terminal_notification: entry.terminal_notification,
                scope: None,
                run: None,
                last_execution: None,
                stopping_run: None,
            });
        }
        let fresh = entries.is_empty() && saved.tasks.notifications.is_empty();
        let stop = Arc::new(Signal::new());
        let lifetime = Arc::new(RunCancellation {
            lifetime: config.lifetime.clone(),
            stop: stop.clone(),
        });
        Ok(Arc::new(Self {
            runner: TaskRunner::new(config.configuration.clone(), config.parser.clone()),
            notifier: TaskNotifier::new(config.diagnostics.clone(), config.lifetime.clone()),
            config,
            stop,
            lifetime,
            gate: tokio::sync::Mutex::new(State {
                entries,
                executions: Vec::new(),
                execution_failure: None,
                revision: saved.tasks.revision,
                complete: saved.tasks.complete,
                all_resolved: saved.tasks.all_resolved,
                settling: false,
                suspended: false,
                execution_active: fresh,
                notifications_active: fresh,
                persistence_failure: None,
                notifications: saved.tasks.notifications,
                in_flight: HashSet::new(),
                stopping: HashSet::new(),
                completion_notification: saved.tasks.completion_notification,
                stop_readiness: HashMap::new(),
                reservations: Vec::new(),
                accepted_calls: saved.tasks.accepted_calls,
            }),
            admission: Mutex::new(Admission::default()),
            worker_gates: Mutex::new(HashMap::new()),
        }))
    }
    /// Admission and FIFO reservation are synchronous. Dropping the returned
    /// observer cannot abandon the update, publication, or any spawned execution.
    pub fn set_tasks(
        self: &Arc<Self>,
        tasks: Vec<Task>,
        selection: TurnSelection<S>,
        boundary: HistoryForkBoundary,
    ) -> ChildResult<Operation<()>> {
        let update = self.set_tasks_origin(tasks, selection, boundary, UpdateOrigin::Direct)?;
        Ok(Operation::start(
            async move { update.join().await.map(|_| ()) },
        ))
    }
    /// Deduplicates one tool call only within the current durable assistant batch.
    pub fn set_tool_tasks(
        self: &Arc<Self>,
        tasks: Vec<Task>,
        selection: TurnSelection<S>,
        boundary: HistoryForkBoundary,
        identity: TaskCallIdentity,
    ) -> ChildResult<Operation<usize>> {
        self.set_tasks_origin(tasks, selection, boundary, UpdateOrigin::Tool(identity))
    }
    pub async fn lookup_accepted(&self, identity: &TaskCallIdentity) -> ChildResult<Option<usize>> {
        self.admitted().await;
        let state = self.gate.lock().await;
        self.ensure_healthy(&state)?;
        Self::find_receipt(&state, identity)
    }
    fn find_receipt(state: &State<S>, identity: &TaskCallIdentity) -> ChildResult<Option<usize>> {
        if state.accepted_calls.first().is_some_and(|receipt| {
            identity.assistant_sequence < receipt.identity.assistant_sequence
        }) {
            return Err(ChildError::registry(
                "task update call belongs to an older settled assistant batch",
            ));
        }
        state
            .accepted_calls
            .iter()
            .find(|receipt| receipt.identity == *identity)
            .map(|receipt| {
                usize::try_from(receipt.accepted_count)
                    .map_err(|_| ChildError::registry("accepted task count exceeds this platform"))
            })
            .transpose()
    }
    fn set_tasks_origin(
        self: &Arc<Self>,
        tasks: Vec<Task>,
        selection: TurnSelection<S>,
        boundary: HistoryForkBoundary,
        origin: UpdateOrigin,
    ) -> ChildResult<Operation<usize>> {
        let mut admission = self.admission.lock().expect("task admission poisoned");
        if admission.settlement.is_some() || admission.freeze.is_some() {
            return Err(shutting_down());
        }
        let previous = admission.tail.clone();
        let scheduler = self.clone();
        let operation = Operation::start(async move {
            if let Some(previous) = previous {
                let _ = previous.join().await;
            }
            scheduler.owner()?;
            let mut state = scheduler.gate.lock().await;
            scheduler.ensure_healthy(&state)?;
            if let UpdateOrigin::Tool(identity) = &origin {
                if let Some(count) = Self::find_receipt(&state, identity)? {
                    return Ok(count);
                }
            }
            let count = tasks.len();
            let names: HashSet<_> = tasks.iter().map(|task| task.name.as_str()).collect();
            let mut merged: Vec<_> = state
                .entries
                .iter()
                .filter(|entry| !names.contains(entry.task.name.as_str()))
                .map(|entry| entry.task.clone())
                .collect();
            merged.extend(tasks.iter().cloned());
            scheduler
                .config
                .parser
                .validate_merged_graph(&merged, "tasks")
                .map_err(|error| ChildError::failure("argument", error.to_string()))?;
            let before: HashMap<_, _> = state
                .entries
                .iter()
                .enumerate()
                .map(|(index, entry)| (entry.task.name.clone(), effective(&state, index)))
                .collect();
            for mut task in tasks {
                if let Some(index) = find(&state, &task.name) {
                    scheduler.apply(&mut state, index, task)?;
                } else {
                    if task.state == ExecutionStatus::Running {
                        task.state = ExecutionStatus::Pending;
                    }
                    let boundary = match &boundary {
                        HistoryForkBoundary::AfterCompletedHistory => {
                            HistoryForkBoundary::AfterSafeHistoryPrefix
                        }
                        boundary => boundary.clone(),
                    };
                    let id = Identifier::event_id()
                        .map_err(|error| ChildError::failure("unexpected", error.to_string()))?;
                    state.entries.push(Entry {
                        task,
                        diagnostic_id: format!("task-{id}"),
                        selection: CapturedSelection {
                            profile_id: selection.profile.id.clone(),
                            model: selection.model.clone(),
                        },
                        boundary,
                        fork_mode: scheduler.config.configuration.fork_history_mode,
                        worker_session_id: None,
                        runner: None,
                        pending_stop: None,
                        terminal_notification: None,
                        scope: None,
                        run: None,
                        last_execution: None,
                        stopping_run: None,
                    });
                }
            }
            if let UpdateOrigin::Tool(identity) = origin {
                state.accepted_calls.retain(|receipt| {
                    receipt.identity.assistant_sequence == identity.assistant_sequence
                });
                state.accepted_calls.push(AcceptedTaskCall {
                    identity,
                    accepted_count: count as u64,
                });
            }
            scheduler.save_graph(&mut state).await?;
            scheduler.reconcile(&mut state).await?;
            let changed: Vec<_> = state
                .entries
                .iter()
                .enumerate()
                .filter_map(|(index, entry)| {
                    (before.get(&entry.task.name) != Some(&effective(&state, index)))
                        .then_some(index)
                })
                .collect();
            if !changed.is_empty() {
                let items = scheduler.items(&state, &changed)?;
                scheduler.enqueue(
                    &mut state,
                    "agent-task.notification-updated",
                    &[("items", &items)],
                    false,
                )?;
            }
            scheduler.notify_completion(&mut state)?;
            let mut result = scheduler.publish(&mut state).await;
            first(&mut result, scheduler.kick_stops(&mut state), |error| {
                scheduler.failure_diagnostic("stop", error)
            });
            first(
                &mut result,
                scheduler.kick_notifications(&mut state),
                |error| scheduler.failure_diagnostic("delivery", error),
            );
            result.map(|()| count)
        });
        let tail = operation.clone();
        admission.tail = Some(Operation::start(
            async move { tail.join().await.map(|_| ()) },
        ));
        Ok(operation)
    }
    async fn admitted(&self) {
        let previous = self
            .admission
            .lock()
            .expect("task admission poisoned")
            .tail
            .clone();
        if let Some(previous) = previous {
            let _ = previous.join().await;
        }
    }
    pub async fn snapshot(&self) -> Vec<Task> {
        self.admitted().await;
        let state = self.gate.lock().await;
        state
            .entries
            .iter()
            .enumerate()
            .map(|(index, entry)| {
                let mut task = entry.task.clone();
                task.state = effective(&state, index);
                task
            })
            .collect()
    }
    pub async fn capture_detail(&self, name: &str) -> Option<TaskDetail> {
        self.admitted().await;
        let state = self.gate.lock().await;
        find(&state, name).map(|index| {
            let entry = &state.entries[index];
            let mut task = entry.task.clone();
            task.state = effective(&state, index);
            TaskDetail {
                task,
                agent_name: entry
                    .scope
                    .as_ref()
                    .map(|scope| scope.identity().session.name.clone()),
            }
        })
    }
    pub fn apply_visibility_changes(
        self: &Arc<Self>,
        previous: Vec<Task>,
        incoming: Vec<Task>,
    ) -> Operation<()> {
        let mut admission = self.admission.lock().expect("task admission poisoned");
        if admission.settlement.is_some() || admission.freeze.is_some() {
            return Operation::start(async { Ok(()) });
        }
        let prior = admission.tail.clone();
        let scheduler = self.clone();
        let operation = Operation::start(async move {
            if let Some(prior) = prior {
                let _ = prior.join().await;
            }
            scheduler.owner()?;
            let mut state = scheduler.gate.lock().await;
            scheduler.ensure_healthy(&state)?;
            let mut changed = false;
            for before in previous {
                let Some(after) = incoming.iter().find(|task| task.name == before.name) else {
                    continue;
                };
                if !has_same_definition(&before, after) || has_same_visibility(&before, after) {
                    continue;
                }
                let Some(index) = find(&state, &after.name) else {
                    continue;
                };
                if !has_same_definition(&state.entries[index].task, after) {
                    continue;
                }
                state.entries[index].task =
                    apply_visibility_changes(&state.entries[index].task, &before, after);
                changed = true;
            }
            if changed {
                scheduler.publish(&mut state).await?;
            }
            Ok(())
        });
        admission.tail = Some(operation.clone());
        operation
    }
    /// Freezes admissions, launches and notices without closing any worker session.
    /// User-session shutdown freezes every graph before suspending shared workers.
    pub fn freeze_for_suspension(self: &Arc<Self>) -> Operation<()> {
        let mut admission = self.admission.lock().expect("task admission poisoned");
        if let Some(operation) = &admission.freeze {
            return operation.clone();
        }
        let previous = admission.tail.clone();
        let scheduler = self.clone();
        let operation = Operation::start(async move {
            if let Some(previous) = previous {
                let _ = previous.join().await;
            }
            let mut state = scheduler.gate.lock().await;
            state.suspended = true;
            state.execution_active = false;
            state.notifications_active = false;
            scheduler.notifier.suspend();
            Ok(())
        });
        admission.freeze = Some(operation.clone());
        operation
    }
    /// Freezes the graph and joins workers without canceling durable task intent.
    pub fn suspend(self: &Arc<Self>) -> Operation<()> {
        let freeze = self.freeze_for_suspension();
        let mut admission = self.admission.lock().expect("task admission poisoned");
        if let Some(operation) = &admission.suspension {
            return operation.clone();
        }
        let scheduler = self.clone();
        let operation = Operation::start(async move {
            freeze.join().await?;
            let (sessions, runs, executions) = {
                let state = scheduler.gate.lock().await;
                let sessions = state
                    .entries
                    .iter()
                    .filter(|entry| entry.run.is_some() || entry.stopping_run.is_some())
                    .filter_map(|entry| entry.scope.as_ref())
                    .map(|scope| scope.get_service::<dyn TaskSession>())
                    .collect::<ChildResult<Vec<_>>>()?;
                let runs = state
                    .entries
                    .iter()
                    .filter_map(|entry| entry.run.clone())
                    .collect::<Vec<_>>();
                (sessions, runs, state.executions.clone())
            };
            let mut result = Ok(());
            for session in sessions {
                first(&mut result, session.suspend().await, |error| {
                    scheduler.failure_diagnostic("suspension", error)
                });
            }
            for run in runs {
                run.cancellation.stop.cancel();
                run.ready.finish(Err(ChildError::Cancelled));
            }
            for execution in executions {
                first(&mut result, execution.join().await, |error| {
                    scheduler.failure_diagnostic("execution", error)
                });
            }
            first(
                &mut result,
                scheduler.notifier.suspend().join().await,
                |error| scheduler.failure_diagnostic("suspension_notifier", error),
            );
            result
        });
        admission.suspension = Some(operation.clone());
        operation
    }
    pub fn settle(self: &Arc<Self>) -> Operation<()> {
        self.notifier.begin_settlement();
        let mut admission = self.admission.lock().expect("task admission poisoned");
        if let Some(settlement) = &admission.settlement {
            return settlement.clone();
        }
        let previous = admission.tail.clone();
        let suspended = admission.suspension.clone();
        let scheduler = self.clone();
        let operation = Operation::start(async move {
            if let Some(suspended) = suspended {
                return suspended.join().await;
            }
            if let Some(previous) = previous {
                let _ = previous.join().await;
            }
            let mut result = Ok(());
            let executions = {
                let mut state = scheduler.gate.lock().await;
                state.settling = true;
                for index in 0..state.entries.len() {
                    if state.entries[index].owns_attempt() {
                        first(
                            &mut result,
                            scheduler.stop_run(&mut state, index, "agent-task.stop-shutdown"),
                            |error| scheduler.failure_diagnostic("settlement_stop", error),
                        );
                    }
                    if matches!(
                        effective(&state, index),
                        ExecutionStatus::Pending
                            | ExecutionStatus::Running
                            | ExecutionStatus::Failed
                    ) {
                        state.entries[index].task.state = ExecutionStatus::Canceled;
                    }
                }
                if !state.entries.is_empty() {
                    first(&mut result, scheduler.publish(&mut state).await, |error| {
                        scheduler.failure_diagnostic("settlement_publish", error)
                    });
                }
                first(&mut result, scheduler.kick_stops(&mut state), |error| {
                    scheduler.failure_diagnostic("settlement_stop", error)
                });
                first(
                    &mut result,
                    scheduler.kick_notifications(&mut state),
                    |error| scheduler.failure_diagnostic("settlement_notifier", error),
                );
                if let Some(error) = state.execution_failure.take() {
                    first(&mut result, Err(error), |error| {
                        scheduler.failure_diagnostic("execution", error)
                    });
                }
                state.executions.clone()
            };
            for execution in executions {
                first(&mut result, execution.join().await, |error| {
                    scheduler.failure_diagnostic("execution", error)
                });
            }
            scheduler.stop.cancel();
            first(
                &mut result,
                scheduler.notifier.settle().join().await,
                |error| scheduler.failure_diagnostic("settlement_notifier", error),
            );
            result
        });
        admission.settlement = Some(operation.clone());
        operation
    }
    fn apply(&self, state: &mut State<S>, index: usize, mut incoming: Task) -> ChildResult<()> {
        let stored = state.entries[index].task.clone();
        if state.entries[index].owns_attempt()
            && incoming.state == ExecutionStatus::Running
            && has_same_definition(&stored, &incoming)
        {
            state.entries[index].task.hidden = incoming.hidden;
            state.entries[index].task.payload = incoming.payload;
            return Ok(());
        }
        if incoming.state == ExecutionStatus::Running {
            incoming.state = ExecutionStatus::Pending;
        }
        if state.entries[index].owns_attempt() {
            self.stop_run(
                state,
                index,
                match incoming.state {
                    ExecutionStatus::Canceled => "agent-task.stop-canceled",
                    ExecutionStatus::Succeeded => "agent-task.stop-succeeded",
                    ExecutionStatus::Failed => "agent-task.stop-failed",
                    _ => "agent-task.stop-restarted",
                },
            )?;
        }
        if stored.state == ExecutionStatus::Succeeded && incoming.state == ExecutionStatus::Pending
        {
            self.reset_dependents(state, &stored.name)?;
        }
        state.entries[index].runner = None;
        state.entries[index].task = incoming;
        Ok(())
    }
    fn reset_dependents(&self, state: &mut State<S>, name: &str) -> ChildResult<()> {
        let mut reset = HashSet::from([name.to_owned()]);
        let mut pending = VecDeque::from([name.to_owned()]);
        while let Some(current) = pending.pop_front() {
            for index in 0..state.entries.len() {
                if !state.entries[index].task.dependencies.contains(&current)
                    || !reset.insert(state.entries[index].task.name.clone())
                {
                    continue;
                }
                pending.push_back(state.entries[index].task.name.clone());
                if !state.entries[index].owns_attempt()
                    && !matches!(
                        state.entries[index].task.state,
                        ExecutionStatus::Running
                            | ExecutionStatus::Succeeded
                            | ExecutionStatus::Failed
                    )
                {
                    continue;
                }
                if state.entries[index].owns_attempt() {
                    self.stop_run(state, index, "agent-task.stop-dependency-reset")?;
                }
                state.entries[index].runner = None;
                let task = &mut state.entries[index].task;
                task.state = ExecutionStatus::Pending;
                task.result = None;
                task.failure = None;
            }
        }
        Ok(())
    }
    async fn reconcile(self: &Arc<Self>, state: &mut State<S>) -> ChildResult<()> {
        for index in 0..state.entries.len() {
            if state.entries[index].owns_attempt()
                && effective(state, index) == ExecutionStatus::Canceled
            {
                self.stop_run(state, index, "agent-task.stop-dependency-canceled")?;
                state.entries[index].task.state = ExecutionStatus::Pending;
            }
        }
        self.ensure_healthy(state)?;
        if state.settling || state.suspended || !state.execution_active {
            return Ok(());
        }
        for index in 0..state.entries.len() {
            if state.entries[index].pending_stop.is_some()
                && state.entries[index].task.state == ExecutionStatus::Pending
                && effective(state, index) == ExecutionStatus::Pending
                && state.entries[index].task.dependencies.iter().all(|name| {
                    find(state, name).is_some_and(|dependency| {
                        state.entries[dependency].task.state == ExecutionStatus::Succeeded
                    })
                })
            {
                if state.entries[index].runner.is_none() {
                    let task = state.entries[index].task.clone();
                    let siblings = state
                        .entries
                        .iter()
                        .enumerate()
                        .filter(|(other, _)| *other != index)
                        .map(|(_, entry)| entry.task.clone())
                        .collect();
                    let dependencies = task
                        .dependencies
                        .iter()
                        .map(|name| {
                            state.entries[find(state, name).expect("validated dependency")]
                                .task
                                .clone()
                        })
                        .collect();
                    state.entries[index].runner =
                        Some(self.runner.begin(task, siblings, dependencies)?);
                }
                state.entries[index].task.state = ExecutionStatus::Running;
                self.save_graph(state).await?;
            }
            let entry = &state.entries[index];
            if entry.run.is_none()
                && entry.pending_stop.is_none()
                && (entry.task.state == ExecutionStatus::Running && entry.runner.is_some()
                    || entry.task.state == ExecutionStatus::Pending)
                && matches!(
                    effective(state, index),
                    ExecutionStatus::Pending | ExecutionStatus::Running
                )
                && entry.task.dependencies.iter().all(|name| {
                    find(state, name).is_some_and(|dependency| {
                        state.entries[dependency].task.state == ExecutionStatus::Succeeded
                    })
                })
            {
                self.start(state, index).await?;
            }
        }
        Ok(())
    }
    async fn start(self: &Arc<Self>, state: &mut State<S>, index: usize) -> ChildResult<()> {
        self.ensure_healthy(state)?;
        prune_executions(
            &mut state.executions,
            &mut state.execution_failure,
            |error| self.failure_diagnostic("execution", error),
        )
        .await;
        if state.entries[index].runner.is_none() {
            let task = state.entries[index].task.clone();
            let siblings = state
                .entries
                .iter()
                .enumerate()
                .filter(|(other, _)| *other != index)
                .map(|(_, entry)| entry.task.clone())
                .collect();
            let dependencies = task
                .dependencies
                .iter()
                .map(|name| {
                    state.entries[find(state, name).expect("validated dependency")]
                        .task
                        .clone()
                })
                .collect();
            state.entries[index].runner = Some(self.runner.begin(task, siblings, dependencies)?);
            self.save_graph(state).await?;
        }
        if state.entries[index].scope.is_none() {
            match self.spawn(state, index).await {
                Ok(scope) => {
                    state.entries[index].worker_session_id =
                        Some(scope.identity().session.session_id.clone());
                    state.entries[index].scope = Some(scope);
                    self.save_graph(state).await?;
                }
                Err(ChildError::Suspended) => {
                    state.execution_active = false;
                    state.notifications_active = false;
                    return Ok(());
                }
                Err(error) => {
                    let task = state.entries[index].task.clone();
                    state.entries[index].task.state = ExecutionStatus::Failed;
                    state.entries[index].task.result = None;
                    state.entries[index].task.failure = Some(error.to_string());
                    state.entries[index].runner = None;
                    self.enqueue(
                        state,
                        "agent-task.notification-failed",
                        &[
                            ("name", &task.name),
                            ("description", &task.description),
                            ("failure", &error.to_string()),
                        ],
                        true,
                    )?;
                    self.save_graph(state).await?;
                    return Ok(());
                }
            }
        }
        state.entries[index].task.state = ExecutionStatus::Running;
        state.entries[index].task.result = None;
        state.entries[index].task.failure = None;
        self.save_graph(state).await?;
        let checkpoint = state.entries[index]
            .runner
            .as_ref()
            .expect("runner checkpoint");
        let run = Arc::new(Run {
            cancellation: Arc::new(RunCancellation {
                lifetime: self.lifetime.clone(),
                stop: Arc::new(Signal::new()),
            }),
            started: Instant::now(),
            generation_id: checkpoint.generation_id.clone(),
            ready: Arc::new(Readiness::default()),
        });
        let entry = &mut state.entries[index];
        entry.run = Some(run.clone());
        let previous = entry.last_execution.clone();
        let execution = Operation::start(self.clone().execute(index, run.clone(), previous));
        state.reservations.retain(|ready| !ready.is_ready());
        state.reservations.push(run.ready.clone());
        let entry = &mut state.entries[index];
        entry.last_execution = Some(execution.clone());
        self.diagnostic(entry, "running", run.started);
        state.executions.push(execution);
        Ok(())
    }
    fn owner(self: &Arc<Self>) -> ChildResult<Arc<Scope<S>>> {
        let registry = self
            .config
            .registry
            .upgrade()
            .ok_or_else(|| ChildError::registry("task scheduler registry is unavailable"))?;
        let scope = registry
            .snapshot_scopes()
            .into_iter()
            .find(|scope| Arc::ptr_eq(scope.identity(), &self.config.owner_identity))
            .ok_or_else(|| ChildError::registry("task scheduler owner scope is not registered"))?;
        if !Arc::ptr_eq(&scope.get_service::<Self>()?, self)
            || !Arc::ptr_eq(
                &scope.get_service::<dyn TaskSession>()?,
                &self.config.owner_session,
            )
        {
            return Err(ChildError::registry(
                "task scheduler owner service does not match registered scope",
            ));
        }
        Ok(scope)
    }
    async fn spawn(self: &Arc<Self>, state: &State<S>, index: usize) -> ChildResult<Arc<Scope<S>>> {
        let owner = self.owner()?;
        let entry = &state.entries[index];
        let task = entry.task.clone();
        let selection = entry.selection.clone();
        let boundary = entry.boundary.clone();
        let source = if entry.fork_mode == ForkHistoryMode::Dependency {
            task.dependencies
                .first()
                .and_then(|name| find(state, name))
                .and_then(|dependency| state.entries[dependency].scope.as_ref())
                .map(|scope| HistorySource::Sibling(scope.session().clone()))
                .unwrap_or(HistorySource::Parent)
        } else {
            HistorySource::Parent
        };
        let scheduler = self.clone();
        let fork_mode = entry.fork_mode;
        let parent = owner.session().clone();
        owner
            .spawner()
            .get_or_spawn(task.name.clone(), move || {
                let model = match &task.model {
                    Some(model) => scheduler.config.router.resolve(model)?,
                    None => selection.model,
                };
                Ok(LaunchRequest {
                    parent,
                    requested_name: task.name.clone(),
                    requested_profile: "agent-task-payload".into(),
                    model,
                    requested_scope: render(
                        &scheduler.config.configuration,
                        "agent-task.child-scope",
                        &[("role", "execute"), ("task_name", &task.name)],
                    )?,
                    delivery: CompletionDelivery::RetainedOnly,
                    boundary,
                    fork: HistoryForkSelection::parse(if fork_mode == ForkHistoryMode::Empty {
                        ""
                    } else {
                        "full"
                    }),
                    history_source: source,
                })
            })?
            .join()
            .await
    }
    fn execute(
        self: Arc<Self>,
        index: usize,
        run: Arc<Run>,
        previous: Option<Operation<()>>,
    ) -> BoxFuture<'static, ChildResult<()>> {
        Box::pin(async move {
            if let Some(previous) = previous {
                let _ = previous.join().await;
            }
            let result = async {
                let (checkpoint, session) = {
                    let state = self.gate.lock().await;
                    self.ensure_healthy(&state)?;
                    if run.cancellation.is_cancelled() {
                        return Err(ChildError::Cancelled);
                    }
                    let entry = &state.entries[index];
                    let checkpoint = entry.runner.clone().ok_or(ChildError::Cancelled)?;
                    let session = entry
                        .scope
                        .as_ref()
                        .ok_or_else(|| {
                            ChildError::registry("A running AgentTask requires an agent scope.")
                        })?
                        .get_service::<dyn TaskSession>()?;
                    (checkpoint, session)
                };
                let progress = RunProgress {
                    scheduler: Arc::downgrade(&self),
                    index,
                    run: run.clone(),
                    owner_session_id: self.config.owner_identity.session.session_id.clone(),
                };
                self.runner
                    .run(
                        checkpoint,
                        session.as_ref(),
                        &progress,
                        run.cancellation.clone(),
                    )
                    .await
            }
            .await;
            let mut state = self.gate.lock().await;
            if state.suspended
                || !state.entries[index]
                    .run
                    .as_ref()
                    .is_some_and(|current| Arc::ptr_eq(current, &run))
            {
                run.ready.finish(Err(ChildError::Cancelled));
                return Ok(());
            }
            if let Err(error) = self.ensure_healthy(&state) {
                run.ready.finish(Err(error.clone()));
                return Err(error);
            }
            let completed = match result {
                Ok(outcome) => self.complete(&mut state, index, outcome, &run).await,
                Err(ChildError::Cancelled) if run.cancellation.is_cancelled() => {
                    state.entries[index].run = None;
                    Ok(())
                }
                Err(error) => {
                    state.persistence_failure = Some(error.clone());
                    Err(error)
                }
            };
            run.ready.finish(completed.clone());
            completed
        })
    }
    async fn complete(
        self: &Arc<Self>,
        state: &mut State<S>,
        index: usize,
        outcome: Task,
        run: &Run,
    ) -> ChildResult<()> {
        let entry = &mut state.entries[index];
        entry.run = None;
        entry.task.state = outcome.state;
        entry.task.result = outcome.result.clone();
        entry.task.failure = outcome.failure.clone();
        self.diagnostic(entry, "terminal", run.started);
        if outcome.state == ExecutionStatus::Succeeded {
            self.enqueue(
                state,
                "agent-task.notification-succeeded",
                &[
                    ("name", &outcome.name),
                    ("description", &outcome.description),
                    ("result", outcome.result.as_deref().unwrap_or("")),
                ],
                true,
            )?;
        } else {
            self.enqueue(
                state,
                "agent-task.notification-failed",
                &[
                    ("name", &outcome.name),
                    ("description", &outcome.description),
                    ("failure", outcome.failure.as_deref().unwrap_or("")),
                ],
                true,
            )?;
        }
        self.save_graph(state).await?;
        self.reconcile(state).await?;
        self.notify_completion(state)?;
        let mut result = self.publish(state).await;
        first(&mut result, self.kick_stops(state), |error| {
            self.failure_diagnostic("stop", error)
        });
        first(&mut result, self.kick_notifications(state), |error| {
            self.failure_diagnostic("delivery", error)
        });
        result
    }
    fn stop_run(&self, state: &mut State<S>, index: usize, template: &str) -> ChildResult<()> {
        if state.entries[index].pending_stop.is_some() {
            return Ok(());
        }
        let reason = render(&self.config.configuration, template, &[])?;
        let text = render(
            &self.config.configuration,
            "agent-task.force-stopped",
            &[("reason", &reason)],
        )?;
        let entry = &mut state.entries[index];
        let run = entry.run.take();
        let checkpoint = entry.runner.take().expect("owned runner checkpoint");
        let Some(worker) = entry.worker_session_id.clone() else {
            return Ok(());
        };
        let message_id = Identifier::message_id()
            .map_err(|error| ChildError::failure("unexpected", error.to_string()))?;
        entry.stopping_run = run.clone();
        entry.pending_stop = Some(TaskStopCheckpoint {
            generation_id: checkpoint.generation_id,
            prompt_message_id: checkpoint.prompt_message_id,
            worker_session_id: worker.clone(),
            notice_message_id: message_id.clone(),
            stopped: false,
        });
        state.notifications.push(TaskNotificationSnapshot {
            message_id,
            target_session_id: worker,
            text,
            wake: false,
            ready: false,
        });
        // Cancellation is performed by the owned stop operation only after the graph is saved.
        if let Some(run) = run {
            self.diagnostic(entry, "stopped", run.started);
        }
        Ok(())
    }
    fn enqueue(
        &self,
        state: &mut State<S>,
        template: &str,
        values: &[(&str, &str)],
        wake: bool,
    ) -> ChildResult<()> {
        let text = render(&self.config.configuration, template, values)?;
        let text = if is_tool_output_oversized(&text) {
            self.config
                .output_blobs
                .persist(&text, &NeverCancelled)
                .map_err(|error| ChildError::failure("io", error.to_string()))?
        } else {
            text
        };
        let message_id = Identifier::message_id()
            .map_err(|error| ChildError::failure("unexpected", error.to_string()))?;
        state.notifications.push(TaskNotificationSnapshot {
            message_id,
            target_session_id: self.config.owner_identity.session.session_id.clone(),
            text,
            wake,
            ready: true,
        });
        Ok(())
    }
    fn items(&self, state: &State<S>, indices: &[usize]) -> ChildResult<String> {
        let mut text = String::new();
        for &index in indices {
            text.push_str(&render(
                &self.config.configuration,
                "agent-task.notification-item",
                &[
                    ("name", &state.entries[index].task.name),
                    ("state", status_name(effective(state, index))),
                ],
            )?);
        }
        Ok(text)
    }
    fn notify_completion(&self, state: &mut State<S>) -> ChildResult<()> {
        let complete = resolved(state);
        if complete && !state.complete {
            let items = self.items(state, &(0..state.entries.len()).collect::<Vec<_>>())?;
            self.enqueue(
                state,
                "agent-task.notification-all-complete",
                &[("items", &items)],
                true,
            )?;
        }
        state.complete = complete;
        Ok(())
    }
    async fn publish(&self, state: &mut State<S>) -> ChildResult<()> {
        let all_resolved = resolved(state);
        if all_resolved && !state.all_resolved {
            for entry in &mut state.entries {
                entry.task.hidden = true;
            }
        }
        state.all_resolved = all_resolved;
        state.revision = state
            .revision
            .checked_add(1)
            .ok_or_else(|| ChildError::failure("unexpected", "task progress revision overflow"))?;
        self.save_graph(state).await?;
        let snapshot = AgentTaskProgressSnapshot {
            revision: state.revision,
            root_nodes: state
                .entries
                .iter()
                .enumerate()
                .map(|(index, entry)| AgentTaskProgressNode {
                    name: entry.task.name.clone(),
                    description: entry.task.description.clone(),
                    hidden: entry.task.hidden,
                    status: match effective(state, index) {
                        ExecutionStatus::Pending => {
                            AgentTaskProgressStatus::AGENT_TASK_PROGRESS_STATUS_PENDING
                        }
                        ExecutionStatus::Running => {
                            AgentTaskProgressStatus::AGENT_TASK_PROGRESS_STATUS_RUNNING
                        }
                        ExecutionStatus::Succeeded => {
                            AgentTaskProgressStatus::AGENT_TASK_PROGRESS_STATUS_SUCCEEDED
                        }
                        ExecutionStatus::Failed => {
                            AgentTaskProgressStatus::AGENT_TASK_PROGRESS_STATUS_FAILED
                        }
                        ExecutionStatus::Canceled => {
                            AgentTaskProgressStatus::AGENT_TASK_PROGRESS_STATUS_CANCELED
                        }
                    }
                    .into(),
                    agent_session_id: entry.scope.as_ref().map_or_else(String::new, |scope| {
                        scope.identity().session.session_id.clone()
                    }),
                    ..AgentTaskProgressNode::new()
                })
                .collect(),
            ..AgentTaskProgressSnapshot::new()
        };
        let mut event = Event {
            id: Identifier::event_id()
                .map_err(|error| ChildError::failure("unexpected", error.to_string()))?,
            agent_session_id: self.config.owner_identity.session.session_id.clone(),
            ..Event::new()
        };
        event.set_tool_snapshot(ToolSnapshot {
            kind: PROGRESS_KIND.into(),
            owner_agent_session_id: self.config.owner_identity.session.session_id.clone(),
            revision: snapshot.revision,
            final_chunk: true,
            data: snapshot
                .write_to_bytes()
                .map_err(|error| ChildError::failure("unexpected", error.to_string()))?,
            ..ToolSnapshot::new()
        });
        self.config
            .history
            .append(event.clone(), None, None)
            .await?;
        self.config
            .publisher
            .publish_committed(&Committed::after_commit(event), &NeverCancelled)
            .await
            .map_err(|error| ChildError::failure("unexpected", error.to_string()))
    }
    fn worker_gate(&self, worker: &str) -> Arc<WorkerGate> {
        self.worker_gates
            .lock()
            .expect("task worker gates poisoned")
            .entry(worker.to_owned())
            .or_default()
            .clone()
    }
    async fn reserve_worker(
        &self,
        slot: Arc<WorkerGate>,
        checkpoint: &TaskRunnerCheckpoint,
        session: &dyn TaskSession,
        run: &Run,
    ) -> ChildResult<TaskWorkerLease> {
        loop {
            let changed = slot.changed.notified();
            tokio::pin!(changed);
            changed.as_mut().enable();
            if run.cancellation.is_cancelled() {
                return Err(ChildError::Cancelled);
            }
            let saved = session.capture_execution().await?;
            if let Some(claim) = saved
                .as_ref()
                .and_then(|saved| saved.result_claim.as_ref())
                .filter(|claim| claim.claim_id != checkpoint.prompt_message_id)
            {
                let owner_ready =
                    {
                        let state = self.gate.lock().await;
                        self.ensure_healthy(&state)?;
                        if claim.owner_session_id != self.config.owner_identity.session.session_id {
                            return Err(ChildError::registry(
                                "task worker is claimed by a different graph",
                            ));
                        }
                        state
                            .entries
                            .iter()
                            .find_map(|entry| {
                                if entry.runner.as_ref().is_some_and(|runner| {
                                    runner.prompt_message_id == claim.claim_id
                                }) {
                                    entry.run.as_ref().map(|run| run.ready.clone())
                                } else {
                                    entry
                                        .pending_stop
                                        .as_ref()
                                        .filter(|stop| stop.prompt_message_id == claim.claim_id)
                                        .and_then(|stop| {
                                            state.stop_readiness.get(&stop.generation_id).cloned()
                                        })
                                }
                            })
                            .ok_or_else(|| {
                                ChildError::registry(
                                    "task worker claim has no matching runner or stop owner",
                                )
                            })?
                    };
                owner_ready.wait().await?;
                run.ready.finish(Ok(()));
                tokio::select! { _ = changed => {}, _ = run.cancellation.cancelled() => return Err(ChildError::Cancelled) }
                continue;
            }
            let lease = tokio::select! { lease = slot.gate.clone().lock_owned() => lease, _ = run.cancellation.cancelled() => return Err(ChildError::Cancelled) };
            let saved = session.capture_execution().await?;
            if saved
                .as_ref()
                .and_then(|saved| saved.result_claim.as_ref())
                .is_some_and(|claim| claim.claim_id != checkpoint.prompt_message_id)
            {
                drop(lease);
                continue;
            }
            return Ok(TaskWorkerLease {
                _guard: lease,
                changed: slot.changed.clone(),
            });
        }
    }
    fn ensure_healthy(&self, state: &State<S>) -> ChildResult<()> {
        state.persistence_failure.clone().map_or(Ok(()), Err)
    }
    async fn save_graph(&self, state: &mut State<S>) -> ChildResult<()> {
        self.ensure_healthy(state)?;
        let snapshot = TaskGraphSnapshot {
            entries: state.entries.iter().map(Entry::snapshot).collect(),
            revision: state.revision,
            complete: state.complete,
            all_resolved: state.all_resolved,
            completion_notification: state.completion_notification.clone(),
            notifications: state.notifications.clone(),
            accepted_calls: state.accepted_calls.clone(),
        };
        if let Err(error) = self
            .config
            .state
            .update(AgentStateUpdate::Tasks(snapshot))
            .await
        {
            let error = state_error(error);
            state.persistence_failure = Some(error.clone());
            for entry in &state.entries {
                if let Some(run) = &entry.run {
                    run.ready.finish(Err(error.clone()));
                }
            }
            return Err(error);
        }
        Ok(())
    }
    /// Reconnects graph references only. It neither admits prompts nor delivers notifications.
    pub async fn resolve_workers(self: &Arc<Self>) -> ChildResult<()> {
        self.admitted().await;
        let owner = self.owner()?;
        let children: HashMap<_, _> = owner
            .children()
            .snapshot_children()
            .into_iter()
            .map(|scope| (scope.identity().session.session_id.clone(), scope))
            .collect();
        let mut state = self.gate.lock().await;
        self.ensure_healthy(&state)?;
        for entry in &mut state.entries {
            if entry.scope.is_some() {
                continue;
            }
            let scope = if let Some(id) = &entry.worker_session_id {
                Some(children.get(id).cloned().ok_or_else(|| {
                    ChildError::registry(format!(
                        "task worker is not a retained direct child: {id}"
                    ))
                })?)
            } else if matches!(
                entry.task.state,
                ExecutionStatus::Pending | ExecutionStatus::Running
            ) {
                owner.spawner().find_retained(&entry.task.name).await?
            } else {
                None
            };
            if let Some(scope) = scope {
                entry.worker_session_id = Some(scope.identity().session.session_id.clone());
                entry.scope = Some(scope);
            }
        }
        self.save_graph(&mut state).await
    }
    /// Returns worker identities whose execution recovery belongs to this graph.
    pub async fn claimed_worker_ids(&self) -> Vec<String> {
        let state = self.gate.lock().await;
        state
            .entries
            .iter()
            .enumerate()
            .filter(|(index, entry)| {
                entry.task.state == ExecutionStatus::Running
                    || entry.pending_stop.is_some()
                    || (entry.task.state == ExecutionStatus::Pending
                        && (entry.runner.is_some()
                            || (effective(&state, *index) == ExecutionStatus::Pending
                                && entry.task.dependencies.iter().all(|name| {
                                    find(&state, name).is_some_and(|dependency| {
                                        state.entries[dependency].task.state
                                            == ExecutionStatus::Succeeded
                                    })
                                }))))
            })
            .filter_map(|(_, entry)| entry.worker_session_id.clone())
            .collect()
    }
    /// Reserves graph-owned executions and waits only for their startup reservations.
    /// Notification wakes remain gated until activate_notifications is called.
    pub fn activate_executions(self: &Arc<Self>) -> Operation<()> {
        let scheduler = self.clone();
        Operation::start(async move {
            scheduler.resolve_workers().await?;
            {
                let mut state = scheduler.gate.lock().await;
                scheduler.ensure_healthy(&state)?;
                if state.suspended || state.settling {
                    return Err(shutting_down());
                }
                state.execution_active = true;
                scheduler.reconcile(&mut state).await?;
                scheduler.notify_completion(&mut state)?;
                scheduler.save_graph(&mut state).await?;
                scheduler.kick_stops(&mut state)?;
            }
            loop {
                let ready = {
                    let mut state = scheduler.gate.lock().await;
                    scheduler.ensure_healthy(&state)?;
                    std::mem::take(&mut state.reservations)
                };
                if ready.is_empty() {
                    return Ok(());
                }
                for ready in ready {
                    ready.wait().await?;
                }
            }
        })
    }
    /// Enables committed notification delivery only after all session owners are reserved.
    pub fn activate_notifications(self: &Arc<Self>) -> Operation<()> {
        let scheduler = self.clone();
        Operation::start(async move {
            scheduler.owner()?;
            let mut state = scheduler.gate.lock().await;
            scheduler.ensure_healthy(&state)?;
            if state.suspended {
                return Err(shutting_down());
            }
            state.notifications_active = true;
            scheduler.kick_notifications(&mut state)
        })
    }
    fn kick_stops(self: &Arc<Self>, state: &mut State<S>) -> ChildResult<()> {
        self.ensure_healthy(state)?;
        if state.suspended {
            return Ok(());
        }
        for index in 0..state.entries.len() {
            let Some(stop) = state.entries[index].pending_stop.clone() else {
                continue;
            };
            if !state.stopping.insert(stop.generation_id.clone()) {
                continue;
            }
            let worker = state.entries[index]
                .scope
                .as_ref()
                .ok_or_else(|| ChildError::registry("stopped task worker is unresolved"))?
                .get_service::<dyn TaskSession>()?;
            let live = state.entries[index].stopping_run.clone();
            let previous = state.entries[index].last_execution.clone();
            let scheduler = self.clone();
            let ready = Arc::new(Readiness::default());
            state.reservations.retain(|ready| !ready.is_ready());
            state.reservations.push(ready.clone());
            state
                .stop_readiness
                .insert(stop.generation_id.clone(), ready.clone());
            let operation = Operation::start(async move {
                let result = async {
                    let slot = scheduler.worker_gate(&stop.worker_session_id);
                    if let Some(live) = live {
                        live.cancellation.stop.cancel();
                    }
                    if let Some(previous) = previous {
                        let _ = previous.join().await;
                    }
                    let lease = slot.gate.clone().lock_owned().await;
                    let claim = TaskResultClaim {
                        owner_session_id: scheduler
                            .config
                            .owner_identity
                            .session
                            .session_id
                            .clone(),
                        claim_id: stop.prompt_message_id.clone(),
                    };
                    let saved = worker.capture_execution().await?;
                    let matching = saved
                        .as_ref()
                        .is_some_and(|saved| saved.initial_message_id == stop.prompt_message_id);
                    if let Some(saved) = &saved {
                        if matching
                            && saved
                                .result_claim
                                .as_ref()
                                .is_some_and(|held| held != &claim)
                        {
                            return Err(ChildError::registry(
                                "task stop conflicts with the worker result claimant",
                            ));
                        }
                        if matching && saved.terminal.is_none() {
                            if saved.result_claim.as_ref() != Some(&claim) {
                                return Err(ChildError::registry(
                                    "task stop lost its protected execution identity",
                                ));
                            }
                            let canceled = worker.cancel_recovered(&stop.prompt_message_id)?;
                            ready.finish(Ok(()));
                            canceled.join().await?;
                        }
                    }
                    {
                        let mut state = scheduler.gate.lock().await;
                        scheduler.ensure_healthy(&state)?;
                        if let Some(current) = &mut state.entries[index].pending_stop {
                            if current.generation_id != stop.generation_id {
                                return Err(ChildError::Cancelled);
                            }
                            current.stopped = true;
                        }
                        scheduler.save_graph(&mut state).await?;
                    }
                    if matching
                        && saved
                            .as_ref()
                            .is_some_and(|saved| saved.result_claim.as_ref() == Some(&claim))
                    {
                        let reference = worker
                            .capture_result(&stop.prompt_message_id, &claim)
                            .await?;
                        worker.acknowledge_result(&reference, &claim).await?;
                    }
                    ready.finish(Ok(()));
                    if !matching {
                        scheduler
                            .acknowledge_notification(&stop.notice_message_id, Ok(()))
                            .await?;
                        drop(lease);
                        slot.changed.notify_waiters();
                        return Ok(());
                    }
                    if scheduler.config.lifetime.is_cancelled() {
                        return Ok(());
                    }
                    let notice = {
                        let state = scheduler.gate.lock().await;
                        if state.suspended {
                            return Ok(());
                        }
                        state
                            .notifications
                            .iter()
                            .find(|notice| notice.message_id == stop.notice_message_id)
                            .cloned()
                            .ok_or_else(|| ChildError::registry("task stop notice is missing"))?
                    };
                    worker
                        .record(
                            vec![ConversationPart::text(notice.text)],
                            notice.message_id.clone(),
                            Delivery::DELIVERY_STEER,
                        )
                        .await?;
                    scheduler
                        .acknowledge_notification(&notice.message_id, Ok(()))
                        .await?;
                    drop(lease);
                    slot.changed.notify_waiters();
                    Ok(())
                }
                .await;
                let mut state = scheduler.gate.lock().await;
                let result = if state.suspended
                    && result.as_ref().is_err_and(|error| {
                        error == &ChildError::Cancelled
                            || error
                                == &ChildError::from(
                                    crate::agent_turn::lifecycle::LifecycleError::ShuttingDown,
                                )
                    }) {
                    Ok(())
                } else {
                    result
                };
                ready.finish(result.clone());
                state.stopping.remove(&stop.generation_id);
                state.stop_readiness.remove(&stop.generation_id);
                if let Err(error) = &result {
                    state.persistence_failure.get_or_insert(error.clone());
                }
                result
            });
            state.executions.push(operation);
        }
        Ok(())
    }
    fn kick_notifications(self: &Arc<Self>, state: &mut State<S>) -> ChildResult<()> {
        self.ensure_healthy(state)?;
        if state.suspended {
            return Ok(());
        }
        for value in state.notifications.clone() {
            let stop_notice = state.entries.iter().any(|entry| {
                entry
                    .pending_stop
                    .as_ref()
                    .is_some_and(|stop| stop.notice_message_id == value.message_id)
            });
            if stop_notice
                || !value.ready
                || !state.notifications_active
                || !state.in_flight.insert(value.message_id.clone())
            {
                continue;
            }
            let session = if value.target_session_id
                == self.config.owner_identity.session.session_id
            {
                self.config.owner_session.clone()
            } else {
                state
                    .entries
                    .iter()
                    .filter_map(|entry| entry.scope.as_ref())
                    .find(|scope| scope.identity().session.session_id == value.target_session_id)
                    .ok_or_else(|| {
                        ChildError::registry("notification target is not a retained task worker")
                    })?
                    .get_service::<dyn TaskSession>()?
            };
            let scheduler = Arc::downgrade(self);
            let id = value.message_id.clone();
            let acknowledge_id = id.clone();
            let queued = self.notifier.enqueue(Notification {
                value,
                session,
                finish: Box::new(move |result| {
                    Box::pin(async move {
                        let scheduler = scheduler.upgrade().ok_or(ChildError::Cancelled)?;
                        scheduler
                            .acknowledge_notification(&acknowledge_id, result)
                            .await
                    })
                }),
            });
            if !queued {
                state.in_flight.remove(&id);
            }
        }
        Ok(())
    }
    async fn acknowledge_notification(
        self: &Arc<Self>,
        id: &str,
        delivered: ChildResult<()>,
    ) -> ChildResult<()> {
        let mut state = self.gate.lock().await;
        state.in_flight.remove(id);
        if delivered.is_err() {
            return Ok(());
        }
        self.ensure_healthy(&state)?;
        state.notifications.retain(|notice| notice.message_id != id);
        for entry in &mut state.entries {
            if entry
                .pending_stop
                .as_ref()
                .is_some_and(|stop| stop.notice_message_id == id)
            {
                entry.pending_stop = None;
                entry.stopping_run = None;
            }
            if let Some(notice) = &mut entry.terminal_notification {
                if notice.message_id == id {
                    notice.admitted = true;
                }
            }
        }
        if let Some(notice) = &mut state.completion_notification {
            if notice.message_id == id {
                notice.admitted = true;
            }
        }
        self.save_graph(&mut state).await?;
        self.reconcile(&mut state).await?;
        self.save_graph(&mut state).await?;
        self.kick_stops(&mut state)?;
        Ok(())
    }
    fn failure_diagnostic(&self, operation: &'static str, error: &ChildError) {
        self.config.diagnostics.record(TaskDiagnostic {
            operation,
            session_id: self.config.owner_identity.session.session_id.clone(),
            correlation_id: String::new(),
            outcome: "failed",
            duration_milliseconds: None,
            error_code: Some(error.code()),
        });
    }
    fn diagnostic(&self, entry: &Entry<S>, operation: &'static str, started: Instant) {
        self.config.diagnostics.record(TaskDiagnostic {
            operation,
            session_id: self.config.owner_identity.session.session_id.clone(),
            correlation_id: entry.diagnostic_id.clone(),
            outcome: status_name(entry.task.state),
            duration_milliseconds: Some(
                started.elapsed().as_millis().min(u128::from(u64::MAX)) as u64
            ),
            error_code: None,
        });
    }
}
impl<S: RestrictableAccessSnapshot + 'static> Entry<S> {
    fn owns_attempt(&self) -> bool {
        self.run.is_some()
            || (matches!(
                self.task.state,
                ExecutionStatus::Pending | ExecutionStatus::Running
            ) && self.runner.is_some())
    }
    fn snapshot(&self) -> TaskEntrySnapshot {
        TaskEntrySnapshot {
            task: self.task.clone(),
            diagnostic_id: self.diagnostic_id.clone(),
            selection: SelectionSnapshot {
                profile_id: self.selection.profile_id.clone(),
                model_selector: self.selection.model.requested_selector.value.clone(),
            },
            fork_boundary: match &self.boundary {
                HistoryForkBoundary::BeforeToolBatch {
                    assistant_sequence,
                    tool_call_id,
                } => ForkBoundarySnapshot::BeforeToolBatch {
                    assistant_sequence: *assistant_sequence,
                    tool_call_id: tool_call_id.clone(),
                },
                HistoryForkBoundary::AfterCompletedHistory => {
                    ForkBoundarySnapshot::AfterCompletedHistory
                }
                HistoryForkBoundary::AfterSafeHistoryPrefix => {
                    ForkBoundarySnapshot::AfterSafeHistoryPrefix
                }
            },
            fork_mode: match self.fork_mode {
                ForkHistoryMode::Dependency => TaskForkModeSnapshot::Dependency,
                ForkHistoryMode::Parent => TaskForkModeSnapshot::Parent,
                ForkHistoryMode::Empty => TaskForkModeSnapshot::Empty,
            },
            worker_session_id: self.worker_session_id.clone(),
            runner: self.runner.clone(),
            pending_stop: self.pending_stop.clone(),
            terminal_notification: self.terminal_notification.clone(),
        }
    }
}
struct SchedulerWork<S: RestrictableAccessSnapshot + 'static, P: TaskArtifactParser + 'static>(
    Arc<TaskScheduler<S, P>>,
);
impl<S: RestrictableAccessSnapshot + 'static, P: TaskArtifactParser + 'static> OwnedWork
    for SchedulerWork<S, P>
{
    type Error = ChildError;
    fn settle(&self) -> BoxFuture<'_, ChildResult<()>> {
        let operation = self.0.settle();
        Box::pin(async move { operation.join().await })
    }
}
fn find<S: RestrictableAccessSnapshot + 'static>(state: &State<S>, name: &str) -> Option<usize> {
    state
        .entries
        .iter()
        .position(|entry| entry.task.name == name)
}
fn effective<S: RestrictableAccessSnapshot + 'static>(
    state: &State<S>,
    index: usize,
) -> ExecutionStatus {
    let task = &state.entries[index].task;
    if matches!(
        task.state,
        ExecutionStatus::Canceled | ExecutionStatus::Succeeded
    ) || !has_canceled_dependency(state, task)
    {
        task.state
    } else {
        ExecutionStatus::Canceled
    }
}
fn has_canceled_dependency<S: RestrictableAccessSnapshot + 'static>(
    state: &State<S>,
    task: &Task,
) -> bool {
    task.dependencies
        .iter()
        .filter_map(|name| find(state, name))
        .any(|index| {
            state.entries[index].task.state == ExecutionStatus::Canceled
                || has_canceled_dependency(state, &state.entries[index].task)
        })
}
fn resolved<S: RestrictableAccessSnapshot + 'static>(state: &State<S>) -> bool {
    !state.entries.is_empty()
        && (0..state.entries.len()).all(|index| {
            matches!(
                effective(state, index),
                ExecutionStatus::Succeeded | ExecutionStatus::Canceled
            )
        })
}
async fn prune_executions(
    executions: &mut Vec<Operation<()>>,
    failure: &mut Option<ChildError>,
    mut secondary: impl FnMut(&ChildError),
) {
    for execution in std::mem::take(executions) {
        // A finished operation includes its stop record. Observe failures before
        // pruning, retaining only one primary error rather than failed handles.
        if !execution.is_finished() {
            executions.push(execution);
        } else if let Err(error) = execution.join().await {
            if failure.is_none() {
                *failure = Some(error);
            } else {
                secondary(&error);
            }
        }
    }
}
fn shutting_down() -> ChildError {
    ChildError::registry("The agent session is shutting down.")
}
fn first(result: &mut ChildResult<()>, next: ChildResult<()>, secondary: impl FnOnce(&ChildError)) {
    if result.is_ok() {
        *result = next;
    } else if let Err(error) = next {
        secondary(&error);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shutdown_error_matches_source_text() {
        assert_eq!(
            shutting_down().to_string(),
            "The agent session is shutting down."
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn pruning_bounds_successes_and_failures_without_dropping_inflight_work() {
        let success = Operation::start(async { Ok(()) });
        success.join().await.unwrap();
        let primary = ChildError::failure("io", "first publication failed");
        let error = primary.clone();
        let failure = Operation::start(async move { Err(error) });
        assert!(failure.join().await.is_err());
        let stop_record = Arc::new(Signal::new());
        let release = stop_record.clone();
        let inflight = Operation::start(async move {
            release.cancelled().await;
            Ok(())
        });
        let mut executions = vec![success, failure, inflight.clone()];
        let mut first_failure = None;
        let mut secondary_codes = Vec::new();

        prune_executions(&mut executions, &mut first_failure, |error| {
            secondary_codes.push(error.code());
        })
        .await;
        assert_eq!(executions.len(), 1);
        assert!(!executions[0].is_finished());
        assert_eq!(first_failure.as_ref(), Some(&primary));
        assert!(secondary_codes.is_empty());

        for _ in 0..64 {
            let success = Operation::start(async { Ok(()) });
            success.join().await.unwrap();
            let failure = Operation::start(async {
                Err(ChildError::failure("timeout", "later private failure body"))
            });
            assert!(failure.join().await.is_err());
            executions.extend([success, failure]);
            prune_executions(&mut executions, &mut first_failure, |error| {
                secondary_codes.push(error.code());
            })
            .await;
            assert_eq!(executions.len(), 1);
            assert_eq!(first_failure.as_ref(), Some(&primary));
        }
        assert_eq!(secondary_codes, vec!["timeout"; 64]);

        stop_record.cancel();
        inflight.join().await.unwrap();
        prune_executions(&mut executions, &mut first_failure, |_| {
            panic!("successful stop recording must not produce diagnostics");
        })
        .await;
        assert!(executions.is_empty());
        assert_eq!(first_failure, Some(primary));
    }

    #[test]
    fn first_failure_wins_and_later_errors_are_observed_by_code() {
        let primary = ChildError::failure("unexpected", "private publication body");
        let mut result = Ok(());
        first(&mut result, Err(primary.clone()), |_| {
            panic!("first error is returned")
        });
        first(&mut result, Ok(()), |_| {
            panic!("success must not be diagnosed")
        });
        let mut codes = Vec::new();
        for code in ["io", "timeout", "http"] {
            first(
                &mut result,
                Err(ChildError::failure(code, "private cleanup body")),
                |error| {
                    codes.push(error.code());
                },
            );
        }
        assert_eq!(result, Err(primary));
        assert_eq!(codes, ["io", "timeout", "http"]);
    }
}
