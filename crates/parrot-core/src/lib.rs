//! Dependency-free turn orchestration behind state, tools, and provider seams.

use std::{future::Future, pin::Pin};

pub mod budget;

pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

#[derive(Debug, Eq, PartialEq)]
pub enum Error<E> {
    Adapter(E),
    MissingTurn,
}

impl<E> From<E> for Error<E> {
    fn from(error: E) -> Self {
        Self::Adapter(error)
    }
}

#[derive(Debug, Eq, PartialEq)]
pub enum Outcome {
    Completed(String),
    Failed(String),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ResponseKind {
    Final,
    ToolCalls,
    TruncatedToolCalls,
    InvalidToolCalls,
}

#[derive(Debug)]
pub struct Response<T> {
    pub value: T,
    pub kind: ResponseKind,
}

#[derive(Debug, Eq, PartialEq)]
pub enum ProviderError<E> {
    RequestTooLarge(E),
    Failed(E),
}

pub type ProviderResult<R, E> = Result<Response<R>, ProviderError<E>>;

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

/// Owns turn effects and opaque values without exposing concrete runtime services.
pub trait State: Send {
    type Selection: Send + Sync;
    type Request: Send;
    type Response: Send + Sync;
    type ToolDefinition: Clone + Send + Sync;
    type Error: Send;
    type ToolContext: ?Sized + Send;
    type ProviderContext: ?Sized + Send;

    /// Borrows only the invocation capabilities required by the tool adapter.
    fn tool_context(&mut self) -> &mut Self::ToolContext;
    /// Borrows only the feedback capabilities required by the provider adapter.
    fn provider_context(&mut self) -> &mut Self::ProviderContext;
    /// Reports whether provider-request limits are disabled for this run.
    fn disables_request_limit(&self) -> bool;
    /// Reports whether a durable turn is already open.
    fn is_turn_open(&self) -> bool;
    /// Reports whether scheduled work should run before opening another turn.
    fn has_scheduled_work(&self) -> bool;
    /// Fails if the owning execution has been cancelled.
    fn check_cancellation(&self) -> Result<(), Self::Error>;
    /// Reports whether the conversation or pending input requires a response.
    fn has_work(&self) -> BoxFuture<'_, Result<bool, Self::Error>>;
    /// Opens a turn and prepares its initial immutable selection.
    fn begin_turn(&mut self) -> BoxFuture<'_, Result<Self::Selection, Self::Error>>;
    /// Restores tool availability after capturing the new turn's tool snapshot.
    fn restore_tool_availability(&mut self) -> BoxFuture<'_, Result<(), Self::Error>>;
    /// Promotes eligible input into the current conversation.
    fn promote_input(&mut self) -> BoxFuture<'_, Result<(), Self::Error>>;
    /// Returns the request allowance for the selected turn.
    fn maximum_requests(&self, selection: &Self::Selection) -> i32;
    /// Consumes a request to enter the interrupted final phase.
    fn take_interruption(&mut self) -> bool;
    /// Prepares context and a provider request for the admitted phase.
    fn prepare_request<'a>(
        &'a mut self,
        selection: &'a Self::Selection,
        definitions: &'a [Self::ToolDefinition],
        phase: budget::TurnRequestPhase,
    ) -> BoxFuture<'a, Result<Self::Request, Self::Error>>;
    /// Attempts to reduce context after a provider rejects an oversized request.
    fn compact_request<'a>(
        &'a mut self,
        selection: &'a Self::Selection,
        definitions: &'a [Self::ToolDefinition],
    ) -> BoxFuture<'a, Result<bool, Self::Error>>;
    /// Records why a malformed or truncated tool response must be retried.
    fn record_retry(
        &mut self,
        reason: RetryReason,
        attempt: i32,
    ) -> BoxFuture<'_, Result<(), Self::Error>>;
    /// Records an assistant response before its pending tools execute.
    fn record_tool_calls<'a>(
        &'a mut self,
        response: &'a Self::Response,
    ) -> BoxFuture<'a, Result<(), Self::Error>>;
    /// Joins accepted mutations and applies completion policies to a final response.
    fn evaluate_completion<'a>(
        &'a mut self,
        selection: &'a Self::Selection,
        response: &'a Self::Response,
    ) -> BoxFuture<'a, Result<CompletionDecision, Self::Error>>;
    /// Settles the accepted final response, closes its turn, and returns the answer.
    fn complete_turn<'a>(
        &'a mut self,
        response: &'a Self::Response,
    ) -> BoxFuture<'a, Result<String, Self::Error>>;
    /// Records request-budget exhaustion and returns its failure message.
    fn fail_runaway(&mut self) -> BoxFuture<'_, Result<String, Self::Error>>;
}

/// Supplies turn-bound tool snapshots and executes calls through a narrow context.
pub trait Tools: Send + Sync {
    type Context: ?Sized + Send;
    type Selection: Send + Sync;
    type Definition: Clone + Send + Sync;
    type Snapshot: Clone + Send + Sync;
    type Error: Send;

