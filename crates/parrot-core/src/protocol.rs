use crate::{ContextCompaction, ContextConfiguration, ContextSnapshot, TokenCalibration, budget};
use std::{fmt, future::Future, pin::Pin};

pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// Borrows live cancellation observation without owning a signal or background work.
#[derive(Clone, Copy)]
pub struct CancellationToken<'a> {
    is_cancelled: &'a (dyn Fn() -> bool + Send + Sync),
    cancelled: &'a (dyn Fn() -> BoxFuture<'a, ()> + Send + Sync),
}

impl<'a> CancellationToken<'a> {
    pub fn new(
        is_cancelled: &'a (dyn Fn() -> bool + Send + Sync),
        cancelled: &'a (dyn Fn() -> BoxFuture<'a, ()> + Send + Sync),
    ) -> Self {
        Self {
            is_cancelled,
            cancelled,
        }
    }

    pub fn is_cancelled(&self) -> bool {
        (self.is_cancelled)()
    }

    pub fn cancelled(&self) -> BoxFuture<'a, ()> {
        (self.cancelled)()
    }
}

#[derive(Debug, Eq, PartialEq)]
pub enum Outcome {
    Completed(String),
    BudgetExhausted,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RetryReason {
    TruncatedToolCalls,
    InvalidToolCalls,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CompletionFeedback {
    pub messages: Vec<String>,
    pub candidate: ProviderResponse,
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
    /// for this turn. The final catalog is the subset permitted in the final phase.
    fn capture_catalog(&self) -> BoxFuture<'_, Result<ToolCatalog, Error>>;
    /// Executes the recorded batch using retained executable identities for the
    /// supplied turn flags and explicit durable assistant correlation. Returns results
    /// in call order and joins all admitted work, including on failure. Native
    /// adapters may settle chunks durably before returning the complete batch to [`States`].
    fn execute<'a>(
        &'a self,
        calls: Vec<ToolCall>,
        receipt: ToolCallReceipt,
        flags: budget::TurnFlags,
        cancellation: CancellationToken<'a>,
    ) -> BoxFuture<'a, Result<Vec<ToolResult>, Error>>;
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PromotedInput {
    pub input_id: String,
    pub message_id: String,
    pub message: Message,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SkillReference {
    pub name: String,
    pub path: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Skill {
    /// Opaque, stable canonical identity used for turn-wide deduplication.
    pub id: String,
    pub name: String,
    /// Presentation or discovery path, not the record identity or read authority.
    pub path: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SkillContent {
    Text(String),
    Unavailable(String),
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct DerivedContext {
    pub content: String,
    pub loaded_paths: Vec<String>,
}

/// Prepares request content without recording conversation effects or invoking a model.
pub trait Preparation: Send {
    /// Expands one configured prompt template from supplied string arguments.
    /// Does not look up or select skills, read their content, or persist context.
    fn render_template(&self, key: &str, arguments: &[(String, String)]) -> Result<String, Error>;
    /// Renders current instructions while retaining native request-scoped bindings.
    fn render_instructions(&mut self) -> BoxFuture<'_, Result<String, Error>>;
    /// Estimates exactly the supplied request content using native tokenizer and
    /// provider-specific costs, without choosing context or output limits.
    fn estimate_input_tokens<'a>(
        &'a self,
        instructions: &'a str,
        definitions: &'a [ToolDefinition],
        messages: &'a [Message],
    ) -> BoxFuture<'a, Result<i64, Error>>;
}

/// Owns conversation effects, input readiness, and durable turn transitions.
pub trait States: Send {
    /// Durably records and applies the complete returned batch before continuing.
    fn record_tool_results<'a>(
        &'a mut self,
        results: &'a [ToolResult],
    ) -> BoxFuture<'a, Result<(), Error>>;
    /// Reads the latest visible logical turn, including retained completion candidates.
    fn get_last_turn(&self) -> BoxFuture<'_, Result<Option<ConversationTurn>, Error>>;
    /// Peeks at the next durable pending input header without reserving, promoting,
    /// or materializing its content.
    fn get_pending_message(&self) -> BoxFuture<'_, Result<Option<PendingMessage>, Error>>;
    /// Promotes all currently pending steers, or one queued input when permitted,
    /// preserving atomic native admission and returning the promoted content explicitly.
    /// Does not discover skills or prepare prompts.
    fn promote_input(
        &mut self,
        admission: InputAdmission,
    ) -> BoxFuture<'_, Result<Vec<PromotedInput>, Error>>;
    /// Retrieves skill metadata in one batch without reading instruction bodies.
    /// Returns one result per reference in the supplied order, preserving backing-store
    /// precedence and name/path matching semantics.
    fn get_skills<'a>(
        &'a self,
        references: &'a [SkillReference],
    ) -> BoxFuture<'a, Result<Vec<Option<Skill>>, Error>>;
    /// Reads the selected record's complete instruction body or unavailable diagnostic.
    /// Does not format prompt text, update turn deduplication, or record context.
    fn read_skill_content<'a>(
        &'a self,
        skill: &'a Skill,
    ) -> BoxFuture<'a, Result<SkillContent, Error>>;
    /// Records supplied derived content and loaded-path facts after preparation.
    /// Empty content records and publishes nothing.
    fn record_input_context<'a>(
        &'a mut self,
        context: &'a DerivedContext,
    ) -> BoxFuture<'a, Result<(), Error>>;
    /// Reads raw model limits and context overrides without deciding whether to compact.
    fn read_context_configuration(&self) -> BoxFuture<'_, Result<ContextConfiguration, Error>>;
    /// Reads the current token calibration without choosing context or output limits.
    fn read_token_calibration(&self) -> BoxFuture<'_, Result<TokenCalibration, Error>>;
    /// Ensures the configured final-request context is recorded before preparation.
    /// Repeated calls do not append or publish duplicate notices.
    fn record_final_request(&mut self) -> BoxFuture<'_, Result<(), Error>>;
    /// Observes readiness and retains an optional steer or status prompt for fitting,
    /// without appending or publishing it. Source-specific cadence and rendering stay native.
    fn get_pending_prompt<'a>(
        &'a mut self,
        context: &'a ContextSnapshot,
    ) -> BoxFuture<'a, Result<Option<Message>, Error>>;
    /// Durably appends the retained prompt idempotently, updating conversation only
    /// when inserted and retaining its native receipt without publishing it yet.
    fn record_pending_prompt(&mut self) -> BoxFuture<'_, Result<bool, Error>>;
    /// Acknowledges the retained prompt using the fitted or persisted context before
    /// uncancelled publication, publishing nothing when insertion was a duplicate.
    fn acknowledge_prompt<'a>(
        &'a mut self,
        context: &'a ContextSnapshot,
    ) -> BoxFuture<'a, Result<(), Error>>;
    /// Reads the current provider-visible conversation after context preparation.
    fn read_messages(&self) -> BoxFuture<'_, Result<Vec<Message>, Error>>;
    /// Reads one coherent summary, durable tail, ownership and checkpoint snapshot,
    /// plus current messages, without sampling prompts or preparing compaction input.
    fn read_conversation_state(
        &self,
    ) -> BoxFuture<'_, Result<crate::compaction::ConversationState, Error>>;
    /// Persists the fitted compaction with the supplied sampled instructions and tools,
    /// awaiting terminal lifecycle effects on success or failure.
    fn record_compaction<'a>(
        &'a mut self,
        instructions: &'a str,
        definitions: &'a [ToolDefinition],
        result: Result<Option<crate::compaction::FittedCompaction>, Error>,
    ) -> BoxFuture<'a, Result<ContextCompaction, Error>>;
    /// Records why a malformed or truncated tool response must be retried.
    fn record_retry(
        &mut self,
        reason: RetryReason,
        attempt: i32,
    ) -> BoxFuture<'_, Result<(), Error>>;
    /// Durably records the assistant's tool calls and returns their correlation
    /// for execution without preparing executable bindings or a live batch.
    fn record_tool_calls<'a>(
        &'a mut self,
        response: &'a ProviderResponse,
    ) -> BoxFuture<'a, Result<ToolCallReceipt, Error>>;
    /// Reads ordered exit-blocker messages after accepted mutations have settled.
    /// Any nonempty list blocks completion, even when its messages are empty strings.
    fn get_exit_blockers(&mut self) -> BoxFuture<'_, Result<Vec<String>, Error>>;
    /// Records the retained candidate once and all blocker messages in supplied order
    /// before applying them to the conversation. Returns no completion decision.
    fn record_completion_feedback(
        &mut self,
        feedback: CompletionFeedback,
    ) -> BoxFuture<'_, Result<(), Error>>;
    /// Settles the accepted response and durably closes its turn before publication
    /// completes, returning its answer only after the operation has finished.
    fn complete_turn<'a>(
        &'a mut self,
        response: &'a ProviderResponse,
    ) -> BoxFuture<'a, Result<String, Error>>;
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

/// Correlates tool execution with the durably recorded assistant message.
#[derive(Debug, Eq, PartialEq)]
pub struct ToolCallReceipt {
    pub assistant_sequence: i64,
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

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TurnOptions {
    pub maximum_requests: i32,
    pub model: String,
    pub reasoning: Option<Reasoning>,
    pub provider_preferences: String,
    pub include_router_metadata: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ErrorKind {
    Failed,
    Cancelled,
    RequestTooLarge,
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
