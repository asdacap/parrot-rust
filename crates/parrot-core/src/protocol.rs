use crate::budget;
use std::sync::Arc;
use std::{fmt, future::Future, pin::Pin};

pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// The caller cancelled an operation before its cancellation boundary.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Cancelled;

impl fmt::Display for Cancelled {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("operation cancelled")
    }
}

impl std::error::Error for Cancelled {}

/// Read-only cancellation observation; only the scope owner may cancel.
pub trait Cancellation: Send + Sync {
    fn is_cancelled(&self) -> bool;

    /// Resolves when cancellation is observed, including prior cancellation.
    fn cancelled(&self) -> BoxFuture<'_, ()>;

    /// Checks the actual current state, rather than assuming success.
    fn check(&self) -> Result<(), Cancelled> {
        if self.is_cancelled() {
            Err(Cancelled)
        } else {
            Ok(())
        }
    }
}

/// Borrows live cancellation observation without owning a signal or background work.
#[derive(Clone, Copy)]
pub struct CancellationToken<'a> {
    cancellation: Option<&'a dyn Cancellation>,
}

impl<'a> CancellationToken<'a> {
    pub fn new(cancellation: &'a dyn Cancellation) -> Self {
        Self {
            cancellation: Some(cancellation),
        }
    }

    pub fn uncancelled() -> CancellationToken<'static> {
        CancellationToken { cancellation: None }
    }

    pub fn is_cancelled(&self) -> bool {
        self.cancellation
            .is_some_and(|cancellation| cancellation.is_cancelled())
    }

    pub fn cancelled(&self) -> BoxFuture<'a, ()> {
        match self.cancellation {
            Some(cancellation) => cancellation.cancelled(),
            None => Box::pin(std::future::pending()),
        }
    }
}

impl Cancellation for CancellationToken<'_> {
    fn is_cancelled(&self) -> bool {
        self.is_cancelled()
    }

    fn cancelled(&self) -> BoxFuture<'_, ()> {
        self.cancelled()
    }
}

#[derive(Debug, Eq, PartialEq)]
pub enum Outcome {
    Completed(String),
    BudgetExhausted,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CompletionFeedback {
    pub messages: Vec<String>,
    pub candidate: ProviderResponse,
}

/// Invokes models using the core protocol while owning transport and stream cleanup.
/// Unsupported invocation and estimation capabilities return explicit errors.
pub trait Provider: Send + Sync {
    /// Reads alias definitions and resolved usage without formatting their descriptions.
    fn read_model_aliases(&self) -> BoxFuture<'_, Result<Vec<ModelAliasDescription>, Error>> {
        Box::pin(async { Err(Error::unsupported("Provider.read_model_aliases")) })
    }
    /// Reads the selected model profile's literal system prompt.
    fn read_model_system_prompt(&self) -> BoxFuture<'_, Result<Option<String>, Error>> {
        Box::pin(async { Err(Error::unsupported("Provider.read_model_system_prompt")) })
    }
    /// Estimates exactly the supplied request using model-specific token costs.
    fn estimate_input_tokens(
        &self,
        _instructions: &str,
        _definitions: &[ToolDefinition],
        _messages: &[Message],
    ) -> Result<i64, Error> {
        Err(Error::unsupported("Provider.estimate_input_tokens"))
    }
    fn estimate_tokens(&self, _messages: &[Message]) -> Result<i64, Error> {
        Err(Error::unsupported("Provider.estimate_tokens"))
    }
    /// Estimates preceding summary, accumulated chunk, and the next atomic group.
    fn estimate_summary_tokens(
        &self,
        _preceding_summary: &str,
        _chunk_tokens: i64,
        _next_group: &[Message],
    ) -> Result<i64, Error> {
        Err(Error::unsupported("Provider.estimate_summary_tokens"))
    }
    /// Collects a private summary stream without publishing its generated content.
    fn summarize<'a, 'c>(
        &'a self,
        _request: ProviderRequest,
        _cancellation: CancellationToken<'c>,
    ) -> BoxFuture<'a, Result<SummaryResponse, Error>>
    where
        'c: 'a,
    {
        Box::pin(async { Err(Error::unsupported("Provider.summarize")) })
    }

    /// Invokes the request and joins stream cleanup before returning, including
    /// after errors or cancellation.
    fn invoke<'a, 'c>(
        &'a self,
        _request: ProviderRequest,
        _cancellation: CancellationToken<'c>,
    ) -> BoxFuture<'a, Result<ProviderResponse, Error>>
    where
        'c: 'a,
    {
        Box::pin(async { Err(Error::unsupported("Provider.invoke")) })
    }
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
    pub message: Message,
    pub publication: crate::ConversationPublication,
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

