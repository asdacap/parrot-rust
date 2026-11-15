//! Awaited context epochs over the authoritative storage history.
use super::effects::{NativeErrors, describe_error};
use super::{CompactionError, CompactionObserver, ContextSize, ContextSnapshot};
use crate::prompt_context::{PromptError, SessionPrompt};
use parrot_core::{
    ContextConfiguration as CoreContextConfiguration, NeverCancelled, PlanningError,
};
use parrot_domain::{
    contracts::{
        async_ops::{BoxFuture, Cancellation},
        storage::Committed,
    },
    protocol::messages::parrot::{
        AgentNotice, CompactionFailed, CompactionFinished, CompactionPhase,
        CompactionProgressEvent, CompactionStarted, Event,
    },
    values::identifiers::Identifier,
};
use parrot_provider::compaction::{Compactor, estimate_input_tokens};
use parrot_provider::{
    routing::aliases::ResolvedModelSelection,
    sessions::{Content, Message, ProviderSessions, Role, Tool, ToolCall},
};
use parrot_storage::conversation::{
    AgentConversation, AgentHistoryView, CompactionSnapshot, EventError, MessageRole, ModelContent,
    ModelMessage,
};
use std::fmt;

#[derive(Debug)]
pub enum ContextError {
    Compaction(CompactionError),
    History(EventError),
    Prompt(PromptError),
    Identifier(std::io::Error),
    InvalidOperation(String),
    Argument(String),
    Operation(String),
    Core(parrot_core::Error),
    Cancelled,
}
impl ContextError {
    pub fn response_body(&self) -> &str {
        match self {
            Self::Compaction(error) => error.response_body(),
            Self::Core(error) => &error.response_body,
            _ => "",
        }
    }
}
impl fmt::Display for ContextError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Compaction(error) => error.fmt(f),
            Self::Core(error) => error.fmt(f),
            Self::History(error) => error.fmt(f),
            Self::Prompt(error) => error.fmt(f),
            Self::Identifier(error) => error.fmt(f),
            Self::InvalidOperation(message)
            | Self::Argument(message)
            | Self::Operation(message) => f.write_str(message),
            Self::Cancelled => f.write_str("The operation was canceled."),
        }
    }
}
impl std::error::Error for ContextError {}
impl From<CompactionError> for ContextError {
    fn from(error: CompactionError) -> Self {
        Self::Compaction(error)
    }
}
impl From<PlanningError> for ContextError {
    fn from(error: PlanningError) -> Self {
        Self::Compaction(CompactionError::Planning(error))
    }
}
impl From<EventError> for ContextError {
    fn from(error: EventError) -> Self {
        Self::History(error)
    }
}
impl From<PromptError> for ContextError {
    fn from(error: PromptError) -> Self {
        Self::Prompt(error)
    }
}
impl From<parrot_core::CompactionFailure> for ContextError {
    fn from(failure: parrot_core::CompactionFailure) -> Self {
        match failure {
            parrot_core::CompactionFailure::NoSafeBoundary
            | parrot_core::CompactionFailure::FittedInputLimitExceeded => {
                Self::InvalidOperation(failure.to_string())
            }
            failure => Self::Compaction(failure.into()),
        }
    }
}
pub type ContextResult<T> = Result<T, ContextError>;

/// Offers the status slots for the bound agent, profile, and selection using the supplied context.
pub trait ContextStatus: Send + Sync {
    /// Offers the standing status slots for a safe point: source-computed
    /// hashes in registry order, over the supplied usage snapshot. Defaults
    /// to no slots, which steers nothing.
    fn slots<'a>(
        &'a self,
        _context: &'a ContextSnapshot,
        _cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, ContextResult<parrot_core::StatusOffering>> {
        Box::pin(async { Ok(parrot_core::StatusOffering::default()) })
    }
}

/// `emit` persists a new event; `publish_committed` requires its real transaction receipt.
/// `publish` retains projection-only notices after their history effect commits;
/// source status/reminder projection-only operations deliberately do not record the event.
/// The caller must await this controller (including terminal publication) before draining.
pub trait ContextEvents: CompactionObserver {
    fn emit<'a>(
        &'a self,
        event: Event,
        cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, ContextResult<()>>;
    fn publish_committed<'a>(
        &'a self,
        event: Committed<Event>,
        cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, ContextResult<()>>;
    fn publish<'a>(
        &'a self,
        event: Event,
        cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, ContextResult<()>>;
}

pub struct EpochInput<'a> {
    pub selection: &'a ResolvedModelSelection,
    pub tools: &'a [Tool],
    pub conversation: &'a mut AgentConversation,
    pub agent_state: &'a mut parrot_core::AgentState,
    pub prompt: &'a SessionPrompt,
    pub sessions: &'a ProviderSessions,
    pub status: &'a dyn ContextStatus,
    pub events: &'a dyn ContextEvents,
}
#[derive(Debug)]
pub struct EpochResult {
    pub instructions: String,
    pub reduced: bool,
}
#[derive(Debug)]
pub struct ContextCompactionResult {
    pub context: ContextSnapshot,
    pub reduced: bool,
}

