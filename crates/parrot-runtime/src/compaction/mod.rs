//! Provider budgets, causal summary execution and awaited durable context epochs.
mod budget;
mod execution;
pub use execution::{CompactionError, CompactionObserver, CompactionResult, Compactor};
pub(crate) use execution::{CompactionEstimates, SummaryProviderAdapter};
mod context;
pub use context::{
    ContextCompactionResult, ContextController, ContextError, ContextEvents, ContextResult,
    ContextStatus, EpochInput, EpochResult, StatusInjection, provider_message,
};
mod planning;

pub use budget::ProviderTokenBudget;
pub use parrot_core::compaction::PlanningError;
pub use parrot_domain::values::{
    accounting::RetainedAgentBudget,
    context::{ContextCompactionPolicy, ContextSize, ContextSnapshot, TokenEstimator},
};
pub use planning::{
    CompactionGroup, CompactionPlanner, CompactionSettings, PlanningInput, RetentionPlan,
    group_messages, is_complete, is_eligible_for_spill,
};
