use super::*;
use crate::events::{
    Admission, AdmittedInput, NeverCancelled, Promotion, SessionUsage, ToolExecutionTerminal,
};
use parrot_domain::protocol::messages::parrot::{Delivery, Event, RequestUsageRecorded};

impl ConversationRepository {
    pub async fn append(
        &self,
        event: Event,
        role: Option<String>,
        content: Option<String>,
    ) -> Result<Option<SessionUsage>, EventError> {
        let origin = match role.as_deref() {
            Some("system") => ConversationOrigin::System,
            Some("user") => ConversationOrigin::UserInput,
            Some("tool") => ConversationOrigin::Tool,
            _ => ConversationOrigin::Model,
        };
        let message = match (role, content) {
            (Some(role), Some(content)) => {
                let parsed = match role.as_str() {
                    "system" => MessageRole::System,
                    "user" => MessageRole::User,
                    "assistant" => MessageRole::Assistant,
                    _ => {
                        return Err(EventError::Invalid(format!(
                            "the message table holds an unknown role {role}"
                        )));
                    }
                };
                Some(ConversationMessage::text(parsed, content))
            }
            _ => None,
        };
        self.append_message(event, message, origin).await
    }
    pub async fn append_message(
        &self,
        event: Event,
        mut message: Option<ConversationMessage>,
        origin: ConversationOrigin,
    ) -> Result<Option<SessionUsage>, EventError> {
        self.validate(&event.agent_session_id)?;
        if let Some(message) = &mut message {
            message.origin = origin;
        }
        let projected = message.is_some();
        let agent = event.agent_session_id.clone();
        let result = self.events.append(event, message).await?;
        if projected {
            self.refresh_agent_history(&agent).await?;
        }
        Ok(result.usage)
    }
    pub async fn append_conversation(
        &self,
        event: Event,
        message: ConversationMessage,
    ) -> Result<(), EventError> {
        self.validate(&event.agent_session_id)?;
        let agent = event.agent_session_id.clone();
        let context = self.events.context().clone();
        let projected_agent = agent.clone();
        self.events
            .transaction(move |tx| {
                crate::events::record(tx, &event, None, &(context.timestamp)())?;
                crate::events::projection::project(
                    tx,
                    &projected_agent,
                    &message,
                    &(context.timestamp)(),
                )?;
                Ok(())
            })
            .await?;
        self.refresh_agent_history(&agent).await
    }
}

