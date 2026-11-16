use super::config::{ConfigKey, ConfigValue};
use super::conversation::ConversationState;
use super::message::{ConversationTurn, InputAdmission, PromotedInput};
use super::turn::AgentState;
use super::{BoxFuture, Error};

/// Owns conversation effects, input readiness, and durable turn transitions.
/// Mutations receive their input from the caller rather than retrieving staged input.
/// Unsupported capabilities return explicit errors rather than inventing state.
pub trait States: Send + Sync {
    /// Commits the supplied messages, updates the live conversation on insertion, then publishes.
    /// Storage metadata selects the atomic write. Core supplies every message and its timing.
    fn add_conversation_messages(
        &mut self,
        _addition: crate::ConversationAddition,
    ) -> BoxFuture<'_, Result<crate::ConversationReceipt, Error>> {
        Box::pin(async { Err(Error::unsupported("States.add_conversation_messages")) })
    }
    /// Reads the status slots each status source offers from its own state,
    /// each with a source-computed hash and rendered text. Callers join the
    /// slot texts to assemble the full status. Defaults to no statuses, which
    /// steers nothing.
    fn read_status_offering(&self) -> BoxFuture<'_, Result<crate::StatusOffering, Error>> {
        Box::pin(async { Ok(crate::StatusOffering::default()) })
    }
    /// Reads typed configuration without rendering prompts or deciding request policy.
    fn get_config(&self, key: ConfigKey) -> BoxFuture<'_, Result<ConfigValue, Error>>;
    /// Reads the agent state core last saved, restored from durable storage after a restart.
    /// Core resets the request count at turn start but retains settlement extensions from earlier turns.
    fn get_agent_state(&self) -> BoxFuture<'_, Result<AgentState, Error>> {
        Box::pin(async { Err(Error::unsupported("States.get_agent_state")) })
    }
    /// Saves and durably persists the whole agent state, so a later read returns it unchanged.
    /// Core saves before provider, tool, and compaction effects can update it, and again
    /// after the turn returns, even on error.
    /// Retains settlement extensions for subsequent turns in the same invocation.
    fn set_agent_state(&mut self, _state: AgentState) -> BoxFuture<'_, Result<(), Error>> {
        Box::pin(async { Err(Error::unsupported("States.set_agent_state")) })
    }
    /// Joins accepted mutations and reads ordered completion blockers.
    fn collect_completion_blockers(&mut self) -> BoxFuture<'_, Result<Vec<String>, Error>> {
        Box::pin(async { Err(Error::unsupported("States.collect_completion_blockers")) })
    }
    /// Captures the complete provider-rendered system prompt and native request bindings.
    fn capture_system_prompt(&mut self) -> BoxFuture<'_, Result<String, Error>> {
        Box::pin(async { Err(Error::unsupported("States.capture_system_prompt")) })
    }
    /// Reads the latest request's tool calls and saved results when settlement is incomplete.
    fn read_tool_recovery(&self) -> BoxFuture<'_, Result<Option<crate::ToolRecovery>, Error>> {
        Box::pin(async { Err(Error::unsupported("States.read_tool_recovery")) })
    }
    /// Reads the latest visible logical turn, including retained completion candidates.
    fn get_last_turn(&self) -> BoxFuture<'_, Result<Option<ConversationTurn>, Error>> {
        Box::pin(async { Err(Error::unsupported("States.get_last_turn")) })
    }
    /// Promotes all currently pending steers, or one queued input when permitted,
    /// preserving atomic native admission and returning the promoted content explicitly.
    /// Returns commit receipts without adding messages, publishing, or preparing prompts.
    fn promote_pending_inputs(
        &mut self,
        _admission: InputAdmission,
    ) -> BoxFuture<'_, Result<Vec<PromotedInput>, Error>> {
        Box::pin(async { Err(Error::unsupported("States.promote_pending_inputs")) })
    }
    /// Reads the token totals the states side recorded for the current turn,
    /// for the terminal TurnEnded event assembled by core at close time.
    fn read_usage_totals(&self) -> BoxFuture<'_, Result<crate::UsageTotals, Error>> {
        Box::pin(async { Err(Error::unsupported("States.read_usage_totals")) })
    }
    /// Captures the persisted summary and its chronological history tail.
    /// Core assembles provider-visible messages from these facts.
    fn read_conversation_state(&self) -> BoxFuture<'_, Result<ConversationState, Error>> {
        Box::pin(async { Err(Error::unsupported("States.read_conversation_state")) })
    }
}
