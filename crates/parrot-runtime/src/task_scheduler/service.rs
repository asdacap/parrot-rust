use super::{
    ForkHistoryMode, SchedulerConfig, TaskDiagnostic, TaskRunner, TaskSession,
    apply_visibility_changes, has_same_definition, has_same_visibility,
    notifier::TaskNotifier,
    ports::{CapturedSelection, RunCancellation, render},
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
        agent_tasks::{ExecutionStatus, Task},
        identifiers::Identifier,
    },
};
use parrot_storage::{
    conversation::{HistoryForkBoundary, HistoryForkSelection},
    events::{ConversationPart, NeverCancelled},
};
use protobuf::Message;
use std::{
    collections::{HashMap, HashSet, VecDeque},
    sync::{Arc, Mutex},
    time::Instant,
};

pub const PROGRESS_KIND: &str = "agent_task_progress";
const NOTIFICATION_CAUSE: &str = "AgentTask update";
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TaskDetail {
    pub task: Task,
    pub agent_name: Option<String>,
}
struct Entry<S: RestrictableAccessSnapshot + 'static> {
    task: Task,
    diagnostic_id: String,
    selection: CapturedSelection<S>,
    boundary: HistoryForkBoundary,
    scope: Option<Arc<Scope<S>>>,
    run: Option<Arc<Run>>,
    last_execution: Option<Operation<()>>,
}
struct Run {
    cancellation: Arc<RunCancellation>,
    started: Instant,
    stop_reason: Mutex<Option<String>>,
}
struct State<S: RestrictableAccessSnapshot + 'static> {
    entries: Vec<Entry<S>>,
    executions: Vec<Operation<()>>,
    execution_failure: Option<ChildError>,
    revision: u64,
    complete: bool,
    all_resolved: bool,
    settling: bool,
}
#[derive(Default)]
struct Admission {
    tail: Option<Operation<()>>,
    settlement: Option<Operation<()>>,
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
}
impl<S: RestrictableAccessSnapshot + 'static, P: TaskArtifactParser + 'static> TaskScheduler<S, P> {
    pub fn work_owner(self: &Arc<Self>) -> Arc<dyn OwnedWork<Error = ChildError>> {
        Arc::new(SchedulerWork(self.clone()))
    }
    pub fn new(config: SchedulerConfig<S, P>) -> ChildResult<Arc<Self>> {
        if !Arc::ptr_eq(&config.owner_identity, config.owner_session.identity())
            || config.history.agent_session_id() != config.owner_identity.session.session_id
        {
            return Err(ChildError::registry(
                "task scheduler session/history does not match owner identity",
            ));
        }
        let stop = Arc::new(Signal::new());
        let lifetime = Arc::new(RunCancellation {
            lifetime: config.lifetime.clone(),
            stop: stop.clone(),
        });
        Ok(Arc::new(Self {
            runner: TaskRunner::new(config.configuration.clone(), config.parser.clone()),
            notifier: TaskNotifier::new(
                config.owner_session.clone(),
                config.output_blobs.clone(),
                config.diagnostics.clone(),
            ),
            config,
            stop,
            lifetime,
            gate: tokio::sync::Mutex::new(State {
                entries: Vec::new(),
                executions: Vec::new(),
                execution_failure: None,
                revision: 0,
                complete: false,
                all_resolved: false,
                settling: false,
            }),
            admission: Mutex::new(Admission::default()),
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
        let mut admission = self.admission.lock().expect("task admission poisoned");
        if admission.settlement.is_some() {
            return Err(shutting_down());
        }
        let previous = admission.tail.clone();
        let scheduler = self.clone();
        let operation = Operation::start(async move {
            if let Some(previous) = previous {
                let _ = previous.join().await;
            }
            let mut state = scheduler.gate.lock().await;
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
                        selection: selection.clone(),
                        boundary,
                        scope: None,
                        run: None,
                        last_execution: None,
                    });
                }
            }
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
                scheduler.enqueue(
                    "agent-task.notification-updated",
                    &[("items", &scheduler.items(&state, &changed)?)],
                    false,
                )?;
            }
            scheduler.notify_completion(&mut state)?;
            scheduler.publish(&mut state).await
        });
        admission.tail = Some(operation.clone());
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
        if admission.settlement.is_some() {
            return Operation::start(async { Ok(()) });
        }
        let prior = admission.tail.clone();
        let scheduler = self.clone();
        let operation = Operation::start(async move {
            if let Some(prior) = prior {
                let _ = prior.join().await;
            }
            let mut state = scheduler.gate.lock().await;
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
    pub fn settle(self: &Arc<Self>) -> Operation<()> {
        let mut admission = self.admission.lock().expect("task admission poisoned");
        if let Some(settlement) = &admission.settlement {
            return settlement.clone();
        }
        let previous = admission.tail.clone();
        let scheduler = self.clone();
        let operation = Operation::start(async move {
            if let Some(previous) = previous {
                let _ = previous.join().await;
            }
            let mut result = Ok(());
            let executions = {
                let mut state = scheduler.gate.lock().await;
                state.settling = true;
                let reason = render(
                    &scheduler.config.configuration,
                    "agent-task.stop-shutdown",
                    &[],
                );
                match reason {
                    Ok(reason) => {
                        for index in 0..state.entries.len() {
                            if let Some(run) = state.entries[index].run.take() {
                                *run.stop_reason.lock().expect("task run poisoned") =
                                    Some(reason.clone());
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
                    }
                    Err(error) => result = Err(error),
                }
                if !state.entries.is_empty() {
                    first(&mut result, scheduler.publish(&mut state).await, |error| {
                        scheduler.failure_diagnostic("settlement_publish", error);
                    });
                }
                if let Some(error) = state.execution_failure.take() {
                    first(&mut result, Err(error), |error| {
                        scheduler.failure_diagnostic("execution", error);
                    });
                }
                std::mem::take(&mut state.executions)
            };
            scheduler.stop.cancel();
            for execution in executions {
                first(&mut result, execution.join().await, |error| {
                    scheduler.failure_diagnostic("execution", error);
                });
            }
            first(
                &mut result,
                scheduler.notifier.settle().join().await,
                |error| {
                    scheduler.failure_diagnostic("settlement_notifier", error);
                },
            );
            result
        });
        admission.settlement = Some(operation.clone());
        operation
    }
    fn apply(&self, state: &mut State<S>, index: usize, mut incoming: Task) -> ChildResult<()> {
        let stored = state.entries[index].task.clone();
        if state.entries[index].run.is_some()
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
        if state.entries[index].run.is_some() {
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
                if !matches!(
                    state.entries[index].task.state,
                    ExecutionStatus::Running | ExecutionStatus::Succeeded | ExecutionStatus::Failed
                ) {
                    continue;
                }
                if state.entries[index].run.is_some() {
                    self.stop_run(state, index, "agent-task.stop-dependency-reset")?;
                }
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
            if state.entries[index].run.is_some()
                && effective(state, index) == ExecutionStatus::Canceled
            {
                self.stop_run(state, index, "agent-task.stop-dependency-canceled")?;
                state.entries[index].task.state = ExecutionStatus::Pending;
            }
        }
        if state.settling {
            return Ok(());
        }
        for index in 0..state.entries.len() {
            let entry = &state.entries[index];
            if entry.run.is_none()
                && entry.task.state == ExecutionStatus::Pending
                && effective(state, index) == ExecutionStatus::Pending
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
        prune_executions(
            &mut state.executions,
            &mut state.execution_failure,
            |error| {
                self.failure_diagnostic("execution", error);
            },
        )
        .await;
        let task = state.entries[index].task.clone();
        let siblings: Vec<_> = state
            .entries
            .iter()
            .enumerate()
            .filter(|(other, _)| *other != index)
            .map(|(_, entry)| entry.task.clone())
            .collect();
        let dependencies: Vec<_> = task
            .dependencies
            .iter()
            .map(|name| {
                state.entries[find(state, name).expect("validated dependency")]
                    .task
                    .clone()
            })
            .collect();
        let started = Instant::now();
        if state.entries[index].scope.is_none() {
            match self.spawn(state, index).await {
                Ok(scope) => state.entries[index].scope = Some(scope),
                Err(error) => {
                    let entry = &mut state.entries[index];
                    entry.task.state = ExecutionStatus::Failed;
                    entry.task.result = None;
                    entry.task.failure = Some(error.to_string());
                    self.diagnostic(entry, "terminal", started);
                    self.enqueue(
                        "agent-task.notification-failed",
                        &[
                            ("name", &task.name),
                            ("description", &task.description),
                            ("failure", &error.to_string()),
                        ],
                        true,
                    )?;
                    return Ok(());
                }
            }
        }
        let run = Arc::new(Run {
            cancellation: Arc::new(RunCancellation {
                lifetime: self.lifetime.clone(),
                stop: Arc::new(Signal::new()),
            }),
            started: Instant::now(),
            stop_reason: Mutex::new(None),
        });
        let entry = &mut state.entries[index];
        entry.task.state = ExecutionStatus::Running;
        entry.task.result = None;
        entry.task.failure = None;
        entry.run = Some(run.clone());
        let scheduler = self.clone();
        let previous = entry.last_execution.clone();
        let original = entry.task.clone();
        let execution = Operation::start(scheduler.execute(
            index,
            run.clone(),
            original,
            siblings,
            dependencies,
            previous,
        ));
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
        let source = if self.config.configuration.fork_history_mode == ForkHistoryMode::Dependency {
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
                    fork: HistoryForkSelection::parse(
                        if scheduler.config.configuration.fork_history_mode
                            == ForkHistoryMode::Empty
                        {
                            ""
                        } else {
                            "full"
                        },
                    ),
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
        original: Task,
        siblings: Vec<Task>,
        dependencies: Vec<Task>,
        previous: Option<Operation<()>>,
    ) -> BoxFuture<'static, ChildResult<()>> {
        Box::pin(async move {
            tokio::task::yield_now().await;
            if let Some(previous) = previous {
                let _ = previous.join().await;
            }
            let result = async {
                let state = self.gate.lock().await;
                if run.cancellation.is_cancelled() {
                    return Err(ChildError::Cancelled);
                }
                let entry = &state.entries[index];
                let task = entry.task.clone();
                let session = entry
                    .scope
                    .as_ref()
                    .ok_or_else(|| {
                        ChildError::registry("A running AgentTask requires an agent scope.")
                    })?
                    .get_service::<dyn TaskSession>()?;
                let mut execution = Box::pin(self.runner.run(
                    task,
                    &siblings,
                    &dependencies,
                    session.as_ref(),
                    run.cancellation.clone(),
                ));
                // C# invokes the runner's synchronous prefix under this gate.
                // Poll once to admit the actual session enqueue atomically with
                // the task capture, but never hold the gate across the turn.
                let initial = std::future::poll_fn(|context| {
                    std::task::Poll::Ready(std::future::Future::poll(execution.as_mut(), context))
                })
                .await;
                drop(state);
                match initial {
                    std::task::Poll::Ready(result) => result,
                    std::task::Poll::Pending => execution.await,
                }
            }
            .await;
            let outcome = match result {
                Ok(task) => Some(task),
                Err(ChildError::Cancelled) if run.cancellation.is_cancelled() => None,
                Err(error) => {
                    let mut task = original;
                    task.state = ExecutionStatus::Failed;
                    task.failure = Some(error.to_string());
                    Some(task)
                }
            };
            let (stop_reason, mut completion) = {
                let mut state = self.gate.lock().await;
                let current = state.entries[index]
                    .run
                    .as_ref()
                    .is_some_and(|current| Arc::ptr_eq(current, &run));
                let completed = current && outcome.is_some();
                let completion = if let (true, Some(outcome)) = (current, outcome) {
                    self.complete(&mut state, index, outcome, &run).await
                } else if current {
                    state.entries[index].run = None;
                    state.entries[index].task.state = ExecutionStatus::Canceled;
                    self.publish(&mut state).await
                } else {
                    Ok(())
                };
                let reason = if completed {
                    None
                } else {
                    run.stop_reason.lock().expect("task run poisoned").clone()
                };
                (
                    if completed {
                        Ok(None)
                    } else {
                        match reason {
                            Some(reason) => Ok(Some(reason)),
                            None => {
                                render(&self.config.configuration, "agent-task.stop-shutdown", &[])
                                    .map(Some)
                            }
                        }
                    },
                    completion,
                )
            };
            match stop_reason {
                Ok(Some(reason)) => self.record_stop(index, reason).await,
                Ok(None) => {}
                Err(error) => first(&mut completion, Err(error), |error| {
                    self.failure_diagnostic("execution", error);
                }),
            }
            completion
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
                "agent-task.notification-failed",
                &[
                    ("name", &outcome.name),
                    ("description", &outcome.description),
                    ("failure", outcome.failure.as_deref().unwrap_or("")),
                ],
                true,
            )?;
        }
        self.reconcile(state).await?;
        self.notify_completion(state)?;
        self.publish(state).await
    }
    async fn record_stop(&self, index: usize, reason: String) {
        let (session, correlation_id) = {
            let state = self.gate.lock().await;
            let entry = &state.entries[index];
            let Some(scope) = &entry.scope else {
                return;
            };
            (
                scope.get_service::<dyn TaskSession>(),
                entry.diagnostic_id.clone(),
            )
        };
        let result = async {
            let text = render(
                &self.config.configuration,
                "agent-task.force-stopped",
                &[("reason", &reason)],
            )?;
            let id = Identifier::message_id()
                .map_err(|error| ChildError::failure("unexpected", error.to_string()))?;
            session?
                .record(
                    vec![ConversationPart::text(text)],
                    id,
                    Delivery::DELIVERY_STEER,
                )
                .await
        }
        .await;
        if let Err(error) = result {
            self.config.diagnostics.record(TaskDiagnostic {
                operation: "stop_record",
                session_id: self.config.owner_identity.session.session_id.clone(),
                correlation_id,
                outcome: "failed",
                duration_milliseconds: None,
                error_code: Some(error.code()),
            });
        }
    }
    fn stop_run(&self, state: &mut State<S>, index: usize, template: &str) -> ChildResult<()> {
        let reason = render(&self.config.configuration, template, &[])?;
        let entry = &mut state.entries[index];
        let run = entry
            .run
            .take()
            .expect("only a running task can be stopped");
        *run.stop_reason.lock().expect("task run poisoned") = Some(reason);
        run.cancellation.stop.cancel();
        self.diagnostic(entry, "stopped", run.started);
        Ok(())
    }
    fn enqueue(&self, template: &str, values: &[(&str, &str)], wake: bool) -> ChildResult<()> {
        self.notifier.enqueue(
            render(&self.config.configuration, template, values)?,
            NOTIFICATION_CAUSE,
            wake,
        );
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
            self.enqueue(
                "agent-task.notification-all-complete",
                &[(
                    "items",
                    &self.items(state, &(0..state.entries.len()).collect::<Vec<_>>())?,
                )],
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