#[derive(Clone, Debug, Default)]
pub struct AgentIdentityDescription {
    pub session_id: String,
    pub parent_session_id: String,
    pub parent_session_name: String,
    pub agent_name: String,
    pub depth: i32,
    pub scope_text: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ModelAliasDescription {
    pub name: String,
    pub model: String,
    pub usage: String,
    pub model_usage: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SkillCatalogEntry {
    pub name: String,
    pub description: String,
    pub discovery_path: String,
    pub enabled: bool,
    pub prompt_visible: bool,
}

/// A prepared context reminder with the information needed to record it.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ContextReminder {
    pub checkpoint: ContextReminderCheckpoint,
    pub content: String,
    pub observed_percentage: i32,
}

impl ContextReminder {
    pub fn render_message(&self) -> Message {
        Message {
            role: Role::System,
            contents: vec![Content::Text(self.content.clone())],
            ..Message::default()
        }
    }
}

/// A recorded context reminder notice awaiting publication.
#[derive(Debug, Eq, PartialEq)]
pub struct ContextReminderPublication {
    pub event_id: String,
    pub agent_session_id: String,
    pub kind: String,
    pub notice: String,
}

/// Publishes compaction lifecycle notifications independently of conversation state.
pub trait EventPublisher: Send + Sync {
    /// Records and publishes the supplied event without adding conversation content.
    fn publish<'a>(
        &'a self,
        _event: crate::ConversationEvent,
        _cancellation: CancellationToken<'a>,
    ) -> BoxFuture<'a, Result<(), Error>> {
        Box::pin(async { Err(Error::unsupported("EventPublisher.publish")) })
    }
    /// Publishes compaction start without request cancellation.
    fn report_compaction_started(&self) -> BoxFuture<'_, Result<(), Error>>;
    /// Publishes compaction failure without request cancellation.
    fn report_compaction_failed(&self, message: String) -> BoxFuture<'_, Result<(), Error>>;
}

