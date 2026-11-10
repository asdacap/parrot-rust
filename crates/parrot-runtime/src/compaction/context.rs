//! Awaited context epochs over the authoritative storage history.
use super::execution::{
    CompactionEstimates, CompactionProvider, NativeErrors, describe_error, progress_event,
};
use super::{
    CompactionError, CompactionObserver, CompactionResult, Compactor, ContextSize, ContextSnapshot,
    PlanningError, ProviderTokenBudget,
};
use crate::prompt_context::{PromptError, SessionPrompt};
use parrot_core::{
    ContextConfiguration as CoreContextConfiguration,
    ContextReminderCheckpoint as CoreContextReminderCheckpoint,
    ContextSnapshot as CoreContextSnapshot,
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
use parrot_foundation::template_catalog::PromptTemplateCatalog;
use parrot_provider::{
    routing::aliases::ResolvedModelSelection,
    sessions::{Content, Message, ProviderSessions, Role, Tool, ToolCall},
};
use parrot_storage::conversation::{
    AgentConversation, AgentHistoryView, CompactionSnapshot, EventError, MessageRole, ModelContent,
    ModelMessage,
};
use std::{fmt, sync::Arc};

#[derive(Debug)]
pub enum ContextError {
    Compaction(CompactionError),
    History(EventError),
    Prompt(PromptError),
    Identifier(std::io::Error),
    InvalidOperation(String),
    Argument(String),
    Operation(String),
    Cancelled,
}
impl ContextError {
    pub fn response_body(&self) -> &str {
        match self {
            Self::Compaction(error) => error.response_body(),
            _ => "",
        }
    }
}
impl fmt::Display for ContextError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Compaction(error) => error.fmt(f),
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
impl From<parrot_core::compaction::CompactionFailure> for ContextError {
    fn from(failure: parrot_core::compaction::CompactionFailure) -> Self {
        match failure {
            parrot_core::compaction::CompactionFailure::NoSafeBoundary
            | parrot_core::compaction::CompactionFailure::FittedInputLimitExceeded => {
                Self::InvalidOperation(failure.to_string())
            }
            failure => Self::Compaction(failure.into()),
        }
    }
}
pub type ContextResult<T> = Result<T, ContextError>;

/// Renders status for the bound agent, profile, and selection using the supplied context.
pub trait ContextStatus: Send + Sync {
    fn observe<'a>(
        &'a self,
        context: &'a ContextSnapshot,
        cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, ContextResult<String>>;
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
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum StatusInjection {
    Complete,
    /// The caller must refresh its full profile/model/security selection and try again.
    RefreshSelection,
}

pub struct ContextController {
    compactor: Compactor,
    history: AgentHistoryView,
    blobs: parrot_storage::artifacts::compaction::CompactionGroupBlobStore,
    templates: Arc<PromptTemplateCatalog>,
    budget: ProviderTokenBudget,
    pub(crate) initial_status_pending: bool,
}
impl ContextController {
    pub fn new(
        compactor: Compactor,
        history: AgentHistoryView,
        blobs: parrot_storage::artifacts::compaction::CompactionGroupBlobStore,
        templates: Arc<PromptTemplateCatalog>,
    ) -> Self {
        Self {
            compactor,
            history,
            blobs,
            templates,
            budget: ProviderTokenBudget::default(),
            initial_status_pending: true,
        }
    }
    pub fn compactor(&self) -> &Compactor {
        &self.compactor
    }
    pub fn budget(&self) -> &ProviderTokenBudget {
        &self.budget
    }
    pub fn budget_mut(&mut self) -> &mut ProviderTokenBudget {
        &mut self.budget
    }

    pub(crate) fn capture_context_configuration(
        &self,
        selection: &ResolvedModelSelection,
    ) -> CoreContextConfiguration {
        self.compactor
            .planner()
            .capture_context_configuration(selection)
    }

    pub(crate) async fn get_last_context_reminder(
        &self,
    ) -> ContextResult<Option<CoreContextReminderCheckpoint>> {
        Ok(self.history.latest_context_reminder().await?.map(|value| {
            CoreContextReminderCheckpoint {
                canonical_model: value.canonical_model,
                context_limit: value.context_limit,
                percentage: value.percentage,
            }
        }))
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

    pub async fn estimate_context_after_tool_result(
        &self,
        input: EpochInput<'_>,
        tool_call_id: &str,
        result: &str,
    ) -> ContextResult<ContextSnapshot> {
        if tool_call_id.trim().is_empty() {
            return Err(ContextError::Argument(
                "toolCallId must not be empty or whitespace".into(),
            ));
        }
        let history = AgentConversation::new(self.history.clone()).await?;
        let mut messages = provider_messages(&history);
        let formatted = self
            .templates
            .render(
                "tool-result.text",
                &[("value".into(), result.into())],
                &Uncancelled,
            )
            .map_err(|error| ContextError::Operation(error.to_string()))?;
        messages.push(Message::tool_result(tool_call_id, formatted));
        let instructions = input.prompt.build(input.selection, &Uncancelled).await?;
        Ok(self.compactor.planner().estimate_selected_context(
            input.selection,
            &instructions,
            input.tools,
            &messages,
        )?)
    }

    /// Fits and commits status through core before input promotion.
    pub async fn inject_status(
        &mut self,
        input: EpochInput<'_>,
        depth: i32,
        profile_id: &str,
        cancellation: &dyn Cancellation,
    ) -> ContextResult<StatusInjection> {
        self.ensure_epoch(input.prompt, cancellation).await?;
        let configuration = self
            .compactor
            .planner()
            .capture_compaction_configuration(input.selection, None);
        let definitions = input
            .tools
            .iter()
            .cloned()
            .map(Into::into)
            .collect::<Vec<_>>();
        let estimates = CompactionEstimates::new(&self.compactor, input.selection);
        let errors = NativeErrors::new();
        let templates = self.templates.clone();
        let controller = tokio::sync::Mutex::new(self);
        let mut states = EpochStates {
            controller: &controller,
            conversation: input.conversation,
            prompt: input.prompt,
            selection: input.selection,
            status: input.status,
            status_request: StatusRequest {
                depth,
                profile_id: profile_id.into(),
            },
            sessions: input.sessions,
            events: input.events,
            configuration,
            templates,
            cancellation,
            estimates: &estimates,
            errors: &errors,
        };
        let provider = EpochProvider {
            estimates: &estimates,
            errors: &errors,
            sessions: input.sessions,
            events: input.events,
        };
        let outcome = parrot_core::prepare_status(
            &mut states,
            &provider,
            &definitions,
            parrot_core::CancellationToken::new(cancellation),
        )
        .await
        .map_err(|error| errors.restore(error, ContextError::from))?;
        Ok(match outcome {
            parrot_core::StatusPreparation::Complete => StatusInjection::Complete,
            parrot_core::StatusPreparation::RefreshSelection => StatusInjection::RefreshSelection,
        })
    }

    pub async fn inject_final_provider_request_prompt(
        &self,
        conversation: &mut AgentConversation,
        content: &str,
        events: &dyn ContextEvents,
        cancellation: &dyn Cancellation,
    ) -> ContextResult<()> {
        let errors = NativeErrors::new();
        let mut states = ContextAdditionStates {
            errors: &errors,
            history: &self.history,
            conversation,
            events,
            cancellation,
        };
        parrot_core::add_phase_prompt(&mut states, content, true)
            .await
            .map_err(|error| errors.restore(error, ContextError::from))?;
        Ok(())
    }

    pub async fn restore_tool_availability(
        &self,
        conversation: &mut AgentConversation,
        content: &str,
        events: &dyn ContextEvents,
        cancellation: &dyn Cancellation,
    ) -> ContextResult<bool> {
        let errors = NativeErrors::new();
        let mut states = ContextAdditionStates {
            errors: &errors,
            history: &self.history,
            conversation,
            events,
            cancellation,
        };
        Ok(parrot_core::add_phase_prompt(&mut states, content, false)
            .await
            .map_err(|error| errors.restore(error, ContextError::from))?
            .inserted)
    }

    pub async fn prepare_epoch(
        &mut self,
        input: EpochInput<'_>,
        cancellation: &dyn Cancellation,
    ) -> ContextResult<EpochResult> {
        cancellation.check().map_err(|_| ContextError::Cancelled)?;
        self.ensure_epoch(input.prompt, cancellation).await?;
        let configuration = self
            .compactor
            .planner()
            .capture_compaction_configuration(input.selection, None);
        let definitions = input
            .tools
            .iter()
            .cloned()
            .map(Into::into)
            .collect::<Vec<_>>();
        let estimates = CompactionEstimates::new(&self.compactor, input.selection);
        let errors = NativeErrors::new();
        let templates = self.templates.clone();
        let controller = tokio::sync::Mutex::new(self);
        let publisher = EpochEventPublisher {
            controller: &controller,
            prompt: input.prompt,
            events: input.events,
            cancellation,
            errors: &errors,
        };
        let mut states = EpochStates {
            controller: &controller,
            conversation: input.conversation,
            prompt: input.prompt,
            selection: input.selection,
            status: input.status,
            status_request: StatusRequest::default(),
            sessions: input.sessions,
            events: input.events,
            configuration,
            templates,
            cancellation,
            estimates: &estimates,
            errors: &errors,
        };
        let provider = EpochProvider {
            estimates: &estimates,
            errors: &errors,
            sessions: input.sessions,
            events: input.events,
        };
        let result = parrot_core::prepare_context_epoch(
            &mut states,
            &provider,
            &publisher,
            &definitions,
            parrot_core::CancellationToken::new(cancellation),
        )
        .await
        .map_err(|error| errors.restore(error, ContextError::from))?;
        Ok(EpochResult {
            instructions: result.instructions,
            reduced: result.reduced,
        })
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
        mut input: EpochInput<'_>,
        target: Option<&ContextSize>,
        cancellation: &dyn Cancellation,
    ) -> ContextResult<EpochResult> {
        let configuration = self
            .compactor
            .planner()
            .capture_compaction_configuration(input.selection, target);
        let definitions: Vec<parrot_core::ToolDefinition> =
            input.tools.iter().cloned().map(Into::into).collect();
        let estimates = CompactionEstimates::new(&self.compactor, input.selection);
        let errors = NativeErrors::new();
        let templates = self.templates.clone();
        let (prepared, result) = {
            let controller = tokio::sync::Mutex::new(&mut *self);
            let publisher = EpochEventPublisher {
                controller: &controller,
                prompt: input.prompt,
                events: input.events,
                cancellation,
                errors: &errors,
            };
            let mut states = EpochStates {
                controller: &controller,
                conversation: input.conversation,
                prompt: input.prompt,
                selection: input.selection,
                status: input.status,
                status_request: StatusRequest::default(),
                sessions: input.sessions,
                events: input.events,
                configuration: configuration.clone(),
                templates,
                cancellation,
                estimates: &estimates,
                errors: &errors,
            };
            let provider = CompactionProvider::new(
                &estimates,
                input.sessions,
                ContextObserver(input.events),
                &errors,
            );
            let preparation_provider = EpochProvider {
                estimates: &estimates,
                errors: &errors,
                sessions: input.sessions,
                events: input.events,
            };
            let prepared = parrot_core::compaction::prepare_compaction(
                &mut states,
                &preparation_provider,
                &publisher,
                &definitions,
                &configuration,
                parrot_core::CancellationToken::new(cancellation),
            )
            .await
            .map_err(|error| errors.restore(error, ContextError::from))?;
            let result = parrot_core::compaction::compact_conversation(
                &prepared,
                &definitions,
                &configuration,
                &mut states,
                &provider,
                parrot_core::CancellationToken::new(cancellation),
            )
            .await
            .map_err(|error| errors.restore(error, ContextError::from));
            (prepared, result)
        };
        self.record_epoch_compaction(&mut input, prepared.instructions, result, cancellation)
            .await
    }

    pub(crate) async fn report_epoch_started(
        &self,
        events: &dyn ContextEvents,
    ) -> ContextResult<()> {
        let mut started = self.event()?;
        started.set_compaction_started(CompactionStarted::new());
        events.emit(started, &Uncancelled).await
    }

    pub(crate) async fn record_epoch_compaction(
        &mut self,
        input: &mut EpochInput<'_>,
        instructions: String,
        result: ContextResult<Option<parrot_core::compaction::FittedCompaction>>,
        cancellation: &dyn Cancellation,
    ) -> ContextResult<EpochResult> {
        let outcome: ContextResult<EpochResult> = async {
            let result = self
                .apply_compaction(input, instructions, result?, cancellation)
                .await?;
            let mut finished = self.event()?;
            finished.set_compaction_finished(CompactionFinished::new());
            input.events.emit(finished, &Uncancelled).await?;
            Ok(result)
        }
        .await;
        match outcome {
            Ok(result) => Ok(result),
            Err(error) => {
                self.fail_epoch_compaction(error.to_string(), input.events)
                    .await?;
                Err(error)
            }
        }
    }

    pub(crate) async fn fail_epoch_compaction(
        &self,
        message: String,
        events: &dyn ContextEvents,
    ) -> ContextResult<()> {
        let mut failed = self.event()?;
        failed.set_compaction_failed(CompactionFailed {
            message,
            ..CompactionFailed::new()
        });
        events.emit(failed, &Uncancelled).await
    }

    pub(crate) fn borrow_blobs(
        &mut self,
    ) -> &mut parrot_storage::artifacts::compaction::CompactionGroupBlobStore {
        &mut self.blobs
    }

    pub(crate) async fn read_conversation_state(
        &self,
        conversation: &AgentConversation,
    ) -> ContextResult<parrot_core::compaction::ConversationState> {
        let state = self.history.read_conversation_state().await?;
        let mut history = Vec::with_capacity(state.items.len());
        for item in state.items {
            history.push(parrot_core::compaction::ConversationHistoryItem {
                sequence: item.item.sequence,
                tool_owner_sequence: item.tool_owner_sequence,
                is_compaction_status: item.is_compaction_status,
                message: provider_message(&conversation.materialize(item.item).await?).into(),
            });
        }
        Ok(parrot_core::compaction::ConversationState {
            summary: state
                .snapshot
                .map(|snapshot| parrot_core::compaction::ConversationSummary {
                    content: snapshot.summary,
                    watermark: snapshot.watermark,
                }),
            history,
            checkpoint_sequences: state.checkpoint_sequences,
            messages: provider_messages(conversation)
                .into_iter()
                .map(Into::into)
                .collect(),
        })
    }

    async fn apply_compaction(
        &mut self,
        input: &mut EpochInput<'_>,
        instructions: String,
        compacted: Option<parrot_core::compaction::FittedCompaction>,
        cancellation: &dyn Cancellation,
    ) -> ContextResult<EpochResult> {
        let mut reduced = false;
        if let Some(compacted) = compacted {
            let status = compacted.status;
            let compacted: CompactionResult = compacted.result.into();
            let mut saving = self.event()?;
            saving.set_compaction_progress(CompactionProgressEvent {
                phase: CompactionPhase::COMPACTION_PHASE_SAVING.into(),
                ..CompactionProgressEvent::new()
            });
            input.events.emit(saving, cancellation).await?;
            let published = self.notice("status", "Status prompt injected".into())?;
            if !self
                .history
                .append_compaction_status(
                    published.clone(),
                    CompactionSnapshot {
                        summary: compacted.summary.content().into(),
                        watermark: compacted.watermark,
                    },
                    status,
                )
                .await?
            {
                return Err(ContextError::InvalidOperation(
                    "compaction snapshot was already persisted".into(),
                ));
            }
            reduced = true;
            self.budget.reset();
            input.conversation.reload().await?;
            input
                .events
                .publish_committed(Committed::after_commit(published), &Uncancelled)
                .await?;
        }
        Ok(EpochResult {
            instructions,
            reduced,
        })
    }

    fn estimate(
        &self,
        input: &EpochInput<'_>,
        instructions: &str,
    ) -> ContextResult<ContextSnapshot> {
        Ok(self.compactor.planner().estimate_selected_context(
            input.selection,
            instructions,
            input.tools,
            &provider_messages(input.conversation),
        )?)
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

struct EpochEventPublisher<'a> {
    controller: &'a tokio::sync::Mutex<&'a mut ContextController>,
    prompt: &'a SessionPrompt,
    events: &'a dyn ContextEvents,
    cancellation: &'a dyn Cancellation,
    errors: &'a NativeErrors<ContextError>,
}

impl parrot_core::EventPublisher for EpochEventPublisher<'_> {
    fn on_epoch_started(&self) -> BoxFuture<'_, Result<(), parrot_core::Error>> {
        Box::pin(async move {
            self.prompt
                .sample_epoch(self.cancellation)
                .await
                .map_err(|error| {
                    let error = ContextError::from(error);
                    let diagnostic = describe_context_error(&error);
                    self.errors.capture(error, diagnostic)
                })
        })
    }
    fn report_compaction_started(&self) -> BoxFuture<'_, Result<(), parrot_core::Error>> {
        Box::pin(async move {
            self.controller
                .lock()
                .await
                .report_epoch_started(self.events)
                .await
                .map_err(|error| {
                    let diagnostic = describe_context_error(&error);
                    self.errors.capture(error, diagnostic)
                })
        })
    }
    fn report_compaction_failed(
        &self,
        message: String,
    ) -> BoxFuture<'_, Result<(), parrot_core::Error>> {
        Box::pin(async move {
            self.controller
                .lock()
                .await
                .fail_epoch_compaction(message, self.events)
                .await
                .map_err(|error| {
                    let diagnostic = describe_context_error(&error);
                    self.errors.capture(error, diagnostic)
                })
        })
    }
}

