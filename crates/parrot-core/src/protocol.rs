pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

use std::{future::Future, pin::Pin};

mod cancellation;
mod compaction;
mod config;
mod context;
mod conversation;
mod error;
mod event;
mod message;
mod provider;
mod skill;
mod states;
mod tool;
mod turn;

pub use cancellation::{Cancellation, CancellationToken, Cancelled, NeverCancelled};
pub use compaction::{
    CompactionConfiguration, CompactionFailure, CompactionGroup, CompactionHistoryGroup,
    CompactionInput, CompactionPlan, CompactionProgress, CompactionResult, FittedCompaction,
    PlanningError, SummaryResponse,
};
pub use config::{ConfigKey, ConfigValue, PromptTemplate};
pub use context::{
    ContextCompaction, ContextConfiguration, ContextLimit, ContextModel, ContextReminderCheckpoint,
    ContextSnapshot, RequestStatistic,
};
pub use conversation::{ConversationHistoryItem, ConversationState, ConversationSummary};
pub use error::{Error, ErrorCode, ErrorDetail, ErrorKind};
pub use event::EventPublisher;
pub use message::{
    Content, ConversationTurn, Delivery, FinishReason, InputAdmission, Message, PendingMessage,
    PromotedInput, ProviderRequest, ProviderResponse, Reasoning, Role,
};
pub use provider::{Provider, Tools};
pub use skill::{DerivedContext, Skill, SkillContent, SkillReference};
pub use states::States;
pub use tool::{
    ToolCall, ToolCallReceipt, ToolCatalog, ToolDefinition, ToolResult, ToolResultMetadata,
    ToolStatus,
};
pub use turn::{
    AgentState, CompletionBlockerFeedback, Outcome, TurnFlags, TurnModel, TurnModelIcon,
    TurnModelIconColor, TurnOptions,
};
