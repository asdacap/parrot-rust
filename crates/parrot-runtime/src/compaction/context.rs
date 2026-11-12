//! Awaited context epochs over the authoritative storage history.
use super::execution::{
    CompactionEstimates, CompactionProvider, NativeErrors, describe_error, progress_event,
};
use super::{
    CompactionError, CompactionObserver, CompactionResult, Compactor, ContextSize, ContextSnapshot,
    PlanningError, ProviderTokenBudget,
};
use crate::prompt_context::{PromptError, SessionPrompt};
use parrot_core::ContextConfiguration as CoreContextConfiguration;
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
    blobs: parrot_storage::artifacts::compaction::CompactionGroupBlobStore,
    templates: Arc<PromptTemplateCatalog>,
    budget: ProviderTokenBudget,
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
        mut input: EpochInput<'_>,
        target: Option<&ContextSize>,
        cancellation: &dyn Cancellation,
    ) -> ContextResult<EpochResult> {
        let configuration = self
            .compactor
            .planner()
            .capture_compaction_configuration(input.selection, target);
        let definitions: Vec<parrot_core::ToolDefinition> = input.tools.to_vec();
        let estimates = CompactionEstimates::new(input.selection);
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
                sessions: input.sessions,
                events: input.events,
                configuration: configuration.clone(),
                templates,
                cancellation,
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
            )
            .await
            .map_err(|error| errors.restore(error, ContextError::from))?;
            let result = parrot_core::compaction::compact_conversation(
                &prepared,
                &definitions,
                &configuration,
                &mut states,
                &provider,
                &publisher,
                parrot_core::CancellationToken::new(cancellation),
            )
            .await
            .map_err(|error| errors.restore(error, ContextError::from));
            (prepared, result)
        };
        let recorded = self
            .record_epoch_compaction(&mut input, prepared.instructions, result, cancellation)
            .await;
        let outcome = match recorded {
            Ok(result) => self
                .report_epoch_finished(input.events)
                .await
                .map(|()| result),
            Err(error) => Err(error),
        };
        match outcome {
            Ok(result) => Ok(result),
            Err(error) => {
                self.fail_epoch_compaction(error.to_string(), input.events)
                    .await?;
                Err(error)
            }
        }
    }

    pub(crate) async fn report_epoch_started(
        &self,
        events: &dyn ContextEvents,
    ) -> ContextResult<()> {
        let mut started = self.event()?;
        started.set_compaction_started(CompactionStarted::new());
        events.emit(started, &Uncancelled).await
    }

    pub(crate) async fn report_epoch_finished(
        &self,
        events: &dyn ContextEvents,
    ) -> ContextResult<()> {
        let mut finished = self.event()?;
        finished.set_compaction_finished(CompactionFinished::new());
        events.emit(finished, &Uncancelled).await
    }

    pub(crate) async fn record_epoch_compaction(
        &mut self,
        input: &mut EpochInput<'_>,
        instructions: String,
        result: ContextResult<Option<parrot_core::FittedCompaction>>,
        cancellation: &dyn Cancellation,
    ) -> ContextResult<EpochResult> {
        self.apply_compaction(input, instructions, result?, cancellation)
            .await
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

    pub(crate) async fn read_conversation_summary(
        &self,
    ) -> ContextResult<Option<parrot_core::ConversationSummary>> {
        let snapshot = self
            .history
            .repository()
            .compaction(self.history.agent_session_id().into())
            .await?;
        Ok(snapshot.map(|snapshot| parrot_core::ConversationSummary {
            content: snapshot.summary,
            watermark: snapshot.watermark,
        }))
    }

    pub(crate) async fn read_conversation_tail(
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

    async fn apply_compaction(
        &mut self,
        input: &mut EpochInput<'_>,
        instructions: String,
        compacted: Option<parrot_core::FittedCompaction>,
        cancellation: &dyn Cancellation,
    ) -> ContextResult<EpochResult> {
        let mut reduced = false;
        if let Some(compacted) = compacted {
            let status = compacted.status;
            let notice = compacted.notice;
            let compacted: CompactionResult = compacted.result.into();
            let mut saving = self.event()?;
            saving.set_compaction_progress(CompactionProgressEvent {
                phase: CompactionPhase::COMPACTION_PHASE_SAVING.into(),
                ..CompactionProgressEvent::new()
            });
            input.events.emit(saving, cancellation).await?;
            let published = self.notice("status", notice)?;
            let appended = self
                .history
                .append_compaction_status(
                    published.clone(),
                    CompactionSnapshot {
                        summary: compacted.summary.content().into(),
                        watermark: compacted.watermark,
                    },
                    status,
                )
                .await?;
            reduced = true;
            self.budget.reset();
            input.conversation.reload().await?;
            if appended {
                input
                    .events
                    .publish_committed(Committed::after_commit(published), &Uncancelled)
                    .await?;
            }
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
    fn report_compaction_finished(&self) -> BoxFuture<'_, Result<(), parrot_core::Error>> {
        Box::pin(async move {
            self.controller
                .lock()
                .await
                .report_epoch_finished(self.events)
                .await
                .map_err(|error| {
                    let diagnostic = describe_context_error(&error);
                    self.errors.capture(error, diagnostic)
                })
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
            ContextObserver(self.events)
                .observe_progress(progress_event(progress), &cancellation)
                .await
                .map_err(CompactionError::from)
                .map_err(ContextError::Compaction)
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
    sessions: &'a ProviderSessions,
    events: &'a dyn ContextEvents,
    configuration: parrot_core::CompactionConfiguration,
    templates: Arc<PromptTemplateCatalog>,
    cancellation: &'a dyn Cancellation,
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
    fn add_conversation_messages(
        &mut self,
        addition: parrot_core::ConversationAddition,
    ) -> BoxFuture<'_, Result<parrot_core::ConversationReceipt, parrot_core::Error>> {
        Box::pin(async move {
            let result = {
                let controller = self.controller.lock().await;
                let history = controller.history.clone();
                super::add_conversation_messages(
                    &history,
                    self.conversation,
                    addition,
                    self.events,
                    self.cancellation,
                )
                .await
            };
            result.map_err(|error| self.capture_error(error))
        })
    }

    fn read_status_offering(
        &self,
        context: parrot_core::ContextSnapshot,
    ) -> BoxFuture<'_, Result<parrot_core::StatusOffering, parrot_core::Error>> {
        Box::pin(async move {
            self.status
                .slots(&context, self.cancellation)
                .await
                .map_err(|error| self.capture_error(error))
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

    fn finalize_compaction<'a>(
        &'a mut self,
        instructions: &'a str,
        definitions: &'a [parrot_core::ToolDefinition],
        result: Result<Option<parrot_core::FittedCompaction>, parrot_core::Error>,
    ) -> BoxFuture<'a, Result<String, parrot_core::Error>> {
        Box::pin(async move {
            let definitions = definitions.to_vec();
            let result = result.map_err(|error| self.errors.restore(error, ContextError::from));
            self.controller
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
                .await
                .map(|result| result.instructions)
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
    fn read_conversation_summary(
        &self,
    ) -> BoxFuture<'_, Result<Option<parrot_core::ConversationSummary>, parrot_core::Error>> {
        Box::pin(async move {
            self.controller
                .lock()
                .await
                .read_conversation_summary()
                .await
                .map_err(|error| self.capture_error(error))
        })
    }
    fn read_conversation_tail(
        &self,
        after: i64,
    ) -> BoxFuture<'_, Result<Vec<parrot_core::ConversationHistoryItem>, parrot_core::Error>> {
        Box::pin(async move {
            self.controller
                .lock()
                .await
                .read_conversation_tail(self.conversation, after)
                .await
                .map_err(|error| self.capture_error(error))
        })
    }
    fn write_to_scratch<'a, 'c>(
        &'a self,
        contents: &'a [u8],
        cancellation: parrot_core::CancellationToken<'c>,
    ) -> BoxFuture<'a, Result<String, parrot_core::Error>>
    where
        'c: 'a,
    {
        Box::pin(async move {
            self.controller
                .lock()
                .await
                .borrow_blobs()
                .write_to_scratch(contents, &cancellation)
                .map(|path| path.to_string_lossy().into_owned())
                .map_err(CompactionError::from)
                .map_err(|error| self.capture_error(error))
        })
    }
    fn capture_system_prompt(&mut self) -> BoxFuture<'_, Result<String, parrot_core::Error>> {
        Box::pin(async move {
            self.prompt
                .build(self.selection, self.cancellation)
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
            .estimate_input_tokens(instructions, definitions, messages)
            .map_err(|error| self.capture_error(error))
    }
}