pub struct ContextController {
    compactor: Compactor,
    history: AgentHistoryView,
}
impl ContextController {
    pub fn new(compactor: Compactor, history: AgentHistoryView) -> Self {
        Self { compactor, history }
    }
    pub fn compactor(&self) -> &Compactor {
        &self.compactor
    }
    pub(crate) fn capture_context_configuration(
        &self,
        selection: &ResolvedModelSelection,
    ) -> CoreContextConfiguration {
        self.compactor.capture_context_configuration(selection)
    }

    pub async fn ensure_epoch(
        &mut self,
        prompt: &SessionPrompt,
        cancellation: &dyn Cancellation,
    ) -> ContextResult<()> {
        prompt.ensure_epoch(cancellation).await?;
        Ok(())
    }

    pub async fn estimate_context(
        &mut self,
        input: EpochInput<'_>,
        cancellation: &dyn Cancellation,
    ) -> ContextResult<ContextSnapshot> {
        self.ensure_epoch(input.prompt, cancellation).await?;
        let instructions = input.prompt.build(input.selection, cancellation).await?;
        self.estimate(&input, &instructions)
    }

    /// Explicit compaction bypasses the trigger, but not an unavailable input limit.
    pub async fn compact(
        &mut self,
        mut input: EpochInput<'_>,
        target: Option<&ContextSize>,
        cancellation: &dyn Cancellation,
    ) -> ContextResult<ContextCompactionResult> {
        cancellation.check().map_err(|_| ContextError::Cancelled)?;
        self.ensure_epoch(input.prompt, cancellation).await?;
        let instructions = input.prompt.build(input.selection, cancellation).await?;
        let before = self.estimate(&input, &instructions)?;
        if before.input_limit <= 0 {
            return Ok(ContextCompactionResult {
                context: before,
                reduced: false,
            });
        }
        let compacted = self
            .compact_epoch(reborrow(&mut input), target, cancellation)
            .await?;
        Ok(ContextCompactionResult {
            context: self.estimate(&input, &compacted.instructions)?,
            reduced: compacted.reduced,
        })
    }

    pub async fn compact_epoch(
        &mut self,
        input: EpochInput<'_>,
        target: Option<&ContextSize>,
        cancellation: &dyn Cancellation,
    ) -> ContextResult<EpochResult> {
        let compactor = self.compactor.clone();
        let errors = NativeErrors::new();
        let selection = input.selection;
        let sessions = input.sessions;
        let prompt = input.prompt;
        let request = parrot_core::CompactionRequest {
            target: target.map(Into::into),
            tools: input.tools.to_vec(),
        };
        let mut effects = EpochEffects {
            controller: self,
            input,
            errors: &errors,
        };
        let replacement = compactor
            .compact(selection, request, sessions, &mut effects, cancellation)
            .await
            .map_err(|error| errors.restore(error, ContextError::from))?;
        Ok(EpochResult {
            instructions: prompt.build(selection, cancellation).await?,
            reduced: replacement.is_some(),
        })
    }

    pub(crate) async fn publish_compaction(
        &self,
        event: parrot_provider::compaction::CompactionEvent,
        events: &dyn ContextEvents,
        cancellation: &dyn Cancellation,
    ) -> ContextResult<()> {
        use parrot_provider::compaction::CompactionEvent;
        let mut published = Event::new();
        match event {
            CompactionEvent::Started => published.set_compaction_started(CompactionStarted::new()),
            CompactionEvent::Saving => published.set_compaction_progress(CompactionProgressEvent {
                phase: CompactionPhase::COMPACTION_PHASE_SAVING.into(),
                ..CompactionProgressEvent::new()
            }),
            CompactionEvent::Finished => {
                published.set_compaction_finished(CompactionFinished::new())
            }
            CompactionEvent::Failed(message) => published.set_compaction_failed(CompactionFailed {
                message,
                ..CompactionFailed::new()
            }),
            CompactionEvent::Progress(progress) => {
                return events
                    .observe_progress(
                        CompactionProgressEvent {
                            phase: CompactionPhase::COMPACTION_PHASE_SUMMARIZING.into(),
                            completed_groups: progress.completed_groups as u64,
                            total_groups: progress.total_groups as u64,
                            ..CompactionProgressEvent::new()
                        },
                        cancellation,
                    )
                    .await
                    .map_err(CompactionError::from)
                    .map_err(ContextError::from);
            }
            CompactionEvent::Retry(event) => {
                return events
                    .observe(&event, cancellation)
                    .await
                    .map_err(CompactionError::from)
                    .map_err(ContextError::from);
            }
        }
        let identity = self.event()?;
        published.id = identity.id;
        published.agent_session_id = identity.agent_session_id;
        events.emit(published, cancellation).await
    }

