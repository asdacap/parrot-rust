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
    /// Uses the core-assigned attempt, estimate, and phase without replacing them.
    /// Every observed completion calibrates input usage before its durable usage
    /// record is awaited, including intermediate completions before a later failure.
    /// Stream cleanup is joined before returning, also after errors or cancellation.
    fn invoke(
        &self,
        request: ProviderRequest,
        context: ProviderContext,
    ) -> BoxFuture<'_, Result<ProviderResponse, Error>>;
}

/// Retains executable tools and settles calls recorded through [`States`].
pub trait Tools: Send + Sync {
    /// Reconciles accepted pending calls and applies their durable settlements to
    /// the conversation before the core checks cancellation or response readiness.
    fn reconcile_pending(&self) -> BoxFuture<'_, Result<(), Error>>;
    /// Captures permitted definitions backed by the original executable identities
    /// for this turn. The final catalog is their interruption-enabled subset.
    fn capture_catalog(&self) -> BoxFuture<'_, Result<ToolCatalog, Error>>;
    /// Settles the recorded calls using the retained catalog for the advertised
    /// phase. Success means durable terminals are applied and accepted work joined;
    /// definitions must never be used to reconstruct executable tool identities.
    fn execute_pending(&self, context: ToolContext) -> BoxFuture<'_, Result<(), Error>>;
}

/// Owns conversation effects, input readiness, and durable turn transitions.
pub trait States: Send {
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
    ) -> BoxFuture<'a, Result<PreparedRequest, Error>>;
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
        tools.reconcile_pending().await?;
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
        let prepared = states.prepare_request(definitions, phase).await?;
        let attempt = budget.count_request();
        let context = ProviderContext {
            attempt,
            estimated_input_tokens: prepared.estimated_input_tokens,
            phase,
        };
        let response = match provider.invoke(prepared.request, context).await {
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
            tools
                .execute_pending(ToolContext {
                    phase: if phase.final_phase {
                        ToolPhase::Final
                    } else {
                        ToolPhase::Normal
                    },
                })
                .await?;
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
