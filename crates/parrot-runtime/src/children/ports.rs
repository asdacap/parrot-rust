use super::{ChildResult, PolicyLineage};
use crate::{
    agent_turn::{
        lifecycle::{AgentSession, DrainBackend, IncomingActivity, OperationHandle, SendResult},
        services::{
            ParentCompletion, SelectionSource, SessionIdentity, TurnProfile, TurnSelection,
        },
    },
    prompt_context::PromptScope,
};
use parrot_domain::{
    contracts::{
        async_ops::{BoxFuture, Cancellation},
        security::RestrictableAccessSnapshot,
    },
    protocol::messages::parrot::{Delivery, Event},
};
use parrot_foundation::template_catalog::PromptTemplateCatalog;
use parrot_provider::routing::aliases::ResolvedModelSelection;
use parrot_storage::{conversation::AgentHistoryView, events::ConversationPart};
use std::{
    any::{Any, TypeId},
    sync::Arc,
};

pub struct AgentIdentity {
    pub session: SessionIdentity,
    pub parent_name: String,
    pub name_path: Vec<String>,
    pub prompt_scope: PromptScope,
    pub templates: Arc<PromptTemplateCatalog>,
}
impl AgentIdentity {
    pub fn root(
        session_id: String,
        name: String,
        templates: Arc<PromptTemplateCatalog>,
    ) -> Arc<Self> {
        Arc::new(Self {
            session: SessionIdentity {
                session_id,
                name: name.clone(),
                parent_session_id: String::new(),
                depth: 0,
            },
            parent_name: String::new(),
            name_path: vec![name],
            prompt_scope: PromptScope::empty(templates.clone()),
            templates,
        })
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CompletionDelivery {
    Automatic,
    RetainedOnly,
}

/// The sole session port erases the backend type, not the actual session implementation.
pub trait ScopedSession<S: RestrictableAccessSnapshot + 'static>: Send + Sync {
    fn identity(&self) -> &Arc<AgentIdentity>;
    fn selection(&self) -> &Arc<dyn SelectionSource<S>>;
    fn is_active(&self) -> bool;
    fn update_selection(&self, selection: TurnSelection<S>) -> ChildResult<()>;
    fn send(
        &self,
        parts: Vec<ConversationPart>,
        message_id: String,
        delivery: Delivery,
        activity: IncomingActivity,
        cancellation: Arc<dyn Cancellation>,
    ) -> ChildResult<OperationHandle<SendResult>>;
    fn close(&self) -> BoxFuture<'_, ChildResult<()>>;
}

pub struct SessionAdapter<
    S: RestrictableAccessSnapshot + 'static,
    B: DrainBackend<Selection = TurnSelection<S>>,
> {
    identity: Arc<AgentIdentity>,
    selection: Arc<dyn SelectionSource<S>>,
    session: AgentSession<B>,
}
impl<S: RestrictableAccessSnapshot + 'static, B: DrainBackend<Selection = TurnSelection<S>>>
    SessionAdapter<S, B>
{
    /// `selection` must be the very same Arc supplied to RuntimeConfig.
    pub fn new(
        identity: Arc<AgentIdentity>,
        selection: Arc<dyn SelectionSource<S>>,
        session: AgentSession<B>,
    ) -> Self {
        Self {
            identity,
            selection,
            session,
        }
    }
    pub fn session(&self) -> &AgentSession<B> {
        &self.session
    }
}
impl<S: RestrictableAccessSnapshot + 'static, B: DrainBackend<Selection = TurnSelection<S>>>
    ScopedSession<S> for SessionAdapter<S, B>
{
    fn identity(&self) -> &Arc<AgentIdentity> {
        &self.identity
    }
    fn selection(&self) -> &Arc<dyn SelectionSource<S>> {
        &self.selection
    }
    fn is_active(&self) -> bool {
        self.session.is_active()
    }
    fn update_selection(&self, selection: TurnSelection<S>) -> ChildResult<()> {
        self.session
            .backend()
            .update_selection(selection)
            .map_err(Into::into)
    }
    fn send(
        &self,
        parts: Vec<ConversationPart>,
        message_id: String,
        delivery: Delivery,
        activity: IncomingActivity,
        cancellation: Arc<dyn Cancellation>,
    ) -> ChildResult<OperationHandle<SendResult>> {
        self.session
            .send_handle(parts, message_id, delivery, activity, cancellation)
            .map_err(Into::into)
    }
    fn close(&self) -> BoxFuture<'_, ChildResult<()>> {
        Box::pin(async move { self.session.close().await.map_err(Into::into) })
    }
}