    pub(crate) async fn read_conversation_state(
        &self,
        conversation: &AgentConversation,
    ) -> ContextResult<parrot_core::ConversationState> {
        let summary = self.read_conversation_summary().await?;
        let watermark = summary.as_ref().map_or(0, |summary| summary.watermark);
        let history = self.read_conversation_tail(conversation, watermark).await?;
        Ok(parrot_core::ConversationState { summary, history })
    }

    async fn read_conversation_summary(
        &self,
    ) -> ContextResult<Option<parrot_core::ConversationSummary>> {
        let snapshot = self
            .history
            .repository()
            .compaction(self.history.agent_session_id().into())
            .await?;
        let Some(snapshot) = snapshot else {
            return Ok(None);
        };
        let status_sequence = self
            .history
            .repository()
            .read_compaction_status_sequence(
                self.history.agent_session_id().into(),
                snapshot.watermark,
            )
            .await?;
        let message = snapshot.message;
        let message = ModelMessage {
            role: message.role,
            contents: self.history.materialize(message.parts).await?,
            tool_calls: message.tool_calls,
            tool_call_id: message.tool_call_id,
            provider_content: message.provider_content,
        };
        Ok(Some(parrot_core::ConversationSummary {
            message: provider_message(&message).into(),
            watermark: snapshot.watermark,
            status_sequence,
        }))
    }

    async fn read_conversation_tail(
        &self,
        conversation: &AgentConversation,
        after: i64,
    ) -> ContextResult<Vec<parrot_core::ConversationHistoryItem>> {
        let items = self
            .history
            .repository()
            .read_conversation_tail(self.history.agent_session_id().into(), after)
            .await?;
        let mut history = Vec::with_capacity(items.len());
        for item in items {
            history.push(parrot_core::ConversationHistoryItem {
                sequence: item.item.sequence,
                tool_owner_sequence: item.tool_owner_sequence,
                is_compaction_status: item.is_compaction_status,
                message: provider_message(&conversation.materialize(item.item).await?).into(),
            });
        }
        Ok(history)
    }

    pub(crate) async fn commit_replacement(
        &self,
        conversation: &mut AgentConversation,
        events: &dyn ContextEvents,
        replacement: parrot_provider::compaction::ReplacementCommit,
    ) -> ContextResult<()> {
        let published = self.notice("status", replacement.notice)?;
        let appended = self
            .history
            .append_compaction_status(
                published.clone(),
                CompactionSnapshot {
                    message: parrot_storage::conversation::ConversationMessage::from_compaction(
                        replacement.message,
                    )?,
                    watermark: replacement.watermark,
                },
                replacement.status,
            )
            .await?;
        conversation.reload().await?;
        if appended {
            events
                .publish_committed(Committed::after_commit(published), &NeverCancelled)
                .await?;
        }
        Ok(())
    }

    fn estimate(
        &self,
        input: &EpochInput<'_>,
        instructions: &str,
    ) -> ContextResult<ContextSnapshot> {
        let configuration = self
            .compactor
            .capture_context_configuration(input.selection);
        let messages = provider_messages(input.conversation)
            .into_iter()
            .map(Into::into)
            .collect::<Vec<_>>();
        let estimated = estimate_input_tokens(
            &input.selection.canonical_model,
            instructions,
            input.tools,
            &messages,
        )?;
        Ok(ContextSnapshot::estimate(estimated, &configuration))
    }
    fn event(&self) -> ContextResult<Event> {
        Ok(Event {
            id: Identifier::event_id().map_err(ContextError::Identifier)?,
            agent_session_id: self.history.agent_session_id().into(),
            ..Event::new()
        })
    }
    fn notice(&self, kind: &str, notice: String) -> ContextResult<Event> {
        let mut event = self.event()?;
        event.set_agent_notice(AgentNotice {
            kind: kind.into(),
            notice,
            ..AgentNotice::new()
        });
        Ok(event)
    }
}

struct EpochEffects<'a> {
    controller: &'a mut ContextController,
    input: EpochInput<'a>,
    errors: &'a NativeErrors<ContextError>,
}

impl EpochEffects<'_> {
    fn capture_error(&self, error: impl Into<ContextError>) -> parrot_core::Error {
        let error = error.into();
        let diagnostic = describe_context_error(&error);
        self.errors.capture(error, diagnostic)
    }
}

