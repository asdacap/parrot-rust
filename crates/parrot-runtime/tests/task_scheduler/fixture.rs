//! Explicit port fixtures; parsing remains T49's responsibility, never reimplemented here.
use super::support::*;
use parrot_domain::{
    contracts::{
        agent_tasks::TaskArtifactParser,
        async_ops::{BoxFuture, Cancellation},
    },
    protocol::messages::parrot::Delivery,
    values::agent_tasks::*,
};
use parrot_provider::{routing::aliases::ResolvedModelSelection, sessions::Request};
use parrot_runtime::{
    agent_turn::lifecycle::{Execution, IncomingActivity},
    children::*,
    task_scheduler::*,
};
use parrot_storage::{conversation::HistoryForkBoundary, events::ConversationPart};
use std::{
    collections::{BTreeMap, VecDeque},
    fmt,
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
};

pub type Scheduler = TaskScheduler<Security, Parser>;
pub struct FinalResource(pub Arc<Trace>, pub &'static str);
impl ScopeResource for FinalResource {
    type Error = ChildError;
    fn close(&self) -> BoxFuture<'_, ChildResult<()>> {
        Box::pin(async move {
            self.0.push(self.1);
            Ok(())
        })
    }
}
#[derive(Clone, Debug)]
pub struct ParseError(pub String);
impl fmt::Display for ParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}
impl std::error::Error for ParseError {}
pub struct Parser {
    pub leaf_inputs: Mutex<Vec<String>>,
    pub graphs: Mutex<Vec<(Vec<Task>, String)>>,
    pub validations: Mutex<VecDeque<Result<(), ParseError>>>,
    pub leaves: Mutex<BTreeMap<String, Result<LeafResponse, ParseError>>>,
}
impl Default for Parser {
    fn default() -> Self {
        Self {
            leaf_inputs: Mutex::new(vec![]),
            graphs: Mutex::new(vec![]),
            validations: Mutex::new(VecDeque::new()),
            leaves: Mutex::new(BTreeMap::from([("answer".into(), Ok(accept("answer")))])),
        }
    }
}
impl Parser {
    pub fn answer(&self, token: &str, response: LeafResponse) {
        self.leaves
            .lock()
            .unwrap()
            .insert(token.into(), Ok(response));
    }
    pub fn error(&self, token: &str, message: &str) {
        self.leaves
            .lock()
            .unwrap()
            .insert(token.into(), Err(ParseError(message.into())));
    }
}
impl TaskArtifactParser for Parser {
    type Error = ParseError;
    fn parse_artifact(&self, _: &str) -> Result<TaskArtifact, ParseError> {
        panic!("runtime must not parse artifacts")
    }
    fn parse_task_set(&self, _: &str) -> Result<Vec<Task>, ParseError> {
        panic!("runtime must not parse task sets")
    }
    fn validate_merged_graph(&self, tasks: &[Task], path: &str) -> Result<(), ParseError> {
        self.graphs
            .lock()
            .unwrap()
            .push((tasks.to_vec(), path.into()));
        self.validations
            .lock()
            .unwrap()
            .pop_front()
            .unwrap_or(Ok(()))
    }
    fn parse_leaf_response(&self, input: &str) -> Result<LeafResponse, ParseError> {
        self.leaf_inputs.lock().unwrap().push(input.into());
        self.leaves
            .lock()
            .unwrap()
            .get(input)
            .unwrap_or_else(|| panic!("unscripted parser input {input:?}"))
            .clone()
    }
}
pub fn accept(result: &str) -> LeafResponse {
    LeafResponse {
        result: result.into(),
        verdict: AcceptanceVerdict::Accept,
    }
}
pub fn task(name: &str) -> Task {
    Task {
        name: name.into(),
        dependencies: vec![],
        description: format!("Do {name}"),
        payload: TaskPayload::Instruction(format!("Work on {name}")),
        acceptance_criteria: format!("{name} works"),
        model: None,
        hidden: false,
        state: ExecutionStatus::Pending,
        result: None,
        failure: None,
    }
}
pub fn state(name: &str, state: ExecutionStatus) -> Task {
    Task {
        state,
        ..task(name)
    }
}
pub fn dependent(name: &str, dependency: &str) -> Task {
    Task {
        dependencies: vec![dependency.into()],
        ..task(name)
    }
}
pub fn composite(name: &str, children: Vec<Task>) -> Task {
    Task {
        payload: TaskPayload::Tasks(children),
        ..task(name)
    }
}
pub fn inner(task: &Task) -> &[Task] {
    match &task.payload {
        TaskPayload::Tasks(tasks) => tasks,
        _ => panic!("composite expected"),
    }
}
pub async fn set(scope: &Arc<Scope<Security>>, tasks: Vec<Task>) {
    bounded(
        scope
            .get_service::<Scheduler>()
            .unwrap()
            .set_tasks(
                tasks,
                scope.selection().current().unwrap(),
                HistoryForkBoundary::AfterCompletedHistory,
            )
            .unwrap()
            .join(),
    )
    .await
    .unwrap();
}
pub async fn snapshot(scope: &Arc<Scope<Security>>) -> Vec<Task> {
    scope.get_service::<Scheduler>().unwrap().snapshot().await
}
pub async fn wait_tasks(
    scope: &Arc<Scope<Security>>,
    predicate: impl Fn(&[Task]) -> bool,
) -> Vec<Task> {
    bounded(async {
        loop {
            let tasks = snapshot(scope).await;
            if predicate(&tasks) {
                return tasks;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
}
pub async fn complete(scope: &Arc<Scope<Security>>) -> Vec<Task> {
    wait_tasks(scope, |tasks| {
        !tasks.is_empty()
            && tasks
                .iter()
                .all(|task| task.state == ExecutionStatus::Succeeded)
    })
    .await
}
pub fn prompt(request: &Request) -> &str {
    request.messages.last().expect("provider input").content()
}
pub fn conversation(request: &Request) -> String {
    request
        .messages
        .iter()
        .map(|message| message.content())
        .collect::<Vec<_>>()
        .join("\n")
}
pub fn requests(provider: &Scripted, name: &str) -> Vec<Request> {
    provider
        .requests
        .lock()
        .unwrap()
        .iter()
        .filter(|request| request.instructions == format!("offline task fixture scope:{name}"))
        .cloned()
        .collect()
}
pub fn task_requests(provider: &Scripted, name: &str) -> Vec<Request> {
    requests(provider, name)
        .into_iter()
        .filter(|request| prompt(request).contains(&format!("Task: {name}\n")))
        .collect()
}
pub struct Reply {
    pub text: String,
    pub gate: Option<Arc<Gate>>,
    pub close_gate: Option<Arc<Gate>>,
}
impl Reply {
    pub fn text(text: &str) -> Self {
        Self {
            text: text.into(),
            gate: None,
            close_gate: None,
        }
    }
    pub fn held(text: &str, gate: &Arc<Gate>) -> Self {
        Self {
            gate: Some(gate.clone()),
            ..Self::text(text)
        }
    }
}
impl Scripted {
    pub fn script(&self, name: &str, replies: impl IntoIterator<Item = Reply>) {
        self.replies
            .lock()
            .unwrap()
            .entry(name.into())
            .or_default()
            .extend(replies);
    }
    pub async fn wait_requests(&self, name: &str, count: usize) {
        bounded(async {
            loop {
                let changed = self.changed.notified();
                tokio::pin!(changed);
                changed.as_mut().enable();
                if requests(self, name).len() >= count {
                    return;
                }
                changed.await;
            }
        })
        .await;
    }
}
#[derive(Default)]
pub struct Router {
    pub provider: Mutex<Option<Scripted>>,
    pub selectors: Mutex<Vec<String>>,
}
impl TaskModelRouter for Router {
    fn resolve(&self, selector: &str) -> ChildResult<ResolvedModelSelection> {
        self.selectors.lock().unwrap().push(selector.into());
        match selector.strip_prefix("scripted/").unwrap_or(selector) {
            "first" | "second" => Ok(self
                .provider
                .lock()
                .unwrap()
                .as_ref()
                .unwrap()
                .selection(selector.strip_prefix("scripted/").unwrap_or(selector))),
            _ => Err(ChildError::failure(
                "argument",
                format!("PRIVATE invalid selector {selector}"),
            )),
        }
    }
}
#[derive(Default)]
pub struct TaskLog(pub Mutex<Vec<TaskDiagnostic>>);
impl TaskDiagnostics for TaskLog {
    fn record(&self, event: TaskDiagnostic) {
        self.0.lock().unwrap().push(event);
    }
}
#[derive(Clone, Debug)]
pub struct DeliveryCall {
    pub kind: &'static str,
    pub id: String,
    pub text: String,
}
pub struct RecordingSession {
    pub actual: Arc<TaskSessionAdapter<Backend>>,
    pub calls: Mutex<Vec<DeliveryCall>>,
    pub send_failures: AtomicUsize,
    pub ack_failures: AtomicUsize,
    pub record_gate: Mutex<Option<Arc<Gate>>>,
    pub enqueue_hook: Mutex<Option<Box<dyn FnOnce() + Send>>>,
    pub changed: tokio::sync::Notify,
}
impl RecordingSession {
    pub fn new(actual: Arc<TaskSessionAdapter<Backend>>) -> Self {
        Self {
            actual,
            calls: Mutex::new(vec![]),
            send_failures: AtomicUsize::new(0),
            ack_failures: AtomicUsize::new(0),
            record_gate: Mutex::new(None),
            enqueue_hook: Mutex::new(None),
            changed: tokio::sync::Notify::new(),
        }
    }
    fn capture(&self, kind: &'static str, id: &str, parts: &[ConversationPart]) {
        let text = parts
            .iter()
            .map(|part| match part {
                ConversationPart::Text(text) => text.as_str(),
                _ => "",
            })
            .collect::<String>();
        self.calls.lock().unwrap().push(DeliveryCall {
            kind,
            id: id.into(),
            text,
        });
        self.changed.notify_waiters();
    }
    pub async fn wait_calls(&self, predicate: impl Fn(&[DeliveryCall]) -> bool) {
        bounded(async {
            loop {
                let changed = self.changed.notified();
                tokio::pin!(changed);
                changed.as_mut().enable();
                if predicate(&self.calls.lock().unwrap()) {
                    return;
                }
                changed.await;
            }
        })
        .await;
    }
}
impl TaskSession for RecordingSession {
    fn identity(&self) -> &Arc<AgentIdentity> {
        self.actual.identity()
    }
    fn capture_execution(
        &self,
    ) -> BoxFuture<'_, ChildResult<Option<parrot_domain::values::agent_state::ExecutionCheckpoint>>>
    {
        self.actual.capture_execution()
    }
    fn recover(&self) -> ChildResult<parrot_runtime::task_scheduler::TaskExecution> {
        self.actual.recover()
    }
    fn cancel_recovered(&self, prompt_id: &str) -> ChildResult<Operation<Execution>> {
        self.actual.cancel_recovered(prompt_id)
    }
    fn suspend(&self) -> BoxFuture<'_, ChildResult<()>> {
        self.actual.suspend()
    }
    fn enqueue_claimed(
        &self,
        parts: Vec<ConversationPart>,
        id: String,
        claim: parrot_domain::values::agent_state::TaskResultClaim,
        cancellation: Arc<dyn Cancellation>,
    ) -> ChildResult<TaskExecution> {
        if let Some(hook) = self.enqueue_hook.lock().unwrap().take() {
            hook();
        }
        self.capture("enqueue", &id, &parts);
        self.actual.enqueue_claimed(parts, id, claim, cancellation)
    }
    fn capture_result<'a>(
        &'a self,
        prompt_id: &'a str,
        claim: &'a parrot_domain::values::agent_state::TaskResultClaim,
    ) -> BoxFuture<'a, ChildResult<parrot_domain::values::agent_state::TaskResponseReference>> {
        self.actual.capture_result(prompt_id, claim)
    }
    fn acknowledge_result<'a>(
        &'a self,
        reference: &'a parrot_domain::values::agent_state::TaskResponseReference,
        claim: &'a parrot_domain::values::agent_state::TaskResultClaim,
    ) -> BoxFuture<'a, ChildResult<()>> {
        Box::pin(async move {
            if self
                .ack_failures
                .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |left| {
                    left.checked_sub(1)
                })
                .is_ok()
            {
                return Err(ChildError::failure(
                    "io",
                    "claim acknowledgement write failed",
                ));
            }
            self.actual.acknowledge_result(reference, claim).await
        })
    }
    fn restore_response<'a>(
        &'a self,
        reference: &'a parrot_domain::values::agent_state::TaskResponseReference,
    ) -> BoxFuture<'a, ChildResult<Execution>> {
        self.actual.restore_response(reference)
    }
    fn enqueue(
        &self,
        parts: Vec<ConversationPart>,
        id: String,
        cancellation: Arc<dyn Cancellation>,
    ) -> ChildResult<Operation<Execution>> {
        let hook = self.enqueue_hook.lock().unwrap().take();
        if let Some(hook) = hook {
            hook();
        }
        self.capture("enqueue", &id, &parts);
        self.actual.enqueue(parts, id, cancellation)
    }
    fn record(
        &self,
        parts: Vec<ConversationPart>,
        id: String,
        delivery: Delivery,
    ) -> BoxFuture<'_, ChildResult<()>> {
        Box::pin(async move {
            self.capture("record", &id, &parts);
            let gate = self.record_gate.lock().unwrap().take();
            if let Some(gate) = gate {
                gate.wait(&parrot_storage::events::NeverCancelled)
                    .await
                    .unwrap();
            }
            self.actual.record(parts, id, delivery).await
        })
    }
    fn send(
        &self,
        parts: Vec<ConversationPart>,
        id: String,
        delivery: Delivery,
        activity: IncomingActivity,
    ) -> BoxFuture<'_, ChildResult<()>> {
        Box::pin(async move {
            self.capture("send", &id, &parts);
            if self
                .send_failures
                .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |left| {
                    left.checked_sub(1)
                })
                .is_ok()
            {
                return Err(ChildError::failure(
                    "notification",
                    "PRIVATE delivery payload",
                ));
            }
            self.actual.send(parts, id, delivery, activity).await
        })
    }
}

