use super::*;
use parrot_core::{ConversationEvent, ConversationOrigin as CoreOrigin, Message};

pub(super) fn render_event(mut event: Event, content: ConversationEvent) -> Event {
    match content {
        ConversationEvent::Message => {}
        ConversationEvent::Notice { kind, notice } => {
            event.set_agent_notice(protocol::AgentNotice {
                kind,
                notice,
                ..Default::default()
            })
        }
        ConversationEvent::InputPromoted {
            input_id,
            message_id,
        } => {
            event.set_input_promoted(protocol::InputPromoted {
                input_id,
                message_id,
                ..Default::default()
            });
        }
        ConversationEvent::Retry { reason, attempt } => {
            event.set_retry_notice(protocol::RetryNotice {
                reason,
                attempt,
                ..Default::default()
            })
        }
        ConversationEvent::ToolRequestReceived { count } => {
            event.set_tool_request_received(protocol::ToolRequestReceived {
                tool_call_count: count,
                ..Default::default()
            });
        }
        ConversationEvent::SkillLoaded { path } => {
            event.set_skill_loaded(protocol::SkillLoadedEvent {
                path,
                ..Default::default()
            })
        }
    }
    event
}

fn render_role(role: parrot_core::Role) -> MessageRole {
    match role {
        parrot_core::Role::System => MessageRole::System,
        parrot_core::Role::User => MessageRole::User,
        parrot_core::Role::Assistant => MessageRole::Assistant,
        parrot_core::Role::Tool => MessageRole::Tool,
    }
}

pub(super) fn render_origin(origin: CoreOrigin) -> ConversationOrigin {
    match origin {
        CoreOrigin::UserInput => ConversationOrigin::UserInput,
        CoreOrigin::Model => ConversationOrigin::Model,
        CoreOrigin::System => ConversationOrigin::System,
        CoreOrigin::Tool => ConversationOrigin::Tool,
    }
}

pub(super) fn render_record(
    message: &Message,
    origin: ConversationOrigin,
) -> Result<ConversationMessage, TurnError> {
    Ok(ConversationMessage {
        origin,
        role: render_role(message.role),
        parts: message
            .contents
            .iter()
            .map(|content| match content {
                parrot_core::Content::Text(text) => Ok(ConversationPart::text(text)),
                parrot_core::Content::Image { .. } => Err(TurnError::Service(
                    "file images must be admitted as input before they are recorded".into(),
                )),
            })
            .collect::<Result<_, _>>()?,
        tool_calls: message
            .tool_calls
            .iter()
            .map(|call| parrot_storage::events::ToolCall {
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
    })
}
