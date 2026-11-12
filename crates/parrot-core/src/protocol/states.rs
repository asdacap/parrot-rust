use super::config::{ConfigKey, ConfigValue};
use super::context::{ContextSnapshot, TokenCalibration};
use super::conversation::{ConversationHistoryItem, ConversationSummary};
use super::message::{ConversationTurn, InputAdmission, Message, PendingMessage, PromotedInput};
use super::skill::{Skill, SkillContent, SkillReference};
use super::tool::ToolDefinition;
use super::turn::AgentState;
use super::{BoxFuture, CancellationToken, Error};

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
    /// Reads the status slots offered for the supplied context snapshot, each
    /// with a source-computed hash and rendered text. Callers join the slot
    /// texts to assemble the full status. Defaults to no statuses, which
    /// steers nothing.
    fn read_status_offering(
        &self,
        _context: ContextSnapshot,
    ) -> BoxFuture<'_, Result<crate::StatusOffering, Error>> {
        Box::pin(async { Ok(crate::StatusOffering::default()) })
    }
    /// Reads the per-slot hashes recorded by the latest persisted status assembly.
    /// Defaults to empty standing state, which would steer the initial assembly.
    /// A states side that never appends will also never offer, so both defaults
    /// keep steering inert until the adapter opts in.
    fn read_standing_statuses(&self) -> BoxFuture<'_, Result<crate::StatusStanding, Error>> {
        Box::pin(async { Ok(crate::StatusStanding::default()) })
    }
    /// Reads typed configuration without rendering prompts or deciding request policy.
    fn get_config(&self, key: ConfigKey) -> BoxFuture<'_, Result<ConfigValue, Error>>;
    /// Reads the consolidated agent state that survives across turns, including
    /// the durable final-phase marker and the provider-request budget. Core resets
    /// the request count at turn start but retains settlement extensions from earlier turns.
    fn get_agent_state(&self) -> BoxFuture<'_, Result<AgentState, Error>> {
        Box::pin(async { Err(Error::unsupported("States.get_agent_state")) })
    }
    /// Saves the updated agent state after the turn returns, even on error.
    /// Retains settlement extensions for subsequent turns in the same invocation.
    fn set_agent_state(&mut self, _state: AgentState) -> BoxFuture<'_, Result<(), Error>> {
        Box::pin(async { Err(Error::unsupported("States.set_agent_state")) })
    }
    /// Begins the turn: captures the selection it runs under, releases the previous
    /// turn's skill snapshot, and begins the provider session. Returns the model the
    /// start event presents.
    fn begin_turn(&mut self) -> BoxFuture<'_, Result<crate::TurnModel, Error>> {
        Box::pin(async { Err(Error::unsupported("States.begin_turn")) })
    }
    /// Records the published start event as the turn's durable key and reports the
    /// turn start to session activity.
    fn record_turn_started(&mut self, _turn_id: &str) -> BoxFuture<'_, Result<(), Error>> {
        Box::pin(async { Err(Error::unsupported("States.record_turn_started")) })
    }
    /// Activates the recorded turn: samples the prompt epoch, binds turn status, and
    /// starts the turn's skill snapshot.
    fn activate_turn(&mut self) -> BoxFuture<'_, Result<(), Error>> {
        Box::pin(async { Err(Error::unsupported("States.activate_turn")) })
    }
    /// Joins accepted mutations and reads ordered completion blockers.
    fn collect_completion_blockers(&mut self) -> BoxFuture<'_, Result<Vec<String>, Error>> {
        Box::pin(async { Err(Error::unsupported("States.collect_completion_blockers")) })
    }
    /// Writes the supplied bytes to agent-local scratch storage and returns the saved path.
    fn write_to_scratch<'a, 'c>(
        &'a self,
        _contents: &'a [u8],
        _cancellation: CancellationToken<'c>,
    ) -> BoxFuture<'a, Result<String, Error>>
    where
        'c: 'a,
    {
        Box::pin(async { Err(Error::unsupported("States.write_to_scratch")) })
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
    /// Peeks at the next durable pending input header without reserving, promoting,
    /// or materializing its content.
    fn peek_pending_input_header(&self) -> BoxFuture<'_, Result<Option<PendingMessage>, Error>> {
        Box::pin(async { Err(Error::unsupported("States.peek_pending_input_header")) })
    }
    /// Reports whether a turn awaits a response or pending input remains.
    fn has_ready_turn(&self) -> BoxFuture<'_, Result<bool, Error>> {
        Box::pin(async {
            let turn = self.get_last_turn().await?;
            Ok(turn.as_ref().is_some_and(ConversationTurn::needs_response)
                || self.peek_pending_input_header().await?.is_some())
        })
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
    /// Retrieves skill metadata in one batch without reading instruction bodies.
    /// Returns one result per reference in the supplied order, preserving backing-store
    /// precedence and name/path matching semantics.
    fn resolve_skill_references<'a>(
        &'a self,
        _references: &'a [SkillReference],
    ) -> BoxFuture<'a, Result<Vec<Option<Skill>>, Error>> {
        Box::pin(async { Err(Error::unsupported("States.resolve_skill_references")) })
    }
    /// Reads the selected record's complete instruction body or unavailable diagnostic.
    /// Does not format prompt text, update turn deduplication, or record context.
    fn read_skill_content<'a>(
        &'a self,
        _skill: &'a Skill,
    ) -> BoxFuture<'a, Result<SkillContent, Error>> {
        Box::pin(async { Err(Error::unsupported("States.read_skill_content")) })
    }
    /// Reads the current token calibration without choosing context or output limits.
    fn read_token_calibration(&self) -> BoxFuture<'_, Result<TokenCalibration, Error>> {
        Box::pin(async { Err(Error::unsupported("States.read_token_calibration")) })
    }
    /// Reads the current provider-visible conversation after context preparation.
    fn read_provider_messages(&self) -> BoxFuture<'_, Result<Vec<Message>, Error>> {
        Box::pin(async { Err(Error::unsupported("States.read_provider_messages")) })
    }
    /// Reads the token totals the states side recorded for the current turn,
    /// for the terminal TurnEnded event assembled by core at close time.
    fn read_usage_totals(&self) -> BoxFuture<'_, Result<crate::UsageTotals, Error>> {
        Box::pin(async { Err(Error::unsupported("States.read_usage_totals")) })
    }
    /// Reads the latest stored summary and its compaction watermark.
    fn read_conversation_summary(
        &self,
    ) -> BoxFuture<'_, Result<Option<ConversationSummary>, Error>> {
        Box::pin(async { Err(Error::unsupported("States.read_conversation_summary")) })
    }
    /// Reads the chronological durable tail after the supplied watermark.
    /// Retains stored ownership and status metadata and materializes message contents.
    fn read_conversation_tail(
        &self,
        _after: i64,
    ) -> BoxFuture<'_, Result<Vec<ConversationHistoryItem>, Error>> {
        Box::pin(async { Err(Error::unsupported("States.read_conversation_tail")) })
    }
    /// Reads active checkpoint assistant sequences after the current summary watermark.
    fn read_checkpoint_sequences(&self) -> BoxFuture<'_, Result<Vec<i64>, Error>> {
        Box::pin(async { Err(Error::unsupported("States.read_checkpoint_sequences")) })
    }
    /// Persists the fitted compaction with the supplied sampled instructions and tools,
    /// awaiting terminal lifecycle effects on success or failure. Returns the instructions
    /// the epoch uses after the persisted compaction; the caller computes the reduction outcome.
    fn finalize_compaction<'a>(
        &'a mut self,
        _instructions: &'a str,
        _definitions: &'a [ToolDefinition],
        _result: Result<Option<crate::FittedCompaction>, Error>,
    ) -> BoxFuture<'a, Result<String, Error>> {
        Box::pin(async { Err(Error::unsupported("States.finalize_compaction")) })
    }
}
