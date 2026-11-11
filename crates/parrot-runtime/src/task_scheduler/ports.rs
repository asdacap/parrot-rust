use crate::{
    agent_turn::lifecycle::{Execution, IncomingActivity},
    children::{AgentIdentity, AgentRegistry, ChildError, ChildResult, Operation},
    event_broker::BrokerError,
};
use parrot_domain::{
    contracts::{
        agent_state::AgentStateStore,
        agent_tasks::TaskArtifactParser,
        async_ops::{BoxFuture, Cancellation},
        events::EventPublisher,
        security::RestrictableAccessSnapshot,
    },
    protocol::messages::parrot::{Delivery, Event},
    values::agent_state::{
        ExecutionCheckpoint, TaskResponseReference, TaskResultClaim, TaskRunnerCheckpoint,
    },
};
use parrot_foundation::template_catalog::PromptTemplateCatalog;
use parrot_provider::routing::aliases::{ModelRouter, ResolvedModelSelection};
use parrot_storage::{
    artifacts::output_blobs::ToolOutputBlobStore, conversation::AgentHistoryView,
    events::ConversationPart,
};
use std::sync::{Arc, Weak};

pub struct TaskExecution {
    pub execution: Operation<Execution>,
    pub reservation: Operation<()>,
}

/// Additional T50 operations on the same actual session used by T48's adapter.
/// Register the exact `Arc<dyn TaskSession>` in `ScopeParts.services`.
pub trait TaskSession: Send + Sync {
    fn identity(&self) -> &Arc<AgentIdentity>;
    /// Reads the worker's durable execution correlation without activating it.
    fn capture_execution(&self) -> BoxFuture<'_, ChildResult<Option<ExecutionCheckpoint>>>;
    /// Reserves the retained owned execution without admitting another prompt.
    fn recover(&self) -> ChildResult<TaskExecution>;
    /// Cancels only the saved matching input, without starting provider work.
    fn cancel_recovered(&self, prompt_id: &str) -> ChildResult<Operation<Execution>>;
    /// Joins the worker while preserving unfinished durable execution intent.
    fn suspend(&self) -> BoxFuture<'_, ChildResult<()>>;
    /// Admits a task prompt with a protected result claim and actual reservation readiness.
    fn enqueue_claimed(
        &self,
        parts: Vec<ConversationPart>,
        message_id: String,
        claim: TaskResultClaim,
        cancellation: Arc<dyn Cancellation>,
    ) -> ChildResult<TaskExecution>;
    /// Captures the exact held terminal before the parent records its response reference.
    fn capture_result<'a>(
        &'a self,
        prompt_id: &'a str,
        claim: &'a TaskResultClaim,
    ) -> BoxFuture<'a, ChildResult<TaskResponseReference>>;
    /// Releases only the matching claim after the parent durably saved its reference.
    fn acknowledge_result<'a>(
        &'a self,
        reference: &'a TaskResponseReference,
        claim: &'a TaskResultClaim,
    ) -> BoxFuture<'a, ChildResult<()>>;
    /// Releases a retiring result without waking pending conversation input.
    fn acknowledge_retired_result<'a>(
        &'a self,
        reference: &'a TaskResponseReference,
        claim: &'a TaskResultClaim,
    ) -> BoxFuture<'a, ChildResult<()>>;
    /// Cancels a known pending notification without removing conversation history.
    fn cancel_notification<'a>(&'a self, message_id: &'a str) -> BoxFuture<'a, ChildResult<()>>;
    /// Restores a saved exact result independently of the worker's latest execution.
    fn restore_response<'a>(
        &'a self,
        reference: &'a TaskResponseReference,
    ) -> BoxFuture<'a, ChildResult<Execution>>;
    fn enqueue(
        &self,
        parts: Vec<ConversationPart>,
        message_id: String,
        cancellation: Arc<dyn Cancellation>,
    ) -> ChildResult<Operation<Execution>>;
    fn record(
        &self,
        parts: Vec<ConversationPart>,
        message_id: String,
        delivery: Delivery,
    ) -> BoxFuture<'_, ChildResult<()>>;
    fn send(
        &self,
        parts: Vec<ConversationPart>,
        message_id: String,
        delivery: Delivery,
        activity: IncomingActivity,
    ) -> BoxFuture<'_, ChildResult<()>>;
}

