use super::support::*;
use parrot_domain::{contracts::async_ops::BoxFuture, values::accounting::RetainedAgentBudget};
use parrot_runtime::{
    agent_turn::{
        lifecycle::{Execution, Signal},
        services::{ParentCompletion, TurnError, TurnSelection},
    },
    children::*,
};
use std::sync::{Arc, Mutex};

struct NoParent;
impl ParentCompletion for NoParent {
    fn deliver_correlated<'a>(
        &'a self,
        execution: &'a parrot_runtime::agent_turn::lifecycle::Execution,
        _: &'a str,
    ) -> parrot_domain::contracts::async_ops::BoxFuture<
        'a,
        Result<(), parrot_runtime::agent_turn::services::TurnError>,
    > {
        self.deliver(execution)
    }

    fn deliver<'a>(&'a self, _: &'a Execution) -> BoxFuture<'a, Result<(), TurnError>> {
        Box::pin(async { Ok(()) })
    }
}
pub struct Harness {
    pub histories: Arc<Histories>,
    pub factory: Arc<Factory>,
    pub provider: Scripted,
    pub diagnostics: Arc<Diagnostics>,
    pub registry: Arc<AgentRegistry<Security>>,
    pub lifetime: Arc<Signal>,
    pub budget: RetainedAgentBudget,
    pub admission_error: Arc<Mutex<Option<ChildError>>>,
}
impl Harness {
    pub fn new() -> Self {
        Self::with_capacity(32)
    }
    pub fn with_capacity(capacity: usize) -> Self {
        let histories = Histories::new();
        Self::with_history_preparer(capacity, histories.clone(), histories)
    }
    pub fn with_history_preparer(
        capacity: usize,
        histories: Arc<Histories>,
        preparer: Arc<dyn HistoryPreparer>,
    ) -> Self {
        Self::with_settings(
            capacity,
            histories.clone(),
            preparer,
            Arc::new(AgentStateFiles::new(
                histories
                    .repository
                    .events()
                    .database()
                    .selected_root()
                    .clone(),
            )),
            parrot_foundation::config::runtime::AgentLimitsConfig {
                disable_recursion_limit: false,
                ..Default::default()
            },
        )
    }
    pub fn with_limits(
        capacity: usize,
        limits: parrot_foundation::config::runtime::AgentLimitsConfig,
    ) -> Self {
        let histories = Histories::new();
        Self::with_settings(
            capacity,
            histories.clone(),
            histories.clone(),
            Arc::new(AgentStateFiles::new(
                histories
                    .repository
                    .events()
                    .database()
                    .selected_root()
                    .clone(),
            )),
            limits,
        )
    }
    fn with_settings(
        capacity: usize,
        histories: Arc<Histories>,
        preparer: Arc<dyn HistoryPreparer>,
        states: Arc<dyn AgentStatePreparer>,
        agent_limits: parrot_foundation::config::runtime::AgentLimitsConfig,
    ) -> Self {
        let factory = Arc::new(Factory::default());
        *factory.histories.lock().unwrap() = Some(histories.clone());
        let diagnostics = Arc::new(Diagnostics::default());
        let lifetime = Arc::new(Signal::new());
        let budget = RetainedAgentBudget::new(capacity.try_into().unwrap());
        let provider = Scripted::default();
        let admission_error = Arc::new(Mutex::new(None));
        let reject = admission_error.clone();
        let registry = AgentRegistry::new(RegistryConfig {
            agent_limits,
            profiles: Arc::new(Profiles),
            factory: factory.clone(),
            histories: preparer,
            states,
            router: provider.router(),
            ids: Arc::new(Ids),
            diagnostics: diagnostics.clone(),
            retained: budget.clone(),
            lifetime: lifetime.clone(),
            validate_child: Arc::new(move |_| reject.lock().unwrap().take().map_or(Ok(()), Err)),
        });
        Self {
            histories,
            factory,
            provider,
            diagnostics,
            registry,
            lifetime,
            budget,
            admission_error,
        }
    }
    pub fn with_state_preparer(
        histories: Arc<Histories>,
        states: Arc<dyn AgentStatePreparer>,
    ) -> Self {
        Self::with_settings(
            32,
            histories.clone(),
            histories,
            states,
            parrot_foundation::config::runtime::AgentLimitsConfig {
                disable_recursion_limit: false,
                ..Default::default()
            },
        )
    }
    pub async fn reopen_root(&self, identity: Arc<AgentIdentity>) -> Arc<Scope<Security>> {
        let scope = self.unregistered_root_identity(identity).await;
        self.registry.register_root(scope.clone()).unwrap();
        scope
    }
    pub async fn root(&self, name: &str) -> Arc<Scope<Security>> {
        let scope = self.unregistered_root(name).await;
        self.registry.register_root(scope.clone()).unwrap();
        scope
    }
    pub async fn unregistered_root(&self, name: &str) -> Arc<Scope<Security>> {
        let identity = AgentIdentity::root(id(), name.into(), templates());
        self.unregistered_root_identity(identity).await
    }
    async fn unregistered_root_identity(
        &self,
        identity: Arc<AgentIdentity>,
    ) -> Arc<Scope<Security>> {
        let history = self.histories.history(&identity);
        let lineage = PolicyLineage::root();
        let selection = Arc::new(SelectionCell::new(
            selection(&self.provider, "first", "root-mode", 0xff),
            lineage.clone(),
        ));
        let parts = self
            .factory
            .build(PreparedScope {
                agent_state: self
                    .registry
                    .prepare_state(
                        &identity,
                        &selection.current().unwrap(),
                        "",
                        CompletionDelivery::RetainedOnly,
                    )
                    .await
                    .unwrap(),
                identity: identity.clone(),
                history: history.clone(),
                selection: selection.clone(),
                lineage,
                parent_completion: Arc::new(NoParent),
                lifetime: self.registry.lifetime(),
            })
            .await
            .unwrap();
        bounded(
            Scope::create(ScopeConfig {
                authority: self.registry.clone(),
                identity,
                history,
                selection,
                parent: None,
                delivery: CompletionDelivery::RetainedOnly,
                retention: None,
                parts,
            })
            .join(),
        )
        .await
        .unwrap()
    }
    async fn prepare_unregistered_state(
        &self,
        identity: &AgentIdentity,
        selection: &TurnSelection<Security>,
    ) -> Arc<dyn parrot_domain::contracts::agent_state::AgentStateStore> {
        use parrot_domain::contracts::paths::{RootOrigin, SelectedSessionRoot};
        use parrot_domain::values::agent_state::{
            AgentDescriptor, AgentStateSnapshot, CompletionDeliverySnapshot, SelectionSnapshot,
        };
        let root = SelectedSessionRoot::new(
            self.histories
                .path
                .join("unregistered")
                .join(&identity.session.session_id),
            RootOrigin::Current,
        )
        .unwrap();
        let store = AgentStateFiles::new(root)
            .open(&identity.name_path, &identity.session.session_id)
            .await
            .unwrap();
        store
            .initialize(AgentStateSnapshot::new(
                identity.session.session_id.clone(),
                AgentDescriptor {
                    parent_session_id: Some(identity.session.parent_session_id.clone()),
                    name: identity.session.name.clone(),
                    selection: SelectionSnapshot {
                        profile_id: selection.profile.id.clone(),
                        model_selector: selection.model.requested_selector.value.clone(),
                    },
                    local_scope: "test scope".into(),
                    completion_delivery: CompletionDeliverySnapshot::RetainedOnly,
                },
            ))
            .await
            .unwrap();
        store
    }
    pub async fn unregistered_child(
        &self,
        parent: &Arc<Scope<Security>>,
        name: &str,
    ) -> Arc<Scope<Security>> {
        let depth = parent.identity().session.depth + 1;
        let mut name_path = parent.identity().name_path.clone();
        name_path.push(name.into());
        let identity = Arc::new(AgentIdentity {
            session: parrot_runtime::agent_turn::services::SessionIdentity {
                session_id: id(),
                name: name.into(),
                parent_session_id: parent.identity().session.session_id.clone(),
                depth,
            },
            parent_name: parent.identity().session.name.clone(),
            name_path,
            prompt_scope: parent
                .identity()
                .prompt_scope
                .derive_child(name, depth, "test scope"),
            templates: parent.identity().templates.clone(),
        });
        let history = self.histories.history(&identity);
        let lineage = parent
            .selection()
            .lineage()
            .link(parent.selection().clone());
        let mut current = selection(&self.provider, "first", "child", 0xff);
        current.security = lineage.resolve(current.security).unwrap();
        let selection = Arc::new(SelectionCell::new(current, lineage.clone()));
        let completion = ParentScope::completion(
            identity.clone(),
            Some(parent),
            CompletionDelivery::RetainedOnly,
            Arc::new(Ids),
        );
        let parts = self
            .factory
            .build(PreparedScope {
                agent_state: self
                    .prepare_unregistered_state(&identity, &selection.current().unwrap())
                    .await,
                identity: identity.clone(),
                history: history.clone(),
                selection: selection.clone(),
                lineage,
                parent_completion: completion,
                lifetime: self.registry.lifetime(),
            })
            .await
            .unwrap();
        bounded(
            Scope::create(ScopeConfig {
                authority: self.registry.clone(),
                identity,
                history,
                selection,
                parent: Some(parent.clone()),
                delivery: CompletionDelivery::RetainedOnly,
                retention: None,
                parts,
            })
            .join(),
        )
        .await
        .unwrap()
    }
    pub fn request(&self, parent: &Arc<Scope<Security>>, name: &str) -> LaunchRequest<Security> {
        LaunchRequest {
            parent: parent.session().clone(),
            requested_name: name.into(),
            requested_profile: "child".into(),
            model: self.provider.selection("first"),
            requested_scope: "test scope".into(),
            delivery: CompletionDelivery::RetainedOnly,
            boundary: parrot_storage::conversation::HistoryForkBoundary::AfterCompletedHistory,
            fork: parrot_storage::conversation::HistoryForkSelection::parse("empty"),
            history_source: HistorySource::Parent,
        }
    }
    pub async fn spawn(&self, parent: &Arc<Scope<Security>>, name: &str) -> Arc<Scope<Security>> {
        bounded(
            parent
                .spawner()
                .spawn(self.request(parent, name))
                .unwrap()
                .join(),
        )
        .await
        .unwrap()
    }
    pub async fn close(&self) {
        bounded(self.registry.shutdown().join()).await.unwrap();
    }
}
