//! Turn-owned service boundaries. Concrete settlement, status and security stay with their owners.
use super::provider_call::{OutputWriter, ProviderActivity, ProviderCallError};
use crate::{
    compaction::{ContextController, ContextError, ContextEvents, ContextStatus},
    event_broker::BrokerError,
    prompt_context::{AgentSkills, PromptError, PromptResult, SkillRendering},
};
use parrot_domain::contracts::{
    async_ops::{BoxFuture, Cancellation, OperationError},
    filesystem::AuthorizedReadService,
    security::{AccessSnapshot, SecurityProvider},
};
use parrot_provider::{routing::aliases::ResolvedModelSelection, sessions};
use parrot_storage::{
    conversation::AgentHistoryView,
    events::{ConversationPart, EventError},
};
use std::{fmt, sync::Arc};

#[derive(Clone, Debug)]
pub struct SessionIdentity {
    pub session_id: String,
    pub name: String,
    pub parent_session_id: String,
    pub depth: i32,
}

#[derive(Clone, Debug)]
pub struct TurnProfile {
    pub id: String,
    pub maximum_requests: i32,
    pub allowed_tools: Option<Vec<String>>,
    pub disabled_tools: Vec<String>,
}
impl TurnProfile {
    pub fn is_tool_permitted(&self, name: &str) -> bool {
        self.allowed_tools
            .as_ref()
            .is_none_or(|allowed| allowed.iter().any(|item| item == name))
            && !self.disabled_tools.iter().any(|item| item == name)
    }
}

pub struct TurnSelection<S: AccessSnapshot> {
    pub model: ResolvedModelSelection,
    pub profile: TurnProfile,
    pub security: Arc<S>,
}
impl<S: AccessSnapshot> Clone for TurnSelection<S> {
    fn clone(&self) -> Self {
        Self {
            model: self.model.clone(),
            profile: self.profile.clone(),
            security: self.security.clone(),
        }
    }
}

/// Capturing resolves the current full selection and owns immutable authority;
/// a profile's display text is never interpreted as a grant by this runtime.
pub trait SelectionSource<S: AccessSnapshot>: Send + Sync {
    fn capture(&self) -> Result<TurnSelection<S>, TurnError>;
    fn update(&self, selection: TurnSelection<S>) -> Result<(), TurnError>;
}

/// In-drain context controls borrow the already-owned state. They must not
/// schedule-and-wait on this drain or lock its backend again. T47 preserves the
/// source sequential classification of mutating tools while batching safe tools.
pub struct TurnResources<'a> {
    pub history: &'a AgentHistoryView,
    pub conversation: &'a mut parrot_storage::conversation::AgentConversation,
    pub context: &'a mut ContextController,
    pub prompt: &'a crate::prompt_context::SessionPrompt,
    pub sessions: &'a sessions::ProviderSessions,
    pub status: &'a dyn ContextStatus,
    pub events: &'a dyn ContextEvents,
    pub usage: &'a super::usage::SessionUsageRecorder,
}

/// The snapshot retains the concrete factory/tool identity in its owner-chosen
/// type. Runtime cannot replace it with a names-only reconstructed catalog.
pub trait TurnTools<S: AccessSnapshot>: Send + Sync {
    type Snapshot: Clone + Send + Sync;
    fn materialize(&self) -> Result<Self::Snapshot, TurnError>;
    fn permitted(
        &self,
        snapshot: &Self::Snapshot,
        selection: &TurnSelection<S>,
    ) -> Result<Self::Snapshot, TurnError>;
    fn definitions<'a>(&self, snapshot: &'a Self::Snapshot) -> &'a [sessions::Tool];
    fn enabled_after_interruption(&self, snapshot: &Self::Snapshot) -> Self::Snapshot;
    fn reconcile<'a>(
        &'a self,
        resources: TurnResources<'a>,
        selection: &'a TurnSelection<S>,
        snapshot: &'a Self::Snapshot,
        capture: &'a dyn SelectionSource<S>,
        cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<(), TurnError>>;
}

pub struct CompletionCandidate<S: AccessSnapshot> {
    pub session_id: String,
    pub message_id: String,
    pub assistant: sessions::Message,
    pub selection: TurnSelection<S>,
}
pub enum CompletionDecision {
    Complete,
    Retry {
        system_message: String,
        retain_candidate_assistant: bool,
        completion_retry_pending: Option<bool>,
    },
}
pub trait TurnCompletion<S: AccessSnapshot>: Send + Sync {
    fn complete<'a>(
        &'a self,
        candidate: &'a CompletionCandidate<S>,
        cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<CompletionDecision, TurnError>>;
}

