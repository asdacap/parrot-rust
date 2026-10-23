use super::support::*;
use parrot_domain::{
    contracts::async_ops::BoxFuture,
    values::{accounting::RetainedAgentBudget, identifiers::UserSessionId},
};
use parrot_foundation::paths::UserSessionResources;
use parrot_runtime::{
    agent_queues::*,
    agent_turn::{
        lifecycle::{Execution, Signal},
        services::{ParentCompletion, TurnError},
    },
    children::*,
};
use parrot_storage::queue_files::{Direction, Owner};
use std::{
    collections::BTreeMap,
    sync::{Arc, Mutex},
};

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

#[derive(Default)]
pub struct QueueLog(pub Mutex<Vec<QueueDiagnostic>>);
impl QueueDiagnostics for QueueLog {
    fn record(&self, event: QueueDiagnostic) {
        self.0.lock().unwrap().push(event);
    }
}

pub struct TrackedPublisher {
    inner: QueueSnapshotPublisher<Security>,
    operation: Mutex<Option<Operation<()>>>,
}
impl SnapshotPublisher for TrackedPublisher {
    fn capture(&self) -> ChildResult<Vec<parrot_domain::protocol::messages::parrot::Event>> {
        self.inner.capture()
    }
    fn start(&self) -> ChildResult<Operation<()>> {
        let operation = self.inner.start()?;
        *self.operation.lock().unwrap() = Some(operation.clone());
        Ok(operation)
    }
}
impl TrackedPublisher {
    pub fn operation(&self) -> Operation<()> {
        self.operation.lock().unwrap().as_ref().unwrap().clone()
    }
}

pub struct QueuedFactory {
    pub sessions: Arc<Factory>,
    pub resources: Arc<UserSessionResources<()>>,
    pub diagnostics: Arc<QueueLog>,
    pub queues: Mutex<BTreeMap<String, Arc<AgentQueues<Security>>>>,
    pub inventories: Mutex<BTreeMap<String, Arc<QueueInventory>>>,
    pub publications: Mutex<BTreeMap<String, Arc<TrackedPublisher>>>,
    root_id: Mutex<Option<String>>,
}
impl QueuedFactory {
    async fn build(&self, prepared: PreparedScope<Security>) -> ChildResult<ScopeParts<Security>> {
        let identity = prepared.identity.clone();
        let name = identity.session.name.clone();
        let root_id = {
            let mut root = self.root_id.lock().unwrap();
            root.get_or_insert_with(|| identity.session.session_id.clone())
                .clone()
        };
        let mut parts = self.sessions.build(prepared).await?;
        let inventory = QueueInventory::new(
            Owner {
                session_id: identity.session.session_id.clone(),
                name: name.clone(),
                parent_session_id: identity.session.parent_session_id.clone(),
                parent_name: identity.parent_name.clone(),
            },
            format!("queue-inventory-{}", id()),
        )
        .unwrap();
        let queues = AgentQueues::new(
            identity,
            self.resources.as_ref(),
            inventory.clone(),
            self.diagnostics.clone(),
        )
        .unwrap();
        queues.initialize().unwrap();
        let broker = self.sessions.get(&name).publisher.broker.clone();
        parts
            .services
            .push(ServiceRegistration::new(queues.clone()));
        let publication = Arc::new(TrackedPublisher {
            inner: QueueSnapshotPublisher::new(queues.clone(), broker, root_id),
            operation: Mutex::new(None),
        });
        parts.publishers.push(publication.clone());
        parts.final_resources.push(queues.clone());
        self.inventories
            .lock()
            .unwrap()
            .insert(name.clone(), inventory);
        self.publications
            .lock()
            .unwrap()
            .insert(name.clone(), publication);
        self.queues.lock().unwrap().insert(name, queues);
        Ok(parts)
    }
    pub fn queues(&self, name: &str) -> Arc<AgentQueues<Security>> {
        self.queues.lock().unwrap().get(name).unwrap().clone()
    }
}
impl ScopeFactory<Security> for QueuedFactory {
    fn create(
        &self,
        prepared: PreparedScope<Security>,
    ) -> BoxFuture<'_, ChildResult<ScopeParts<Security>>> {
        Box::pin(self.build(prepared))
    }
}

