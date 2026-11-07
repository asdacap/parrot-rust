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
    let mut retry_pending = false;
    let mut compacted_oversized = false;
    let mut selected = None;
    loop {
        if !states.is_turn_open().await && states.has_scheduled_work() {
            return Ok(Outcome::Completed(answer));
        }
        if cancellation.is_cancelled() {
            return Err(Error::cancelled());
        }
        if !retry_pending && !states.has_work().await? {
            return Ok(Outcome::Completed(answer));
        }
        if !states.is_turn_open().await {
            budget.begin_turn();
            let options = states.begin_turn().await?;
            let catalog = tools.capture_catalog().await?;
            states.restore_tool_availability().await?;
            selected = Some((options, catalog));
        }
        states.promote_input().await?;
        let (options, catalog) = selected.as_ref().ok_or_else(Error::missing_turn)?;
        let Some(phase) = budget.admit(options.maximum_requests, states.take_interruption()) else {
            return Ok(Outcome::Failed(states.fail_runaway().await?));
        };
        let definitions = if phase.final_phase {
            &catalog.final_phase
        } else {
            &catalog.normal
        };
        let request = states.prepare_request(definitions, phase).await?;
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
                return Ok(Outcome::Failed(states.fail_runaway().await?));
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
                answer = states.complete_turn(&response).await?;
                retry_pending = false;
                selected = None;
            }
        }
    }
}
