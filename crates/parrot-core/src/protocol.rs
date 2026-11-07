use crate::budget;
use std::{fmt, future::Future, pin::Pin};

pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

#[derive(Debug, Eq, PartialEq)]
pub enum Outcome {
    Completed(String),
    Failed(String),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RetryReason {
    TruncatedToolCalls,
    InvalidToolCalls,
}

#[derive(Debug, Eq, PartialEq)]
pub enum CompletionDecision {
    Complete,
    Retry { pending: Option<bool> },
}

/// Invokes models using the core protocol while owning transport and stream cleanup.
pub trait Provider: Send + Sync {
    /// Invokes the request and joins stream cleanup before returning, including
    /// after errors or cancellation.
    fn invoke(&self, request: ProviderRequest) -> BoxFuture<'_, Result<ProviderResponse, Error>>;
}

/// Retains executable tools and executes explicitly supplied calls.
pub trait Tools: Send + Sync {
    /// Captures permitted definitions backed by the original executable identities
    /// for this turn. The final catalog is their interruption-enabled subset.
    fn capture_catalog(&self) -> BoxFuture<'_, Result<ToolCatalog, Error>>;
    /// Executes the complete supplied batch using retained executable identities,
    /// returning results in call order and joining all admitted work before returning,
    /// including on failure. Scheduling and batch-local bookkeeping stay internal;
    /// conversation recovery and result recording belong to [`States`].
    fn execute(&self, calls: Vec<ToolCall>) -> BoxFuture<'_, Result<Vec<ToolResult>, Error>>;
}

/// Owns conversation effects, input readiness, and durable turn transitions.
pub trait States: Send {
    /// Restores saved tool results and records crash errors for missing historical
    /// results without invoking tools, before cancellation or readiness is checked.
    fn restore_tool_results(&mut self) -> BoxFuture<'_, Result<(), Error>>;
    /// Durably records and applies the complete returned batch before continuing.
    fn record_tool_results<'a>(
        &'a mut self,
        results: &'a [ToolResult],
    ) -> BoxFuture<'a, Result<(), Error>>;
    /// Reports whether provider-request limits are disabled for this run.
    fn disables_request_limit(&self) -> bool;
    /// Reports whether scheduled work should run before opening another turn.
    fn has_scheduled_work(&self) -> bool;
    /// Fails with typed cancellation if the owning execution was cancelled.
    fn check_cancellation(&self) -> Result<(), Error>;
    /// Reports whether a durable turn is already open.
    fn is_turn_open(&self) -> BoxFuture<'_, bool>;
    /// Reports whether the conversation or pending input requires a response.
    fn has_work(&self) -> BoxFuture<'_, Result<bool, Error>>;
    /// Opens a turn and freezes its model, profile, and authority privately.
    fn begin_turn(&mut self) -> BoxFuture<'_, Result<TurnOptions, Error>>;
    /// Restores tool availability after the core captures the turn's tool catalog.
    fn restore_tool_availability(&mut self) -> BoxFuture<'_, Result<(), Error>>;
    /// Promotes eligible input into the current conversation.
    fn promote_input(&mut self) -> BoxFuture<'_, Result<(), Error>>;
    /// Consumes a request to enter the interrupted final phase.
    fn take_interruption(&mut self) -> bool;
    /// Prepares context and a provider request using exactly the advertised definitions.
    fn prepare_request<'a>(
        &'a mut self,
        definitions: &'a [ToolDefinition],
        phase: budget::TurnRequestPhase,
    ) -> BoxFuture<'a, Result<ProviderRequest, Error>>;
    /// Attempts to reduce context after a provider rejects an oversized request.
    fn compact_request<'a>(
        &'a mut self,
        definitions: &'a [ToolDefinition],
    ) -> BoxFuture<'a, Result<bool, Error>>;
    /// Records why a malformed or truncated tool response must be retried.
    fn record_retry(
        &mut self,
        reason: RetryReason,
        attempt: i32,
    ) -> BoxFuture<'_, Result<(), Error>>;
    /// Durably records the assistant's tool calls before execution begins.
    fn record_tool_calls<'a>(
        &'a mut self,
        response: &'a ProviderResponse,
    ) -> BoxFuture<'a, Result<(), Error>>;
    /// Joins accepted mutations and applies completion policies in their order.
    fn evaluate_completion<'a>(
        &'a mut self,
        response: &'a ProviderResponse,
    ) -> BoxFuture<'a, Result<CompletionDecision, Error>>;
    /// Settles the accepted response and durably closes its turn before publication
    /// completes, returning its answer only after the operation has finished.
    fn complete_turn<'a>(
        &'a mut self,
        response: &'a ProviderResponse,
    ) -> BoxFuture<'a, Result<String, Error>>;
    /// Records request-budget exhaustion and returns its failure message.
    fn fail_runaway(&mut self) -> BoxFuture<'_, Result<String, Error>>;
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

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum ToolStatus {
    #[default]
    Finished,
    Cancelled,
    Error,
    ImageBudgetExceeded,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct ToolResult {
    pub call_id: String,
    pub tool_name: String,
    pub status: ToolStatus,
    pub text: String,
    pub error_message: String,
    pub images: Vec<ToolImage>,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct ToolImage {
    pub artifact_id: String,
    pub sha256: String,
    pub media_type: String,
    pub byte_length: i64,
    pub width: u32,
    pub height: u32,
    pub frame_count: u32,
    pub aggregate_pixels: i64,
    pub display_name: String,
    pub origin: String,
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