pub struct Harness {
    pub histories: Arc<Histories>,
    pub factory: Arc<QueuedFactory>,
    pub provider: Scripted,
    pub registry: Arc<AgentRegistry<Security>>,
}
impl Harness {
    pub fn new() -> Self {
        Self::with_histories(Histories::new())
    }
    pub fn with_histories(histories: Arc<Histories>) -> Self {
        let sessions = Arc::new(Factory::default());
        *sessions.histories.lock().unwrap() = Some(histories.clone());
        let root = histories
            .repository
            .events()
            .database()
            .selected_root()
            .clone();
        let resource_id =
            UserSessionId::parse(histories.path.file_name().unwrap().to_str().unwrap()).unwrap();
        let resources = Arc::new(UserSessionResources::new(root, resource_id, ()).unwrap());
        let factory = Arc::new(QueuedFactory {
            sessions,
            resources,
            diagnostics: Arc::new(QueueLog::default()),
            queues: Mutex::new(BTreeMap::new()),
            inventories: Mutex::new(BTreeMap::new()),
            publications: Mutex::new(BTreeMap::new()),
            root_id: Mutex::new(None),
        });
        let registry = AgentRegistry::new(RegistryConfig {
            agent_limits: parrot_foundation::config::runtime::AgentLimitsConfig {
                disable_recursion_limit: false,
                ..Default::default()
            },
            profiles: Arc::new(Profiles),
            factory: factory.clone(),
            histories: histories.clone(),
            states: Arc::new(AgentStateFiles::new(
                histories
                    .repository
                    .events()
                    .database()
                    .selected_root()
                    .clone(),
            )),
            router: Scripted::default().router(),
            ids: Arc::new(Ids),
            diagnostics: Arc::new(Diagnostics::default()),
            retained: RetainedAgentBudget::new(32),
            lifetime: Arc::new(Signal::new()),
            validate_child: Arc::new(QueueChildAdmissionValidator::validate),
        });
        Self {
            histories,
            factory,
            provider: Scripted::default(),
            registry,
        }
    }
    pub async fn root(&self) -> Arc<Scope<Security>> {
        let identity = AgentIdentity::root(id(), "root".into(), templates());
        let history = self.histories.history(&identity);
        let lineage = PolicyLineage::root();
        let selection = Arc::new(SelectionCell::new(
            selection(&self.provider, "first", "root", 0xff),
            lineage.clone(),
        ));
        let agent_state = self
            .registry
            .prepare_state(
                &identity,
                &selection.current().unwrap(),
                "",
                CompletionDelivery::RetainedOnly,
            )
            .await
            .unwrap();
        let parts = self
            .factory
            .build(PreparedScope {
                agent_state: agent_state.clone(),
                identity: identity.clone(),
                history: history.clone(),
                selection: selection.clone(),
                lineage,
                parent_completion: Arc::new(NoParent),
                lifetime: self.registry.lifetime(),
            })
            .await
            .unwrap();
        let root = bounded(
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
        .unwrap();
        self.q(&root).bind_scope(&root).unwrap();
        self.registry.register_root(root.clone()).unwrap();
        root
    }
    pub fn q(&self, scope: &Arc<Scope<Security>>) -> Arc<AgentQueues<Security>> {
        scope.get_service().unwrap()
    }
    pub async fn spawn(&self, parent: &Arc<Scope<Security>>, name: &str) -> Arc<Scope<Security>> {
        let request = LaunchRequest {
            parent: parent.session().clone(),
            requested_name: name.into(),
            requested_profile: "child".into(),
            model: self.provider.selection("first"),
            requested_scope: "queue test".into(),
            delivery: CompletionDelivery::RetainedOnly,
            boundary: parrot_storage::conversation::HistoryForkBoundary::AfterCompletedHistory,
            fork: parrot_storage::conversation::HistoryForkSelection::parse("empty"),
            history_source: HistorySource::Parent,
        };
        bounded(parent.spawner().spawn(request).unwrap().join())
            .await
            .unwrap()
    }
    pub async fn replacement_child(
        &self,
        parent: &Arc<Scope<Security>>,
        previous: &Arc<Scope<Security>>,
    ) -> Arc<Scope<Security>> {
        let old = previous.identity();
        let identity = Arc::new(AgentIdentity {
            session: old.session.clone(),
            parent_name: old.parent_name.clone(),
            name_path: old.name_path.clone(),
            prompt_scope: parent.identity().prompt_scope.derive_child(
                &old.session.name,
                old.session.depth,
                "same-id queue replacement",
            ),
            templates: old.templates.clone(),
        });
        let history = self.histories.history(&identity);
        let lineage = parent
            .selection()
            .lineage()
            .link(parent.selection().clone());
        let selection = Arc::new(SelectionCell::new(
            selection(&self.provider, "first", "child", 0xff),
            lineage.clone(),
        ));
        let agent_state = self
            .registry
            .prepare_state(
                &identity,
                &selection.current().unwrap(),
                "same-id queue replacement",
                CompletionDelivery::RetainedOnly,
            )
            .await
            .unwrap();
        let parts = self
            .factory
            .build(PreparedScope {
                agent_state: agent_state.clone(),
                identity: identity.clone(),
                history: history.clone(),
                selection: selection.clone(),
                lineage,
                parent_completion: ParentScope::completion(
                    identity.clone(),
                    Some(parent),
                    CompletionDelivery::RetainedOnly,
                    Arc::new(Ids),
                ),
                lifetime: self.registry.lifetime(),
            })
            .await
            .unwrap();
        let scope = bounded(
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
        .unwrap();
        assert!(parent.children().try_add(scope.clone()).unwrap());
        scope
    }
    pub async fn close(&self) {
        bounded(self.registry.shutdown().join()).await.unwrap();
    }
}

pub fn names(queues: &AgentQueues<Security>) -> Vec<String> {
    queues.list().unwrap().into_iter().map(|q| q.name).collect()
}
pub fn push(queues: &AgentQueues<Security>, name: &str, items: &[&str], close: bool) {
    queues
        .push(
            name,
            &items.iter().map(|s| (*s).into()).collect::<Vec<_>>(),
            Direction::Back,
            close,
            &parrot_storage::events::NeverCancelled,
        )
        .unwrap();
}