/// A bound view shares the selected actor/cache and adds only derived-file hooks.
#[derive(Clone)]
pub struct AgentHistoryView {
    repository: ConversationRepository,
    agent: String,
}
impl AgentHistoryView {
    pub(crate) fn new(repository: ConversationRepository, agent: String) -> Self {
        Self { repository, agent }
    }
    pub fn repository(&self) -> &ConversationRepository {
        &self.repository
    }
    pub fn agent_session_id(&self) -> &str {
        &self.agent
    }
    pub async fn append(
        &self,
        event: Event,
        role: Option<String>,
        content: Option<String>,
    ) -> Result<Option<SessionUsage>, EventError> {
        self.repository.append(event, role, content).await
    }
    pub async fn append_message(
        &self,
        event: Event,
        message: Option<ConversationMessage>,
        origin: ConversationOrigin,
    ) -> Result<Option<SessionUsage>, EventError> {
        self.repository.append_message(event, message, origin).await
    }
    pub async fn append_conversation(
        &self,
        event: Event,
        message: ConversationMessage,
    ) -> Result<(), EventError> {
        self.repository.append_conversation(event, message).await
    }
    pub async fn has_tool_synthetic(&self, assistant: i64) -> Result<bool, EventError> {
        self.repository
            .events
            .has_tool_synthetic(self.agent.clone(), assistant)
            .await
    }
    pub async fn append_tool_synthetic(
        &self,
        event: Event,
        assistant: i64,
        parts: Vec<ConversationPart>,
    ) -> Result<Option<ConversationItem>, EventError> {
        self.repository.validate(&event.agent_session_id)?;
        let agent = event.agent_session_id.clone();
        let item = self
            .repository
            .events
            .append_tool_synthetic(event, assistant, parts)
            .await?;
        if item.is_some() {
            self.repository.refresh_agent_history(&agent).await?;
        }
        Ok(item)
    }
    pub async fn append_tool_settlement(
        &self,
        event: Event,
        assistant: i64,
        terminal: ToolExecutionTerminal,
    ) -> Result<Option<ConversationItem>, EventError> {
        self.repository.validate(&event.agent_session_id)?;
        let agent = event.agent_session_id.clone();
        let item = self
            .repository
            .events
            .append_tool_settlement(event, assistant, terminal)
            .await?;
        if item.is_some() {
            self.repository.refresh_agent_history(&agent).await?;
        }
        Ok(item)
    }
    pub async fn tool_terminals_of_calls(
        &self,
        calls: Vec<String>,
    ) -> Result<Vec<ToolExecutionTerminal>, EventError> {
        self.repository
            .events
            .tool_terminals_of_calls(self.agent.clone(), calls)
            .await
    }
    pub async fn admit_parts<F>(
        &self,
        message_id: String,
        parts: Vec<ConversationPart>,
        delivery: Delivery,
        compose: F,
    ) -> Result<Admission, EventError>
    where
        F: FnOnce(&AdmittedInput) -> Result<Event, EventError> + Send + 'static,
    {
        let ticket = self
            .repository
            .events
            .admit_input(
                self.agent.clone(),
                message_id,
                parts,
                delivery,
                compose,
                &NeverCancelled,
            )
            .map_err(|error| match error {
                parrot_domain::contracts::storage::AdmissionError::Rejected(error) => error,
                parrot_domain::contracts::storage::AdmissionError::Cancelled => {
                    unreachable!("uncancelled admission")
                }
            })?;
        Ok(ticket.into_completion().await?.into_value().into_output())
    }
    pub async fn cancel_pending_input<F>(
        &self,
        input_id: String,
        compose: F,
    ) -> Result<Option<Event>, EventError>
    where
        F: FnOnce() -> Result<Event, EventError> + Send + 'static,
    {
        self.repository
            .events
            .cancel_pending_input(self.agent.clone(), input_id, compose)
            .await
    }
    pub async fn promote_steers<F>(&self, compose: F) -> Result<Vec<Promotion>, EventError>
    where
        F: FnMut(&AdmittedInput) -> Result<Event, EventError> + Send + 'static,
    {
        self.repository.validate(&self.agent)?;
        let promoted = self
            .repository
            .events
            .promote_steers(self.agent.clone(), compose)
            .await?;
        if !promoted.is_empty() {
            self.refresh().await?;
        }
        Ok(promoted)
    }
    pub async fn promote_next_queue<F>(&self, compose: F) -> Result<Vec<Promotion>, EventError>
    where
        F: FnMut(&AdmittedInput) -> Result<Event, EventError> + Send + 'static,
    {
        self.repository.validate(&self.agent)?;
        let promoted = self
            .repository
            .events
            .promote_next_queue(self.agent.clone(), compose)
            .await?;
        if !promoted.is_empty() {
            self.refresh().await?;
        }
        Ok(promoted)
    }
    pub async fn inputs_for_next_promotion(&self) -> Result<Vec<AdmittedInput>, EventError> {
        self.repository
            .events
            .inputs_for_next_promotion(self.agent.clone())
            .await
    }
    pub async fn has_pending_inputs(&self) -> Result<bool, EventError> {
        self.repository
            .events
            .has_pending_inputs(self.agent.clone())
            .await
    }
    pub async fn notices(
        &self,
        kind: String,
    ) -> Result<Vec<parrot_domain::protocol::messages::parrot::AgentNotice>, EventError> {
        self.repository
            .events
            .notices(self.agent.clone(), kind)
            .await
    }
    pub async fn messages(&self) -> Result<Vec<String>, EventError> {
        self.repository.messages(self.agent.clone()).await
    }
    pub async fn materialize(
        &self,
        parts: Vec<ConversationPart>,
    ) -> Result<Vec<ModelContent>, EventError> {
        self.repository.materialize(parts).await
    }
    pub async fn resolve_image_artifact(
        &self,
        id: String,
    ) -> Result<Option<crate::artifacts::ImageArtifactMetadata>, EventError> {
        self.repository.resolve_image_artifact(id).await
    }
    pub async fn conversation(&self) -> Result<Vec<ConversationItem>, EventError> {
        self.repository.conversation(self.agent.clone()).await
    }
    pub async fn conversation_after(
        &self,
        watermark: i64,
    ) -> Result<Vec<ConversationItem>, EventError> {
        self.repository
            .conversation_after(self.agent.clone(), watermark)
            .await
    }
    pub async fn record_checkpoint(
        &self,
        title: String,
        assistant: i64,
        call: String,
    ) -> Result<bool, EventError> {
        self.repository
            .record_checkpoint(self.agent.clone(), title, assistant, call)
            .await
    }
    pub async fn active_checkpoint_assistant_sequences(
        &self,
    ) -> Result<std::collections::BTreeSet<i64>, EventError> {
        self.repository
            .active_checkpoint_assistant_sequences(self.agent.clone())
            .await
    }
    pub async fn effective_conversation_groups(
        &self,
    ) -> Result<EffectiveConversationHistory, EventError> {
        self.repository
            .effective_conversation_groups(self.agent.clone())
            .await
    }
    pub async fn compaction_history(&self) -> Result<Option<CompactionContext>, EventError> {
        self.repository.compaction_history(self.agent.clone()).await
    }
    pub async fn append_compaction_status(
        &self,
        event: Event,
        snapshot: CompactionSnapshot,
        content: String,
    ) -> Result<bool, EventError> {
        self.repository
            .append_compaction_status(event, snapshot, content)
            .await
    }
    pub async fn pending_status(&self) -> Result<Option<PendingStatus>, EventError> {
        self.repository.pending_status(self.agent.clone()).await
    }
    pub async fn append_status_prompt(
        &self,
        event: Event,
        expected: PendingStatus,
        content: String,
    ) -> Result<bool, EventError> {
        self.repository
            .append_status_prompt(event, expected, content)
            .await
    }
    pub async fn append_system_prompt(
        &self,
        event: Event,
        content: String,
    ) -> Result<(), EventError> {
        self.repository.append_system_prompt(event, content).await
    }
    pub async fn append_retained_reminder(
        &self,
        event: Event,
        assistant: ConversationMessage,
        reminder: String,
    ) -> Result<(), EventError> {
        self.repository
            .append_retained_reminder(event, assistant, reminder)
            .await
    }
    pub async fn latest_context_reminder(
        &self,
    ) -> Result<Option<ContextReminderCheckpoint>, EventError> {
        self.repository
            .latest_context_reminder(self.agent.clone())
            .await
    }
    pub async fn append_context_reminder(
        &self,
        event: Event,
        checkpoint: ContextReminderCheckpoint,
        content: String,
    ) -> Result<bool, EventError> {
        self.repository
            .append_context_reminder(event, checkpoint, content)
            .await
    }
    pub async fn append_final_provider_request_prompt(
        &self,
        event: Event,
        content: String,
    ) -> Result<(), EventError> {
        self.repository
            .append_final_provider_request_prompt(event, content)
            .await
    }
    pub async fn append_tool_availability_restored_prompt(
        &self,
        event: Event,
        content: String,
    ) -> Result<bool, EventError> {
        self.repository
            .append_tool_availability_restored_prompt(event, content)
            .await
    }
    pub async fn find_request_usage(
        &self,
        tool_call_id: String,
    ) -> Result<Option<RequestUsageRecorded>, EventError> {
        self.repository
            .events
            .find_request_usage(self.agent.clone(), tool_call_id)
            .await
    }
    pub async fn initialize_fork(
        &self,
        source: String,
        boundary: HistoryForkBoundary,
        selection: HistoryForkSelection,
        fallback: Option<HistoryForkSource>,
    ) -> Result<(), ForkError> {
        self.repository
            .initialize_forked_agent_history(
                source,
                self.agent.clone(),
                boundary,
                selection,
                fallback,
            )
            .await
    }
    pub async fn cleanup_fork(&self) -> Result<(), EventError> {
        self.repository
            .cleanup_forked_agent_history(self.agent.clone())
            .await
    }
    pub async fn refresh(&self) -> Result<(), EventError> {
        self.repository.refresh_agent_history(&self.agent).await
    }
}

