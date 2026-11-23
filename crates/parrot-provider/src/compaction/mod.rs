//! Awaited conversation compaction with provider-owned execution and publication.
mod engine;
mod execution;
mod fitting;
mod model;
mod planning;
mod preparation;
mod reasoning;
mod service;
use execution::Execution;
pub use service::{Compactor, estimate_input_tokens};

#[cfg(test)]
mod engine_tests;
#[cfg(test)]
mod fitting_tests;
#[cfg(test)]
mod operation_tests;
#[cfg(test)]
mod planning_tests;
#[cfg(test)]
mod reasoning_estimate_tests;
#[cfg(test)]
mod test_support;
use parrot_core::{ContextSnapshot, ConversationState, Error, Message, StatusOffering};
use parrot_domain::contracts::async_ops::{BoxFuture, Cancellation};

#[derive(Clone, Debug)]
pub struct CompactionSettings {
    pub trigger_percent: i32,
    pub target_percent: i32,
    pub maximum_input_tokens: i32,
    /// Hard cap on the output tokens requested from the model; zero disables it.
    pub maximum_output_tokens: i32,
    pub summary_output_tokens: i32,
    pub summary_instructions: String,
    pub summary_prefix: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ContextCapture {
    pub instructions: String,
    pub conversation: ConversationState,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ReplacementCommit {
    pub message: Message,
    pub watermark: i64,
    pub status: String,
    pub notice: String,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct CompactionProgress {
    pub completed_groups: usize,
    pub total_groups: usize,
}

pub enum CompactionEvent {
    Started(parrot_core::CompactionTrigger),
    Progress(CompactionProgress),
    Saving,
    Retry(crate::sessions::Event),
    Finished,
    Failed(String),
}

/// Supplies infrastructure effects without selecting or executing compaction stages.
/// Implementations retain agent-local storage, authorization, and serialized admission.
pub trait CompactionEffects: Send + Sync {
    /// Refreshes the prompt epoch and reads raw durable and transient conversation facts.
    fn capture_context<'a>(
        &'a mut self,
        cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<ContextCapture, Error>>;

    /// Offers status text for the supplied snapshot without iterating token fitting.
    fn offer_status<'a>(
        &'a self,
        context: ContextSnapshot,
        cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<StatusOffering, Error>>;

    /// Persists the supplied replacement and associated status, then refreshes live history.
    /// Completes committed-status publication and reduction bookkeeping before returning.
    /// An error after persistence does not roll back the replacement.
    fn commit_replacement<'a>(
        &'a mut self,
        replacement: ReplacementCommit,
        cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<(), Error>>;

    /// Records and delivers the supplied event without choosing phases or driving execution.
    /// Uses the supplied cancellation, including shielding selected by the provider.
    fn publish<'a>(
        &'a self,
        event: CompactionEvent,
        cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<(), Error>>;
}
