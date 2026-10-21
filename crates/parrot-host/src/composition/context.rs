//! Retained context-tool routing. Only the invocation owns a route to the
//! already-borrowed drain; external compaction uses the session's scheduled work.
use parrot_domain::{
    contracts::{
        async_ops::{BoxFuture, Cancellation, OperationError, OperationResult},
        runtime::SessionControl,
        security::{AccessSnapshot, InvocationContext},
        tools::Tool,
    },
    values::context::{ContextSize, ContextSnapshot},
};
use parrot_foundation::template_catalog::PromptTemplateCatalog;
use parrot_provider::{routing::aliases::ResolvedModelSelection, sessions};
use parrot_runtime::{
    agent_turn::{
        engine::RuntimeBackend,
        lifecycle::AgentSession,
        services::{TurnError, TurnSelection, TurnTools},
    },
    compaction::{CompactionPlanner, ContextCompactionResult, EpochInput, provider_message},
    tool_settlement::{
        BorrowedTool, BorrowedToolContext, SharedSecurity, ToolFailure, ToolInvocation,
        ToolMetadata, ToolOutput,
    },
};
use parrot_storage::{conversation::AgentConversation, events::NeverCancelled};
use parrot_tools::{
    catalog,
    session::{Compaction, Failure, FailureKind, SessionServices},
};
use std::sync::{Arc, Mutex, OnceLock, Weak};
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
fn compact_result(result: ContextCompactionResult) -> Compaction {
    Compaction {
        reduced: result.reduced,
        context: result.context,
    }
}

enum InlineRequest {
    Compact {
        target: Option<ContextSize>,
        reply: oneshot::Sender<OperationResult<Compaction, Failure>>,
    },
    Estimate {
        reply: oneshot::Sender<OperationResult<ContextSnapshot, Failure>>,
    },
}
#[derive(Clone)]
struct InlineRoute {
    sender: mpsc::Sender<InlineRequest>,
    estimate: Arc<Mutex<Option<EstimateSnapshot>>>,
}

/// Owned selection handed to the existing retained Tool. No borrowed resource
/// escapes through the domain's owned + 'static invocation contract.
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
    /// Observes the actual current epoch, conversation and permitted tool list
    /// while the retained invocation owns the drain's borrowed resources.
    /// External protocol observations have a separate runtime-owned boundary.
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

struct EstimateSnapshot {
    planner: Arc<CompactionPlanner>,
    selected: ResolvedModelSelection,
    instructions: String,
    tools: Vec<sessions::Tool>,
    messages: Vec<sessions::Message>,
    templates: Arc<PromptTemplateCatalog>,
}
impl EstimateSnapshot {
    fn estimate(&self, call_id: &str, result: &str) -> Result<ContextSnapshot, Failure> {
        if call_id.trim().is_empty() {
            return Err(Failure::new(
                FailureKind::Argument,
                "toolCallId must not be empty or whitespace",
            ));
        }
        let rendered = self
            .templates
            .render(
                "tool-result.text",
                &[("value".into(), result.into())],
                &NeverCancelled,
            )
            .map_err(|error| failure(error.to_string()))?;
        let mut messages = self.messages.clone();
        messages.push(sessions::Message::tool_result(call_id, rendered));
        self.planner
            .estimate_selected_context(&self.selected, &self.instructions, &self.tools, &messages)
            .map_err(|error| failure(error.to_string()))
    }
}

enum CompactRoute {
    Inline(InlineRoute),
    External(Arc<dyn Cancellation>),
}
/// The frozen control's concrete request. An inline capability can only be
/// constructed while a borrowed invocation is being driven below.
pub struct CompactRequest {
    target: Option<ContextSize>,
    route: CompactRoute,
}
impl CompactRequest {
    pub fn external(target: Option<ContextSize>, cancellation: Arc<dyn Cancellation>) -> Self {
        Self {
            target,
            route: CompactRoute::External(cancellation),
        }
    }
}

