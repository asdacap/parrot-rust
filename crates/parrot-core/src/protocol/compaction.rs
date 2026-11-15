use super::context::ContextLimit;
use super::tool::ToolDefinition;

/// Why compaction started. The origin changes only how the run is reported.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum CompactionTrigger {
    /// The runtime started compaction itself. The context crossed its cost
    /// threshold, or the provider rejected an oversized request.
    #[default]
    Automatic,
    /// The model started compaction by calling the compaction tool.
    ModelRequested,
    /// A user command or RPC call requested compaction explicitly.
    UserRequested,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct CompactionRequest {
    pub target: Option<ContextLimit>,
    pub tools: Vec<ToolDefinition>,
    pub trigger: CompactionTrigger,
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
