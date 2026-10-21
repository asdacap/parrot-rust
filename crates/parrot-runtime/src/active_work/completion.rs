//! The ordered source completion callbacks, using the actual history and broker.
use super::{blockers::ActiveWorkReminder, exit_reminders::ExitReminders};
use crate::{
    agent_turn::services::{CompletionCandidate, CompletionDecision, TurnCompletion, TurnError},
    event_broker::{BrokerError, EventBroker},
    interactions::ChildQuestionCoordinator,
};
use parrot_domain::{
    contracts::{
        async_ops::{BoxFuture, Cancellation},
        events::EventPublisher,
        security::AccessSnapshot,
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

/// Supply the same reminder Arc to this callback and `RuntimeConfig.reminders`.
/// The T46 drain owns this callback and joins its reminder mutations at the
/// terminal boundary. Cancellation does not clear any of the observed work.
pub struct SessionCompletion<P: EventPublisher<Event = Event, Error = BrokerError> + ?Sized> {
    pub questions: Option<Arc<ChildQuestionCoordinator>>,
    pub active_work: Arc<ActiveWorkReminder>,
    pub reminders: Arc<ExitReminders<P>>,
    pub history: AgentHistoryView,
    pub broker: Arc<EventBroker>,
    pub publisher: Arc<P>,
}
impl<S, P> TurnCompletion<S> for SessionCompletion<P>
where
    S: AccessSnapshot,
    P: EventPublisher<Event = Event, Error = BrokerError> + ?Sized,
{
    fn complete<'a>(
        &'a self,
        candidate: &'a CompletionCandidate<S>,
        cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<CompletionDecision, TurnError>> {
        Box::pin(async move {
            cancellation.check().map_err(|_| TurnError::Cancelled)?;
            if let Some(questions) = &self.questions {
                if let Some(reminder) = questions
                    .completion_reminder()
                    .map_err(|error| TurnError::Service(error.to_string()))?
                {
                    return self
                        .retained(
                            candidate,
                            reminder,
                            "pending_child_question_reminder",
                            "Retrying with pending child question reminder",
                            cancellation,
                        )
                        .await;
                }
            }
            if let Some(reminder) = self.active_work.build().await? {
                let event = notice(
                    candidate,
                    "active_work_reminder",
                    "Active work reminder injected",
                )?;
                // Source AppendSystemPrompt projects the prompt, but does not
                // record this notice. Its fanout is retained, not storage replay.
                self.history
                    .append_system_prompt(event.clone(), reminder.clone())
                    .await?;
                cancellation.check().map_err(|_| TurnError::Cancelled)?;
                self.broker.publish_retained(&event);
                return Ok(CompletionDecision::Retry {
                    system_message: reminder,
                    retain_candidate_assistant: false,
                    completion_retry_pending: None,
                });
            }
            if let Some(reminder) = self
                .reminders
                .build()
                .await
                .map_err(|error| TurnError::Service(error.to_string()))?
            {
                return self
                    .retained(
                        candidate,
                        reminder,
                        "exit_reminder",
                        "Exit reminder injected",
                        cancellation,
                    )
                    .await;
            }
            Ok(CompletionDecision::Complete)
        })
    }
}
impl<P: EventPublisher<Event = Event, Error = BrokerError> + ?Sized> SessionCompletion<P> {
    async fn retained<S: AccessSnapshot>(
        &self,
        candidate: &CompletionCandidate<S>,
        reminder: String,
        kind: &str,
        text: &str,
        cancellation: &dyn Cancellation,
    ) -> Result<CompletionDecision, TurnError> {
        let event = notice(candidate, kind, text)?;
        self.history
            .append_retained_reminder(
                event.clone(),
                assistant(&candidate.assistant),
                reminder.clone(),
            )
            .await?;
        self.publisher
            .publish_committed(&Committed::after_commit(event), cancellation)
            .await
            .map_err(TurnError::Publication)?;
        Ok(CompletionDecision::Retry {
            system_message: reminder,
            retain_candidate_assistant: true,
            completion_retry_pending: Some(true),
        })
    }
}
fn notice<S: AccessSnapshot>(
    candidate: &CompletionCandidate<S>,
    kind: &str,
    text: &str,
) -> Result<Event, TurnError> {
    let mut event = Event::new();
    event.id = Identifier::event_id().map_err(|error| TurnError::Service(error.to_string()))?;
    event.agent_session_id = candidate.session_id.clone();
    event.set_agent_notice(AgentNotice {
        kind: kind.into(),
        notice: text.into(),
        ..Default::default()
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