#[derive(Default)]
struct StatusRequest {
    depth: i32,
    profile_id: String,
}

struct ContextAdditionStates<'a> {
    errors: &'a NativeErrors<ContextError>,
    history: &'a AgentHistoryView,
    conversation: &'a mut AgentConversation,
    events: &'a dyn ContextEvents,
    cancellation: &'a dyn Cancellation,
}

impl parrot_core::States for ContextAdditionStates<'_> {
    fn get_config(
        &self,
        _key: parrot_core::ConfigKey,
    ) -> BoxFuture<'_, Result<parrot_core::ConfigValue, parrot_core::Error>> {
        Box::pin(async { Err(parrot_core::Error::unsupported("States.get_config")) })
    }

    fn apply_conversation_addition(
        &mut self,
        addition: parrot_core::ConversationAddition,
    ) -> BoxFuture<'_, Result<parrot_core::ConversationReceipt, parrot_core::Error>> {
        Box::pin(async move {
            super::apply_conversation_addition(
                self.history,
                self.conversation,
                &mut false,
                addition,
                self.events,
                self.cancellation,
            )
            .await
            .map_err(|error| {
                let diagnostic = describe_context_error(&error);
                self.errors.capture(error, diagnostic)
            })
        })
    }
}

struct EpochStates<'a> {
    controller: &'a tokio::sync::Mutex<&'a mut ContextController>,
    conversation: &'a mut AgentConversation,
    prompt: &'a SessionPrompt,
    selection: &'a ResolvedModelSelection,
    status: &'a dyn ContextStatus,
    status_request: StatusRequest,
    sessions: &'a ProviderSessions,
    events: &'a dyn ContextEvents,
    configuration: parrot_core::CompactionConfiguration,
    templates: Arc<PromptTemplateCatalog>,
    cancellation: &'a dyn Cancellation,
    estimates: &'a CompactionEstimates,
    errors: &'a NativeErrors<ContextError>,
}

