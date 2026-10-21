//! Nonserialized source repository carriers; protobuf events use generated types.
use parrot_domain::protocol::messages::parrot::{Delivery, Event};

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ConversationPart {
    Text(String),
    ImageArtifact {
        artifact_id: String,
        media_type: String,
        display_name: String,
    },
}
impl ConversationPart {
    pub fn text(value: impl Into<String>) -> Self {
        Self::Text(value.into())
    }
    pub fn image(
        artifact_id: impl Into<String>,
        media_type: impl Into<String>,
        display_name: impl Into<String>,
    ) -> Self {
        Self::ImageArtifact {
            artifact_id: artifact_id.into(),
            media_type: media_type.into(),
            display_name: display_name.into(),
        }
    }
}
pub fn content(parts: &[ConversationPart]) -> String {
    parts
        .iter()
        .filter_map(|part| match part {
            ConversationPart::Text(text) => Some(text.as_str()),
            _ => None,
        })
        .collect()
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ConversationOrigin {
    UserInput,
    Model,
    System,
    Tool,
}
impl ConversationOrigin {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::UserInput => "user_input",
            Self::Model => "model",
            Self::System => "system",
            Self::Tool => "tool",
        }
    }
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MessageRole {
    System,
    User,
    Assistant,
    Tool,
}
impl MessageRole {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::System => "system",
            Self::User => "user",
            Self::Assistant => "assistant",
            Self::Tool => "tool",
        }
    }
    pub fn origin(self) -> ConversationOrigin {
        match self {
            Self::System => ConversationOrigin::System,
            Self::User => ConversationOrigin::UserInput,
            Self::Assistant => ConversationOrigin::Model,
            Self::Tool => ConversationOrigin::Tool,
        }
    }
}
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ToolCall {
    pub id: String,
    pub name: String,
    pub arguments_json: String,
}
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct ProviderContent {
    pub provider_id: String,
    pub content: String,
}
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ConversationMessage {
    pub origin: ConversationOrigin,
    pub role: MessageRole,
    pub parts: Vec<ConversationPart>,
    pub tool_calls: Vec<ToolCall>,
    pub tool_call_id: String,
    pub provider_content: ProviderContent,
}
impl ConversationMessage {
    pub fn text(role: MessageRole, text: impl Into<String>) -> Self {
        Self {
            origin: role.origin(),
            role,
            parts: vec![ConversationPart::text(text)],
            tool_calls: Vec::new(),
            tool_call_id: String::new(),
            provider_content: ProviderContent::default(),
        }
    }
}
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ConversationItem {
    pub sequence: i64,
    pub message: ConversationMessage,
}
#[derive(Clone, Debug, PartialEq)]
pub struct AdmittedInput {
    pub id: String,
    pub message_id: String,
    pub parts: Vec<ConversationPart>,
    pub delivery: Delivery,
}
impl AdmittedInput {
    pub fn content(&self) -> String {
        content(&self.parts)
    }
}
#[derive(Clone, Debug, PartialEq)]
pub struct Admission {
    pub input: AdmittedInput,
    pub published: Option<Event>,
}
impl Admission {
    pub fn created(&self) -> bool {
        self.published.is_some()
    }
}
#[derive(Clone, Debug, PartialEq)]
pub struct Promotion {
    pub input: AdmittedInput,
    pub published: Event,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ToolExecutionStatus {
    Finished,
    Cancelled,
    Error,
    ImageBudgetExceeded,
}
impl ToolExecutionStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Finished => "finished",
            Self::Cancelled => "cancelled",
            Self::Error => "error",
            Self::ImageBudgetExceeded => "image_budget_exceeded",
        }
    }
}
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ToolExecutionTerminal {
    pub tool_call_id: String,
    pub tool_name: String,
    pub status: ToolExecutionStatus,
    pub result_parts: Vec<ConversationPart>,
    pub message: String,
}
#[derive(Clone, Debug, Default, PartialEq)]
pub struct SessionUsage {
    pub revision: u64,
    pub input_tokens: i64,
    pub cached_input_tokens: i64,
    pub output_tokens: i64,
    pub context_size: i64,
    pub context_limit: i64,
    pub input_cost: f64,
    pub output_cost: f64,
}
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AgentLineageRecord {
    pub agent_session_id: String,
    pub parent_agent_session_id: String,
    pub name: String,
}
#[derive(Clone, Debug, PartialEq)]
pub struct StoredEvent {
    pub sequence: i64,
    pub event: Event,
    pub raw: Vec<u8>,
}
