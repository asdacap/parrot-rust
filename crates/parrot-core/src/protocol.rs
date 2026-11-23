pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

use std::{future::Future, pin::Pin};

mod cancellation;
mod compaction;
mod config;
mod context;
mod conversation;
mod error;
mod event;
mod filter;
mod message;
mod provider;
mod skill;
mod states;
mod status;
mod template;
mod tool;
mod turn;

pub use cancellation::{Cancellation, CancellationToken, Cancelled, NeverCancelled};
pub use compaction::{CompactionFailure, CompactionRequest, CompactionTrigger, PlanningError};
pub use config::{ConfigKey, ConfigValue, PromptTemplate};
pub use context::{
    ContextCompaction, ContextConfiguration, ContextLimit, ContextReminderCheckpoint,
    ContextSnapshot, OutputTokenError, RequestStatistic,
};
pub use conversation::{
    ConversationAddition, ConversationEvent, ConversationHistoryItem, ConversationOrigin,
    ConversationPersistence, ConversationPublication, ConversationReceipt, ConversationState,
    ConversationSummary, HistoryGroup, HistoryGroupMember, UsageTotals, assemble_history_groups,
};
pub use error::{Error, ErrorCode, ErrorDetail, ErrorKind};
pub use event::EventPublisher;
pub use filter::{
    FilterAmendment, FilterProvider, FilterRequest, FilterSettings, ReasoningFilter, ReasoningKind,
};
pub use message::{
    Content, ConversationTurn, FinishReason, InputAdmission, Message, PromotedInput,
    ProviderRequest, ProviderResponse, Reasoning, Role,
};
pub use provider::{Provider, ProviderInvokeError, Tools};
pub use skill::{
    PreparedSkillPrompt, Skill, SkillContent, SkillMention, SkillProvider, SkillReference,
    parse_skill_mentions,
};
pub use states::States;
pub use status::{
    StatusKey, StatusOffer, StatusOffering, StatusStanding, join_status_offering,
    status_steering_needed,
};
pub use template::{MiniJinjaPromptTemplateEngine, ScalarPromptTemplateEngine, validate_arguments};
pub use tool::{
    ToolCall, ToolCallReceipt, ToolCatalog, ToolDefinition, ToolExecutionRequest, ToolRecovery,
    ToolResult, ToolSettlement, ToolStatus,
};
pub use turn::{
    AgentState, CompletionBlockerFeedback, Outcome, Turn, TurnEngine, TurnFlags, TurnHooks,
    TurnLifecycle, TurnOptions, TurnServices,
};
