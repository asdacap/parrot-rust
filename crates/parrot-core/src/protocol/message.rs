use super::tool::{ToolCall, ToolDefinition, ToolImage};

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PromotedInput {
    pub input_id: String,
    pub message: Message,
    pub publication: crate::ConversationPublication,
}
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct ProviderRequest {
    pub model: String,
    pub instructions: String,
    pub messages: Vec<Message>,
    pub tools: Vec<ToolDefinition>,
    pub max_output_tokens: i32,
    pub reasoning: Option<Reasoning>,
    pub provider_preferences: String,
    pub include_router_metadata: bool,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct Reasoning {
    pub effort: String,
    pub summary: String,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum Role {
    System,
    #[default]
    User,
    Assistant,
    Tool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Content {
    Text(String),
    /// Durable image metadata that state materializes before provider-visible reads.
    Artifact(ToolImage),
    Image {
        path: String,
        url: String,
        media_type: String,
        width: i32,
        height: i32,
        bytes: Option<Vec<u8>>,
    },
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct Message {
    pub role: Role,
    pub contents: Vec<Content>,
    pub tool_calls: Vec<ToolCall>,
    pub tool_call_id: String,
    pub provider_id: String,
    pub provider_content: String,
}

/// The latest visible logical segment, not a durable turn-event interval.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ConversationTurn {
    pub messages: Vec<Message>,
    /// Present only when the non-system tail is a no-tool assistant response,
    /// including a retained candidate; it does not prove durable turn closure.
    pub result: Option<Message>,
}

impl ConversationTurn {
    pub fn needs_response(&self) -> bool {
        self.result.is_none()
            && matches!(
                self.messages
                    .iter()
                    .rev()
                    .find(|message| message.role != Role::System)
                    .map(|message| message.role),
                Some(Role::User | Role::Tool)
            )
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PendingMessage {
    pub input_id: String,
    pub message_id: String,
    pub delivery: Delivery,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Delivery {
    Steer,
    Queue,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum InputAdmission {
    SteersOnly,
    SteersOrNextQueued,
}
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum FinishReason {
    Stop,
    Length,
    ToolCalls,
    ContentFilter,
    Incomplete,
    Interrupted,
    Other(String),
}

impl Default for FinishReason {
    fn default() -> Self {
        Self::Other(String::new())
    }
}

/// A complete provider response. Raw provider content and argument strings stay lossless.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct ProviderResponse {
    pub response_id: String,
    pub finish_reason: FinishReason,
    pub input_tokens: i32,
    pub cached_input_tokens: i32,
    pub output_tokens: i32,
    pub assistant_text: String,
    pub tool_calls: Vec<ToolCall>,
    pub provider_id: String,
    pub provider_content: String,
}