/// Owns conversation effects, input readiness, and durable turn transitions.
/// Mutations receive their input from the caller rather than retrieving staged input.
/// Unsupported capabilities return explicit errors rather than inventing state.
pub trait States: Send + Sync {
    /// Commits the supplied messages, updates the live conversation on insertion, then publishes.
    /// Storage conditions control deduplication only. Core supplies every message and its timing.
    fn add_conversation(
        &mut self,
        _addition: crate::ConversationAddition,
    ) -> BoxFuture<'_, Result<crate::ConversationReceipt, Error>> {
        Box::pin(async { Err(Error::unsupported("States.add_conversation")) })
    }
    /// Reads the durable final-phase marker without changing the conversation.
    fn read_final_phase(&self) -> BoxFuture<'_, Result<bool, Error>> {
        Box::pin(async { Err(Error::unsupported("States.read_final_phase")) })
    }
    /// Reads current status revisions and the bound profile without choosing insertion.
    fn read_status_observation(&self) -> BoxFuture<'_, Result<crate::StatusObservation, Error>> {
        Box::pin(async { Err(Error::unsupported("States.read_status_observation")) })
    }
    /// Reads materialized pending input without admitting it to the conversation.
    fn read_pending_inputs(&self) -> BoxFuture<'_, Result<Vec<Message>, Error>> {
        Box::pin(async { Err(Error::unsupported("States.read_pending_inputs")) })
    }
    /// Reads typed configuration without rendering prompts or deciding request policy.
    fn get_config(&self, key: ConfigKey) -> BoxFuture<'_, Result<ConfigValue, Error>>;
    /// Loads the provider-request budget that core updates during one turn, not a token budget.
    /// Core resets the request count at turn start but retains settlement extensions from earlier turns.
    fn read_turn_budget(&self) -> Result<TurnBudget, Error> {
        Err(Error::unsupported("States.read_turn_budget"))
    }
    /// Saves the updated provider-request budget after the turn returns, even on error.
    /// Retains settlement extensions for subsequent turns in the same invocation.
    fn write_turn_budget(&mut self, _budget: TurnBudget) -> Result<(), Error> {
        Err(Error::unsupported("States.write_turn_budget"))
    }
    /// Reads the last stored context reminder in the current compaction epoch.
    fn get_last_context_reminder(
        &self,
    ) -> BoxFuture<'_, Result<Option<ContextReminderCheckpoint>, Error>> {
        Box::pin(async { Err(Error::unsupported("States.get_last_context_reminder")) })
    }
    /// Joins accepted mutations and reads ordered completion blockers.
    fn get_exit_blockers(&mut self) -> BoxFuture<'_, Result<Vec<String>, Error>> {
        Box::pin(async { Err(Error::unsupported("States.get_exit_blockers")) })
    }
    /// Refreshes raw epoch observations without rendering instructions.
    fn sample_epoch(&mut self) -> BoxFuture<'_, Result<(), Error>> {
        Box::pin(async { Err(Error::unsupported("States.sample_epoch")) })
    }
    /// Observes native status for the exact context selected by core.
    fn render_status<'a, 'c>(
        &'a self,
        _context: ContextSnapshot,
        _cancellation: CancellationToken<'c>,
    ) -> BoxFuture<'a, Result<String, Error>>
    where
        'c: 'a,
    {
        Box::pin(async { Err(Error::unsupported("States.render_status")) })
    }
    /// Persists an original atomic group and returns notices with its actual saved path.
    fn spill_group<'a, 'c>(
        &'a self,
        _messages: &'a [Message],
        _cancellation: CancellationToken<'c>,
    ) -> BoxFuture<'a, Result<Vec<Message>, Error>>
    where
        'c: 'a,
    {
        Box::pin(async { Err(Error::unsupported("States.spill_group")) })
    }
    /// Publishes aggregate summary progress without disclosing generated content.
    fn report_compaction_progress<'a, 'c>(
        &'a self,
        _progress: CompactionProgress,
        _cancellation: CancellationToken<'c>,
    ) -> BoxFuture<'a, Result<(), Error>>
    where
        'c: 'a,
    {
        Box::pin(async { Err(Error::unsupported("States.report_compaction_progress")) })
    }

    /// Reads configured literal prompts, including selected alias augmentation, and their
    /// namespace keys. Retains native request bindings and samples raw epoch inputs
    /// without rendering templates.
    fn read_system_prompts(&mut self) -> BoxFuture<'_, Result<Vec<(String, String)>, Error>> {
        Box::pin(async { Err(Error::unsupported("States.read_system_prompts")) })
    }
    /// Reads literal template arguments for the supplied registered source.
    fn read_template_arguments<'a>(
        &'a self,
        _source_key: &'a str,
    ) -> BoxFuture<'a, Result<Vec<(String, serde_json::Value)>, Error>> {
        Box::pin(async { Err(Error::unsupported("States.read_template_arguments")) })
    }
    /// Reads sampled AGENTS paths and unwrapped file contents.
    fn read_agents_files(&self) -> BoxFuture<'_, Result<Vec<(String, String)>, Error>> {
        Box::pin(async { Err(Error::unsupported("States.read_agents_files")) })
    }
    /// Reads the supplied agent identity and scope without formatting either value.
    fn read_agent_identity(
        &self,
    ) -> BoxFuture<'_, Result<Option<AgentIdentityDescription>, Error>> {
        Box::pin(async { Err(Error::unsupported("States.read_agent_identity")) })
    }
    /// Reads the current unwrapped standing instruction.
    fn read_standing_instruction(&self) -> BoxFuture<'_, Result<String, Error>> {
        Box::pin(async { Err(Error::unsupported("States.read_standing_instruction")) })
    }
    /// Reads current catalog metadata without rendering entries or applying prompt budgets.
    fn read_skill_catalog(&self) -> BoxFuture<'_, Result<Vec<SkillCatalogEntry>, Error>> {
        Box::pin(async { Err(Error::unsupported("States.read_skill_catalog")) })
    }
    /// Durably records and applies the complete returned batch before continuing.
    fn record_tool_results<'a>(
        &'a mut self,
        _results: &'a [ToolResult],
    ) -> BoxFuture<'a, Result<(), Error>> {
        Box::pin(async { Err(Error::unsupported("States.record_tool_results")) })
    }
    /// Reads the latest visible logical turn, including retained completion candidates.
    fn get_last_turn(&self) -> BoxFuture<'_, Result<Option<ConversationTurn>, Error>> {
        Box::pin(async { Err(Error::unsupported("States.get_last_turn")) })
    }
    /// Peeks at the next durable pending input header without reserving, promoting,
    /// or materializing its content.
    fn get_pending_message(&self) -> BoxFuture<'_, Result<Option<PendingMessage>, Error>> {
        Box::pin(async { Err(Error::unsupported("States.get_pending_message")) })
    }
    /// Promotes all currently pending steers, or one queued input when permitted,
    /// preserving atomic native admission and returning the promoted content explicitly.
    /// Does not discover skills or prepare prompts.
    fn promote_input(
        &mut self,
        _admission: InputAdmission,
    ) -> BoxFuture<'_, Result<Vec<PromotedInput>, Error>> {
        Box::pin(async { Err(Error::unsupported("States.promote_input")) })
    }
    /// Retrieves skill metadata in one batch without reading instruction bodies.
    /// Returns one result per reference in the supplied order, preserving backing-store
    /// precedence and name/path matching semantics.
    fn get_skills<'a>(
        &'a self,
        _references: &'a [SkillReference],
    ) -> BoxFuture<'a, Result<Vec<Option<Skill>>, Error>> {
        Box::pin(async { Err(Error::unsupported("States.get_skills")) })
    }
    /// Reads the selected record's complete instruction body or unavailable diagnostic.
    /// Does not format prompt text, update turn deduplication, or record context.
    fn read_skill_content<'a>(
        &'a self,
        _skill: &'a Skill,
    ) -> BoxFuture<'a, Result<SkillContent, Error>> {
        Box::pin(async { Err(Error::unsupported("States.read_skill_content")) })
    }
    /// Reads the current token calibration without choosing context or output limits.
    fn read_token_calibration(&self) -> BoxFuture<'_, Result<TokenCalibration, Error>> {
        Box::pin(async { Err(Error::unsupported("States.read_token_calibration")) })
    }
    /// Reads the current provider-visible conversation after context preparation.
    fn read_messages(&self) -> BoxFuture<'_, Result<Vec<Message>, Error>> {
        Box::pin(async { Err(Error::unsupported("States.read_messages")) })
    }
    /// Reads one coherent summary, durable tail, ownership and checkpoint snapshot,
    /// plus current messages, without sampling prompts or preparing compaction input.
    fn read_conversation_state(
        &self,
    ) -> BoxFuture<'_, Result<crate::compaction::ConversationState, Error>> {
        Box::pin(async { Err(Error::unsupported("States.read_conversation_state")) })
    }
    /// Persists the fitted compaction with the supplied sampled instructions and tools,
    /// awaiting terminal lifecycle effects on success or failure.
    fn record_compaction<'a>(
        &'a mut self,
        _instructions: &'a str,
        _definitions: &'a [ToolDefinition],
        _result: Result<Option<crate::compaction::FittedCompaction>, Error>,
    ) -> BoxFuture<'a, Result<ContextCompaction, Error>> {
        Box::pin(async { Err(Error::unsupported("States.record_compaction")) })
    }
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
    pub detail: Option<ErrorDetail>,
}