impl parrot_provider::compaction::CompactionEffects for EpochEffects<'_> {
    fn capture_context<'a>(
        &'a mut self,
        cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<parrot_provider::compaction::ContextCapture, parrot_core::Error>>
    {
        Box::pin(async move {
            self.input
                .prompt
                .sample_epoch(cancellation)
                .await
                .map_err(|error| self.capture_error(error))?;
            let instructions = self
                .input
                .prompt
                .build(self.input.selection, cancellation)
                .await
                .map_err(|error| self.capture_error(error))?;
            let conversation = self
                .controller
                .read_conversation_state(self.input.conversation)
                .await
                .map_err(|error| self.capture_error(error))?;
            Ok(parrot_provider::compaction::ContextCapture {
                instructions,
                conversation,
            })
        })
    }

    fn offer_status<'a>(
        &'a self,
        context: parrot_core::ContextSnapshot,
        cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<parrot_core::StatusOffering, parrot_core::Error>> {
        Box::pin(async move {
            self.input
                .status
                .slots(&context, cancellation)
                .await
                .map_err(|error| self.capture_error(error))
        })
    }

    fn commit_replacement<'a>(
        &'a mut self,
        replacement: parrot_provider::compaction::ReplacementCommit,
        _cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<(), parrot_core::Error>> {
        Box::pin(async move {
            self.controller
                .commit_replacement(self.input.conversation, self.input.events, replacement)
                .await
                .map_err(|error| self.capture_error(error))?;
            self.input.agent_state.last_request_statistic.reset();
            Ok(())
        })
    }

    fn publish<'a>(
        &'a self,
        event: parrot_provider::compaction::CompactionEvent,
        cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<(), parrot_core::Error>> {
        Box::pin(async move {
            let result = self
                .controller
                .publish_compaction(event, self.input.events, cancellation)
                .await;
            result.map_err(|error| self.capture_error(error))
        })
    }
}

fn describe_context_error(error: &ContextError) -> parrot_core::Error {
    use parrot_core::{Error, ErrorCode};
    match error {
        ContextError::Compaction(error) => describe_error(error),
        ContextError::Core(error) => error.clone(),
        ContextError::Cancelled => Error::cancelled(),
        ContextError::InvalidOperation(message) => Error {
            code: ErrorCode::InvalidOperation,
            ..Error::failed(message.clone())
        },
        ContextError::Argument(message) => Error {
            code: ErrorCode::Argument,
            ..Error::failed(message.clone())
        },
        _ => Error::failed(error.to_string()),
    }
}

impl From<parrot_core::Error> for ContextError {
    fn from(error: parrot_core::Error) -> Self {
        match error.detail {
            Some(parrot_core::ErrorDetail::Compaction(failure)) => failure.into(),
            Some(parrot_core::ErrorDetail::Planning(failure)) => failure.into(),
            Some(parrot_core::ErrorDetail::Template) => {
                Self::Prompt(PromptError::Template(error.message))
            }
            None if error.kind == parrot_core::ErrorKind::Cancelled => Self::Cancelled,
            None if error.code == parrot_core::ErrorCode::InvalidOperation => {
                Self::InvalidOperation(error.message)
            }
            None => Self::Core(error),
        }
    }
}

fn reborrow<'a>(input: &'a mut EpochInput<'_>) -> EpochInput<'a> {
    EpochInput {
        selection: input.selection,
        tools: input.tools,
        conversation: &mut *input.conversation,
        agent_state: &mut *input.agent_state,
        prompt: input.prompt,
        sessions: input.sessions,
        status: input.status,
        events: input.events,
    }
}
fn provider_messages(conversation: &AgentConversation) -> Vec<Message> {
    conversation
        .messages()
        .iter()
        .map(provider_message)
        .collect()
}
/// Converts the storage materialization without loading image bytes during estimates.
pub fn provider_message(message: &ModelMessage) -> Message {
    let role = match message.role {
        MessageRole::System => Role::System,
        MessageRole::User => Role::User,
        MessageRole::Assistant => Role::Assistant,
        MessageRole::Tool => Role::Tool,
    };
    let mut mapped = Message::new(
        role,
        message
            .contents
            .iter()
            .map(|content| match content {
                ModelContent::Text(text) => Content::text(text),
                ModelContent::ImageFile {
                    path,
                    media_type,
                    width,
                    height,
                } => Content::image_file(path.to_string_lossy(), media_type, *width, *height),
            })
            .collect(),
    );
    mapped.tool_calls = message
        .tool_calls
        .iter()
        .map(|call| ToolCall {
            id: call.id.clone(),
            name: call.name.clone(),
            arguments_json: call.arguments_json.clone(),
        })
        .collect();
    mapped.tool_call_id.clone_from(&message.tool_call_id);
    mapped
        .provider_id
        .clone_from(&message.provider_content.provider_id);
    mapped
        .provider_content
        .clone_from(&message.provider_content.content);
    mapped
}
