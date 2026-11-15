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
mod status;
mod template;
mod tool;
mod turn;

pub use cancellation::{Cancellation, CancellationToken, Cancelled, NeverCancelled};
pub use compaction::{
    CompactionConfiguration, CompactionFailure, CompactionGroup, CompactionHistoryGroup,
    CompactionInput, CompactionPlan, CompactionPolicy, CompactionProgress, CompactionRequest,
    CompactionResult, FittedCompaction, PlanningError, SummaryResponse,
};
pub use config::{ConfigKey, ConfigValue, PromptTemplate};
pub use context::{
    ContextCompaction, ContextConfiguration, ContextLimit, ContextModel, ContextReminderCheckpoint,
    ContextSnapshot, OutputTokenError, RequestStatistic,
};
pub use conversation::{
    ConversationAddition, ConversationEvent, ConversationHistoryItem, ConversationOrigin,
    ConversationPersistence, ConversationPublication, ConversationReceipt, ConversationState,
    ConversationSummary, HistoryGroup, HistoryGroupMember, UsageTotals,
};
pub use error::{Error, ErrorCode, ErrorDetail, ErrorKind};
pub use event::EventPublisher;
pub use message::{
    Content, ConversationTurn, FinishReason, InputAdmission, Message, PromotedInput,
    ProviderRequest, ProviderResponse, Reasoning, Role,
};
pub use provider::{Provider, ProviderInvokeError, Tools};
pub use skill::{DerivedContext, Skill, SkillContent, SkillMention, SkillProvider, SkillReference};
pub use states::States;
pub use status::{StatusKey, StatusOffer, StatusOffering, StatusStanding};
pub use template::{MiniJinjaPromptTemplateEngine, ScalarPromptTemplateEngine};
pub use tool::{
    ToolCall, ToolCallReceipt, ToolCatalog, ToolDefinition, ToolExecutionRequest, ToolRecovery,
    ToolResult, ToolSettlement, ToolStatus,
};
pub use turn::{
    AgentState, CompletionBlockerFeedback, OpenedTurn, Outcome, TurnFlags, TurnHooks,
    TurnLifecycle, TurnModel, TurnModelIcon, TurnModelIconColor, TurnOptions,
};
