use super::message::{ProviderResponse, Reasoning};
use super::{
    BoxFuture, CancellationToken, Error, EventPublisher, Provider, SkillProvider, States, Tools,
};

/// Drives turn execution and records turn lifecycle effects through core contracts.
pub struct TurnEngine;

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
}
/// Consolidates the agent-local state that survives across turns and restarts.
/// Core reads this state at turn start and reloads it after provider and tool effects.
/// The states side persists complete values core saves, including after a turn fails.
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

/// Source turn-request and bounded settlement budget, distinct from token limits.
impl AgentState {
    /// Clears the per-turn request count when a new turn starts.
    pub fn begin_turn(&mut self) {
        self.provider_requests = 0;
    }

    /// Resets the invocation budget, keeping the phase, standing statuses and
    /// last request statistics.
    pub fn restart(&mut self) {
        self.provider_requests = 0;
        self.settlement_extensions = 0;
    }

    pub fn admit(&mut self, maximum: i32) -> Option<TurnFlags> {
        if self.provider_requests >= maximum.wrapping_add(self.settlement_extensions) {
            return None;
        }
        let final_phase = self.provider_requests.wrapping_add(1) >= maximum;
        Some(TurnFlags { final_phase })
    }

    pub fn count_request(&mut self) -> i32 {
        self.provider_requests = self.provider_requests.wrapping_add(1);
        self.provider_requests
    }

    pub fn try_extend_settlement(&mut self) -> bool {
        self.settlement_extensions = self.settlement_extensions.wrapping_add(1);
        self.settlement_extensions <= 4
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct TurnFlags {
    pub final_phase: bool,
}

/// Supplies the drain loop with runtime-owned lifecycle facts and per-turn work.
pub trait TurnLifecycle: Send {
    /// Reports whether runtime-owned work is scheduled and must run before another turn.
    fn has_scheduled_work(&self) -> bool;
    /// Reports whether a turn awaits a response or pending input remains.
    fn has_ready_turn(&mut self) -> BoxFuture<'_, Result<bool, Error>>;
    /// Prepares and publishes a turn before core activates and executes it.
    /// An opening failure returns no turn and does not require disposal.
    fn open_turn<'a>(
        &'a mut self,
        cancellation: CancellationToken<'a>,
    ) -> BoxFuture<'a, Result<Box<dyn Turn + 'a>, Error>>;
}

/// Owns one published turn and its runtime resources until execution finishes.
pub trait Turn: Send {
    /// Borrows the services core uses throughout this turn's execution.
    fn borrow_services(&mut self) -> TurnServices<'_>;
    /// Releases resources and records the result after execution, including errors.
    /// Core awaits disposal once before propagating the result. Disposal errors take precedence.
    /// Dropping the execution future does not perform asynchronous disposal.
    fn dispose<'a>(
        &'a mut self,
        result: &'a Result<Outcome, Error>,
    ) -> BoxFuture<'a, Result<(), Error>>;
}

/// Borrows the published identity and services needed to execute one turn.
pub struct TurnServices<'a> {
    pub start_event_id: &'a str,
    pub states: &'a mut dyn States,
    pub skills: &'a dyn SkillProvider,
    pub tools: &'a dyn Tools,
    pub provider: &'a dyn Provider,
    pub publisher: &'a dyn EventPublisher,
    pub hooks: &'a dyn TurnHooks,
}

/// Receives core's lifecycle notifications independently of conversation storage.
pub trait TurnHooks: Send + Sync {
    /// Captures the initial model selection and notifies provider sessions before publication.
    fn begin_conversation_turn(&self) -> BoxFuture<'_, Result<crate::TurnModel, Error>>;
    /// Starts runtime activity and prepares the prompt epoch and skill snapshot after publication.
    fn activate_conversation_turn(&self, start_event_id: &str) -> BoxFuture<'_, Result<(), Error>>;
    /// Captures settings for the next step without changing in-flight operations.
    fn begin_step(&self) -> BoxFuture<'_, Result<(), Error>>;
}