pub trait TaskModelRouter: Send + Sync {
    fn resolve(&self, selector: &str) -> ChildResult<ResolvedModelSelection>;
}
impl TaskModelRouter for ModelRouter {
    fn resolve(&self, selector: &str) -> ChildResult<ResolvedModelSelection> {
        self.resolve(selector)
            .map_err(|error| ChildError::failure("argument", error.to_string()))
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum ForkHistoryMode {
    #[default]
    Dependency,
    Parent,
    Empty,
}
#[derive(Clone)]
pub struct TaskConfiguration {
    pub maximum_attempts: usize,
    pub maximum_response_repairs: usize,
    pub fork_history_mode: ForkHistoryMode,
    pub templates: Arc<PromptTemplateCatalog>,
}
impl TaskConfiguration {
    /// Defaults from the pinned `predefined_config.agent_tasks.yaml`.
    pub fn defaults(templates: Arc<PromptTemplateCatalog>) -> Self {
        Self {
            maximum_attempts: 5,
            maximum_response_repairs: 3,
            fork_history_mode: ForkHistoryMode::Dependency,
            templates,
        }
    }
}

/// No task names, instructions, model selectors, failure bodies, or policy data.
pub struct TaskDiagnostic {
    pub operation: &'static str,
    pub session_id: String,
    pub correlation_id: String,
    pub outcome: &'static str,
    pub duration_milliseconds: Option<u64>,
    pub error_code: Option<&'static str>,
}
pub trait TaskDiagnostics: Send + Sync {
    fn record(&self, event: TaskDiagnostic);
}
pub type TaskPublisher = dyn EventPublisher<Event = Event, Error = BrokerError>;

pub struct SchedulerConfig<S: RestrictableAccessSnapshot + 'static, P: TaskArtifactParser + 'static>
{
    pub owner_identity: Arc<AgentIdentity>,
    pub state: Arc<dyn AgentStateStore>,
    /// Weak authority avoids a registry -> scope -> scheduler -> registry cycle.
    pub registry: Weak<AgentRegistry<S>>,
    pub owner_session: Arc<dyn TaskSession>,
    pub history: AgentHistoryView,
    pub parser: Arc<P>,
    pub router: Arc<dyn TaskModelRouter>,
    pub configuration: TaskConfiguration,
    pub concurrency: Arc<super::TaskConcurrencyBudget>,
    pub output_blobs: Arc<ToolOutputBlobStore>,
    pub publisher: Arc<TaskPublisher>,
    pub diagnostics: Arc<dyn TaskDiagnostics>,
    pub lifetime: Arc<dyn Cancellation>,
}

pub(crate) struct RunCancellation {
    pub lifetime: Arc<dyn Cancellation>,
    pub stop: Arc<crate::agent_turn::lifecycle::Signal>,
}
impl Cancellation for RunCancellation {
    fn is_cancelled(&self) -> bool {
        self.stop.is_cancelled() || self.lifetime.is_cancelled()
    }
    fn cancelled(&self) -> BoxFuture<'_, ()> {
        Box::pin(async move {
            tokio::select! { _ = self.stop.cancelled() => {}, _ = self.lifetime.cancelled() => {} }
        })
    }
}

pub(crate) fn render(
    config: &TaskConfiguration,
    id: &str,
    values: &[(&str, &str)],
) -> ChildResult<String> {
    let arguments: Vec<_> = values
        .iter()
        .map(|(name, value)| {
            (
                (*name).to_owned(),
                serde_json::Value::String((*value).to_owned()),
            )
        })
        .collect();
    config
        .templates
        .render(id, &arguments, &parrot_storage::events::NeverCancelled)
        .map_err(|error| ChildError::failure("argument", error.to_string()))
}

#[derive(Clone)]
pub(crate) struct CapturedSelection {
    pub profile_id: String,
    pub model: ResolvedModelSelection,
}

/// Exclusive prompt/result handoff for one retained worker.
pub struct TaskWorkerLease {
    pub(crate) _guard: tokio::sync::OwnedMutexGuard<()>,
    pub(crate) changed: Arc<tokio::sync::Notify>,
}
impl Drop for TaskWorkerLease {
    fn drop(&mut self) {
        self.changed.notify_waiters();
    }
}

/// Persists a runner generation and announces its first owned reservation.
pub trait TaskRunProgress: Send + Sync {
    /// Identifies this graph as the owner of a stable prompt result.
    fn claim(&self, prompt_id: &str) -> TaskResultClaim;
    /// Waits outside worker lifecycle slots until this prompt may claim its worker.
    fn acquire<'a>(
        &'a self,
        checkpoint: &'a TaskRunnerCheckpoint,
        session: &'a dyn TaskSession,
    ) -> BoxFuture<'a, ChildResult<TaskWorkerLease>>;
    /// Rejects stale generations and commits progress before new prompt admission.
    fn save(&self, checkpoint: TaskRunnerCheckpoint) -> BoxFuture<'_, ChildResult<()>>;
    /// Releases the startup barrier once an owned worker operation is reserved.
    fn reserve(&self);
}

pub(crate) fn state_error(
    error: parrot_domain::contracts::agent_state::AgentStateError,
) -> ChildError {
    ChildError::failure("io", error.to_string())
}