#[derive(Default)]
pub struct GraphFaults {
    pub owner: Mutex<String>,
    pub fail_before: std::sync::atomic::AtomicBool,
    pub fail_after: std::sync::atomic::AtomicBool,
    pub fail_ack: std::sync::atomic::AtomicBool,
    pub fail_response: std::sync::atomic::AtomicBool,
    pub fail_stopped: std::sync::atomic::AtomicBool,
}
pub struct GraphState {
    inner: Arc<dyn parrot_domain::contracts::agent_state::AgentStateStore>,
    faults: Arc<GraphFaults>,
    owner: String,
}
impl GraphState {
    pub fn bind(
        inner: Arc<dyn parrot_domain::contracts::agent_state::AgentStateStore>,
        faults: Arc<GraphFaults>,
        owner: String,
    ) -> Arc<dyn parrot_domain::contracts::agent_state::AgentStateStore> {
        Arc::new(Self {
            inner,
            faults,
            owner,
        })
    }
}
impl parrot_domain::contracts::agent_state::AgentStateStore for GraphState {
    fn load(
        &self,
    ) -> BoxFuture<
        '_,
        Result<
            Option<parrot_domain::values::agent_state::AgentStateSnapshot>,
            parrot_domain::contracts::agent_state::AgentStateError,
        >,
    > {
        self.inner.load()
    }
    fn initialize(
        &self,
        value: parrot_domain::values::agent_state::AgentStateSnapshot,
    ) -> BoxFuture<'_, Result<(), parrot_domain::contracts::agent_state::AgentStateError>> {
        self.inner.initialize(value)
    }
    fn retire(
        &self,
    ) -> BoxFuture<'_, Result<(), parrot_domain::contracts::agent_state::AgentStateError>> {
        self.inner.retire()
    }
    fn update(
        &self,
        value: parrot_domain::values::agent_state::AgentStateUpdate,
    ) -> BoxFuture<
        '_,
        Result<
            parrot_domain::values::agent_state::AgentStateSnapshot,
            parrot_domain::contracts::agent_state::AgentStateError,
        >,
    > {
        Box::pin(async move {
            let selected = *self.faults.owner.lock().unwrap() == self.owner;
            let graph = matches!(
                &value,
                parrot_domain::values::agent_state::AgentStateUpdate::Tasks(_)
            );
            let failure = || {
                parrot_domain::contracts::agent_state::AgentStateError::new(
                    parrot_domain::contracts::agent_state::AgentStateErrorKind::Io,
                    "graph snapshot write failed",
                )
            };
            if selected && graph && self.faults.fail_before.swap(false, Ordering::SeqCst) {
                return Err(failure());
            }
            if let (true, parrot_domain::values::agent_state::AgentStateUpdate::Tasks(next)) =
                (selected, &value)
            {
                let old = self.inner.load().await?.unwrap();
                if next.entries.iter().any(|entry| {
                    entry
                        .runner
                        .as_ref()
                        .is_some_and(|runner| runner.response_reference.is_some())
                }) && old.tasks.entries.iter().all(|entry| {
                    entry
                        .runner
                        .as_ref()
                        .is_none_or(|runner| runner.response_reference.is_none())
                }) && self.faults.fail_response.swap(false, Ordering::SeqCst)
                {
                    return Err(failure());
                }
                if next.notifications.len() < old.tasks.notifications.len()
                    && self.faults.fail_ack.swap(false, Ordering::SeqCst)
                {
                    return Err(failure());
                }
            }
            let stopped = matches!(&value, parrot_domain::values::agent_state::AgentStateUpdate::Tasks(graph) if graph.entries.iter().any(|entry| entry.pending_stop.as_ref().is_some_and(|stop| stop.stopped)));
            let saved = self.inner.update(value).await?;
            if selected && stopped && self.faults.fail_stopped.swap(false, Ordering::SeqCst) {
                return Err(failure());
            }
            if selected && graph && self.faults.fail_after.swap(false, Ordering::SeqCst) {
                return Err(failure());
            }
            Ok(saved)
        })
    }
}
