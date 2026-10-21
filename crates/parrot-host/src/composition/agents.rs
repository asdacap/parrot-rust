//! Caller-scoped bindings for retained agent and question tools.

use std::sync::{Arc, OnceLock, Weak};

use parrot_domain::{
    contracts::{
        async_ops::{BoxFuture, Cancellation, OperationError},
        interactions::{
            ChildQuestionAnswers, QuestionDefinition, QuestionReply, QuestionRequester,
        },
        runtime::AgentRuntime,
        security::RestrictableAccessSnapshot,
        status::ProcessStatusSource,
    },
    protocol::messages::parrot::Delivery,
    values::{activity::AgentSessionActivity, identifiers::Identifier},
};
use parrot_foundation::template_catalog::PromptTemplateCatalog;
use parrot_provider::routing::aliases::ModelRouter;
use parrot_runtime::{
    active_work::{
        blockers::ActiveWorkReminder,
        wait::{WaitDuration, WaitService},
    },
    agent_turn::{
        engine::RuntimeBackend,
        lifecycle::{AgentSession, IncomingActivity, LifecycleError, Signal},
    },
    children::{
        AgentRegistry, AgentResolver, ChildError, CompletionDelivery, HistorySource, LaunchRequest,
        Scope,
    },
    interactions::{
        ChildIdentity, ChildQuestion, ChildQuestionCoordinator, ChildQuestionDelivery,
        ChildQuestionRegistry, ChildQuestionTemplates, InteractionError, InteractionErrorKind,
        QuestionBroker,
    },
    tool_settlement::{SharedSecurity, ToolSettlement},
};
use parrot_storage::{
    conversation::{HistoryForkBoundary, HistoryForkSelection},
    events::{ConversationPart, NeverCancelled},
};
use parrot_tools::agents::{
    self, AgentToolError, NamedRequest, SendRequest, SpawnRequest,
    input::{ForkSelection, WaitInput},
    status::{StatusChild, StatusProcess, StatusSnapshot},
};

pub type RuntimeSession<S> = AgentSession<RuntimeBackend<S, ToolSettlement<S>>>;

/// Registered beside the actual runtime session in its owning scope. These are
/// the same activity and work observations supplied to the drain, not copies.
pub struct AgentSessionServices<S: RestrictableAccessSnapshot + 'static> {
    pub session: Arc<RuntimeSession<S>>,
    pub activity: Arc<AgentSessionActivity>,
    pub output_path: String,
    pub active_work: Arc<ActiveWorkReminder>,
}

/// One retained binding per scope. The topology backlink is weak, and binding
/// happens before root publication/child admission completes.
pub struct ScopedAgentRuntime<S: RestrictableAccessSnapshot + 'static> {
    owner: OnceLock<Weak<Scope<S>>>,
    registry: Weak<AgentRegistry<S>>,
    models: Arc<ModelRouter>,
    user_questions: Arc<QuestionBroker>,
    templates: Arc<PromptTemplateCatalog>,
}

impl<S: RestrictableAccessSnapshot + 'static> ScopedAgentRuntime<S> {
    pub fn new(
        registry: Weak<AgentRegistry<S>>,
        models: Arc<ModelRouter>,
        user_questions: Arc<QuestionBroker>,
        templates: Arc<PromptTemplateCatalog>,
    ) -> Self {
        Self {
            owner: OnceLock::new(),
            registry,
            models,
            user_questions,
            templates,
        }
    }

    pub fn bind(&self, scope: &Arc<Scope<S>>) -> Result<(), ChildError> {
        if !std::ptr::eq(self, scope.get_service::<Self>()?.as_ref()) {
            return Err(ChildError::registry(
                "agent runtime does not match its scope service",
            ));
        }
        if self.owner.set(Arc::downgrade(scope)).is_err() {
            return Err(ChildError::registry("agent runtime scope is already bound"));
        }
        Ok(())
    }

    fn owner(&self) -> Result<Arc<Scope<S>>, ChildError> {
        let scope = self
            .owner
            .get()
            .and_then(Weak::upgrade)
            .ok_or_else(|| ChildError::registry("agent runtime scope is not bound"))?;
        scope.parent_scope().require_owner()?;
        Ok(scope)
    }

    fn resolver(&self, scope: &Arc<Scope<S>>) -> Result<AgentResolver<S>, ChildError> {
        let registry = self
            .registry
            .upgrade()
            .ok_or_else(|| ChildError::registry("the user session is shutting down"))?;
        Ok(AgentResolver::new(scope, &registry))
    }

    fn services(scope: &Arc<Scope<S>>) -> Result<Arc<AgentSessionServices<S>>, ChildError> {
        scope.get_service::<AgentSessionServices<S>>()
    }

    fn coordinator(
        scope: &Arc<Scope<S>>,
    ) -> Result<Arc<ChildQuestionCoordinator>, InteractionError> {
        scope
            .get_service::<ChildQuestionCoordinator>()
            .map_err(interaction_child)
    }

    pub fn child_question(
        identity: &parrot_runtime::children::AgentIdentity,
    ) -> Arc<ChildQuestion> {
        Arc::new(ChildQuestion::new(Arc::new(ChildIdentity {
            session_id: identity.session.session_id.clone(),
            name: identity.session.name.clone(),
            parent_session_id: identity.session.parent_session_id.clone(),
        })))
    }
}

