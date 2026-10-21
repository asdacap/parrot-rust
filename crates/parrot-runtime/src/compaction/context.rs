//! Awaited context epochs over the authoritative storage history.
use super::{
    CompactionError, CompactionGroup, Compactor, ContextCadence, ContextSize, ContextSnapshot,
    PlanningError, PlanningInput, ProviderTokenBudget, RetryObserver,
};
use crate::prompt_context::{PromptError, SessionPrompt};
use parrot_domain::{
    contracts::{
        async_ops::{BoxFuture, Cancellation},
        storage::Committed,
    },
    protocol::messages::parrot::{
        AgentNotice, CompactionFailed, CompactionFinished, CompactionStarted, Event,
    },
    values::{context::ContextCadenceError, identifiers::Identifier},
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
    Cadence(ContextCadenceError),
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
            Self::Cadence(error) => error.fmt(f),
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
impl From<ContextCadenceError> for ContextError {
    fn from(error: ContextCadenceError) -> Self {
        Self::Cadence(error)
    }
}
pub type ContextResult<T> = Result<T, ContextError>;

/// The actual status owner binds its agent/profile/selection in this adapter.
/// Fitting must evaluate the supplied estimator against the actual rendered status.
pub trait ContextStatus: Send + Sync {
    fn observe<'a>(
        &'a self,
        context: &'a ContextSnapshot,
        cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, ContextResult<String>>;
    fn observe_fitted<'a>(
        &'a self,
        estimate: &'a (dyn Fn(&str) -> ContextResult<ContextSnapshot> + Send + Sync),
        cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, ContextResult<String>>;
}

/// `emit` persists a new event; `publish_committed` requires its real transaction receipt.
/// `publish` retains projection-only notices after their history effect commits;
/// source status/reminder projection-only operations deliberately do not record the event.
/// The caller must await this controller (including terminal publication) before draining.
pub trait ContextEvents: RetryObserver {
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
    cadence: ContextCadence,
    cadence_restored: bool,
    initial_status_pending: bool,
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
            cadence: ContextCadence::new(),
            cadence_restored: false,
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

    /// Called before promotion. The fit estimate includes actual pending input parts.
    pub async fn inject_status(
        &mut self,
        mut input: EpochInput<'_>,
        depth: i32,
        profile_id: &str,
        cancellation: &dyn Cancellation,
    ) -> ContextResult<StatusInjection> {
        self.ensure_epoch(input.prompt, cancellation).await?;
        if depth > 0 {
            if self.initial_status_pending {
                let content = self.status_for_insertion(&mut input, cancellation).await?;
                let published = self.notice("status", "Status prompt injected".into())?;
                self.history
                    .append_system_prompt(published.clone(), content.clone())
                    .await?;
                input.conversation.add(system_model_message(content));
                self.initial_status_pending = false;
                input.events.publish(published, cancellation).await?;
            }
            return Ok(StatusInjection::Complete);
        }
        while let Some(pending) = self.history.pending_status().await? {
            cancellation.check().map_err(|_| ContextError::Cancelled)?;
            if profile_id != pending.mode {
                return Ok(StatusInjection::RefreshSelection);
            }
            let content = self.status_for_insertion(&mut input, cancellation).await?;
            let published = self.notice("status", "Status prompt injected".into())?;
            if !self
                .history
                .append_status_prompt(published.clone(), pending, content.clone())
                .await?
            {
                // Source refreshes the entire selection before retrying a version conflict.
                return Ok(StatusInjection::RefreshSelection);
            }
            input.conversation.add(system_model_message(content));
            input.events.publish(published, cancellation).await?;
        }
        Ok(StatusInjection::Complete)
    }

    async fn status_for_insertion(
        &self,
        input: &mut EpochInput<'_>,
        cancellation: &dyn Cancellation,
    ) -> ContextResult<String> {
        let mut history = provider_messages(input.conversation);
        for pending in self.history.inputs_for_next_promotion().await? {
            let contents = self.history.materialize(pending.parts).await?;
            history.push(provider_message(&ModelMessage {
                role: MessageRole::User,
                contents,
                tool_calls: Vec::new(),
                tool_call_id: String::new(),
                provider_content: Default::default(),
            }));
        }
        let instructions = input.prompt.build(input.selection, cancellation).await?;
        let estimate = |content: &str| -> ContextResult<ContextSnapshot> {
            let mut candidate = history.clone();
            candidate.push(Message::system(content));
            Ok(self.compactor.planner().estimate_selected_context(
                input.selection,
                &instructions,
                input.tools,
                &candidate,
            )?)
        };
        input.status.observe_fitted(&estimate, cancellation).await
    }

    pub async fn inject_final_provider_request_prompt(
        &self,
        conversation: &mut AgentConversation,
        content: &str,
        events: &dyn ContextEvents,
        cancellation: &dyn Cancellation,
    ) -> ContextResult<()> {
        let published = self.notice(
            "final_provider_request_prompt",
            "Final provider request prompt injected".into(),
        )?;
        self.history
            .append_final_provider_request_prompt(published.clone(), content.into())
            .await?;
        conversation.add(system_model_message(content.into()));
        events.publish(published, cancellation).await
    }

    pub async fn restore_tool_availability(
        &self,
        conversation: &mut AgentConversation,
        content: &str,
        events: &dyn ContextEvents,
        cancellation: &dyn Cancellation,
    ) -> ContextResult<bool> {
        let published = self.notice(
            "tool_availability_restored_prompt",
            "Tool availability restored prompt injected".into(),
        )?;
        if !self
            .history
            .append_tool_availability_restored_prompt(published.clone(), content.into())
            .await?
        {
            return Ok(false);
        }
        conversation.add(system_model_message(content.into()));
        events.publish(published, cancellation).await?;
        Ok(true)
    }

    pub async fn prepare_epoch(
        &mut self,
        mut input: EpochInput<'_>,
        cancellation: &dyn Cancellation,
    ) -> ContextResult<EpochResult> {
        cancellation.check().map_err(|_| ContextError::Cancelled)?;
        self.ensure_epoch(input.prompt, cancellation).await?;
        let mut instructions = input.prompt.build(input.selection, cancellation).await?;
        let mut context = self.estimate(&input, &instructions)?;
        let calibrated = self.budget.estimate_input_tokens(
            &input.selection.canonical_model.selector(),
            context.estimated_tokens,
        );
        let mut reduced = false;
        if context.exceeds_trigger()
            || (context.input_limit > 0 && context.exceeds_compaction_trigger(calibrated))
        {
            let compacted = self
                .compact_epoch(reborrow(&mut input), None, cancellation)
                .await?;
            instructions = compacted.instructions;
            reduced = compacted.reduced;
            context = self.estimate(&input, &instructions)?;
            ensure_request_fits(&context)?;
        }
        if !self.cadence_restored {
            self.cadence
                .restore(self.history.latest_context_reminder().await?.map(|value| {
                    parrot_domain::values::context::ContextReminderCheckpoint {
                        canonical_model: value.canonical_model,
                        context_limit: value.context_limit,
                        percentage: value.percentage,
                    }
                }));
            self.cadence_restored = true;
        }
        let selector = input.selection.canonical_model.selector();
        if let Some(percentage) =
            self.cadence
                .observe(&context, &selector, input.conversation.len() as i64)?
        {
            let reminder = self.render_reminder(&context, percentage)?;
            let mut candidate = provider_messages(input.conversation);
            candidate.push(Message::system(&reminder));
            let inserted = self.compactor.planner().estimate_selected_context(
                input.selection,
                &instructions,
                input.tools,
                &candidate,
            )?;
            if inserted.is_available() {
                if inserted.exceeds_input_limit() || inserted.exceeds_trigger() {
                    let compacted = self
                        .compact_epoch(reborrow(&mut input), None, cancellation)
                        .await?;
                    instructions = compacted.instructions;
                    reduced |= compacted.reduced;
                    ensure_request_fits(&self.estimate(&input, &instructions)?)?;
                } else {
                    let published = self.notice(
                        "context_reminder",
                        format!(
                            "Context reminder injected ({}% context used)",
                            context.usage_percent.unwrap_or_default(),
                        ),
                    )?;
                    let checkpoint = parrot_storage::conversation::ContextReminderCheckpoint {
                        canonical_model: selector.clone(),
                        context_limit: context.context_limit,
                        percentage,
                    };
                    if self
                        .history
                        .append_context_reminder(published.clone(), checkpoint, reminder.clone())
                        .await?
                    {
                        input.conversation.add(system_model_message(reminder));
                        let persisted = self.estimate(&input, &instructions)?;
                        self.cadence.acknowledge(
                            &persisted,
                            &selector,
                            input.conversation.len() as i64,
                        )?;
                        input.events.publish(published, &Uncancelled).await?;
                    } else {
                        self.cadence.acknowledge(
                            &inserted,
                            &selector,
                            input.conversation.len() as i64,
                        )?;
                    }
                }
            }
        }
        Ok(EpochResult {
            instructions,
            reduced,
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
        let mut started = self.event()?;
        started.set_compaction_started(CompactionStarted::new());
        input.events.emit(started, &Uncancelled).await?;
        let outcome: ContextResult<EpochResult> = async {
            let result = self.compact_inner(&mut input, target, cancellation).await?;
            let mut finished = self.event()?;
            finished.set_compaction_finished(CompactionFinished::new());
            input.events.emit(finished, &Uncancelled).await?;
            Ok(result)
        }
        .await;
        match outcome {
            Ok(result) => Ok(result),
            Err(error) => {
                let mut failed = self.event()?;
                failed.set_compaction_failed(CompactionFailed {
                    message: error.to_string(),
                    ..CompactionFailed::new()
                });
                input.events.emit(failed, &Uncancelled).await?;
                Err(error)
            }
        }
    }

    async fn compact_inner(
        &mut self,
        input: &mut EpochInput<'_>,
        target: Option<&ContextSize>,
        cancellation: &dyn Cancellation,
    ) -> ContextResult<EpochResult> {
        input.prompt.sample_epoch(cancellation).await?;
        let instructions = input.prompt.build(input.selection, cancellation).await?;
        let effective = self.history.effective_conversation_groups().await?;
        let checkpoints = self.history.active_checkpoint_assistant_sequences().await?;
        let base = effective
            .snapshot
            .as_ref()
            .map_or(0, |snapshot| snapshot.watermark);
        let mut materialized = Vec::new();
        if let Some(snapshot) = &effective.snapshot {
            materialized.push(vec![Message::system(&snapshot.summary)]);
        }
        for group in &effective.groups {
            let mut messages = Vec::new();
            for item in &group.items {
                messages.push(provider_message(
                    &input.conversation.materialize(item.clone()).await?,
                ));
            }
            materialized.push(messages);
        }
        let mut groups = Vec::new();
        let offset = usize::from(effective.snapshot.is_some());
        if offset > 0 {
            groups.push(CompactionGroup {
                messages: &materialized[0],
                end_watermark: base,
                has_checkpoint_before: false,
                is_complete: true,
            });
        }
        groups.extend(
            effective
                .groups
                .iter()
                .enumerate()
                .map(|(index, group)| CompactionGroup {
                    messages: &materialized[index + offset],
                    end_watermark: group.end_watermark,
                    has_checkpoint_before: group.assistant_sequence > 0
                        && checkpoints.contains(&group.assistant_sequence),
                    is_complete: group.is_complete,
                }),
        );
        let status = input
            .status
            .observe(&self.estimate(input, &instructions)?, cancellation)
            .await?;
        let fixed = Message::system(status);
        let compacted = self
            .compactor
            .compact(
                input.selection,
                target,
                &PlanningInput {
                    instructions: &instructions,
                    tools: input.tools,
                    groups: &groups,
                    base_watermark: base,
                    fixed_message: &fixed,
                },
                &mut self.blobs,
                input.sessions,
                &ContextRetry(input.events),
                cancellation,
            )
            .await?;
        let mut reduced = false;
        if let Some(compacted) = compacted.filter(|value| value.watermark > base) {
            let estimate = |content: &str| -> ContextResult<ContextSnapshot> {
                let history = replace_status(&compacted.history, content);
                Ok(self.compactor.planner().estimate_selected_context(
                    input.selection,
                    &instructions,
                    input.tools,
                    &history,
                )?)
            };
            let status = input.status.observe_fitted(&estimate, cancellation).await?;
            let rendered = estimate(&status)?;
            if rendered.exceeds_input_limit() {
                return Err(ContextError::InvalidOperation(
                    "The compacted conversation exceeds the selected model input limit.".into(),
                ));
            }
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
            let persisted = self.estimate(input, &instructions)?;
            self.cadence.acknowledge(
                &persisted,
                &input.selection.canonical_model.selector(),
                input.conversation.len() as i64,
            )?;
            input
                .events
                .publish_committed(Committed::after_commit(published), &Uncancelled)
                .await?;
        } else if self.estimate(input, &instructions)?.estimated_tokens
            > i64::from(input.selection.canonical_model.model.input_token_limit())
        {
            return Err(ContextError::InvalidOperation(
                "The conversation has no safe compaction boundary before the input limit.".into(),
            ));
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
    fn render_reminder(&self, context: &ContextSnapshot, percentage: i32) -> ContextResult<String> {
        let mut arguments = vec![
            ("percentage".into(), percentage.to_string().into()),
            (
                "estimated_tokens".into(),
                context.estimated_tokens.to_string().into(),
            ),
            (
                "context_limit".into(),
                context.context_limit.to_string().into(),
            ),
            (
                "notification_interval".into(),
                ContextCadence::NOTIFICATION_INTERVAL.to_string().into(),
            ),
            ("trigger".into(), context.trigger_percent.to_string().into()),
        ];
        if context.has_context_limit_override {
            arguments.push((
                "trigger_tokens".into(),
                context
                    .trigger_tokens
                    .map_or_else(|| "unavailable".into(), |value| value.to_string())
                    .into(),
            ));
        }
        self.templates
            .render(
                if context.has_context_limit_override {
                    "agent-session.context-limit-reminder"
                } else {
                    "agent-session.context-reminder"
                },
                &arguments,
                &Uncancelled,
            )
            .map_err(|error| ContextError::Operation(error.to_string()))
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
fn ensure_request_fits(context: &ContextSnapshot) -> ContextResult<()> {
    if context.exceeds_input_limit() {
        return Err(ContextError::InvalidOperation(
            "The compacted conversation exceeds the selected model input limit.".into(),
        ));
    }
    Ok(())
}
fn replace_status(history: &[Message], content: &str) -> Vec<Message> {
    let mut history = history.to_vec();
    if let Some(status) = history
        .iter_mut()
        .skip(1)
        .find(|message| message.role == Role::System)
    {
        *status = Message::system(content);
    }
    history
}
fn system_model_message(text: String) -> ModelMessage {
    ModelMessage {
        role: MessageRole::System,
        contents: vec![ModelContent::Text(text)],
        tool_calls: Vec::new(),
        tool_call_id: String::new(),
        provider_content: Default::default(),
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
struct ContextRetry<'a>(&'a dyn ContextEvents);
impl RetryObserver for ContextRetry<'_> {
    fn observe<'a>(
        &'a self,
        event: &'a parrot_provider::sessions::Event,
        cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<(), parrot_provider::sessions::Error>> {
        self.0.observe(event, cancellation)
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
