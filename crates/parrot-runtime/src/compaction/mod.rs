//! Provider budgets, causal summary execution and awaited durable context epochs.
mod budget;
mod execution;
pub use execution::{CompactionError, CompactionObserver, CompactionResult, Compactor};
pub(crate) use execution::{CompactionEstimates, SummaryText, read_file};
mod context;
mod conversation;
pub use context::{
    ContextCompactionResult, ContextController, ContextError, ContextEvents, ContextResult,
    ContextStatus, EpochInput, EpochResult, provider_message,
};
pub use conversation::apply_conversation_addition;
pub(crate) use conversation::materialize_message;
pub use parrot_core::{StatusKey, StatusOffer, StatusOffering, StatusStanding};
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
