//! Turn orchestration with core-owned provider, tool, and state contracts.

use std::{future::Future, pin::Pin};

pub mod budget;
mod protocol;
pub use protocol::*;

pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

#[derive(Debug, Eq, PartialEq)]
pub enum Outcome {
    Completed(String),
    Failed(String),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RetryReason {
    TruncatedToolCalls,
    InvalidToolCalls,
}

#[derive(Debug, Eq, PartialEq)]
pub enum CompletionDecision {
    Complete,
    Retry { pending: Option<bool> },
}

/// Invokes models using the core protocol while owning transport and stream cleanup.
pub trait Provider: Send + Sync {
    /// Invokes the request and joins stream cleanup before returning, including
    /// after errors or cancellation.
    fn invoke(&self, request: ProviderRequest) -> BoxFuture<'_, Result<ProviderResponse, Error>>;
}

/// Retains executable tools and executes explicitly supplied calls.
pub trait Tools: Send + Sync {
    /// Captures permitted definitions backed by the original executable identities
    /// for this turn. The final catalog is their interruption-enabled subset.
    fn capture_catalog(&self) -> BoxFuture<'_, Result<ToolCatalog, Error>>;
    /// Executes a nonempty ordered prefix of the supplied calls using retained
    /// executable identities, joining all admitted work before returning, including
    /// on failure. Conversation recovery and result recording belong to [`States`].
    fn execute(
        &self,
        request: ToolExecutionRequest,
    ) -> BoxFuture<'_, Result<Vec<ToolResult>, Error>>;
}

/// Owns conversation effects, input readiness, and durable turn transitions.
pub trait States: Send {
    /// Restores saved tool results and records crash errors for missing historical
    /// results without invoking tools, before cancellation or readiness is checked.
    fn restore_tool_results(&mut self) -> BoxFuture<'_, Result<(), Error>>;
    /// Prepares explicitly queued calls from a newly recorded provider response.
    /// Returning none means the fresh batch and result projection are complete.
    fn prepare_tool_execution(
        &mut self,
    ) -> BoxFuture<'_, Result<Option<ToolExecutionRequest>, Error>>;
    /// Durably records and applies the returned prefix before any further execution.
    fn record_tool_results<'a>(
        &'a mut self,
        results: &'a [ToolResult],
    ) -> BoxFuture<'a, Result<(), Error>>;
    /// Reports whether provider-request limits are disabled for this run.
    fn disables_request_limit(&self) -> bool;
    /// Reports whether scheduled work should run before opening another turn.
    fn has_scheduled_work(&self) -> bool;
    /// Fails with typed cancellation if the owning execution was cancelled.
    fn check_cancellation(&self) -> Result<(), Error>;
    /// Reports whether a durable turn is already open.
    fn is_turn_open(&self) -> BoxFuture<'_, bool>;
    /// Reports whether the conversation or pending input requires a response.
    fn has_work(&self) -> BoxFuture<'_, Result<bool, Error>>;
    /// Opens a turn and freezes its model, profile, and authority privately.
    fn begin_turn(&mut self) -> BoxFuture<'_, Result<TurnOptions, Error>>;
    /// Restores tool availability after the core captures the turn's tool catalog.
    fn restore_tool_availability(&mut self) -> BoxFuture<'_, Result<(), Error>>;
    /// Promotes eligible input into the current conversation.
    fn promote_input(&mut self) -> BoxFuture<'_, Result<(), Error>>;
    /// Consumes a request to enter the interrupted final phase.
    fn take_interruption(&mut self) -> bool;
    /// Prepares context and a provider request using exactly the advertised definitions.
    fn prepare_request<'a>(
        &'a mut self,
        definitions: &'a [ToolDefinition],
        phase: budget::TurnRequestPhase,
    ) -> BoxFuture<'a, Result<ProviderRequest, Error>>;
    /// Attempts to reduce context after a provider rejects an oversized request.
    fn compact_request<'a>(
        &'a mut self,
        definitions: &'a [ToolDefinition],
    ) -> BoxFuture<'a, Result<bool, Error>>;
    /// Records why a malformed or truncated tool response must be retried.
    fn record_retry(
        &mut self,
        reason: RetryReason,
        attempt: i32,
    ) -> BoxFuture<'_, Result<(), Error>>;
    /// Durably records the assistant's tool calls before execution begins.
    fn record_tool_calls<'a>(
        &'a mut self,
        response: &'a ProviderResponse,
    ) -> BoxFuture<'a, Result<(), Error>>;
    /// Joins accepted mutations and applies completion policies in their order.
    fn evaluate_completion<'a>(
        &'a mut self,
        response: &'a ProviderResponse,
    ) -> BoxFuture<'a, Result<CompletionDecision, Error>>;
    /// Settles the accepted response and durably closes its turn before publication
    /// completes, returning its answer only after the operation has finished.
    fn complete_turn<'a>(
        &'a mut self,
        response: &'a ProviderResponse,
    ) -> BoxFuture<'a, Result<String, Error>>;
    /// Records request-budget exhaustion and returns its failure message.
    fn fail_runaway(&mut self) -> BoxFuture<'_, Result<String, Error>>;
}

/// Runs the core-owned admission, response validation, retry, and completion policy.
pub async fn run(
    states: &mut dyn States,
    tools: &dyn Tools,
    provider: &dyn Provider,
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
        states.restore_tool_results().await?;
        states.check_cancellation()?;
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
        if response.finish_reason == "length" && !response.tool_calls.is_empty() {
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
            execute_tools(states, tools).await?;
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

async fn execute_tools(states: &mut dyn States, tools: &dyn Tools) -> Result<(), Error> {
    while let Some(request) = states.prepare_tool_execution().await? {
        let results = tools.execute(request).await?;
        if results.is_empty() {
            return Err(Error::failed("tool execution returned no results"));
        }
        states.record_tool_results(&results).await?;
    }
    Ok(())
}
