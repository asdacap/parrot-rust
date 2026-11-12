//! Provider budgets, causal summary execution and awaited durable context epochs.
mod execution;
pub use execution::{
    CompactionError, CompactionObserver, CompactionResources, CompactionResult, Compactor,
};
pub(crate) use execution::{CompactionEstimates, SummaryText, progress_event};
mod context;
mod conversation;
pub use context::{
    ContextCompactionResult, ContextController, ContextError, ContextEvents, ContextResult,
    ContextStatus, EpochInput, EpochResult, provider_message,
};
pub use conversation::add_conversation_messages;
pub(crate) use conversation::materialize_message;
pub use parrot_core::{StatusKey, StatusOffer, StatusOffering, StatusStanding};
mod planning;

pub use parrot_core::PlanningError;
pub use parrot_domain::values::{
    accounting::RetainedAgentBudget,
    context::{ContextSize, ContextSnapshot, TokenEstimator},
};
pub use planning::{CompactionGroup, CompactionPlanner, CompactionSettings, PlanningInput};