fn describe_context_error(error: &ContextError) -> parrot_core::Error {
    use parrot_core::{Error, ErrorCode};
    match error {
        ContextError::Compaction(error) => describe_error(error),
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
            Some(parrot_core::ErrorDetail::PromptContext) => {
                Self::Prompt(PromptError::Context(error.message))
            }
            None if error.kind == parrot_core::ErrorKind::Cancelled => Self::Cancelled,
            None if error.code == parrot_core::ErrorCode::InvalidOperation => {
                Self::InvalidOperation(error.message)
            }
            None => Self::Operation(error.message),
        }
    }
}

impl EpochStates<'_> {
    fn capture_error(&self, error: impl Into<ContextError>) -> parrot_core::Error {
        let error = error.into();
        let diagnostic = describe_context_error(&error);
        self.errors.capture(error, diagnostic)
    }
}

impl parrot_core::States for EpochStates<'_> {
    fn apply_conversation_addition(
        &mut self,
        addition: parrot_core::ConversationAddition,
    ) -> BoxFuture<'_, Result<parrot_core::ConversationReceipt, parrot_core::Error>> {
        Box::pin(async move {
            let result = {
                let mut controller = self.controller.lock().await;
                let history = controller.history.clone();
                super::apply_conversation_addition(
                    &history,
                    self.conversation,
                    &mut controller.initial_status_pending,
                    addition,
                    self.events,
                    self.cancellation,
                )
                .await
            };
            result.map_err(|error| self.capture_error(error))
        })
    }

    fn read_status_observation(
        &self,
    ) -> BoxFuture<'_, Result<parrot_core::StatusObservation, parrot_core::Error>> {
        Box::pin(async move {
            let controller = self.controller.lock().await;
            let pending = if self.status_request.depth == 0 {
                controller
                    .history
                    .pending_status()
                    .await
                    .map_err(|error| self.capture_error(error))?
                    .map(|pending| parrot_core::StatusCheckpoint::Mode {
                        mode: pending.mode,
                        version: pending.version,
                    })
            } else {
                None
            };
            Ok(parrot_core::StatusObservation {
                depth: self.status_request.depth,
                profile_id: self.status_request.profile_id.clone(),
                initial_pending: controller.initial_status_pending,
                pending,
            })
        })
    }

    fn read_next_input_messages(
        &self,
    ) -> BoxFuture<'_, Result<Vec<parrot_core::Message>, parrot_core::Error>> {
        Box::pin(async move {
            let history = self.controller.lock().await.history.clone();
            let mut messages = Vec::new();
            for pending in history
                .inputs_for_next_promotion()
                .await
                .map_err(|error| self.capture_error(error))?
            {
                let contents = history
                    .materialize(pending.parts)
                    .await
                    .map_err(|error| self.capture_error(error))?;
                messages.push(
                    provider_message(&ModelMessage {
                        role: MessageRole::User,
                        contents,
                        tool_calls: Vec::new(),
                        tool_call_id: String::new(),
                        provider_content: Default::default(),
                    })
                    .into(),
                );
            }
            Ok(messages)
        })
    }

    fn read_provider_messages(
        &self,
    ) -> BoxFuture<'_, Result<Vec<parrot_core::Message>, parrot_core::Error>> {
        Box::pin(async move {
            Ok(self
                .conversation
                .messages()
                .iter()
                .map(provider_message)
                .map(Into::into)
                .collect())
        })
    }

    fn read_token_calibration(
        &self,
    ) -> BoxFuture<'_, Result<parrot_core::TokenCalibration, parrot_core::Error>> {
        Box::pin(async move { Ok(self.controller.lock().await.budget.capture_state()) })
    }

    fn read_last_context_reminder_checkpoint(
        &self,
    ) -> BoxFuture<'_, Result<Option<CoreContextReminderCheckpoint>, parrot_core::Error>> {
        Box::pin(async move {
            self.controller
                .lock()
                .await
                .get_last_context_reminder()
                .await
                .map_err(|error| self.capture_error(error))
        })
    }

    fn finalize_compaction<'a>(
        &'a mut self,
        instructions: &'a str,
        definitions: &'a [parrot_core::ToolDefinition],
        result: Result<Option<parrot_core::FittedCompaction>, parrot_core::Error>,
    ) -> BoxFuture<'a, Result<parrot_core::ContextCompaction, parrot_core::Error>> {
        Box::pin(async move {
            let definitions = definitions
                .iter()
                .cloned()
                .map(Into::into)
                .collect::<Vec<_>>();
            let result = result.map_err(|error| self.errors.restore(error, ContextError::from));
            let recorded = self
                .controller
                .lock()
                .await
                .record_epoch_compaction(
                    &mut EpochInput {
                        selection: self.selection,
                        tools: &definitions,
                        conversation: self.conversation,
                        prompt: self.prompt,
                        sessions: self.sessions,
                        status: self.status,
                        events: self.events,
                    },
                    instructions.into(),
                    result,
                    self.cancellation,
                )
                .await;
            recorded
                .map(|result| parrot_core::ContextCompaction {
                    instructions: result.instructions,
                    reduced: result.reduced,
                })
                .map_err(|error| self.capture_error(error))
        })
    }

    fn get_config(
        &self,
        key: parrot_core::ConfigKey,
    ) -> BoxFuture<'_, Result<parrot_core::ConfigValue, parrot_core::Error>> {
        Box::pin(async move {
            use parrot_core::{ConfigKey, ConfigValue};
            match key {
                ConfigKey::Compaction => Ok(ConfigValue::Compaction(self.configuration.clone())),
                ConfigKey::Context => Ok(ConfigValue::Context(self.configuration.context.clone())),
                ConfigKey::SystemPromptBindings => self
                    .prompt
                    .read_core_prompt_bindings(self.cancellation)
                    .await
                    .map(ConfigValue::SystemPromptBindings)
                    .map_err(|error| self.capture_error(error)),
                ConfigKey::Turn => Err(parrot_core::Error::failed(
                    "turn configuration is unavailable for a compaction epoch",
                )),
                key => self
                    .templates
                    .get_template(
                        crate::prompt_context::get_prompt_template_id(key)
                            .ok_or_else(|| parrot_core::Error::unsupported("configuration"))?,
                    )
                    .map(ConfigValue::PromptTemplate)
                    .map_err(|error| self.capture_error(PromptError::Template(error.to_string()))),
            }
        })
    }
    fn read_conversation_state(
        &self,
    ) -> BoxFuture<'_, Result<parrot_core::ConversationState, parrot_core::Error>> {
        Box::pin(async move {
            self.controller
                .lock()
                .await
                .read_conversation_state(self.conversation)
                .await
                .map_err(|error| self.capture_error(error))
        })
    }
    fn render_status<'a, 'c>(
        &'a self,
        context: CoreContextSnapshot,
        cancellation: parrot_core::CancellationToken<'c>,
    ) -> BoxFuture<'a, Result<String, parrot_core::Error>>
    where
        'c: 'a,
    {
        Box::pin(async move {
            self.status
                .observe(
                    &ContextSnapshot {
                        estimated_tokens: context.estimated_tokens,
                        context_limit: context.context_limit,
                        usage_percent: context.usage_percent,
                        trigger_percent: context.trigger_percent,
                        input_limit: context.input_limit,
                        trigger_tokens: context.trigger_tokens,
                        has_context_limit_override: context.has_context_limit_override,
                    },
                    &cancellation,
                )
                .await
                .map_err(|error| self.capture_error(error))
        })
    }
    fn spill_group<'a, 'c>(
        &'a self,
        messages: &'a [parrot_core::Message],
        cancellation: parrot_core::CancellationToken<'c>,
    ) -> BoxFuture<'a, Result<Vec<parrot_core::Message>, parrot_core::Error>>
    where
        'c: 'a,
    {
        Box::pin(async move {
            self.estimates
                .spill_group(
                    self.controller.lock().await.borrow_blobs(),
                    messages,
                    cancellation,
                )
                .map_err(|error| self.capture_error(error))
        })
    }
    fn report_compaction_progress<'a, 'c>(
        &'a self,
        progress: parrot_core::CompactionProgress,
        cancellation: parrot_core::CancellationToken<'c>,
    ) -> BoxFuture<'a, Result<(), parrot_core::Error>>
    where
        'c: 'a,
    {
        Box::pin(async move {
            self.events
                .observe_progress(progress_event(progress), &cancellation)
                .await
                .map_err(CompactionError::from)
                .map_err(|error| self.capture_error(error))
        })
    }
    fn capture_system_prompt_sections(
        &mut self,
    ) -> BoxFuture<'_, Result<Vec<(String, String)>, parrot_core::Error>> {
        Box::pin(async move {
            self.prompt
                .read_system_prompts(self.selection, self.cancellation)
                .await
                .map_err(|error| self.capture_error(error))
        })
    }

    fn read_agents_files(
        &self,
    ) -> BoxFuture<'_, Result<Vec<(String, String)>, parrot_core::Error>> {
        Box::pin(async move {
            self.prompt
                .read_agents_files(self.cancellation)
                .await
                .map_err(|error| self.capture_error(error))
        })
    }

    fn read_agent_identity(
        &self,
    ) -> BoxFuture<'_, Result<Option<parrot_core::AgentIdentityDescription>, parrot_core::Error>>
    {
        Box::pin(async move {
            self.prompt
                .read_agent_identity(self.cancellation)
                .await
                .map_err(|error| self.capture_error(error))
        })
    }

    fn read_standing_instruction(&self) -> BoxFuture<'_, Result<String, parrot_core::Error>> {
        Box::pin(async move {
            self.prompt
                .read_standing_instruction(self.cancellation)
                .await
                .map_err(|error| self.capture_error(error))
        })
    }

    fn read_skill_catalog(
        &self,
    ) -> BoxFuture<'_, Result<Vec<parrot_core::SkillCatalogEntry>, parrot_core::Error>> {
        Box::pin(async move {
            self.prompt
                .read_skill_catalog(self.cancellation)
                .await
                .map_err(|error| self.capture_error(error))
        })
    }

    fn read_template_arguments<'a>(
        &'a self,
        source_key: &'a str,
    ) -> BoxFuture<'a, Result<Vec<(String, serde_json::Value)>, parrot_core::Error>> {
        Box::pin(async move {
            self.prompt
                .read_template_arguments(source_key, self.cancellation)
                .await
                .map_err(|error| self.capture_error(error))
        })
    }
}

