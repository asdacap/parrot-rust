use super::context::ContextLimit;
use super::tool::ToolDefinition;

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct CompactionRequest {
    pub target: Option<ContextLimit>,
    pub tools: Vec<ToolDefinition>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum CompactionFailure {
    Cancelled,
    GroupExceedsBudget,
    InputLimitExceeded,
    FittedInputLimitExceeded,
    NoSafeBoundary,
    NoSummary(String),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PlanningError {
    NoCompactionCapacity,
    GroupExceedsBudget,
    TokenOverflow,
}
