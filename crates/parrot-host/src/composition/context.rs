//! Retained context-tool routing. Status observes the already-borrowed drain.
use parrot_domain::{
    contracts::{
        async_ops::{BoxFuture, Cancellation, OperationError, OperationResult},
        security::{AccessSnapshot, InvocationContext},
        tools::Tool,
    },
    values::{context::ContextSnapshot, progress::ProgressUpdate},
};
use parrot_runtime::{
    agent_turn::services::{TurnError, TurnSelection},
    compaction::EpochInput,
    tool_settlement::{
        BorrowedTool, BorrowedToolContext, SharedSecurity, ToolFailure, ToolInvocation,
        ToolMetadata, ToolOutput,
    },
};
use parrot_tools::{
    catalog,
    session::{Failure, FailureKind, SessionServices},
};
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

enum InlineRequest {
    Estimate {
        reply: oneshot::Sender<OperationResult<ContextSnapshot, Failure>>,
    },
}
#[derive(Clone)]
struct InlineRoute {
    sender: mpsc::Sender<InlineRequest>,
}

/// Owned selection handed to the retained tool. Borrowed resources stay in its invocation.
pub struct ContextSelection<S: AccessSnapshot> {
    pub turn: TurnSelection<S>,
    route: Option<InlineRoute>,
}
impl<S: AccessSnapshot> Clone for ContextSelection<S> {
    fn clone(&self) -> Self {
        Self {
            turn: self.turn.clone(),
            route: self.route.clone(),
        }
    }
}
impl<S: AccessSnapshot> ContextSelection<S> {
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
                    OperationError::Failed(failure("The context invocation has ended."))
                })?;
            response
                .await
                .map_err(|_| OperationError::Failed(failure("The context invocation has ended.")))?
        })
    }
}

/// Delegates retained session operations while status uses the invocation's borrowed context.
pub struct ContextServices<S: AccessSnapshot, O: SessionServices> {
    ancillary: Arc<O>,
    marker: std::marker::PhantomData<fn() -> S>,
}
impl<S: AccessSnapshot, O: SessionServices> ContextServices<S, O> {
    pub fn new(ancillary: Arc<O>) -> Self {
        Self {
            ancillary,
            marker: std::marker::PhantomData,
        }
    }
}
impl<S, O> SessionServices for ContextServices<S, O>
where
    S: AccessSnapshot + 'static,
    O: SessionServices<Security = SharedSecurity<S>, Selection = ContextSelection<S>>,
{
    type Security = SharedSecurity<S>;
    type Selection = ContextSelection<S>;
    fn set_reminder<'a>(
        &'a self,
        title: &'a str,
        description: &'a str,
        cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, OperationResult<(), Failure>> {
        self.ancillary
            .set_reminder(title, description, cancellation)
    }
    fn clear_reminder<'a>(
        &'a self,
        title: &'a str,
        cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, OperationResult<bool, Failure>> {
        self.ancillary.clear_reminder(title, cancellation)
    }
    fn reminder_titles(&self) -> BoxFuture<'_, Vec<String>> {
        self.ancillary.reminder_titles()
    }
    fn update_progress<'a>(
        &'a self,
        update: ProgressUpdate,
        cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, OperationResult<bool, Failure>> {
        self.ancillary.update_progress(update, cancellation)
    }
    fn runtime_status<'a>(
        &'a self,
        selection: &'a ContextSelection<S>,
        cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, OperationResult<String, Failure>> {
        self.ancillary.runtime_status(selection, cancellation)
    }
    fn statistics_status<'a>(
        &'a self,
        selection: &'a ContextSelection<S>,
        cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, OperationResult<String, Failure>> {
        self.ancillary.statistics_status(selection, cancellation)
    }
}

type RetainedSessionTool<S> = dyn Tool<
        Definition = parrot_domain::values::tools::ConfiguredToolDefinition,
        Invocation = parrot_tools::session::Invocation,
        Selection = ContextSelection<S>,
        Security = SharedSecurity<S>,
        Output = parrot_tools::session::Output,
        Error = Failure,
    >;
