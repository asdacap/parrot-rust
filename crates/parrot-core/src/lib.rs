//! Turn orchestration with core-owned provider, tool, and state contracts.

pub mod budget;
pub mod compaction;
mod context;
mod protocol;
pub mod rendering;
mod skills;
pub mod template;
pub use context::*;
pub use protocol::*;
pub use skills::{SkillMention, parse_skill_mentions};

/// Drains ready turns using state-backed settings and the invocation budget.
pub async fn run(
    states: &mut dyn States,
    tools: &dyn Tools,
    provider: &dyn Provider,
    cancellation: CancellationToken<'_>,
) -> Result<Outcome, Error> {
    let mut answer = String::new();
    loop {
        if cancellation.is_cancelled() {
            return Err(Error::cancelled());
        }
        let turn = states.get_last_turn().await?;
        if !turn.as_ref().is_some_and(ConversationTurn::needs_response)
            && states.get_pending_message().await?.is_none()
        {
            return Ok(Outcome::Completed(answer));
        }
        match run_turn(states, tools, provider, cancellation).await? {
            Outcome::Completed(completed) => answer = completed,
            Outcome::BudgetExhausted => return Ok(Outcome::BudgetExhausted),
        }
    }
}

/// Runs one active turn using state-backed settings and tools captured once.
pub async fn run_turn(
    states: &mut dyn States,
    tools: &dyn Tools,
    provider: &dyn Provider,
    cancellation: CancellationToken<'_>,
) -> Result<Outcome, Error> {
    let mut budget = states.read_turn_budget()?;
    let result = execute_turn(states, tools, provider, cancellation, &mut budget).await;
    states.write_turn_budget(budget)?;
    result
}

async fn execute_turn(
    states: &mut dyn States,
    tools: &dyn Tools,
    provider: &dyn Provider,
    cancellation: CancellationToken<'_>,
    budget: &mut TurnBudget,
) -> Result<Outcome, Error> {
    if cancellation.is_cancelled() {
        return Err(Error::cancelled());
    }
    let options = states.get_config(ConfigKey::Turn).await?.into_turn()?;
    budget.begin_turn();
    let catalog = tools.capture_catalog().await?;
    let mut compacted_oversized = false;
    let mut skills = skills::SkillPreparation::default();
    loop {
        if cancellation.is_cancelled() {
            return Err(Error::cancelled());
        }
        let turn = states.get_last_turn().await?;
        let admission = if turn.as_ref().is_some_and(ConversationTurn::needs_response) {
            InputAdmission::SteersOnly
        } else {
            InputAdmission::SteersOrNextQueued
        };
        let inputs = states.promote_input(admission).await?;
        let context = skills.prepare(states, &inputs, cancellation).await?;
        states.record_input_context(&context).await?;
        let Some(flags) = budget.admit(options.maximum_requests) else {
            return Ok(Outcome::BudgetExhausted);
        };
        let definitions = if flags.final_phase {
            &catalog.final_phase
        } else {
            &catalog.normal
        };
        let request =
            context::prepare_request(states, provider, definitions, flags, &options, cancellation)
                .await?;
        let attempt = budget.count_request();
        let response = match provider.invoke(request, cancellation).await {
            Ok(response) => response,
            Err(error) if error.kind == ErrorKind::RequestTooLarge && !compacted_oversized => {
                compacted_oversized = true;
                if !context::compact_context(states, provider, definitions, cancellation)
                    .await?
                    .reduced
                {
                    return Err(error);
                }
                continue;
            }
            Err(error) => return Err(error),
        };
        compacted_oversized = false;
        if response.finish_reason == FinishReason::Length && !response.tool_calls.is_empty() {
            let content =
                rendering::render_template(states, ConfigKey::TruncatedToolCallTemplate, &[])
                    .await?;
            states.record_retry(&content, attempt).await?;
            continue;
        }
        if response
            .tool_calls
            .iter()
            .any(|call| serde_json::from_str::<serde_json::Value>(&call.arguments_json).is_err())
        {
            let content =
                rendering::render_template(states, ConfigKey::InvalidToolCallTemplate, &[]).await?;
            states.record_retry(&content, attempt).await?;
            continue;
        }
        if !response.tool_calls.is_empty() {
            let receipt = states.record_tool_calls(&response).await?;
            let results = tools
                .execute(response.tool_calls.clone(), receipt, flags, cancellation)
                .await?;
            if results.is_empty() {
                return Err(Error::failed("tool execution returned no results"));
            }
            states.record_tool_results(&results).await?;
            if flags.final_phase && !budget.try_extend_settlement() {
                return Ok(Outcome::BudgetExhausted);
            }
            continue;
        }
        let messages = states.get_exit_blockers().await?;
        if messages.is_empty() {
            return Ok(Outcome::Completed(states.complete_turn(&response).await?));
        }
        states
            .record_completion_feedback(CompletionFeedback {
                messages,
                candidate: response,
            })
            .await?;
        budget.restart();
    }
}
