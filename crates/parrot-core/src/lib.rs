//! Turn orchestration with core-owned provider, tool, and state contracts.

pub mod budget;
pub mod compaction;
mod protocol;
pub use protocol::*;

/// Runs admission, response validation, retry, and completion using the caller-owned budget.
/// The supplied settings apply to every execution turn drained by this call.
pub async fn run<'a>(
    states: &mut dyn States,
    tools: &'a dyn Tools,
    provider: &dyn Provider,
    options: &TurnOptions,
    budget: &mut budget::TurnBudget,
    cancellation: CancellationToken<'a>,
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
        match run_turn(states, tools, provider, options, budget, cancellation).await? {
            Outcome::Completed(completed) => answer = completed,
            Outcome::BudgetExhausted => return Ok(Outcome::BudgetExhausted),
        }
    }
}

/// Runs one active execution turn using supplied settings and tools captured once.
pub async fn run_turn<'a>(
    states: &mut dyn States,
    tools: &'a dyn Tools,
    provider: &dyn Provider,
    options: &TurnOptions,
    budget: &mut budget::TurnBudget,
    cancellation: CancellationToken<'a>,
) -> Result<Outcome, Error> {
    if cancellation.is_cancelled() {
        return Err(Error::cancelled());
    }
    budget.begin_turn();
    let catalog = tools.capture_catalog().await?;
    let mut compacted_oversized = false;
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
        states.promote_input(admission).await?;
        let Some(phase) = budget.admit(options.maximum_requests) else {
            return Ok(Outcome::BudgetExhausted);
        };
        let definitions = if phase.final_phase {
            &catalog.final_phase
        } else {
            &catalog.normal
        };
        let context = states.prepare_context(definitions, phase).await?;
        let messages = states.read_messages().await?;
        let request = ProviderRequest {
            model: options.model.clone(),
            instructions: context.instructions,
            messages,
            tools: definitions.to_vec(),
            max_output_tokens: context.max_output_tokens,
            reasoning: options.reasoning.clone(),
            provider_preferences: options.provider_preferences.clone(),
            include_router_metadata: options.include_router_metadata,
        };
        let attempt = budget.count_request();
        let response = match provider.invoke(request).await {
            Ok(response) => response,
            Err(error) if error.kind == ErrorKind::RequestTooLarge && !compacted_oversized => {
                compacted_oversized = true;
                let compacted = states
                    .prepare_compaction(definitions)
                    .await?
                    .run(cancellation)
                    .await;
                if !states.record_compaction(compacted).await? {
                    return Err(error);
                }
                continue;
            }
            Err(error) => return Err(error),
        };
        compacted_oversized = false;
        if response.finish_reason == FinishReason::Length && !response.tool_calls.is_empty() {
            states
                .record_retry(RetryReason::TruncatedToolCalls, attempt)
                .await?;
            continue;
        }
        if response
            .tool_calls
            .iter()
            .any(|call| serde_json::from_str::<serde_json::Value>(&call.arguments_json).is_err())
        {
            states
                .record_retry(RetryReason::InvalidToolCalls, attempt)
                .await?;
            continue;
        }
        if !response.tool_calls.is_empty() {
            states.record_tool_calls(&response).await?;
            let results = tools
                .execute(response.tool_calls.clone(), cancellation)
                .await?;
            if results.is_empty() {
                return Err(Error::failed("tool execution returned no results"));
            }
            states.record_tool_results(&results).await?;
            if phase.final_phase && !budget.try_extend_settlement() {
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
