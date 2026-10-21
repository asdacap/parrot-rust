use crate::{
    agent_turn::{
        lifecycle::{AgentSession, DrainBackend, Execution, IncomingActivity},
        services::TurnSelection,
    },
    children::{AgentIdentity, AgentRegistry, ChildError, ChildResult, Operation},
    event_broker::BrokerError,
};
use parrot_domain::{
    contracts::{
        agent_tasks::TaskArtifactParser,
        async_ops::{BoxFuture, Cancellation},
        events::EventPublisher,
        security::RestrictableAccessSnapshot,
    },
    protocol::messages::parrot::{Delivery, Event},
};
use parrot_foundation::template_catalog::PromptTemplateCatalog;
use parrot_provider::routing::aliases::{ModelRouter, ResolvedModelSelection};
use parrot_storage::{
    artifacts::output_blobs::ToolOutputBlobStore, conversation::AgentHistoryView,
    events::ConversationPart,
};
use std::sync::{Arc, Weak};

/// Additional T50 operations on the same actual session used by T48's adapter.
/// Register the exact `Arc<dyn TaskSession>` in `ScopeParts.services`.
pub trait TaskSession: Send + Sync {
    fn identity(&self) -> &Arc<AgentIdentity>;
    fn enqueue(
        &self,
        parts: Vec<ConversationPart>,
        message_id: String,
        cancellation: Arc<dyn Cancellation>,
    ) -> ChildResult<Operation<Execution>>;
    fn send_and_wait(
        &self,
        parts: Vec<ConversationPart>,
        message_id: String,
        cancellation: Arc<dyn Cancellation>,
    ) -> BoxFuture<'_, ChildResult<Execution>> {
        Box::pin(async move { self.enqueue(parts, message_id, cancellation)?.join().await })
    }
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

/// Construct with a clone of the actual `AgentSession` supplied to T48.
pub struct TaskSessionAdapter<B: DrainBackend> {
    identity: Arc<AgentIdentity>,
    session: AgentSession<B>,
}
impl<B: DrainBackend> TaskSessionAdapter<B> {
    pub fn new(identity: Arc<AgentIdentity>, session: AgentSession<B>) -> Self {
        Self { identity, session }
    }
    pub fn session(&self) -> &AgentSession<B> {
        &self.session
    }
}
impl<B: DrainBackend> TaskSession for TaskSessionAdapter<B> {
    fn identity(&self) -> &Arc<AgentIdentity> {
        &self.identity
    }
    fn enqueue(
        &self,
        parts: Vec<ConversationPart>,
        message_id: String,
        cancellation: Arc<dyn Cancellation>,
    ) -> ChildResult<Operation<Execution>> {
        let execution =
            self.session
                .enqueue_execution(parts, message_id, None, None, cancellation)?;
        // Join, rather than cancellation-sensitive observation: stop recording and
        // a restarted run must follow the actual execution's terminal callbacks.
        Ok(Operation::start(async move {
            execution.join().await.map_err(Into::into)
        }))
    }
    fn record(
        &self,
        parts: Vec<ConversationPart>,
        message_id: String,
        delivery: Delivery,
    ) -> BoxFuture<'_, ChildResult<()>> {
        Box::pin(async move {
            self.session
                .record_handle(
                    parts,
                    message_id,
                    delivery,
                    Arc::new(parrot_storage::events::NeverCancelled),
                )?
                .join()
                .await?;
            Ok(())
        })
    }
    fn send(
        &self,
        parts: Vec<ConversationPart>,
        message_id: String,
        delivery: Delivery,
        activity: IncomingActivity,
    ) -> BoxFuture<'_, ChildResult<()>> {
        Box::pin(async move {
            self.session
                .send_handle(
                    parts,
                    message_id,
                    delivery,
                    activity,
                    Arc::new(parrot_storage::events::NeverCancelled),
                )?
                .join()
                .await?;
            Ok(())
        })
    }
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
    /// Weak authority avoids a registry -> scope -> scheduler -> registry cycle.
    pub registry: Weak<AgentRegistry<S>>,
    pub owner_session: Arc<dyn TaskSession>,
    pub history: AgentHistoryView,
    pub parser: Arc<P>,
    pub router: Arc<dyn TaskModelRouter>,
    pub configuration: TaskConfiguration,
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

// Keep the full captured selection at initial entry admission, not only its model.
pub(crate) type CapturedSelection<S> = TurnSelection<S>;
