//! Retained invocation-tool routing. Status observes the already-borrowed drain.
use crate::composition::catalog;
use parrot_domain::{
    contracts::{
        async_ops::{BoxFuture, Cancellation, OperationError, OperationResult},
        security::{AccessSnapshot, InvocationScope},
    },
    values::context::ContextSnapshot,
};
use parrot_runtime::{
    agent_turn::services::{TurnError, TurnSelection},
    tool_settlement::{
        BorrowedTool, BorrowedToolState, SharedSecurity, ToolFailure, ToolInvocation, ToolMetadata,
        ToolOutput,
    },
};
use parrot_tools::session::{Failure, FailureKind};
use std::sync::Arc;
use tokio::sync::{mpsc, oneshot};

fn failure(message: impl Into<String>) -> Failure {
    Failure::new(FailureKind::InvalidOperation, message)
}
fn turn_failure(error: TurnError) -> OperationError<Failure> {
    if error.is_cancelled() {
        OperationError::Cancelled
    } else {
        OperationError::Failed(failure(error.to_string()))
    }
}

pub enum InlineRequest {
    Estimate {
        reply: oneshot::Sender<OperationResult<ContextSnapshot, Failure>>,
    },
}
#[derive(Clone)]
pub struct InlineRoute {
    pub sender: mpsc::Sender<InlineRequest>,
}

/// Owned selection handed to the retained tool. Borrowed resources stay in its invocation.
pub struct InvocationSelection<S: AccessSnapshot> {
    pub turn: TurnSelection<S>,
    pub route: Option<InlineRoute>,
}
impl<S: AccessSnapshot> Clone for InvocationSelection<S> {
    fn clone(&self) -> Self {
        Self {
            turn: self.turn.clone(),
            route: self.route.clone(),
        }
    }
}
impl<S: AccessSnapshot> InvocationSelection<S> {
    pub fn external(turn: TurnSelection<S>) -> Self {
        Self { turn, route: None }
    }
    /// Observes the current epoch, conversation, and permitted tools during a borrowed invocation.
    pub fn estimate_context<'a>(
        &'a self,
        cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, OperationResult<ContextSnapshot, Failure>> {
        Box::pin(async move {
            cancellation
                .check()
                .map_err(|_| OperationError::Cancelled)?;
            let route = self.route.as_ref().ok_or_else(|| {
                OperationError::Failed(failure(
                    "Context estimation requires an active borrowed invocation.",
                ))
            })?;
            let (reply, response) = oneshot::channel();
            route
                .sender
                .send(InlineRequest::Estimate { reply })
                .await
                .map_err(|_| {
                    OperationError::Failed(failure("The borrowed invocation has ended."))
                })?;
            response.await.map_err(|_| {
                OperationError::Failed(failure("The borrowed invocation has ended."))
            })?
        })
    }
}

pub struct RetainedCompactTool<S: AccessSnapshot + 'static> {
    tool: Arc<catalog::RetainedTool<SharedSecurity<S>, InvocationSelection<S>>>,
    metadata: ToolMetadata,
}
impl<S: AccessSnapshot + 'static> RetainedCompactTool<S> {
    pub fn new(
        factory: &catalog::Factory<SharedSecurity<S>, InvocationSelection<S>>,
    ) -> Result<Self, Failure> {
        Self::from_factory(factory, "compact_context")
    }
    pub fn status(
        factory: &catalog::Factory<SharedSecurity<S>, InvocationSelection<S>>,
    ) -> Result<Self, Failure> {
        Self::from_factory(factory, "status")
    }
    fn from_factory(
        factory: &catalog::Factory<SharedSecurity<S>, InvocationSelection<S>>,
        name: &str,
    ) -> Result<Self, Failure> {
        if factory.name() != name {
            return Err(failure(format!(
                "The borrowed invocation adapter requires the {name} factory."
            )));
        }
        Ok(Self {
            tool: factory.create(),
            metadata: ToolMetadata {
                name: factory.name().into(),
                aliases: factory
                    .aliases()
                    .iter()
                    .map(|name| (*name).into())
                    .collect(),
            },
        })
    }
}
impl<S: AccessSnapshot + 'static> BorrowedTool<S> for RetainedCompactTool<S> {
    fn definition(&self) -> &ToolMetadata {
        &self.metadata
    }
    fn enabled_after_interruption(&self) -> bool {
        self.tool.enabled_after_interruption()
    }
    fn invoke<'a>(
        &'a self,
        invocation: ToolInvocation,
        state: BorrowedToolState<'a, S>,
        cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, OperationResult<ToolOutput, ToolFailure>> {
        Box::pin(async move {
            let (sender, mut requests) = mpsc::channel::<InlineRequest>(1);
            let scoped = InvocationSelection {
                turn: state.selection.clone(),
                route: Some(InlineRoute { sender }),
            };
            let selected = catalog::Selection {
                requested_model: state
                    .selection
                    .model
                    .requested_selector
                    .value
                    .as_str()
                    .into(),
                scoped,
            };
            let security = SharedSecurity(state.selection.security.clone());
            let call = catalog::Invocation {
                call_id: invocation.call_id,
                assistant_sequence: invocation.assistant_sequence,
                arguments_json: invocation.arguments_json,
                prompt_templates: invocation.prompt_templates,
                image_budget: None,
            };
            let invoke: BoxFuture<'_, OperationResult<catalog::Output, catalog::Error>> = self
                .tool
                .invoke(call, InvocationScope::new(security, selected, cancellation));
            tokio::pin!(invoke);
            loop {
                tokio::select! {
                    result = &mut invoke => {
                        return result.map(|output| ToolOutput::text(output.text)).map_err(|error| match error {
                            OperationError::Cancelled => OperationError::Cancelled,
                            OperationError::Failed(error) => OperationError::Failed(ToolFailure::new(error.to_string(), parrot_storage::diagnostics::FailureKind::InvalidOperation)),
                        });
                    }
                    Some(InlineRequest::Estimate { reply }) = requests.recv() => {
                        let result = state.resources.context.estimate_context(
                            &state.selection.model,
                            state.definitions,
                            state.resources.conversation,
                            state.resources.prompt,
                            cancellation,
                        ).await
                            .map_err(|error| turn_failure(TurnError::Context(error)));
                        let _ = reply.send(result);
                    }
                }
            }
        })
    }
}
