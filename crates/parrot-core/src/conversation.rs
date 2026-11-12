use crate::{CompletionBlockerFeedback, Content, FinishReason, Message, ProviderResponse, Role};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ConversationOrigin {
    UserInput,
    Model,
    System,
    Tool,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub enum ConversationEvent {
    #[default]
    Message,
    Notice {
        kind: String,
        notice: String,
    },
    InputPromoted {
        input_id: String,
        message_id: String,
    },
    Retry {
        reason: String,
        attempt: i32,
    },
    ToolRequestReceived {
        count: i32,
    },
    SkillLoaded {
        path: String,
    },
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct ConversationPublication {
    pub event_id: String,
    pub agent_session_id: String,
    pub event: ConversationEvent,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ConversationPersistence {
    Append {
        origin: ConversationOrigin,
        event: ConversationEvent,
    },
    Committed(ConversationPublication),
    CompletionBlockerFeedback(CompletionBlockerFeedback),
    ToolResult {
        assistant_sequence: i64,
        result: crate::ToolResult<Vec<u8>>,
        recovered: bool,
    },
    CloseTurn {
        start_event_id: String,
        finish_reason: FinishReason,
        usage: UsageTotals,
    },
}

/// The request token totals a close-turn terminal event reports, as recorded by
/// the states side for the current turn.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct UsageTotals {
    pub input_tokens: i64,
    pub output_tokens: i64,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ConversationAddition {
    pub messages: Vec<Message>,
    pub persistence: ConversationPersistence,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct ConversationReceipt {
    pub inserted: bool,
    pub sequence: i64,
}

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