pub trait TurnSkills: Send + Sync {
    fn begin_turn<'a>(
        &'a self,
        cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, PromptResult<()>>;
    fn select<'a>(
        &'a self,
        parts: &'a [ConversationPart],
        cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, PromptResult<()>>;
    fn render<'a>(
        &'a self,
        cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, PromptResult<SkillRendering>>;
    fn end_turn(&self) -> BoxFuture<'_, ()>;
}
impl<R, P> TurnSkills for tokio::sync::Mutex<AgentSkills<R, P>>
where
    R: AuthorizedReadService + ?Sized,
    P: SecurityProvider<Snapshot = R::Security>,
{
    fn begin_turn<'a>(
        &'a self,
        cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, PromptResult<()>> {
        Box::pin(async move { self.lock().await.begin_turn(cancellation).await })
    }
    fn select<'a>(
        &'a self,
        parts: &'a [ConversationPart],
        cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, PromptResult<()>> {
        Box::pin(async move { self.lock().await.select(parts, cancellation).await })
    }
    fn render<'a>(
        &'a self,
        cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, PromptResult<SkillRendering>> {
        Box::pin(async move { self.lock().await.render(cancellation).await })
    }
    fn end_turn(&self) -> BoxFuture<'_, ()> {
        Box::pin(async move {
            self.lock().await.end_turn();
        })
    }
}

pub trait TurnStatus<S: AccessSnapshot>: Send + Sync {
    fn bind(&self, selection: &TurnSelection<S>) -> Arc<dyn ContextStatus>;
}

pub trait SessionOutput: OutputWriter {
    fn close(&self) -> BoxFuture<'_, std::io::Result<()>>;
}

pub trait SessionActivity: ProviderActivity {
    fn change_state(&self, state: super::lifecycle::DrainState);
    fn begin_execution(&self) -> i64;
    fn execution_duration(&self) -> std::time::Duration;
    fn finish_execution(&self, execution: i64, outcome: &super::lifecycle::Execution);
    /// Safe bounded classification only; never provider bodies/raw error metadata.
    fn turn_started(&self, turn_id: &str);
    fn turn_finished(&self, turn_id: &str, outcome: &str, error_code: Option<&str>);
    fn assistant_message(&self, message: &str);
}

/// Delivers an execution outcome to the owning parent, when configured.
pub trait ParentCompletion: Send + Sync {
    fn deliver<'a>(
        &'a self,
        execution: &'a super::lifecycle::Execution,
    ) -> BoxFuture<'a, Result<(), TurnError>>;
    /// Uses a stable message identity so recovery can retry durable admission.
    fn deliver_correlated<'a>(
        &'a self,
        execution: &'a super::lifecycle::Execution,
        message_id: &'a str,
    ) -> BoxFuture<'a, Result<(), TurnError>>;
}

