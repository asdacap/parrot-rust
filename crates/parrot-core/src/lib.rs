//! Turn orchestration with core-owned provider, tool, and state contracts.

pub mod budget;
mod protocol;
pub use protocol::*;

/// Runs the core-owned admission, response validation, retry, and completion policy.
pub async fn run<'a>(
    states: &mut dyn States,
    tools: &'a dyn Tools,
    provider: &dyn Provider,
    cancellation: CancellationToken<'a>,
) -> Result<Outcome, Error> {
    let mut answer = String::new();
    let mut budget = budget::TurnBudget::new(states.disables_request_limit());
    loop {
        match run_turn(states, tools, provider, &mut budget, cancellation).await? {
            Some(Outcome::Completed(completed)) => answer = completed,
            Some(failed @ Outcome::Failed(_)) => return Ok(failed),
            None => return Ok(Outcome::Completed(answer)),
        }
    }
}

/// Runs at most one completed turn using the caller-owned run-scope budget.
/// Returns none when the factual conversation and pending input need no response.
pub async fn run_turn<'a>(
    states: &mut dyn States,
    tools: &'a dyn Tools,
    provider: &dyn Provider,
    budget: &mut budget::TurnBudget,
    cancellation: CancellationToken<'a>,
) -> Result<Option<Outcome>, Error> {
    let mut retry_pending = false;
    let mut compacted_oversized = false;
    let mut selected = None;
    loop {
        if cancellation.is_cancelled() {
            return Err(Error::cancelled());
        }
        if !retry_pending {
            let turn = states.get_last_turn().await?;
            if !needs_response(turn.as_ref()) && states.get_pending_message().await?.is_none() {
                return Ok(None);
            }
        }
        if !states.is_turn_open().await {
            budget.begin_turn();
            let options = states.begin_turn().await?;
            let catalog = tools.capture_catalog().await?;
            states.restore_tool_availability().await?;
            selected = Some((options, catalog));
        }
        let turn = states.get_last_turn().await?;
        let admission = if needs_response(turn.as_ref()) {
            InputAdmission::SteersOnly
        } else {
            InputAdmission::SteersOrNextQueued
        };
        states.promote_input(admission).await?;
        let (options, catalog) = selected.as_ref().ok_or_else(Error::missing_turn)?;
        let Some(phase) = budget.admit(options.maximum_requests, states.take_interruption()) else {
            return Ok(Some(Outcome::Failed(states.fail_runaway().await?)));
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
                if !states.compact_request(definitions).await? {
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
                return Ok(Some(Outcome::Failed(states.fail_runaway().await?)));
            }
            continue;
        }
        match states.evaluate_completion(&response).await? {
            CompletionDecision::Retry { pending } => {
                if let Some(pending) = pending {
                    retry_pending = pending;
                }
                budget.restart();
            }
            CompletionDecision::Complete => {
                return Ok(Some(Outcome::Completed(
                    states.complete_turn(&response).await?,
                )));
            }
        }
    }
}

fn needs_response(turn: Option<&ConversationTurn>) -> bool {
    turn.is_some_and(|turn| {
        turn.result.is_none()
            && matches!(
                turn.messages
                    .iter()
                    .rev()
                    .find(|message| message.role != Role::System)
                    .map(|message| message.role),
                Some(Role::User | Role::Tool)
            )
    })
}
