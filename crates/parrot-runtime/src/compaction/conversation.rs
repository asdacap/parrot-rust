use super::{ContextError, ContextEvents, ContextResult};
use parrot_core::{
    Content, ConversationAddition, ConversationEvent, ConversationPersistence, ConversationReceipt,
    Message, Role,
};
use parrot_domain::{
    contracts::async_ops::Cancellation,
    protocol::messages::parrot::{AgentNotice, Event},
    values::identifiers::Identifier,
};
use parrot_storage::{
    conversation::{AgentConversation, AgentHistoryView, ModelContent, ModelMessage},
    events::{MessageRole, ProviderContent, ToolCall},
};

pub async fn add_conversation_messages(
    history: &AgentHistoryView,
    conversation: &mut AgentConversation,
    addition: ConversationAddition,
    events: &dyn ContextEvents,
    cancellation: &dyn Cancellation,
) -> ContextResult<ConversationReceipt> {
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
            history.append_phase_prompt(final_phase, content).await?;
            (true, published)
        }
        ConversationPersistence::StatusSteering { standing, event } => {
            let published = notice(history, event)?;
            history
                .append_status_assembly(standing.clone(), content)
                .await?;
            (true, published)
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
        events.publish(published, cancellation).await?;
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
    let mut contents = Vec::new();
    for content in message.contents {
        match content {
            Content::Text(text) => contents.push(ModelContent::Text(text)),
            Content::Image {
                path,
                media_type,
                width,
                height,
                ..
            } => contents.push(ModelContent::ImageFile {
                path: path.into(),
                media_type,
                width,
                height,
            }),
        }
    }
    ModelMessage {
        role: match message.role {
            Role::System => MessageRole::System,
            Role::User => MessageRole::User,
            Role::Assistant => MessageRole::Assistant,
            Role::Tool => MessageRole::Tool,
        },
        contents,
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
