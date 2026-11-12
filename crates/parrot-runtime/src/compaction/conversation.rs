use parrot_core::{Content, Message, Role};
use parrot_storage::{
    conversation::{ModelContent, ModelMessage},
    events::{MessageRole, ProviderContent, ToolCall},
};

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