impl Error {
    pub fn template(message: impl Into<String>) -> Self {
        Self {
            detail: Some(ErrorDetail::Template),
            ..Self::failed(message)
        }
    }

    pub fn prompt_context(message: impl Into<String>) -> Self {
        Self {
            detail: Some(ErrorDetail::PromptContext),
            ..Self::failed(message)
        }
    }

    pub fn unsupported(operation: &str) -> Self {
        Self {
            code: ErrorCode::InvalidOperation,
            ..Self::failed(format!("{operation} is not supported"))
        }
    }

    pub fn failed(message: impl Into<String>) -> Self {
        Self {
            kind: ErrorKind::Failed,
            code: ErrorCode::Unexpected,
            message: message.into(),
            response_body: String::new(),
            detail: None,
        }
    }

    pub fn request_too_large(message: impl Into<String>) -> Self {
        Self {
            kind: ErrorKind::RequestTooLarge,
            code: ErrorCode::RequestTooLarge,
            message: message.into(),
            response_body: String::new(),
            detail: None,
        }
    }

    pub fn cancelled() -> Self {
        Self {
            kind: ErrorKind::Cancelled,
            code: ErrorCode::Cancelled,
            message: "The operation was canceled.".into(),
            response_body: String::new(),
            detail: None,
        }
    }
}