impl<S: RestrictableAccessSnapshot + 'static> AgentRuntime for ScopedAgentRuntime<S> {
    type Security = SharedSecurity<S>;
    type SpawnRequest = SpawnRequest;
    type SpawnResult = agents::SpawnResult;
    type SendRequest = SendRequest;
    type SendResult = agents::SendResult;
    type InterruptRequest = NamedRequest;
    type InterruptResult = ();
    type StatusRequest = NamedRequest;
    type StatusResult = StatusSnapshot;
    type WaitRequest = WaitInput;
    type WaitResult = String;
    type Error = AgentToolError;

    fn spawn<'a>(
        &'a self,
        request: SpawnRequest,
        security: SharedSecurity<S>,
        cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<Self::SpawnResult, OperationError<AgentToolError>>> {
        Box::pin(async move {
            cancellation
                .check()
                .map_err(|_| OperationError::Cancelled)?;
            let owner = self.owner().map_err(operation_child)?;
            let model = self.models.resolve(&request.input.model).map_err(|error| {
                OperationError::Failed(AgentToolError::Provider(error.to_string()))
            })?;
            let fork = match &request.input.fork {
                ForkSelection::Empty => "empty",
                ForkSelection::Full => "full",
                ForkSelection::Named(title) => title,
            };
            // The source's AgentLaunchRequest.Selection is null-checked, but
            // SelectChild deliberately resolves the live parent policy lineage
            // (pinned AgentSpawner.cs:191,219; resume:178-183). Do not permanently fold
            // this invocation capture into the child's raw profile policy.
            let _invocation_security = security;
            let operation = owner
                .spawner()
                .spawn_or_resume(LaunchRequest {
                    parent: owner.session().clone(),
                    requested_name: request.input.name,
                    requested_profile: request.input.agent,
                    model,
                    requested_scope: request.input.scope,
                    delivery: CompletionDelivery::Automatic,
                    boundary: HistoryForkBoundary::BeforeToolBatch {
                        assistant_sequence: request.assistant_sequence,
                        tool_call_id: request.call_id,
                    },
                    fork: HistoryForkSelection::parse(fork),
                    history_source: HistorySource::Parent,
                })
                .map_err(operation_child)?;
            let child = operation
                .wait(cancellation)
                .await
                .map_err(operation_child)?;
            let services = Self::services(&child).map_err(operation_child)?;
            send_text(&services.session, request.input.prompt, cancellation).await?;
            Ok(agents::SpawnResult {
                name: child.identity().session.name.clone(),
                depth: child.identity().session.depth,
                output_path: services.output_path.clone(),
            })
        })
    }

    fn send<'a>(
        &'a self,
        request: SendRequest,
        cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<Self::SendResult, OperationError<AgentToolError>>> {
        Box::pin(async move {
            cancellation
                .check()
                .map_err(|_| OperationError::Cancelled)?;
            let owner = self.owner().map_err(operation_child)?;
            let resolver = self.resolver(&owner).map_err(operation_child)?;
            let recipient = resolver
                .resolve_recipient(&request.input.name)
                .map_err(operation_child)?;
            if !request.allow_parent
                && (recipient.identity().session.session_id
                    == owner.identity().session.parent_session_id
                    || resolver.is_ancestor(&recipient))
            {
                return Err(OperationError::Failed(AgentToolError::Registry(
                    "sending to the parent agent is disabled; report through the final message instead".into())));
            }
            // Resolve through the same scoped address rules, retaining the real
            // recipient. Bare parent aliases are special only for send.
            let scope = if owner
                .parent_scope()
                .parent()
                .is_some_and(|parent| Arc::ptr_eq(parent.session(), &recipient))
            {
                owner.parent_scope().parent().expect("parent checked")
            } else {
                resolver
                    .resolve_status_scope(&request.input.name)
                    .map_err(operation_child)?
            };
            let services = Self::services(&scope).map_err(operation_child)?;
            let sent = send_text(&services.session, request.input.message, cancellation).await?;
            Ok(agents::SendResult {
                name: recipient.identity().session.name.clone(),
                message_id: sent.admission.input.message_id,
                follow_up: sent.follow_up,
            })
        })
    }

    fn interrupt<'a>(
        &'a self,
        request: NamedRequest,
        _cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<(), OperationError<AgentToolError>>> {
        Box::pin(async move {
            let owner = self.owner().map_err(operation_child)?;
            // Interrupt is direct-child-only, unlike status's address resolver.
            let child = owner
                .children()
                .resolve(&request.name)
                .map_err(operation_child)?;
            Self::services(&child)
                .map_err(operation_child)?
                .session
                .interrupt_handle()
                .map_err(operation_lifecycle)?;
            // The actual session retains and joins the unwind; awaiting it here
            // would deadlock a parent's settlement tool on its child.
            Ok(())
        })
    }

    fn status(&self, request: NamedRequest) -> Result<StatusSnapshot, AgentToolError> {
        let owner = self.owner().map_err(agent_child)?;
        let target = self
            .resolver(&owner)
            .map_err(agent_child)?
            .resolve_status_scope(&request.name)
            .map_err(agent_child)?;
        let services = Self::services(&target).map_err(agent_child)?;
        let processes = target
            .get_service::<dyn ProcessStatusSource>()
            .map_err(agent_child)?;
        Ok(StatusSnapshot {
            session_id: target.identity().session.session_id.clone(),
            name: target.identity().session.name.clone(),
            output_path: services.output_path.clone(),
            activity: services.activity.capture(),
            children: target
                .children()
                .snapshot_descendants()
                .into_iter()
                .map(|child| StatusChild {
                    session_id: child.identity().session.session_id.clone(),
                    parent_session_id: child.identity().session.parent_session_id.clone(),
                    name: child.identity().session.name.clone(),
                    active: child.is_active(),
                })
                .collect(),
            processes: processes
                .snapshot()
                .into_iter()
                .map(|process| StatusProcess {
                    name: process.name.clone(),
                    id: process.id(),
                })
                .collect(),
        })
    }

    fn wait<'a>(
        &'a self,
        request: WaitInput,
        cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<String, OperationError<AgentToolError>>> {
        Box::pin(async move {
            let owner = self.owner().map_err(operation_child)?;
            let services = Self::services(&owner).map_err(operation_child)?;
            let duration = WaitDuration::new(request.duration_ms).ok_or_else(|| {
                OperationError::Failed(AgentToolError::Argument(
                    "wait duration is outside the allowed range".into(),
                ))
            })?;
            WaitService {
                session: services.session.as_ref().clone(),
                active_work: services.active_work.clone(),
            }
            .wait(duration, cancellation)
            .await
            .map_err(|error| {
                if error.is_cancelled() {
                    OperationError::Cancelled
                } else {
                    OperationError::Failed(AgentToolError::Service(error.to_string()))
                }
            })
        })
    }
}

