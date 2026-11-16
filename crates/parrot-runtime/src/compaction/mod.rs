//! Agent-local infrastructure effects for provider-owned conversation compaction.
mod effects;
pub use effects::{CompactionError, CompactionObserver};
mod context;
mod conversation;
pub use context::{
    ContextCompactionResult, ContextController, ContextError, ContextEvents, ContextResult,
    EpochInput, EpochResult, provider_message,
};
pub(crate) use conversation::materialize_message;
pub use parrot_core::{StatusKey, StatusOffer, StatusOffering, StatusStanding};
pub use parrot_domain::values::{
    accounting::RetainedAgentBudget,
    context::{ContextSize, ContextSnapshot, TokenEstimator},
};
