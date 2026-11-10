use super::context::{ContextConfiguration, ContextLimit};
use super::message::{Message, Reasoning};
use super::tool::ToolDefinition;

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
