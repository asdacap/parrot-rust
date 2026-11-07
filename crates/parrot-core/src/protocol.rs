use std::fmt;

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

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct ToolDefinition {
    pub name: String,
    pub description: String,
    pub parameters_json: String,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct ToolCall {
    pub id: String,
    pub name: String,
    pub arguments_json: String,
}

/// A complete provider response. Raw provider content and argument strings stay lossless.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct ProviderResponse {
    pub response_id: String,
    pub finish_reason: String,
    pub input_tokens: i32,
    pub cached_input_tokens: i32,
    pub output_tokens: i32,
    pub assistant_text: String,
    pub tool_calls: Vec<ToolCall>,
    pub provider_id: String,
    pub provider_content: String,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ToolPhase {
    Normal,
    Final,
}

/// Chooses the retained executable catalog advertised for the current request.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ToolContext {
    pub phase: ToolPhase,
}

/// Provider-visible definitions backed by retained executable identities in the adapter.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct ToolCatalog {
    pub normal: Vec<ToolDefinition>,
    pub final_phase: Vec<ToolDefinition>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct TurnOptions {
    pub maximum_requests: i32,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ErrorKind {
    Failed,
    Cancelled,
    RequestTooLarge,
    MissingTurn,
}

/// Bounded diagnostic metadata, separate from error messages and provider bodies.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ErrorCode {
    Cancelled,
    AccessDenied,
    Io,
    Timeout,
    Http,
    Argument,
    InvalidOperation,
    WebSocketClosed,
    WebSocket,
    WireProtocol,
    WebSocketUpgrade,
    WebSocketMessageTooLarge,
    RequestTooLarge,
    WebSocketTransport,
    Unexpected,
}

impl ErrorCode {
    pub fn code(self) -> &'static str {
        match self {
            Self::Cancelled => "cancelled",
            Self::AccessDenied => "access_denied",
            Self::Io => "io",
            Self::Timeout => "timeout",
            Self::Http => "http",
            Self::Argument => "argument",
            Self::InvalidOperation => "invalid_operation",
            Self::WebSocketClosed => "websocket_closed",
            Self::WebSocket => "websocket",
            Self::WireProtocol => "wire_protocol",
            Self::WebSocketUpgrade => "websocket_upgrade",
            Self::WebSocketMessageTooLarge => "websocket_message_too_large",
            Self::RequestTooLarge => "request_too_large",
            Self::WebSocketTransport => "websocket_transport",
            Self::Unexpected => "unexpected",
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Error {
    pub kind: ErrorKind,
    pub code: ErrorCode,
    pub message: String,
    pub response_body: String,
}

impl Error {
    pub fn failed(message: impl Into<String>) -> Self {
        Self {
            kind: ErrorKind::Failed,
            code: ErrorCode::Unexpected,
            message: message.into(),
            response_body: String::new(),
        }
    }

    pub fn request_too_large(message: impl Into<String>) -> Self {
        Self {
            kind: ErrorKind::RequestTooLarge,
            code: ErrorCode::RequestTooLarge,
            message: message.into(),
            response_body: String::new(),
        }
    }

    pub fn missing_turn() -> Self {
        Self {
            kind: ErrorKind::MissingTurn,
            code: ErrorCode::Unexpected,
            message: "turn selection is unavailable".into(),
            response_body: String::new(),
        }
    }

    pub fn cancelled() -> Self {
        Self {
            kind: ErrorKind::Cancelled,
            code: ErrorCode::Cancelled,
            message: "The operation was canceled.".into(),
            response_body: String::new(),
        }
    }
}

impl fmt::Display for Error {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for Error {}