enum CompactToolHandle<S: AccessSnapshot + 'static> {
    Catalog(Arc<catalog::RetainedTool<SharedSecurity<S>, ContextSelection<S>>>),
    Session(Arc<RetainedSessionTool<S>>),
}
pub struct RetainedCompactTool<S: AccessSnapshot + 'static> {
    tool: CompactToolHandle<S>,
    metadata: ToolMetadata,
}
impl<S: AccessSnapshot + 'static> RetainedCompactTool<S> {
    pub fn new(
        factory: &catalog::Factory<SharedSecurity<S>, ContextSelection<S>>,
    ) -> Result<Self, Failure> {
        Self::from_factory(factory, "compact_context")
    }
    pub fn status(
        factory: &catalog::Factory<SharedSecurity<S>, ContextSelection<S>>,
    ) -> Result<Self, Failure> {
        Self::from_factory(factory, "status")
    }
    fn from_factory(
        factory: &catalog::Factory<SharedSecurity<S>, ContextSelection<S>>,
        name: &str,
    ) -> Result<Self, Failure> {
        if factory.name() != name {
            return Err(failure(format!(
                "The borrowed context adapter requires the {name} factory."
            )));
        }
        Ok(Self {
            tool: CompactToolHandle::Catalog(factory.create()),
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
    pub fn from_session<O>(
        tool: Arc<parrot_tools::session::SessionTool<O>>,
    ) -> Result<Self, Failure>
    where
        O: SessionServices<Security = SharedSecurity<S>, Selection = ContextSelection<S>> + 'static,
    {
        Self::from_session_kind(tool, "compact_context")
    }
    pub fn from_status_session<O>(
        tool: Arc<parrot_tools::session::SessionTool<O>>,
    ) -> Result<Self, Failure>
    where
        O: SessionServices<Security = SharedSecurity<S>, Selection = ContextSelection<S>> + 'static,
    {
        Self::from_session_kind(tool, "status")
    }
    fn from_session_kind<O>(
        tool: Arc<parrot_tools::session::SessionTool<O>>,
        name: &str,
    ) -> Result<Self, Failure>
    where
        O: SessionServices<Security = SharedSecurity<S>, Selection = ContextSelection<S>> + 'static,
    {
        if tool.name() != name {
            return Err(failure(format!(
                "The borrowed context adapter requires the {name} tool."
            )));
        }
        Ok(Self {
            tool: CompactToolHandle::Session(tool),
            metadata: ToolMetadata {
                name: name.into(),
                aliases: vec![],
            },
        })
    }
}
impl<S: AccessSnapshot + 'static> BorrowedTool<S> for RetainedCompactTool<S> {
    fn definition(&self) -> &ToolMetadata {
        &self.metadata
    }
    fn enabled_after_interruption(&self) -> bool {
        match &self.tool {
            CompactToolHandle::Catalog(tool) => tool.enabled_after_interruption(),
            CompactToolHandle::Session(tool) => tool.enabled_after_interruption(),
        }
    }
    fn invoke<'a>(
        &'a self,
        invocation: ToolInvocation,
        context: BorrowedToolContext<'a, S>,
        cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, OperationResult<ToolOutput, ToolFailure>> {
        Box::pin(async move {
            let (sender, mut requests) = mpsc::channel::<InlineRequest>(1);
            let scoped = ContextSelection {
                turn: context.selection.clone(),
                route: Some(InlineRoute { sender }),
            };
            let selected = catalog::Selection {
                requested_model: context
                    .selection
                    .model
                    .requested_selector
                    .value
                    .as_str()
                    .into(),
                scoped,
            };
            let security = SharedSecurity(context.selection.security.clone());
            let call = catalog::Invocation {
                call_id: invocation.call_id,
                assistant_sequence: invocation.assistant_sequence,
                arguments_json: invocation.arguments_json,
                prompt_templates: invocation.prompt_templates,
                image_budget: None,
            };
            let invoke: BoxFuture<'_, OperationResult<catalog::Output, catalog::Error>> =
                match &self.tool {
                    CompactToolHandle::Catalog(tool) => tool.invoke(
                        call,
                        InvocationContext::new(security, selected, cancellation),
                    ),
                    CompactToolHandle::Session(tool) => Box::pin(async move {
                        tool.invoke(
                            call.into(),
                            InvocationContext::new(security, selected.scoped, cancellation),
                        )
                        .await
                        .map(Into::into)
                        .map_err(|error| match error {
                            OperationError::Cancelled => OperationError::Cancelled,
                            OperationError::Failed(error) => {
                                OperationError::Failed(catalog::Error::Session(error))
                            }
                        })
                    }),
                };
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
                        let result = context.resources.context.estimate_context(EpochInput {
                            selection: &context.selection.model,
                            tools: context.definitions,
                            conversation: context.resources.conversation,
                            agent_state: context.resources.agent_state,
                            prompt: context.resources.prompt,
                            sessions: context.resources.sessions,
                            status: context.resources.status,
                            events: context.resources.events,
                        }, cancellation).await
                            .map_err(|error| turn_failure(TurnError::Context(error)));
                        let _ = reply.send(result);
                    }
                }
            }
        })
    }
}
