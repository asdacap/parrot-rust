use crate::{Content, Message, ProviderResponse, Role};

impl Message {
    pub fn text(role: Role, content: impl Into<String>) -> Self {
        Self {
            role,
            contents: vec![Content::Text(content.into())],
            ..Self::default()
        }
    }

    pub fn from_response(response: &ProviderResponse) -> Self {
        Self {
            role: Role::Assistant,
            contents: vec![Content::Text(response.assistant_text.clone())],
            tool_calls: response.tool_calls.clone(),
            provider_id: response.provider_id.clone(),
            provider_content: response.provider_content.clone(),
            ..Self::default()
        }
    }
}