/// In-memory conversation rebuilt only from authoritative SQLite history.
pub struct AgentConversation {
    history: AgentHistoryView,
    messages: Arc<std::sync::Mutex<Vec<ModelMessage>>>,
}

/// Read-only access to the same canonical messages mutated by the drain.
#[derive(Clone)]
pub struct ConversationReadView {
    messages: Arc<std::sync::Mutex<Vec<ModelMessage>>>,
}
impl ConversationReadView {
    pub fn messages(&self) -> Vec<ModelMessage> {
        self.messages.lock().expect("conversation poisoned").clone()
    }
}
impl AgentConversation {
    pub async fn new(history: AgentHistoryView) -> Result<Self, EventError> {
        let messages = restore(&history).await?;
        Ok(Self {
            history,
            messages: Arc::new(std::sync::Mutex::new(messages)),
        })
    }
    pub fn read_view(&self) -> ConversationReadView {
        ConversationReadView {
            messages: self.messages.clone(),
        }
    }
    pub fn messages(&self) -> Vec<ModelMessage> {
        self.read_view().messages()
    }
    pub fn len(&self) -> usize {
        self.messages.lock().expect("conversation poisoned").len()
    }
    pub fn is_empty(&self) -> bool {
        self.messages
            .lock()
            .expect("conversation poisoned")
            .is_empty()
    }
    pub fn add(&mut self, message: ModelMessage) {
        self.messages
            .lock()
            .expect("conversation poisoned")
            .push(message);
    }
    pub async fn reload(&mut self) -> Result<(), EventError> {
        match restore(&self.history).await {
            Ok(messages) => {
                *self.messages.lock().expect("conversation poisoned") = messages;
                Ok(())
            }
            Err(error) => {
                self.messages.lock().expect("conversation poisoned").clear();
                Err(error)
            }
        }
    }
    pub async fn materialize(&self, item: ConversationItem) -> Result<ModelMessage, EventError> {
        materialize(&self.history, item).await
    }
    pub fn is_answerable(&self) -> bool {
        matches!(
            self.messages
                .lock()
                .expect("conversation poisoned")
                .iter()
                .rev()
                .find(|message| message.role != MessageRole::System)
                .map(|message| message.role),
            Some(MessageRole::User | MessageRole::Tool)
        )
    }
}
async fn materialize(
    history: &AgentHistoryView,
    item: ConversationItem,
) -> Result<ModelMessage, EventError> {
    Ok(ModelMessage {
        role: item.message.role,
        contents: history.materialize(item.message.parts).await?,
        tool_calls: item.message.tool_calls,
        tool_call_id: item.message.tool_call_id,
        provider_content: item.message.provider_content,
    })
}
async fn restore(history: &AgentHistoryView) -> Result<Vec<ModelMessage>, EventError> {
    let mut messages = Vec::new();
    let items = if let Some(context) = history.compaction_history().await? {
        messages.push(ModelMessage {
            role: MessageRole::System,
            contents: vec![ModelContent::Text(context.snapshot.summary)],
            tool_calls: vec![],
            tool_call_id: String::new(),
            provider_content: ProviderContent::default(),
        });
        if let Some(status) = context.status {
            messages.push(materialize(history, status).await?);
        }
        context.tail
    } else {
        history.conversation().await?
    };
    for item in items {
        messages.push(materialize(history, item).await?);
    }
    Ok(messages)
}