impl<S: RestrictableAccessSnapshot + 'static> QuestionRequester for ScopedAgentRuntime<S> {
    type Error = AgentToolError;
    fn ask<'a>(
        &'a self,
        questions: Vec<QuestionDefinition>,
        cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<QuestionReply, OperationError<AgentToolError>>> {
        Box::pin(async move {
            let owner = self.owner().map_err(operation_child)?;
            if owner.identity().session.depth == 0 {
                return self
                    .user_questions
                    .ask(questions, cancellation)
                    .await
                    .map_err(operation_interaction);
            }
            let parent = owner.parent_scope().parent().ok_or_else(|| {
                OperationError::Failed(AgentToolError::Registry(
                    "child agent parent scope not found".into(),
                ))
            })?;
            let child = owner
                .get_service::<ChildQuestion>()
                .map_err(operation_child)?;
            Self::coordinator(&parent)
                .map_err(operation_interaction_failure)?
                .ask(child.identity.clone(), questions, cancellation)
                .await
                .map_err(operation_interaction)
        })
    }
}

impl<S: RestrictableAccessSnapshot + 'static> ChildQuestionAnswers for ScopedAgentRuntime<S> {
    type Error = AgentToolError;
    fn reply(&self, agent_name: &str, reply: QuestionReply) -> Result<(), AgentToolError> {
        let owner = self.owner().map_err(agent_child)?;
        Self::coordinator(&owner)
            .map_err(agent_interaction)?
            .reply(agent_name, reply)
            .map_err(agent_interaction)
    }
}

impl<S: RestrictableAccessSnapshot + 'static> ChildQuestionRegistry for ScopedAgentRuntime<S> {
    fn find(&self, name: &str) -> Option<Arc<ChildQuestion>> {
        self.owner()
            .ok()?
            .children()
            .find(name)?
            .get_service::<ChildQuestion>()
            .ok()
    }
    fn snapshot(&self) -> Vec<Arc<ChildQuestion>> {
        self.owner().map_or_else(
            |_| Vec::new(),
            |owner| {
                owner
                    .children()
                    .snapshot_children()
                    .into_iter()
                    .filter_map(|child| child.get_service::<ChildQuestion>().ok())
                    .collect()
            },
        )
    }
}

