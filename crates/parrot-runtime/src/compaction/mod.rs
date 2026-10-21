//! Provider budgets, causal summary execution and awaited durable context epochs.
mod budget;
mod execution;
pub use execution::{CompactionError, CompactionResult, Compactor, RetryObserver};
mod context;
pub use context::{
    ContextCompactionResult, ContextController, ContextError, ContextEvents, ContextResult,
    ContextStatus, EpochInput, EpochResult, StatusInjection, provider_message,
};
mod planning;

pub use budget::ProviderTokenBudget;
pub use parrot_domain::values::{
    accounting::RetainedAgentBudget,
    context::{
        ContextCadence, ContextCompactionPolicy, ContextSize, ContextSnapshot, TokenEstimator,
    },
};
pub use planning::{
    CompactionGroup, CompactionPlanner, CompactionSettings, PlanningInput, RetentionPlan,
    group_messages, is_complete, is_eligible_for_spill,
};
use std::fmt;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PlanningError {
    InputLimitExceeded,
    NoOutputCapacity,
    NoCompactionCapacity,
    GroupExceedsBudget,
    TokenOverflow,
}
impl fmt::Display for PlanningError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::InputLimitExceeded => "The conversation exceeds the selected model input limit.",
            Self::NoOutputCapacity => {
                "The conversation leaves no output capacity in the selected model context window."
            }
            Self::NoCompactionCapacity => {
                "The selected model leaves no room for a compaction request."
            }
            Self::GroupExceedsBudget => {
                "A complete conversation group exceeds the compaction input budget."
            }
            Self::TokenOverflow => "Token estimate overflow.",
        })
    }
}
impl std::error::Error for PlanningError {}
