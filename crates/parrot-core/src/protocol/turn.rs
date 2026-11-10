use super::message::{ProviderResponse, Reasoning};

#[derive(Debug, Eq, PartialEq)]
pub enum Outcome {
    Completed(String),
    BudgetExhausted,
}

/// One rejected completion attempt: the blocker feedback messages steering the
/// next turn and the assistant candidate response they answered. The states side
/// replays each message to its completion source when this addition is applied.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CompletionBlockerFeedback {
    pub messages: Vec<String>,
    pub candidate: ProviderResponse,
}
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TurnOptions {
    pub maximum_requests: i32,
    pub model: String,
    pub reasoning: Option<Reasoning>,
    pub provider_preferences: String,
    pub include_router_metadata: bool,
}
/// Consolidates the agent-local state that survives across turns. Core reads it at
/// turn start and saves it after the turn returns, even on error.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AgentState {
    /// Durable final-phase marker read without changing the conversation.
    pub final_phase: bool,
    /// Per-invocation setting that turns off the provider-request limit entirely.
    pub(crate) disable_max_turns: bool,
    /// Provider requests counted during the current turn. Not a token budget.
    pub(crate) provider_requests: i32,
    /// Settlement extensions retained across turns within the same invocation.
    pub(crate) settlement_extensions: i32,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct TurnFlags {
    pub final_phase: bool,
}