pub struct ChildProfile<S: RestrictableAccessSnapshot + 'static> {
    pub profile: TurnProfile,
    pub security: Arc<S>,
    pub recursion_limit: usize,
}
/// Child eligibility/profile lookup belongs to composition's accepted profile registry.
pub trait ProfileResolver<S: RestrictableAccessSnapshot + 'static>: Send + Sync {
    fn resolve_child(&self, id: &str) -> ChildResult<ChildProfile<S>>;
}

pub struct ServiceRegistration {
    pub(crate) key: TypeId,
    pub(crate) value: Arc<dyn Any + Send + Sync>,
}
impl ServiceRegistration {
    pub fn new<T: ?Sized + Send + Sync + 'static>(service: Arc<T>) -> Self {
        Self {
            key: TypeId::of::<T>(),
            value: Arc::new(service),
        }
    }
}
/// Publishers capture synchronously, and start their retained publication only after admission.
pub trait SnapshotPublisher: Send + Sync {
    fn capture(&self) -> ChildResult<Vec<Event>>;
    fn start(&self) -> ChildResult<super::Operation<()>>;
}
pub use parrot_domain::contracts::async_ops::{
    OwnedWork as WorkOwner, ScopeLifetime as ScopeResource,
};

pub struct ScopeParts<S: RestrictableAccessSnapshot + 'static> {
    pub session: Arc<dyn ScopedSession<S>>,
    pub services: Vec<ServiceRegistration>,
    pub publishers: Vec<Arc<dyn SnapshotPublisher>>,
    pub work: Vec<Arc<dyn WorkOwner<Error = super::ChildError>>>,
    /// Composition disposal order, followed by queue disposal and question close.
    pub resources: Vec<Arc<dyn ScopeResource<Error = super::ChildError>>>,
    pub final_resources: Vec<Arc<dyn ScopeResource<Error = super::ChildError>>>,
}
pub struct PreparedScope<S: RestrictableAccessSnapshot + 'static> {
    pub identity: Arc<AgentIdentity>,
    pub history: AgentHistoryView,
    pub selection: Arc<dyn SelectionSource<S>>,
    pub lineage: PolicyLineage<S>,
    pub parent_completion: Arc<dyn ParentCompletion>,
    pub lifetime: Arc<dyn Cancellation>,
}
/// Builds a real AgentSession adapter. Failed construction must finish its own partial cleanup.
pub trait ScopeFactory<S: RestrictableAccessSnapshot + 'static>: Send + Sync {
    fn create(&self, prepared: PreparedScope<S>) -> BoxFuture<'_, ChildResult<ScopeParts<S>>>;
}
pub trait HistoryPreparer: Send + Sync {
    fn prepare<'a>(
        &'a self,
        identity: &'a AgentIdentity,
    ) -> BoxFuture<'a, ChildResult<AgentHistoryView>>;
}
pub trait IdGenerator: Send + Sync {
    fn agent_session(&self) -> ChildResult<String>;
    fn message(&self) -> ChildResult<String>;
}
/// Deliberately bounded: no prompts, profiles, names, exception bodies or policy data.
pub trait ChildDiagnostics: Send + Sync {
    fn record(&self, event: ChildDiagnostic);
}
pub struct ChildDiagnostic {
    pub event: &'static str,
    pub session_id: String,
    pub correlation_id: String,
    pub duration_milliseconds: Option<u64>,
    pub outcome: Option<&'static str>,
    pub error_code: Option<&'static str>,
}

pub(crate) fn select_child<S: RestrictableAccessSnapshot + 'static>(
    lineage: &PolicyLineage<S>,
    profile: ChildProfile<S>,
    model: ResolvedModelSelection,
    agent_limits: parrot_foundation::config::runtime::AgentLimitsConfig,
) -> ChildResult<TurnSelection<S>> {
    if !agent_limits.disable_recursion_limit
        && lineage.count_profile(&profile.profile.id)? >= profile.recursion_limit
    {
        return Err(super::ChildError::registry(
            "subagent profile recursion limit reached",
        ));
    }
    Ok(TurnSelection {
        model,
        profile: profile.profile,
        security: lineage.resolve(profile.security)?,
    })
}