type ExternalCompact = dyn Fn(
        Option<ContextSize>,
        Arc<dyn Cancellation>,
    ) -> BoxFuture<'static, OperationResult<Compaction, Failure>>
    + Send
    + Sync;

/// Adds concrete compact routing to an existing scoped SessionControl. All
/// unrelated operations retain the injected control's types and behavior.
pub struct ContextControl<C: SessionControl> {
    ancillary: Arc<C>,
    external: OnceLock<Arc<ExternalCompact>>,
}
impl<C: SessionControl> ContextControl<C> {
    pub fn new(ancillary: Arc<C>) -> Self {
        Self {
            ancillary,
            external: OnceLock::new(),
        }
    }
    /// Bind once after the real session is constructed. The weak edge avoids a
    /// session -> retained tool -> control -> session ownership cycle.
    pub fn bind_session<S, T>(
        &self,
        session: Weak<AgentSession<RuntimeBackend<S, T>>>,
    ) -> Result<(), Failure>
    where
        S: AccessSnapshot + 'static,
        T: TurnTools<S> + 'static,
        T::Snapshot: 'static,
    {
        self.external
            .set(Arc::new(move |target, cancellation| {
                let session = session.clone();
                Box::pin(async move {
                    let session = session.upgrade().ok_or_else(|| {
                        OperationError::Failed(failure("The session has closed."))
                    })?;
                    session
                        .compact_context(target, cancellation)
                        .await
                        .map(compact_result)
                        .map_err(turn_failure)
                })
            }))
            .map_err(|_| failure("The context control is already bound."))
    }
    pub fn is_bound(&self) -> bool {
        self.external.get().is_some()
    }
}
impl<C> SessionControl for ContextControl<C>
where
    C: SessionControl<Error = Failure>,
{
    type CheckpointRequest = C::CheckpointRequest;
    type CheckpointResult = C::CheckpointResult;
    type ReminderRequest = C::ReminderRequest;
    type ReminderResult = C::ReminderResult;
    type ClearReminderRequest = C::ClearReminderRequest;
    type ClearReminderResult = C::ClearReminderResult;
    type CompactRequest = CompactRequest;
    type CompactResult = Compaction;
    type Status = C::Status;
    type Error = Failure;

    fn checkpoint<'a>(
        &'a self,
        request: Self::CheckpointRequest,
        cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, OperationResult<Self::CheckpointResult, Failure>> {
        self.ancillary.checkpoint(request, cancellation)
    }
    fn compact<'a>(
        &'a self,
        request: CompactRequest,
        cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, OperationResult<Compaction, Failure>> {
        Box::pin(async move {
            cancellation
                .check()
                .map_err(|_| OperationError::Cancelled)?;
            match request.route {
                CompactRoute::Inline(route) => {
                    let (reply, response) = oneshot::channel();
                    route
                        .sender
                        .send(InlineRequest::Compact {
                            target: request.target,
                            reply,
                        })
                        .await
                        .map_err(|_| {
                            OperationError::Failed(failure("The compact invocation has ended."))
                        })?;
                    // The borrowed invocation owns and joins the mutation. Do not
                    // abandon its response after admission on caller cancellation.
                    response.await.map_err(|_| {
                        OperationError::Failed(failure("The compact invocation has ended."))
                    })?
                }
                CompactRoute::External(caller) => {
                    let external = self.external.get().ok_or_else(|| {
                        OperationError::Failed(failure(
                            "The context control has not been bound to its session.",
                        ))
                    })?;
                    external(request.target, caller).await
                }
            }
        })
    }
    fn set_reminder(
        &self,
        request: Self::ReminderRequest,
    ) -> Result<Self::ReminderResult, Failure> {
        self.ancillary.set_reminder(request)
    }
    fn clear_reminder(
        &self,
        request: Self::ClearReminderRequest,
    ) -> Result<Self::ClearReminderResult, Failure> {
        self.ancillary.clear_reminder(request)
    }
    fn status(&self) -> Result<Self::Status, Failure> {
        self.ancillary.status()
    }
}