fn reborrow<'a>(input: &'a mut EpochInput<'_>) -> EpochInput<'a> {
    EpochInput {
        selection: input.selection,
        tools: input.tools,
        conversation: &mut *input.conversation,
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
struct ContextObserver<'a>(&'a dyn ContextEvents);
impl CompactionObserver for ContextObserver<'_> {
    fn observe<'a>(
        &'a self,
        event: &'a parrot_provider::sessions::Event,
        cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<(), parrot_provider::sessions::Error>> {
        self.0.observe(event, cancellation)
    }
    fn observe_progress<'a>(
        &'a self,
        progress: CompactionProgressEvent,
        cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<(), parrot_provider::sessions::Error>> {
        self.0.observe_progress(progress, cancellation)
    }
}
struct Uncancelled;
impl Cancellation for Uncancelled {
    fn is_cancelled(&self) -> bool {
        false
    }
    fn cancelled(&self) -> BoxFuture<'_, ()> {
        Box::pin(std::future::pending())
    }
}

struct EpochProvider<'a> {
    estimates: &'a CompactionEstimates,
    errors: &'a NativeErrors<ContextError>,
    sessions: &'a ProviderSessions,
    events: &'a dyn ContextEvents,
}

impl EpochProvider<'_> {
    fn capture_error(&self, error: impl Into<ContextError>) -> parrot_core::Error {
        let error = error.into();
        let diagnostic = describe_context_error(&error);
        self.errors.capture(error, diagnostic)
    }
}

