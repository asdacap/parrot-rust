use super::{
    operation::{LinkedLifetime, first, stopped},
    *,
};
use crate::agent_turn::lifecycle::Signal;
use parrot_domain::{
    contracts::{async_ops::Cancellation, security::RestrictableAccessSnapshot},
    values::accounting::RetainedAgentBudget,
};
use parrot_foundation::config::runtime::AgentLimitsConfig;
use std::sync::{Arc, Mutex};

pub type ChildAdmission<S> = Arc<dyn Fn(&Arc<Scope<S>>) -> ChildResult<()> + Send + Sync>;

pub struct RegistryConfig<S: RestrictableAccessSnapshot + 'static> {
    pub agent_limits: AgentLimitsConfig,
    pub profiles: Arc<dyn ProfileResolver<S>>,
    pub factory: Arc<dyn ScopeFactory<S>>,
    pub histories: Arc<dyn HistoryPreparer>,
    pub states: Arc<dyn AgentStatePreparer>,
    pub router: Arc<parrot_provider::routing::aliases::ModelRouter>,
    pub ids: Arc<dyn IdGenerator>,
    pub diagnostics: Arc<dyn ChildDiagnostics>,
    pub retained: RetainedAgentBudget,
    pub lifetime: Arc<dyn Cancellation>,
    pub validate_child: ChildAdmission<S>,
}
struct RegistryState<S: RestrictableAccessSnapshot + 'static> {
    roots: Vec<Arc<Scope<S>>>,
    constructions: Vec<Operation<()>>,
    rejected: Vec<Operation<()>>,
    freeze: Option<Operation<()>>,
    shutdown: Option<Operation<()>>,
    cleanup_failure: Option<ChildError>,
}
pub struct AgentRegistry<S: RestrictableAccessSnapshot + 'static> {
    pub(crate) config: RegistryConfig<S>,
    state: Mutex<RegistryState<S>>,
    stop: Arc<Signal>,
    lifetime: Arc<dyn Cancellation>,
}
impl<S: RestrictableAccessSnapshot + 'static> AgentRegistry<S> {
    pub fn new(config: RegistryConfig<S>) -> Arc<Self> {
        let stop = Arc::new(Signal::new());
        let lifetime = Arc::new(LinkedLifetime {
            parent: config.lifetime.clone(),
            stop: stop.clone(),
        });
        Arc::new(Self {
            config,
            state: Mutex::new(RegistryState {
                roots: Vec::new(),
                constructions: Vec::new(),
                rejected: Vec::new(),
                freeze: None,
                shutdown: None,
                cleanup_failure: None,
            }),
            stop,
            lifetime,
        })
    }
    pub fn lifetime(&self) -> Arc<dyn Cancellation> {
        self.lifetime.clone()
    }
    pub fn is_accepting(&self) -> bool {
        let state = self.state.lock().expect("registry poisoned");
        state.shutdown.is_none() && state.freeze.is_none()
    }
    pub(crate) fn ensure_admission(&self) -> ChildResult<()> {
        let state = self.state.lock().expect("registry poisoned");
        if state.shutdown.is_some() {
            Err(stopped())
        } else if state.freeze.is_some() {
            Err(ChildError::Suspended)
        } else {
            Ok(())
        }
    }
    pub fn register_root(&self, scope: Arc<Scope<S>>) -> ChildResult<()> {
        let identity = scope.identity();
        if identity.session.depth != 0
            || !identity.session.parent_session_id.is_empty()
            || scope.parent_scope().has_parent()
        {
            return Err(ChildError::registry(
                "only the root agent scope may be registered directly",
            ));
        }
        let mut state = self.state.lock().expect("registry poisoned");
        if state.shutdown.is_some() {
            return Err(stopped());
        }
        if state.freeze.is_some() {
            return Err(ChildError::Suspended);
        }
        if state.roots.iter().any(|registered| {
            registered.identity().session.session_id == identity.session.session_id
                || Arc::ptr_eq(registered.session(), scope.session())
        }) {
            return Err(ChildError::registry(format!(
                "agent scope identity is already registered: {}",
                identity.session.session_id
            )));
        }
        state.roots.push(scope.clone());
        if let Err(error) = scope.publish_snapshots() {
            state.roots.pop();
            return Err(error);
        }
        Ok(())
    }
    pub fn unregister_root(&self, scope: &Arc<Scope<S>>) -> ChildResult<()> {
        let mut state = self.state.lock().expect("registry poisoned");
        let index = state
            .roots
            .iter()
            .position(|registered| {
                Arc::ptr_eq(registered, scope) && scope.identity().session.depth == 0
            })
            .ok_or_else(|| {
                ChildError::registry(format!(
                    "agent scope not found: {}",
                    scope.identity().session.session_id
                ))
            })?;
        state.roots.remove(index);
        Ok(())
    }
    pub fn snapshot_roots(&self) -> Vec<Arc<Scope<S>>> {
        let state = self.state.lock().expect("registry poisoned");
        if state.shutdown.is_some() {
            Vec::new()
        } else {
            state.roots.clone()
        }
    }
    /// Internal authority topology, not a tool-visible resource enumeration grant.
    pub fn snapshot_scopes(&self) -> Vec<Arc<Scope<S>>> {
        fn append<S: RestrictableAccessSnapshot + 'static>(
            scope: Arc<Scope<S>>,
            output: &mut Vec<Arc<Scope<S>>>,
        ) {
            output.push(scope.clone());
            for child in scope.children().snapshot_children() {
                append(child, output);
            }
        }
        let mut output = Vec::new();
        for root in self.snapshot_roots() {
            append(root, &mut output);
        }
        output
    }
    pub fn contains(&self, candidate: &Arc<Scope<S>>) -> bool {
        self.snapshot_roots().iter().any(|root| {
            Arc::ptr_eq(root, candidate) || root.children().contains_descendant(candidate)
        })
    }
    pub fn active_snapshot(&self) -> Vec<crate::agent_turn::services::SessionIdentity> {
        let mut active: Vec<_> = self
            .snapshot_scopes()
            .into_iter()
            .filter(|scope| scope.identity().session.depth != 0 && scope.session().is_active())
            .map(|scope| scope.identity().session.clone())
            .collect();
        active.sort_by(|a, b| a.session_id.cmp(&b.session_id));
        active
    }
    pub(crate) fn start_construction(
        &self,
        launch: impl FnOnce() -> Operation<Arc<Scope<S>>>,
    ) -> ChildResult<Operation<Arc<Scope<S>>>> {
        let mut state = self.state.lock().expect("registry poisoned");
        if state.shutdown.is_some() {
            return Err(stopped());
        }
        if state.freeze.is_some() {
            return Err(ChildError::Suspended);
        }
        let operation = launch();
        let observer = operation.clone();
        state.constructions.push(Operation::start(async move {
            let _ = observer.join().await;
            Ok(())
        }));
        Ok(operation)
    }
    pub(crate) fn track_rejected(&self, operation: Operation<()>) {
        self.state
            .lock()
            .expect("registry poisoned")
            .rejected
            .push(operation);
    }
    pub(crate) fn record_cleanup_failure(&self, error: ChildError) {
        self.state
            .lock()
            .expect("registry poisoned")
            .cleanup_failure
            .get_or_insert(error);
    }
    /// Stops new construction and joins accepted admissions without canceling session work.
    pub fn freeze_admissions(self: &Arc<Self>) -> Operation<()> {
        let mut state = self.state.lock().expect("registry poisoned");
        if let Some(freeze) = &state.freeze {
            return freeze.clone();
        }
        let constructions = state.constructions.clone();
        let registry = self.clone();
        let freeze = Operation::start(async move {
            let mut result = Ok(());
            for construction in constructions {
                first(&mut result, construction.join().await);
            }
            let rejected = registry
                .state
                .lock()
                .expect("registry poisoned")
                .rejected
                .clone();
            for rejection in rejected {
                first(&mut result, rejection.join().await);
            }
            if let Some(error) = registry
                .state
                .lock()
                .expect("registry poisoned")
                .cleanup_failure
                .clone()
            {
                first(&mut result, Err(error));
            }
            result
        });
        state.freeze = Some(freeze.clone());
        freeze
    }
    pub fn shutdown(self: &Arc<Self>) -> Operation<()> {
        let mut state = self.state.lock().expect("registry poisoned");
        if let Some(shutdown) = &state.shutdown {
            return shutdown.clone();
        }
        self.stop.cancel();
        let roots = state.roots.clone();
        let constructions = state.constructions.clone();
        let registry = self.clone();
        let operation = Operation::start(async move {
            let mut result = Ok(());
            for root in roots {
                first(&mut result, root.close().join().await);
            }
            for construction in constructions {
                first(&mut result, construction.join().await);
            }
            let rejected = registry
                .state
                .lock()
                .expect("registry poisoned")
                .rejected
                .clone();
            for rejection in rejected {
                first(&mut result, rejection.join().await);
            }
            if let Some(error) = registry
                .state
                .lock()
                .expect("registry poisoned")
                .cleanup_failure
                .clone()
            {
                first(&mut result, Err(error));
            }
            result
        });
        state.shutdown = Some(operation.clone());
        operation
    }
}