/// The retained SessionTool uses this service object for its entire scope.
/// Noncompact operations are delegated without defaults or placeholder results.
pub struct ContextServices<S: AccessSnapshot, O: SessionServices, C: SessionControl> {
    ancillary: Arc<O>,
    control: Arc<ContextControl<C>>,
    marker: std::marker::PhantomData<fn() -> S>,
}
impl<S: AccessSnapshot, O: SessionServices, C: SessionControl> ContextServices<S, O, C> {
    pub fn new(ancillary: Arc<O>, control: Arc<ContextControl<C>>) -> Self {
        Self {
            ancillary,
            control,
            marker: std::marker::PhantomData,
        }
    }
    pub fn control(&self) -> &Arc<ContextControl<C>> {
        &self.control
    }
}
impl<S, O, C> SessionServices for ContextServices<S, O, C>
where
    S: AccessSnapshot + 'static,
    O: SessionServices<Security = SharedSecurity<S>, Selection = ContextSelection<S>>,
    C: SessionControl<Error = Failure>,
{
    type Security = SharedSecurity<S>;
    type Selection = ContextSelection<S>;
    fn checkpoint<'a>(
        &'a self,
        title: &'a str,
        assistant_sequence: i64,
        call_id: &'a str,
    ) -> BoxFuture<'a, Result<(), Failure>> {
        self.ancillary
            .checkpoint(title, assistant_sequence, call_id)
    }
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
    fn compact<'a>(
        &'a self,
        selection: &'a ContextSelection<S>,
        target: Option<ContextSize>,
        cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, OperationResult<Compaction, Failure>> {
        Box::pin(async move {
            let route = selection.route.clone().ok_or_else(|| OperationError::Failed(failure(
                "A retained compact tool requires its active invocation route; external controls use CompactRequest::external.",
            )))?;
            SessionControl::compact(
                self.control.as_ref(),
                CompactRequest {
                    target,
                    route: CompactRoute::Inline(route),
                },
                cancellation,
            )
            .await
        })
    }
    fn estimate_after_tool_result(
        &self,
        selection: &ContextSelection<S>,
        call_id: &str,
        rendered_result: &str,
    ) -> Result<ContextSnapshot, Failure> {
        let route = selection
            .route
            .as_ref()
            .ok_or_else(|| failure("The compact invocation has ended."))?;
        let estimate = route.estimate.lock().expect("context estimate poisoned");
        estimate
            .as_ref()
            .ok_or_else(|| failure("The compact invocation has no estimate."))?
            .estimate(call_id, rendered_result)
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
/// A retained tool handle is adapted, never recreated per invocation.
/// The planner must be constructed with the SAME settings/profiles used for the
/// scope's ContextController; both perform the same pure estimate algorithm.
pub struct RetainedCompactTool<S: AccessSnapshot + 'static> {
    tool: CompactToolHandle<S>,
    metadata: ToolMetadata,
    planner: Arc<CompactionPlanner>,
    templates: Arc<PromptTemplateCatalog>,
}
impl<S: AccessSnapshot + 'static> RetainedCompactTool<S> {
    pub fn new(
        factory: &catalog::Factory<SharedSecurity<S>, ContextSelection<S>>,
        planner: Arc<CompactionPlanner>,
        templates: Arc<PromptTemplateCatalog>,
    ) -> Result<Self, Failure> {
        Self::from_factory(factory, "compact_context", planner, templates)
    }
    /// The status Tool also observes the current in-drain context and must not
    /// attempt to re-lock the backend that is awaiting its retained invocation.
    pub fn status(
        factory: &catalog::Factory<SharedSecurity<S>, ContextSelection<S>>,
        planner: Arc<CompactionPlanner>,
        templates: Arc<PromptTemplateCatalog>,
    ) -> Result<Self, Failure> {
        Self::from_factory(factory, "status", planner, templates)
    }
    fn from_factory(
        factory: &catalog::Factory<SharedSecurity<S>, ContextSelection<S>>,
        name: &str,
        planner: Arc<CompactionPlanner>,
        templates: Arc<PromptTemplateCatalog>,
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
            planner,
            templates,
        })
    }
    /// Also accepts an already-retained SessionTool before catalog erasure.
    pub fn from_session<O>(
        tool: Arc<parrot_tools::session::SessionTool<O>>,
        planner: Arc<CompactionPlanner>,
        templates: Arc<PromptTemplateCatalog>,
    ) -> Result<Self, Failure>
    where
        O: SessionServices<Security = SharedSecurity<S>, Selection = ContextSelection<S>> + 'static,
    {
        Self::from_session_kind(tool, "compact_context", planner, templates)
    }
    /// Accepts the same retained status SessionTool before catalog erasure.
    pub fn from_status_session<O>(
        tool: Arc<parrot_tools::session::SessionTool<O>>,
        planner: Arc<CompactionPlanner>,
        templates: Arc<PromptTemplateCatalog>,
    ) -> Result<Self, Failure>
    where
        O: SessionServices<Security = SharedSecurity<S>, Selection = ContextSelection<S>> + 'static,
    {
        Self::from_session_kind(tool, "status", planner, templates)
    }
    fn from_session_kind<O>(
        tool: Arc<parrot_tools::session::SessionTool<O>>,
        name: &str,
        planner: Arc<CompactionPlanner>,
        templates: Arc<PromptTemplateCatalog>,
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
            planner,
            templates,
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
            let route = InlineRoute {
                sender,
                estimate: Arc::new(Mutex::new(None)),
            };
            let scoped = ContextSelection {
                turn: context.selection.clone(),
                route: Some(route.clone()),
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
                        *route.estimate.lock().expect("context estimate poisoned") = None;
                        return result.map(|output| ToolOutput::text(output.text)).map_err(|error| match error {
                            OperationError::Cancelled => OperationError::Cancelled,
                            OperationError::Failed(error) => OperationError::Failed(ToolFailure::new(error.to_string(), parrot_storage::diagnostics::FailureKind::InvalidOperation)),
                        });
                    }
                    Some(request) = requests.recv() => match request {
                        InlineRequest::Estimate { reply } => {
                            let result = context.resources.context.estimate_context(EpochInput {
                                selection: &context.selection.model,
                                tools: context.definitions,
                                conversation: context.resources.conversation,
                                prompt: context.resources.prompt,
                                sessions: context.resources.sessions,
                                status: context.resources.status,
                                events: context.resources.events,
                            }, cancellation).await
                                .map_err(|error| turn_failure(TurnError::Context(error)));
                            let _ = reply.send(result);
                        }
                        InlineRequest::Compact { target, reply } => {
                        let result: OperationResult<Compaction, Failure> = async {
                            let result = context.resources.context.compact(EpochInput {
                                selection: &context.selection.model,
                                tools: context.definitions,
                                conversation: context.resources.conversation,
                                prompt: context.resources.prompt,
                                sessions: context.resources.sessions,
                                status: context.resources.status,
                                events: context.resources.events,
                            }, target.as_ref(), cancellation).await
                                .map_err(|error| turn_failure(TurnError::Context(error)))?;
                            // Match ContextController::estimate_context_after_tool_result:
                            // reload authoritative history after compaction, then retain
                            // only owned inputs for SessionServices' synchronous estimate.
                            let history = AgentConversation::new(context.resources.history.clone()).await
                                .map_err(|error| OperationError::Failed(failure(error.to_string())))?;
                            let instructions = context.resources.prompt.build(&context.selection.model, cancellation).await
                                .map_err(|error| turn_failure(TurnError::Prompt(error)))?;
                            *route.estimate.lock().expect("context estimate poisoned") = Some(EstimateSnapshot {
                                planner: self.planner.clone(), selected: context.selection.model.clone(),
                                instructions, tools: context.definitions.to_vec(),
                                messages: history.messages().iter().map(provider_message).collect(),
                                templates: self.templates.clone(),
                            });
                            Ok(compact_result(result))
                        }.await;
                        let _ = reply.send(result);
                        }
                    }
                }
            }
        })
    }
}