impl parrot_core::Provider for EpochProvider<'_> {
    fn estimate_tokens(
        &self,
        messages: &[parrot_core::Message],
    ) -> Result<i64, parrot_core::Error> {
        self.estimates
            .estimate_tokens(messages)
            .map_err(|error| self.capture_error(error))
    }

    fn estimate_summary_tokens(
        &self,
        preceding: &str,
        tokens: i64,
        messages: &[parrot_core::Message],
    ) -> Result<i64, parrot_core::Error> {
        self.estimates
            .estimate_summary_tokens(preceding, tokens, messages)
            .map_err(|error| self.capture_error(error))
    }

    fn summarize<'a, 'c>(
        &'a self,
        request: parrot_core::ProviderRequest,
        cancellation: parrot_core::CancellationToken<'c>,
    ) -> BoxFuture<'a, Result<parrot_core::SummaryResponse, parrot_core::Error>>
    where
        'c: 'a,
    {
        Box::pin(async move {
            let provider = CompactionProvider::new(
                self.estimates,
                self.sessions,
                ContextObserver(self.events),
                self.errors,
            );
            parrot_core::Provider::summarize(&provider, request, cancellation).await
        })
    }

    fn estimate_input_tokens(
        &self,
        instructions: &str,
        definitions: &[parrot_core::ToolDefinition],
        messages: &[parrot_core::Message],
    ) -> Result<i64, parrot_core::Error> {
        self.estimates
            .estimate_context_tokens(instructions, definitions, messages)
            .map_err(|error| self.capture_error(error))
    }
}
