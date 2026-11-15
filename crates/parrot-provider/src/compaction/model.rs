use parrot_core::{ContextConfiguration, ContextLimit, Message, Reasoning, ToolDefinition};

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct CompactionConfiguration {
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
pub(super) struct CompactionGroup {
    pub messages: Vec<Message>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct CompactionHistoryGroup {
    pub messages: Vec<Message>,
    pub end_watermark: i64,
    pub is_complete: bool,
}
#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct CompactionInput {
    pub groups: Vec<CompactionHistoryGroup>,
    pub base_watermark: i64,
    pub instructions: String,
    pub messages: Vec<Message>,
    pub fixed_message: Message,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct CompactionPlan {
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
pub(super) struct CompactionResult {
    pub history: Vec<Message>,
    pub summary: Message,
    pub watermark: i64,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct FittedCompaction {
    pub result: CompactionResult,
    pub status: String,
    pub notice: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) enum SummaryResponse {
    Summary(String),
    NoSummary(String),
}