struct ChildrenState<S: RestrictableAccessSnapshot + 'static> {
    children: Vec<Arc<Scope<S>>>,
    shutdown: Option<Operation<()>>,
}
pub struct ChildRegistry<S: RestrictableAccessSnapshot + 'static> {
    owner: Arc<AgentIdentity>,
    validate: ChildAdmission<S>,
    state: Mutex<ChildrenState<S>>,
}
impl<S: RestrictableAccessSnapshot + 'static> ChildRegistry<S> {
    pub fn new(owner: Arc<AgentIdentity>, validate: ChildAdmission<S>) -> Arc<Self> {
        Arc::new(Self {
            owner,
            validate,
            state: Mutex::new(ChildrenState {
                children: Vec::new(),
                shutdown: None,
            }),
        })
    }
    pub fn is_accepting(&self) -> bool {
        self.state
            .lock()
            .expect("children poisoned")
            .shutdown
            .is_none()
    }
    pub fn find(&self, name: &str) -> Option<Arc<Scope<S>>> {
        let state = self.state.lock().expect("children poisoned");
        if state.shutdown.is_some() {
            None
        } else {
            state
                .children
                .iter()
                .find(|child| child.identity().session.name == name)
                .cloned()
        }
    }
    pub fn resolve(&self, name: &str) -> ChildResult<Arc<Scope<S>>> {
        self.find(name)
            .ok_or_else(|| ChildError::registry(format!("child agent not found: {name}")))
    }
    pub fn try_add(&self, scope: Arc<Scope<S>>) -> ChildResult<bool> {
        let mut state = self.state.lock().expect("children poisoned");
        if !self.check_admission(&state, &scope)? {
            return Ok(false);
        }
        (self.validate)(&scope)?;
        Self::check_name(&state, &scope)?;
        Self::insert(&mut state, scope)
    }
    pub(crate) fn prepare_admission(&self, scope: &Arc<Scope<S>>) -> ChildResult<bool> {
        let state = self.state.lock().expect("children poisoned");
        if !self.check_admission(&state, scope)? {
            return Ok(false);
        }
        (self.validate)(scope)?;
        Self::check_name(&state, scope)?;
        Ok(true)
    }
    pub(crate) fn admit_prepared(&self, scope: Arc<Scope<S>>) -> ChildResult<bool> {
        let mut state = self.state.lock().expect("children poisoned");
        if !self.check_admission(&state, &scope)? {
            return Ok(false);
        }
        Self::check_name(&state, &scope)?;
        Self::insert(&mut state, scope)
    }
    fn check_admission(
        &self,
        state: &ChildrenState<S>,
        scope: &Arc<Scope<S>>,
    ) -> ChildResult<bool> {
        if state.shutdown.is_some() {
            return Ok(false);
        }
        if scope.identity().session.parent_session_id != self.owner.session.session_id {
            return Err(ChildError::registry(format!(
                "parent agent scope not found: {}",
                self.owner.session.session_id
            )));
        }
        Ok(true)
    }
    fn check_name(state: &ChildrenState<S>, scope: &Arc<Scope<S>>) -> ChildResult<()> {
        if state
            .children
            .iter()
            .any(|registered| registered.identity().session.name == scope.identity().session.name)
        {
            return Err(ChildError::NameConflict(format!(
                "child agent name is already registered: {}",
                scope.identity().session.name
            )));
        }
        Ok(())
    }
    fn insert(state: &mut ChildrenState<S>, scope: Arc<Scope<S>>) -> ChildResult<bool> {
        state.children.push(scope.clone());
        if let Err(error) = scope.publish_snapshots() {
            state.children.pop();
            return Err(error);
        }
        Ok(true)
    }
    /// Structural removal only. Durable retained-child removal must use the owning spawner's retire operation.
    pub fn detach(&self, scope: &Arc<Scope<S>>) -> ChildResult<Option<Arc<Scope<S>>>> {
        let mut state = self.state.lock().expect("children poisoned");
        if let Some(index) = state
            .children
            .iter()
            .position(|registered| Arc::ptr_eq(registered, scope))
        {
            return Ok(Some(state.children.remove(index)));
        }
        if state.shutdown.is_some() {
            Ok(None)
        } else {
            Err(ChildError::registry(format!(
                "child agent scope not found: {}",
                scope.identity().session.session_id
            )))
        }
    }
    /// Borrows the current children while holding the same gate used by admission.
    ///
    /// The callback must not reenter this registry (including its other methods):
    /// the gate is non-reentrant. Callers nesting different registries must retain
    /// their ordinary parent-before-own ordering and acquire a root's own gate once.
    pub fn with_children<R>(&self, callback: impl FnOnce(&[Arc<Scope<S>>]) -> R) -> R {
        let state = self.state.lock().expect("children poisoned");
        callback(if state.shutdown.is_some() {
            &[]
        } else {
            &state.children
        })
    }
    pub fn snapshot_children(&self) -> Vec<Arc<Scope<S>>> {
        let state = self.state.lock().expect("children poisoned");
        if state.shutdown.is_some() {
            Vec::new()
        } else {
            state.children.clone()
        }
    }
    pub fn contains_descendant(&self, candidate: &Arc<Scope<S>>) -> bool {
        self.snapshot_children().iter().any(|child| {
            Arc::ptr_eq(child, candidate) || child.children().contains_descendant(candidate)
        })
    }
    pub fn snapshot_descendants(&self) -> Vec<Arc<dyn ScopedSession<S>>> {
        let mut output = Vec::new();
        for child in self.snapshot_children() {
            output.push(child.session().clone());
            output.extend(child.children().snapshot_descendants());
        }
        output
    }
    pub fn close(&self) -> Operation<()> {
        let mut state = self.state.lock().expect("children poisoned");
        if let Some(shutdown) = &state.shutdown {
            return shutdown.clone();
        }
        let children = std::mem::take(&mut state.children);
        let operation = Operation::start(async move {
            let mut result = Ok(());
            for child in children.into_iter().rev() {
                first(&mut result, child.close().join().await);
            }
            result
        });
        state.shutdown = Some(operation.clone());
        operation
    }
}
