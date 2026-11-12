//! Completion context observations and recording, with completion policy owned by core.
use super::{blockers::ActiveWorkReminder, exit_reminders::ExitReminders};
use crate::{
    agent_turn::services::{
        CompletionBlockerFeedback, CompletionBlockerFeedbackKind, CompletionStateSource, TurnError,
    },
    event_broker::{BrokerError, EventBroker},
    interactions::AgentAsks,
};
use parrot_domain::{
    contracts::{
        async_ops::{BoxFuture, Cancellation},
        events::EventPublisher,
        storage::Committed,
    },
    protocol::messages::parrot::{AgentNotice, Event},
    values::identifiers::Identifier,
};
use parrot_provider::sessions;
use parrot_storage::{
    conversation::{AgentHistoryView, ConversationMessage, MessageRole},
    events::{ConversationOrigin, ConversationPart, ProviderContent, ToolCall},
};
use std::sync::Arc;

/// Supply the same reminder Arc to this source and `RuntimeConfig.reminders`.
/// Native state orders these observations; core retains its candidate for any blockers.
pub struct SessionCompletion<P: EventPublisher<Event = Event, Error = BrokerError> + ?Sized> {
    pub asks: Option<Arc<AgentAsks>>,
    pub active_work: Arc<ActiveWorkReminder>,
    pub reminders: Arc<ExitReminders<P>>,
    pub history: AgentHistoryView,
    pub broker: Arc<EventBroker>,
    pub publisher: Arc<P>,
}
impl<P> CompletionStateSource for SessionCompletion<P>
where
    P: EventPublisher<Event = Event, Error = BrokerError> + ?Sized,
{
    fn get_pending_ask_reminder<'a>(
        &'a self,
        cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<Option<String>, TurnError>> {
        Box::pin(async move {
            cancellation.check().map_err(|_| TurnError::Cancelled)?;
            match &self.asks {
                Some(asks) => asks
                    .completion_reminder()
                    .map_err(|error| TurnError::Service(error.to_string())),
                None => Ok(None),
            }
        })
    }
    fn get_active_work_reminder<'a>(
        &'a self,
        _cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<Option<String>, TurnError>> {
        Box::pin(self.active_work.build())
    }
    fn get_exit_reminder<'a>(
        &'a self,
        _cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<Option<String>, TurnError>> {
        Box::pin(async move {
            self.reminders
                .build()
                .await
                .map_err(|error| TurnError::Service(error.to_string()))
        })
    }
    fn record_completion_blocker_feedback<'a>(
        &'a self,
        feedback: &'a CompletionBlockerFeedback,
        cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<(), TurnError>> {
        Box::pin(async move {
            let (kind, text) = match feedback.kind {
                CompletionBlockerFeedbackKind::PendingAsks => {
                    ("pending_ask_reminder", "Retrying with pending ask reminder")
                }
                CompletionBlockerFeedbackKind::ActiveWork => {
                    ("active_work_reminder", "Active work reminder injected")
                }
                CompletionBlockerFeedbackKind::ExitReminders => {
                    ("exit_reminder", "Exit reminder injected")
                }
            };
            let event = notice(self.history.agent_session_id(), kind, text)?;
            if let Some(candidate) = &feedback.candidate {
                let completed: sessions::Completion = candidate.clone().into();
                self.history
                    .append_retained_reminder(
                        event.clone(),
                        assistant(&completed.into_message()),
                        feedback.message.clone(),
                    )
                    .await?;
                self.publisher
                    .publish_committed(&Committed::after_commit(event), cancellation)
                    .await
                    .map_err(TurnError::Publication)?;
            } else {
                // Source AppendSystemPrompt projects the prompt, but does not
                // record this notice. Its fanout is retained, not storage replay.
                self.history
                    .append_system_prompt(feedback.message.clone())
                    .await?;
                cancellation.check().map_err(|_| TurnError::Cancelled)?;
                self.broker.publish_retained(&event);
            }
            Ok(())
        })
    }
}
fn notice(session_id: &str, kind: &str, text: &str) -> Result<Event, TurnError> {
    let mut event = Event::new();
    event.id = Identifier::event_id().map_err(|error| TurnError::Service(error.to_string()))?;
    event.agent_session_id = session_id.to_owned();
    event.set_agent_notice(AgentNotice {
        kind: kind.into(),
        notice: text.into(),
        ..AgentNotice::new()
    });
    Ok(event)
}
fn assistant(message: &sessions::Message) -> ConversationMessage {
    ConversationMessage {
        origin: ConversationOrigin::Model,
        role: MessageRole::Assistant,
        parts: vec![ConversationPart::text(message.content())],
        tool_calls: message
            .tool_calls
            .iter()
            .map(|call| ToolCall {
                id: call.id.clone(),
                name: call.name.clone(),
                arguments_json: call.arguments_json.clone(),
            })
            .collect(),
        tool_call_id: message.tool_call_id.clone(),
        provider_content: ProviderContent {
            provider_id: message.provider_id.clone(),
            content: message.provider_content.clone(),
        },
    }
}