impl<S: RestrictableAccessSnapshot + 'static> ChildQuestionDelivery for ScopedAgentRuntime<S> {
    fn steer<'a>(
        &'a self,
        owner_session_id: &'a str,
        request_id: &'a str,
        text: String,
    ) -> BoxFuture<'a, Result<(), InteractionError>> {
        Box::pin(async move {
            let owner = self.owner().map_err(interaction_child)?;
            if owner.identity().session.session_id != owner_session_id {
                return Err(InteractionError::new(
                    InteractionErrorKind::Registry,
                    "question delivery must target its owning parent",
                ));
            }
            owner
                .session()
                .send(
                    vec![ConversationPart::text(text)],
                    request_id.into(),
                    Delivery::DELIVERY_STEER,
                    IncomingActivity::default(),
                    Arc::new(Signal::new()),
                )
                .map_err(interaction_child)?
                .join()
                .await
                .map_err(|error| interaction_child(error.into()))?;
            Ok(())
        })
    }
}

impl<S: RestrictableAccessSnapshot + 'static> ChildQuestionTemplates for ScopedAgentRuntime<S> {
    fn question(&self, agent_name: &str, questions: &str) -> Result<String, InteractionError> {
        self.templates
            .render(
                "agent-session.child-question",
                &[
                    ("agent_name".into(), agent_name.into()),
                    ("questions".into(), questions.into()),
                ],
                &NeverCancelled,
            )
            .map_err(|error| {
                InteractionError::new(InteractionErrorKind::Delivery, error.to_string())
            })
    }
    fn reminder(&self, children: &str) -> Result<String, InteractionError> {
        self.templates
            .render(
                "agent-session.pending-child-question-reminder",
                &[("children".into(), children.into())],
                &NeverCancelled,
            )
            .map_err(|error| {
                InteractionError::new(InteractionErrorKind::Delivery, error.to_string())
            })
    }
}

struct CallerSignal(Arc<Signal>);
impl Drop for CallerSignal {
    fn drop(&mut self) {
        self.0.cancel();
    }
}

async fn send_text<S: RestrictableAccessSnapshot + 'static>(
    session: &RuntimeSession<S>,
    message: String,
    cancellation: &dyn Cancellation,
) -> Result<parrot_runtime::agent_turn::lifecycle::SendResult, OperationError<AgentToolError>> {
    let caller = CallerSignal(Arc::new(Signal::new()));
    let id = Identifier::message_id()
        .map_err(|error| OperationError::Failed(AgentToolError::Service(error.to_string())))?;
    let operation = session
        .send_and_select_execution(vec![ConversationPart::text(message)], id, caller.0.clone())
        .map_err(operation_lifecycle)?;
    operation
        .wait(cancellation)
        .await
        .map_err(operation_lifecycle)
}

fn agent_child(error: ChildError) -> AgentToolError {
    match error {
        ChildError::Registry(message) | ChildError::NameConflict(message) => {
            AgentToolError::Registry(message)
        }
        error => AgentToolError::Service(error.to_string()),
    }
}
fn operation_child(error: ChildError) -> OperationError<AgentToolError> {
    if error == ChildError::Cancelled {
        OperationError::Cancelled
    } else {
        OperationError::Failed(agent_child(error))
    }
}
fn operation_lifecycle(error: LifecycleError) -> OperationError<AgentToolError> {
    if error.is_cancelled() {
        OperationError::Cancelled
    } else {
        OperationError::Failed(AgentToolError::Service(error.to_string()))
    }
}
fn interaction_child(error: ChildError) -> InteractionError {
    InteractionError::new(InteractionErrorKind::Registry, error.to_string())
}
fn agent_interaction(error: InteractionError) -> AgentToolError {
    match error.kind {
        InteractionErrorKind::Registry => AgentToolError::Registry(error.message),
        InteractionErrorKind::QuestionRejected | InteractionErrorKind::Disposed => {
            AgentToolError::QuestionRejected(error.message)
        }
        InteractionErrorKind::Question
        | InteractionErrorKind::QuestionNotFound
        | InteractionErrorKind::Argument => AgentToolError::Question(error.message),
        _ => AgentToolError::Service(error.message),
    }
}
fn operation_interaction_failure(error: InteractionError) -> OperationError<AgentToolError> {
    OperationError::Failed(agent_interaction(error))
}
fn operation_interaction(
    error: OperationError<InteractionError>,
) -> OperationError<AgentToolError> {
    match error {
        OperationError::Cancelled => OperationError::Cancelled,
        OperationError::Failed(error) => operation_interaction_failure(error),
    }
}
