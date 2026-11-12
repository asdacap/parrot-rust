use super::message::{ProviderResponse, Reasoning};

#[derive(Debug, Eq, PartialEq)]
pub enum Outcome {
    Completed(String),
    /// The invocation budget ran out before the turn completed. Carries the
    /// rendered terminal notice core records for that failure.
    BudgetExhausted(String),
}

/// A rejected completion attempt and the reminders steering the next request.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CompletionBlockerFeedback {
    pub messages: Vec<String>,
    pub candidate: ProviderResponse,
}
/// Describes the model a turn runs under, as presented by the turn start event.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TurnModel {
    /// Provider-qualified selector of the selected model.
    pub selector: String,
    /// Alias icon configured for the selected model.
    pub alias_icon: Option<TurnModelIcon>,
}

/// Icon an alias associates with the model a turn runs under.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TurnModelIcon {
    /// Grapheme shown beside the model.
    pub glyph: String,
    /// Color the presentation assigns to the glyph.
    pub color: TurnModelIconColor,
}

/// Colors available to model alias icons.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TurnModelIconColor {
    Black,
    Red,
    Green,
    Yellow,
    Blue,
    Magenta,
    Cyan,
    White,
    Gray,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TurnOptions {
    pub maximum_requests: i32,
    pub model: String,
    pub reasoning: Option<Reasoning>,
    pub provider_preferences: String,
    pub include_router_metadata: bool,
    /// Turns off the provider-request limit entirely.
    pub disable_max_turns: bool,
}
/// Consolidates the agent-local state that survives across turns and restarts.
/// Core owns every field: it reads the state at turn start and saves it after the
/// turn returns, even on error. The states side persists what core saves.
#[derive(Clone, Debug, Default, Eq, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AgentState {
    /// Whether the latest phase prompt restricted tools to the final-phase set.
    pub final_phase: bool,
    /// Latest model-specific input-token estimate and provider-reported usage.
    pub last_request_statistic: crate::RequestStatistic,
    /// Per-slot hashes of the latest steered status assembly.
    pub standing_statuses: crate::StatusStanding,
    /// Provider requests counted during the current turn. Not a token budget.
    pub(crate) provider_requests: i32,
    /// Settlement extensions retained across turns within the same invocation.
    pub(crate) settlement_extensions: i32,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct TurnFlags {
    pub final_phase: bool,
}
