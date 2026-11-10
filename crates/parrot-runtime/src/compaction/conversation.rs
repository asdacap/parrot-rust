use super::{ContextError, ContextEvents, ContextResult};
use parrot_core::{
    Content, ConversationAddition, ConversationEvent, ConversationPersistence, ConversationReceipt,
    Message, Role, StatusCheckpoint,
};
use parrot_domain::{
    contracts::async_ops::Cancellation,
    protocol::messages::parrot::{AgentNotice, Event},
    values::identifiers::Identifier,
};
use parrot_storage::{
    conversation::{
        AgentConversation, AgentHistoryView, ModelContent, ModelMessage, PendingStatus,
    },
    events::{MessageRole, NeverCancelled, ProviderContent, ToolCall},
};

pub(crate) async fn add_conversation(
    history: &AgentHistoryView,
    conversation: &mut AgentConversation,
    initial_status_pending: &mut bool,
    addition: ConversationAddition,
    events: &dyn ContextEvents,
    cancellation: &dyn Cancellation,
) -> ContextResult<ConversationReceipt> {
    let publish_uncancelled = matches!(
        addition.persistence,
        ConversationPersistence::Reminder { .. }
    );
    let message = addition
        .messages
        .first()
        .ok_or_else(|| ContextError::Argument("conversation addition has no message".into()))?;
    let content = message
        .contents
        .iter()
        .filter_map(|part| match part {
            Content::Text(text) => Some(text.as_str()),
            Content::Image { .. } => None,
        })
        .collect::<String>();
    let (inserted, published) = match addition.persistence {
        ConversationPersistence::Phase { final_phase, event } => {
            let published = notice(history, event)?;
            let inserted = if final_phase {
                history
                    .append_final_provider_request_prompt(published.clone(), content)
                    .await?
            } else {
                history
                    .append_tool_availability_restored_prompt(published.clone(), content)
                    .await?
            };
            (inserted, published)
        }
        ConversationPersistence::Reminder { checkpoint, event } => {
            let published = notice(history, event)?;
            let inserted = history
                .append_context_reminder(
                    published.clone(),
                    parrot_storage::conversation::ContextReminderCheckpoint {
                        canonical_model: checkpoint.canonical_model,
                        context_limit: checkpoint.context_limit,
                        percentage: checkpoint.percentage,
                    },
                    content,
                )
                .await?;
            (inserted, published)
        }
        ConversationPersistence::Status { checkpoint, event } => {
            let published = notice(history, event)?;
            let inserted = match checkpoint {
                StatusCheckpoint::Initial => {
                    if *initial_status_pending {
                        history
                            .append_system_prompt(published.clone(), content)
                            .await?;
                        *initial_status_pending = false;
                        true
                    } else {
                        false
                    }
                }
                StatusCheckpoint::Mode { mode, version } => {
                    history
                        .append_status_prompt(
                            published.clone(),
                            PendingStatus { mode, version },
                            content,
                        )
                        .await?
                }
            };
            (inserted, published)
        }
        _ => {
            return Err(ContextError::InvalidOperation(
                "unsupported context conversation persistence".into(),
            ));
        }
    };
    if inserted {
        for message in addition.messages {
            conversation.add(materialize_message(message));
        }
        if publish_uncancelled {
            events.publish(published, &NeverCancelled).await?;
        } else {
            events.publish(published, cancellation).await?;
        }
    }
    Ok(ConversationReceipt {
        inserted,
        sequence: 0,
    })
}

fn notice(history: &AgentHistoryView, event: ConversationEvent) -> ContextResult<Event> {
    let ConversationEvent::Notice { kind, notice } = event else {
        return Err(ContextError::Argument(
            "context persistence requires a notice event".into(),
        ));
    };
    let mut published = Event {
        id: Identifier::event_id().map_err(ContextError::Identifier)?,
        agent_session_id: history.agent_session_id().into(),
        ..Event::new()
    };
    published.set_agent_notice(AgentNotice {
        kind,
        notice,
        ..AgentNotice::new()
    });
    Ok(published)
}

pub(crate) fn materialize_message(message: Message) -> ModelMessage {
    ModelMessage {
        role: match message.role {
            Role::System => MessageRole::System,
            Role::User => MessageRole::User,
            Role::Assistant => MessageRole::Assistant,
            Role::Tool => MessageRole::Tool,
        },
        contents: message
            .contents
            .into_iter()
            .map(|content| match content {
                Content::Text(text) => ModelContent::Text(text),
                Content::Image {
                    path,
                    media_type,
                    width,
                    height,
                    ..
                } => ModelContent::ImageFile {
                    path: path.into(),
                    media_type,
                    width,
                    height,
                },
            })
            .collect(),
        tool_calls: message
            .tool_calls
            .into_iter()
            .map(|call| ToolCall {
                id: call.id,
                name: call.name,
                arguments_json: call.arguments_json,
            })
            .collect(),
        tool_call_id: message.tool_call_id,
        provider_content: ProviderContent {
            provider_id: message.provider_id,
            content: message.provider_content,
        },
    }
}