#[derive(Debug)]
pub enum TurnError {
    Storage(EventError),
    ProviderCall(ProviderCallError),
    Provider(sessions::Error),
    Context(ContextError),
    Planning(crate::compaction::PlanningError),
    Prompt(PromptError),
    Publication(OperationError<BrokerError>),
    Service(String),
    Cancelled,
}
impl TurnError {
    pub fn response_body(&self) -> &str {
        match self {
            Self::Provider(error) | Self::ProviderCall(ProviderCallError::Provider(error)) => {
                error.response_body()
            }
            Self::Context(error) => error.response_body(),
            _ => "",
        }
    }
    pub fn lifecycle_error(&self) -> super::lifecycle::LifecycleError {
        if self.is_cancelled() {
            super::lifecycle::LifecycleError::Cancelled
        } else {
            super::lifecycle::LifecycleError::Failed(self.to_string())
        }
    }
    pub fn diagnostic_code(&self) -> &'static str {
        use parrot_provider::sessions::{
            diagnostic_session::failure_kind, diagnostics::FailureKind,
        };
        if self.is_cancelled() {
            return "cancelled";
        }
        match self {
            Self::Provider(error) | Self::ProviderCall(ProviderCallError::Provider(error)) => {
                failure_kind(error).code()
            }
            Self::ProviderCall(ProviderCallError::Output(error)) => {
                FailureKind::from_io(error).code()
            }
            Self::Storage(error) | Self::ProviderCall(ProviderCallError::Storage(error)) => {
                storage_code(error)
            }
            Self::Context(ContextError::Compaction(
                crate::compaction::CompactionError::Provider(error),
            )) => failure_kind(error).code(),
            Self::Context(ContextError::History(error)) => storage_code(error),
            Self::Context(ContextError::InvalidOperation(_)) => "invalid_operation",
            Self::Context(ContextError::Argument(_)) => "argument",
            Self::Planning(error)
            | Self::Context(ContextError::Compaction(
                crate::compaction::CompactionError::Planning(error),
            )) => planning_code(*error),
            Self::Context(ContextError::Identifier(error)) => FailureKind::from_io(error).code(),
            Self::Prompt(error) | Self::Context(ContextError::Prompt(error)) => prompt_code(error),
            Self::Context(ContextError::Compaction(
                crate::compaction::CompactionError::InvalidOperation(_),
            )) => "invalid_operation",
            Self::Context(ContextError::Compaction(crate::compaction::CompactionError::Blob(
                error,
            ))) => match error.kind {
                parrot_storage::artifacts::compaction::CompactionErrorKind::Io => error
                    .native_error
                    .as_ref()
                    .map_or("io", |error| FailureKind::from_io(error).code()),
                parrot_storage::artifacts::compaction::CompactionErrorKind::InvalidOperation => {
                    "invalid_operation"
                }
                _ => "unexpected",
            },
            _ => "unexpected",
        }
    }
    pub fn is_cancelled(&self) -> bool {
        if let Self::Provider(error) | Self::ProviderCall(ProviderCallError::Provider(error)) = self
        {
            return provider_cancelled(error);
        }
        if let Self::Context(error) = self {
            return match error {
                ContextError::Cancelled
                | ContextError::Prompt(PromptError::Cancelled)
                | ContextError::Compaction(crate::compaction::CompactionError::Cancelled) => true,
                ContextError::Compaction(crate::compaction::CompactionError::Provider(error)) => {
                    provider_cancelled(error)
                }
                ContextError::Compaction(crate::compaction::CompactionError::Blob(error)) => {
                    error.kind
                        == parrot_storage::artifacts::compaction::CompactionErrorKind::Cancelled
                }
                _ => false,
            };
        }
        matches!(
            self,
            Self::Cancelled
                | Self::Prompt(PromptError::Cancelled)
                | Self::Publication(OperationError::Cancelled)
                | Self::ProviderCall(ProviderCallError::Publication(OperationError::Cancelled))
        )
    }
}
fn planning_code(error: crate::compaction::PlanningError) -> &'static str {
    match error {
        crate::compaction::PlanningError::TokenOverflow => "unexpected",
        _ => "invalid_operation",
    }
}
fn storage_code(error: &EventError) -> &'static str {
    match error {
        EventError::Database(parrot_storage::database::DatabaseError::Io(error)) => {
            parrot_provider::sessions::diagnostics::FailureKind::from_io(error).code()
        }
        EventError::Database(parrot_storage::database::DatabaseError::Protobuf(_)) => "io",
        EventError::Database(parrot_storage::database::DatabaseError::Closed)
        | EventError::Invalid(_)
        | EventError::Conflict(_) => "invalid_operation",
        EventError::Argument(_) => "argument",
        _ => "unexpected",
    }
}
fn prompt_code(error: &PromptError) -> &'static str {
    use parrot_domain::contracts::filesystem::FileReadErrorKind;
    match error {
        PromptError::Cancelled => "cancelled",
        PromptError::InvalidProviderKey => "argument",
        PromptError::Read(error) => match error.kind {
            FileReadErrorKind::Denied => "access_denied",
            FileReadErrorKind::InvalidPath => "argument",
            FileReadErrorKind::InvalidOperation => "invalid_operation",
            FileReadErrorKind::NotSupported => "unexpected",
            _ => "io",
        },
        _ => "unexpected",
    }
}
fn provider_cancelled(error: &sessions::Error) -> bool {
    matches!(
        error,
        sessions::Error::Cancelled
            | sessions::Error::Cancellation(_)
            | sessions::Error::Http(
                parrot_provider::http_stream::HttpError::Cancelled
                    | parrot_provider::http_stream::HttpError::RequestCancelled(_)
            )
            | sessions::Error::Socket(
                parrot_provider::responses_websocket::WebSocketError::Cancelled
            )
    )
}
impl fmt::Display for TurnError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Storage(error) => error.fmt(f),
            Self::ProviderCall(error) => error.fmt(f),
            Self::Provider(error) => error.fmt(f),
            Self::Context(error) => error.fmt(f),
            Self::Planning(error) => error.fmt(f),
            Self::Prompt(error) => error.fmt(f),
            Self::Publication(error) => error.fmt(f),
            Self::Service(message) => f.write_str(message),
            Self::Cancelled => f.write_str("The operation was canceled."),
        }
    }
}
impl std::error::Error for TurnError {}
impl From<EventError> for TurnError {
    fn from(value: EventError) -> Self {
        Self::Storage(value)
    }
}
impl From<ProviderCallError> for TurnError {
    fn from(value: ProviderCallError) -> Self {
        Self::ProviderCall(value)
    }
}
impl From<ContextError> for TurnError {
    fn from(value: ContextError) -> Self {
        Self::Context(value)
    }
}
impl From<PromptError> for TurnError {
    fn from(value: PromptError) -> Self {
        Self::Prompt(value)
    }
}
impl From<sessions::Error> for TurnError {
    fn from(value: sessions::Error) -> Self {
        Self::Provider(value)
    }
}