    /// Reconciles previously accepted tools before checking for new turn work.
    fn reconcile<'a>(
        &'a self,
        context: &'a mut Self::Context,
    ) -> BoxFuture<'a, Result<(), Self::Error>>;
    /// Captures tools permitted by the selected turn.
    fn snapshot(&self, selection: &Self::Selection) -> Result<Self::Snapshot, Self::Error>;
    /// Borrows provider-visible definitions from an owning snapshot.
    fn definitions<'a>(&self, snapshot: &'a Self::Snapshot) -> &'a [Self::Definition];
    /// Restricts the snapshot to tools available during final settlement.
    fn restrict_for_final_phase(&self, snapshot: &Self::Snapshot) -> Self::Snapshot;
    /// Executes recorded calls under the exact snapshot advertised to the provider.
    fn execute_pending<'a>(
        &'a self,
        context: &'a mut Self::Context,
        selection: &'a Self::Selection,
        snapshot: &'a Self::Snapshot,
    ) -> BoxFuture<'a, Result<(), Self::Error>>;
}

/// Invokes a model while retaining adapter-owned streaming and cleanup behavior.
pub trait Provider: Send + Sync {
    type Context: ?Sized + Send;
    type Request: Send;
    type Response: Send + Sync;
    type Error: Send;

    /// Returns a normalized response or distinguishes an oversized request failure.
    fn invoke<'a>(
        &'a self,
        request: Self::Request,
        context: &'a mut Self::Context,
    ) -> BoxFuture<'a, ProviderResult<Self::Response, Self::Error>>;
}

/// Runs until no response work remains, scheduled work is ready, or a failure occurs.
pub async fn run<S, T, P>(
    state: &mut S,
    tools: &T,
    provider: &P,
) -> Result<Outcome, Error<S::Error>>
where
    S: State,
    T: Tools<
            Context = S::ToolContext,
            Selection = S::Selection,
            Definition = S::ToolDefinition,
            Error = S::Error,
        >,
    P: Provider<
            Context = S::ProviderContext,
            Request = S::Request,
            Response = S::Response,
            Error = S::Error,
        >,
{
    let mut answer = String::new();
    let mut budget = budget::TurnBudget::new(state.disables_request_limit());
    let mut retry_pending = false;
    let mut compacted_oversized = false;
    let mut selected = None;
    loop {
        if !state.is_turn_open() && state.has_scheduled_work() {
            return Ok(Outcome::Completed(answer));
        }
        tools.reconcile(state.tool_context()).await?;
        state.check_cancellation()?;
        if !retry_pending && !state.has_work().await? {
            return Ok(Outcome::Completed(answer));
        }
        if !state.is_turn_open() {
            budget.begin_turn();
            let selection = state.begin_turn().await?;
            let snapshot = tools.snapshot(&selection)?;
            state.restore_tool_availability().await?;
            selected = Some((selection, snapshot));
        }
        state.promote_input().await?;
        let (selection, active) = selected.as_ref().ok_or(Error::MissingTurn)?;
        let Some(phase) =
            budget.admit(state.maximum_requests(selection), state.take_interruption())
        else {
            return Ok(Outcome::Failed(state.fail_runaway().await?));
        };
        let snapshot = if phase.final_phase {
            tools.restrict_for_final_phase(active)
        } else {
            active.clone()
        };
        let definitions = tools.definitions(&snapshot).to_vec();
        let request = state
            .prepare_request(selection, &definitions, phase)
            .await?;
        let attempt = budget.count_request();
        let response = match provider.invoke(request, state.provider_context()).await {
            Ok(response) => response,
            Err(ProviderError::RequestTooLarge(error)) if !compacted_oversized => {
                compacted_oversized = true;
                if !state.compact_request(selection, &definitions).await? {
                    return Err(error.into());
                }
                continue;
            }
            Err(ProviderError::RequestTooLarge(error) | ProviderError::Failed(error)) => {
                return Err(error.into());
            }
        };
        compacted_oversized = false;
        match response.kind {
            ResponseKind::TruncatedToolCalls | ResponseKind::InvalidToolCalls => {
                let reason = match response.kind {
                    ResponseKind::TruncatedToolCalls => RetryReason::TruncatedToolCalls,
                    _ => RetryReason::InvalidToolCalls,
                };
                state.record_retry(reason, attempt).await?;
            }
            ResponseKind::ToolCalls => {
                state.record_tool_calls(&response.value).await?;
                tools
                    .execute_pending(state.tool_context(), selection, &snapshot)
                    .await?;
                if phase.final_phase && !budget.try_extend_settlement() {
                    return Ok(Outcome::Failed(state.fail_runaway().await?));
                }
            }
            ResponseKind::Final => {
                match state
                    .evaluate_completion(selection, &response.value)
                    .await?
                {
                    CompletionDecision::Retry { pending } => {
                        if let Some(pending) = pending {
                            retry_pending = pending;
                        }
                        budget.restart();
                    }
                    CompletionDecision::Complete => {
                        answer = state.complete_turn(&response.value).await?;
                        retry_pending = false;
                        selected = None;
                    }
                }
            }
        }
    }
}