impl fmt::Display for Error {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for Error {}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ContextLimit {
    Tokens(i64),
    Percentage(i64),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ContextModel {
    pub canonical_model: String,
    pub context_window: i32,
    pub maximum_input_tokens: i32,
    pub maximum_output_tokens: i32,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ContextConfiguration {
    pub model: ContextModel,
    pub trigger_percent: i32,
    pub context_limit: Option<ContextLimit>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ContextSnapshot {
    pub estimated_tokens: i64,
    pub context_limit: i32,
    pub usage_percent: Option<i32>,
    pub trigger_percent: i32,
    pub input_limit: i32,
    pub trigger_tokens: Option<i64>,
    pub has_context_limit_override: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ContextReminderCheckpoint {
    pub canonical_model: String,
    pub context_limit: i32,
    pub percentage: i32,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ContextCompaction {
    pub instructions: String,
    pub reduced: bool,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct TokenCalibration {
    pub(crate) selector: Option<String>,
    pub(crate) estimated_input_tokens: i64,
    pub(crate) actual_input_tokens: i32,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct TurnFlags {
    pub final_phase: bool,
}

#[derive(Clone, Debug)]
pub struct TurnBudget {
    pub(crate) disable_max_turns: bool,
    pub(crate) provider_requests: i32,
    pub(crate) settlement_extensions: i32,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CompactionGroup {
    pub messages: Vec<Message>,
    pub is_complete: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CompactionHistoryGroup {
    pub messages: Vec<Message>,
    pub end_watermark: i64,
    pub has_checkpoint_before: bool,
    pub is_complete: bool,
}

/// Captures durable history facts and the independent current provider-visible view.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct ConversationState {
    pub summary: Option<ConversationSummary>,
    /// Contains the chronological durable tail after the captured summary watermark.
    pub history: Vec<ConversationHistoryItem>,
    pub checkpoint_sequences: Vec<i64>,
    pub messages: Vec<Message>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ConversationSummary {
    pub content: String,
    pub watermark: i64,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ConversationHistoryItem {
    pub sequence: i64,
    pub tool_owner_sequence: Option<i64>,
    pub is_compaction_status: bool,
    pub message: Message,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CompactionInput {
    pub groups: Vec<CompactionHistoryGroup>,
    pub base_watermark: i64,
    pub instructions: String,
    pub messages: Vec<Message>,
    pub fixed_message: Message,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CompactionPlan {
    pub groups: Vec<CompactionGroup>,
    pub keep_group_from: usize,
    pub watermark: i64,
    pub input_budget: i64,
    pub summary_output_tokens: i32,
    pub input_token_limit: i32,
    pub instructions: String,
    pub tools: Vec<ToolDefinition>,
    pub fixed_message: Message,
    pub summary_instructions: String,
    pub summary_prefix: String,
    pub model: String,
    pub reasoning: Option<Reasoning>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CompactionResult {
    pub history: Vec<Message>,
    pub summary: Message,
    pub retained_messages: usize,
    pub watermark: i64,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FittedCompaction {
    pub result: CompactionResult,
    pub status: String,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct CompactionProgress {
    pub completed_groups: usize,
    pub total_groups: usize,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SummaryResponse {
    Summary(String),
    NoSummary(String),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CompactionConfiguration {
    pub context: ContextConfiguration,
    pub target: Option<ContextLimit>,
    pub target_percent: i32,
    pub maximum_input_tokens: i32,
    pub summary_output_tokens: i32,
    pub profile_maximum_input_tokens: Option<i32>,
    pub profile_summary_output_tokens: Option<i32>,
    pub model: String,
    pub reasoning: Option<Reasoning>,
    pub summary_instructions: String,
    pub summary_prefix: String,
}

/// Selects a typed runtime configuration value without an additional service contract.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub enum ConfigKey {
    Turn,
    Context,
    Compaction,
    SystemPromptBindings,
    AgentsFileTemplate,
    ModelAliasesTemplate,
    AgentIdentityTemplate,
    StandingInstructionTemplate,
    SkillCatalogTemplate,
    SkillUnavailableTemplate,
    SkillSelectedTemplate,
    SkillTooLargeTemplate,
    ContextReminderTemplate,
    ContextLimitReminderTemplate,
    FinalRequestTemplate,
    RestoredToolsTemplate,
    InterruptedTemplate,
    TruncatedToolCallTemplate,
    InvalidToolCallTemplate,
    OptionalCliUtilitiesTemplate,
    ExpectedCliUtilitiesTemplate,
    PlatformTemplate,
    WorkingDirectoryTemplate,
    GitRepositoryTemplate,
    SubagentsTemplate,
    SecurityProfileTemplate,
    AgentPathEnvironmentTemplate,
    AgentScratchTemplate,
    AgentHistoryTemplate,
}

pub enum ConfigValue {
    Turn(TurnOptions),
    Context(ContextConfiguration),
    Compaction(CompactionConfiguration),
    /// Ordered namespace keys and their explicitly enumerated template configuration keys.
    SystemPromptBindings(Vec<(String, ConfigKey)>),
    PromptTemplate(Arc<PromptTemplate>),
}

impl ConfigValue {
    pub fn into_turn(self) -> Result<TurnOptions, Error> {
        match self {
            Self::Turn(value) => Ok(value),
            _ => Err(Error::failed("expected turn configuration")),
        }
    }
    pub fn into_context(self) -> Result<ContextConfiguration, Error> {
        match self {
            Self::Context(value) => Ok(value),
            _ => Err(Error::failed("expected context configuration")),
        }
    }
    pub fn into_compaction(self) -> Result<CompactionConfiguration, Error> {
        match self {
            Self::Compaction(value) => Ok(value),
            _ => Err(Error::failed("expected compaction configuration")),
        }
    }
    pub fn into_system_prompt_bindings(self) -> Result<Vec<(String, ConfigKey)>, Error> {
        match self {
            Self::SystemPromptBindings(value) => Ok(value),
            _ => Err(Error::failed(
                "expected system prompt bindings configuration",
            )),
        }
    }
    pub fn into_prompt_template(self) -> Result<Arc<PromptTemplate>, Error> {
        match self {
            Self::PromptTemplate(value) => Ok(value),
            _ => Err(Error::failed("expected prompt template configuration")),
        }
    }
}

/// A compiled prompt template shared by core and native configuration catalogs.
pub struct PromptTemplate {
    pub(crate) path: String,
    pub(crate) allowed: std::collections::BTreeSet<String>,
    pub(crate) required: Vec<String>,
    pub(crate) engine: crate::template::Engine,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum CompactionFailure {
    Cancelled,
    GroupExceedsBudget,
    TokenOverflow,
    InputLimitExceeded,
    FittedInputLimitExceeded,
    NoSafeBoundary,
    NoSummary(String),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PlanningError {
    InputLimitExceeded,
    NoOutputCapacity,
    NoCompactionCapacity,
    GroupExceedsBudget,
    TokenOverflow,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ErrorDetail {
    Template,
    PromptContext,
    Planning(PlanningError),
    Compaction(CompactionFailure),
}
