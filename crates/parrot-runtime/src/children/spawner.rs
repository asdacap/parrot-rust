use super::{
    operation::{LinkedLifetime, stopped},
    ports::select_child,
    *,
};
use crate::agent_turn::{lifecycle::Signal, services::SessionIdentity};
use parrot_domain::contracts::{async_ops::Cancellation, security::RestrictableAccessSnapshot};
use parrot_foundation::paths::AgentScratchDirectory;
use parrot_provider::routing::aliases::ResolvedModelSelection;
use parrot_storage::conversation::{HistoryForkBoundary, HistoryForkSelection, HistoryForkSource};
use std::{
    sync::{Arc, Mutex, Weak},
    time::Instant,
};

pub enum HistorySource<S: RestrictableAccessSnapshot + 'static> {
    Parent,
    Sibling(Arc<dyn ScopedSession<S>>),
}
pub struct LaunchRequest<S: RestrictableAccessSnapshot + 'static> {
    pub parent: Arc<dyn ScopedSession<S>>,
    pub requested_name: String,
    pub requested_profile: String,
    pub model: ResolvedModelSelection,
    pub requested_scope: String,
    pub delivery: CompletionDelivery,
    pub boundary: HistoryForkBoundary,
    pub fork: HistoryForkSelection,
    pub history_source: HistorySource<S>,
}
struct SpawnerState {
    constructions: Vec<Operation<()>>,
    rejected: Vec<Operation<()>>,
    shutdown: Option<Operation<()>>,
    cleanup_failure: Option<ChildError>,
}
pub struct AgentSpawner<S: RestrictableAccessSnapshot + 'static> {
    owner: Weak<Scope<S>>,
    authority: Weak<AgentRegistry<S>>,
    state: Mutex<SpawnerState>,
    spawn_gate: tokio::sync::Mutex<()>,
    stop: Arc<Signal>,
    lifetime: Arc<dyn Cancellation>,
}
impl<S: RestrictableAccessSnapshot + 'static> AgentSpawner<S> {
    pub(crate) fn new(
        owner: Weak<Scope<S>>,
        authority: Weak<AgentRegistry<S>>,
        parent: Arc<dyn Cancellation>,
    ) -> Arc<Self> {
        let stop = Arc::new(Signal::new());
        let lifetime = Arc::new(LinkedLifetime {
            parent,
            stop: stop.clone(),
        });
        Arc::new(Self {
            owner,
            authority,
            state: Mutex::new(SpawnerState {
                constructions: Vec::new(),
                rejected: Vec::new(),
                shutdown: None,
                cleanup_failure: None,
            }),
            spawn_gate: tokio::sync::Mutex::new(()),
            stop,
            lifetime,
        })
    }
    fn owner(&self) -> ChildResult<Arc<Scope<S>>> {
        self.owner
            .upgrade()
            .ok_or_else(|| ChildError::registry("parent agent scope not found"))
    }
    fn authority(&self) -> ChildResult<Arc<AgentRegistry<S>>> {
        self.authority.upgrade().ok_or_else(stopped)
    }
    fn ensure_accepting(&self) -> ChildResult<()> {
        if self
            .state
            .lock()
            .expect("spawner poisoned")
            .shutdown
            .is_some()
            || !self.authority()?.is_accepting()
            || !self.owner()?.children().is_accepting()
        {
            Err(stopped())
        } else {
            Ok(())
        }
    }
    fn start(
        self: &Arc<Self>,
        work: impl FnOnce(
            Arc<Self>,
        ) -> parrot_domain::contracts::async_ops::BoxFuture<
            'static,
            ChildResult<Arc<Scope<S>>>,
        > + Send
        + 'static,
    ) -> ChildResult<Operation<Arc<Scope<S>>>> {
        let authority = self.authority()?;
        let mut state = self.state.lock().expect("spawner poisoned");
        if state.shutdown.is_some() {
            return Err(stopped());
        }
        let spawner = self.clone();
        let operation = authority.start_construction(|| Operation::start(work(spawner)))?;
        let observer = operation.clone();
        state.constructions.push(Operation::start(async move {
            let _ = observer.join().await;
            Ok(())
        }));
        Ok(operation)
    }
    pub fn spawn(
        self: &Arc<Self>,
        request: LaunchRequest<S>,
    ) -> ChildResult<Operation<Arc<Scope<S>>>> {
        self.start(move |spawner| {
            Box::pin(async move {
                let mut conflicts = 0;
                loop {
                    match spawner.spawn_candidate(&request, conflicts).await {
                        Err(ChildError::NameConflict(_)) => conflicts += 1,
                        result => return result,
                    }
                }
            })
        })
    }
    pub fn get_or_spawn(
        self: &Arc<Self>,
        requested_name: String,
        select_launch: impl FnOnce() -> ChildResult<LaunchRequest<S>> + Send + 'static,
    ) -> ChildResult<Operation<Arc<Scope<S>>>> {
        self.start(move |spawner| {
            Box::pin(async move {
                let _serial = spawner.spawn_gate.lock().await;
                spawner.ensure_accepting()?;
                let name = sanitize(&requested_name);
                if let Some(existing) = spawner.owner()?.children().find(&name) {
                    return Ok(existing);
                }
                let request = select_launch()?;
                match spawner.spawn_candidate(&request, 0).await {
                    Err(ChildError::NameConflict(_)) => spawner.owner()?.children().resolve(&name),
                    result => result,
                }
            })
        })
    }
    pub fn spawn_or_resume(
        self: &Arc<Self>,
        request: LaunchRequest<S>,
    ) -> ChildResult<Operation<Arc<Scope<S>>>> {
        let spawner = self.clone();
        Ok(Operation::start(async move {
            let _serial = spawner.spawn_gate.lock().await;
            let name = sanitize(&request.requested_name);
            if name.is_empty() {
                return Err(ChildError::registry(
                    "child agent name must contain at least one letter or digit",
                ));
            }
            if let Some(existing) = spawner.owner()?.children().find(&name) {
                return spawner.resume(existing, &request);
            }
            // Only fresh construction enters lifetime admission; retained resume
            // matches the source busy-check/selection-update branch after stop.
            spawner
                .start(move |spawner| {
                    Box::pin(async move {
                        match spawner.spawn_candidate(&request, 0).await {
                            Err(ChildError::NameConflict(_)) => spawner
                                .resume(spawner.owner()?.children().resolve(&name)?, &request),
                            result => result,
                        }
                    })
                })?
                .join()
                .await
        }))
    }
    fn resume(
        &self,
        scope: Arc<Scope<S>>,
        request: &LaunchRequest<S>,
    ) -> ChildResult<Arc<Scope<S>>> {
        if scope.session().is_active() {
            return Err(ChildError::registry(format!(
                "child agent '{}' is busy; wait for it to finish or use agent_send",
                scope.identity().session.name
            )));
        }
        let authority = self.authority()?;
        let profile = authority
            .config
            .profiles
            .resolve_child(&request.requested_profile)?;
        let selection = select_child(
            scope.selection().lineage(),
            profile,
            request.model.clone(),
            authority.config.agent_limits,
        )?;
        scope.session().update_selection(selection)?;
        Ok(scope)
    }
    async fn spawn_candidate(
        &self,
        request: &LaunchRequest<S>,
        conflicts: usize,
    ) -> ChildResult<Arc<Scope<S>>> {
        let owner = self.owner()?;
        if !Arc::ptr_eq(request.parent.identity(), owner.identity()) {
            return Err(ChildError::registry(format!(
                "launch parent does not match child registry owner: expected {}, actual {}",
                owner.identity().session.session_id,
                request.parent.identity().session.session_id
            )));
        }
        let depth = owner.identity().session.depth + 1;
        let authority = self.authority()?;
        if !authority.config.agent_limits.disable_recursion_limit && depth > 4 {
            return Err(ChildError::registry("subagent depth limit reached"));
        }
        let parent = owner.parent_scope().require_owner()?;
        let profile = authority
            .config
            .profiles
            .resolve_child(&request.requested_profile)?;
        let reservation = authority
            .config
            .retained
            .reserve()
            .map_err(|error| ChildError::registry(error.to_string()))?;
        let mut history = None;
        let mut constructed: Option<Arc<Scope<S>>> = None;
        let result: ChildResult<Arc<Scope<S>>> = async {
            self.ensure_accepting()?;
            let lineage = owner.selection().lineage().link(owner.selection().clone());
            let selection = select_child(&lineage, profile, request.model.clone(), authority.config.agent_limits)?;
            let session_id = authority.config.ids.agent_session()?;
            let name = select_name(&request.requested_name, &session_id, conflicts);
            if AgentScratchDirectory::RESERVED_NAMES.contains(&name.as_str()) { return Err(ChildError::registry(format!("child agent name '{name}' is reserved"))); }
            if owner.children().find(&name).is_some() { return Err(ChildError::NameConflict(format!("child agent name is already registered: {name}"))); }
            let mut name_path = owner.identity().name_path.clone(); name_path.push(name.clone());
            let identity = Arc::new(AgentIdentity { session: SessionIdentity { session_id, name: name.clone(), parent_session_id: owner.identity().session.session_id.clone(), depth }, parent_name: owner.identity().session.name.clone(), name_path, prompt_scope: owner.identity().prompt_scope.derive_child(&name, depth, &request.requested_scope), templates: owner.identity().templates.clone() });
            let (source, boundary, fallback) = match &request.history_source {
                HistorySource::Parent => (owner.identity().session.session_id.clone(), request.boundary.clone(), None),
                HistorySource::Sibling(sibling) => {
                    if sibling.identity().session.parent_session_id != owner.identity().session.session_id || !authority.snapshot_scopes().iter().any(|scope| Arc::ptr_eq(scope.session(), sibling)) { return Err(ChildError::registry("history source must be a registered sibling under the child's owning parent")); }
                    (sibling.identity().session.session_id.clone(), HistoryForkBoundary::AfterSafeHistoryPrefix, Some(HistoryForkSource { agent_session_id: owner.identity().session.session_id.clone(), boundary: request.boundary.clone() }))
                }
            };
            let prepared_history = authority.config.histories.prepare(&identity).await?;
            history = Some(prepared_history.clone());
            prepared_history.repository().initialize_forked_agent_history(source, identity.session.session_id.clone(), boundary, request.fork.clone(), fallback).await?;
            let selection = Arc::new(SelectionCell::new(selection, lineage.clone()));
            let callback = ParentScope::completion(identity.clone(), Some(&parent), request.delivery, authority.config.ids.clone());
            let started = Instant::now();
            authority.config.diagnostics.record(ChildDiagnostic { event: "child_scope_start", session_id: identity.session.session_id.clone(), correlation_id: identity.session.parent_session_id.clone(), duration_milliseconds: None, outcome: None, error_code: None });
            let parts = authority.config.factory.create(PreparedScope { identity: identity.clone(), history: prepared_history.clone(), selection: selection.clone(), lineage, parent_completion: callback, lifetime: self.lifetime.clone() }).await;
            let built = async { Scope::create(ScopeConfig { authority: authority.clone(), identity: identity.clone(), history: prepared_history, selection, parent: Some(parent.clone()), delivery: request.delivery, retention: Some(reservation.clone()), parts: parts? }).join().await }.await;
            authority.config.diagnostics.record(ChildDiagnostic { event: "child_scope_completed", session_id: identity.session.session_id.clone(), correlation_id: identity.session.parent_session_id.clone(), duration_milliseconds: Some(started.elapsed().as_millis().min(u128::from(u64::MAX)) as u64), outcome: Some(match &built { Ok(_) => "succeeded", Err(ChildError::Cancelled) => "cancelled", Err(_) => "failed" }), error_code: built.as_ref().err().map(ChildError::code) });
            let scope = built?;
            constructed = Some(scope.clone());
            self.ensure_accepting()?;
            reservation.commit();
            if !owner.children().try_add(scope.clone())? { return Err(stopped()); }
            Ok(scope)
        }.await;
        if result.is_err() {
            reservation.rollback();
            if let Some(scope) = constructed {
                let rejected = scope.close();
                self.state
                    .lock()
                    .expect("spawner poisoned")
                    .rejected
                    .push(rejected.clone());
                authority.track_rejected(rejected);
            }
            if let Some(history) = history {
                if let Err(error) = history
                    .repository()
                    .cleanup_forked_agent_history(history.agent_session_id().to_owned())
                    .await
                {
                    self.record_cleanup(&authority, error.into());
                }
            }
        }
        result
    }
    fn record_cleanup(&self, authority: &AgentRegistry<S>, error: ChildError) {
        self.state
            .lock()
            .expect("spawner poisoned")
            .cleanup_failure
            .get_or_insert(error.clone());
        authority.record_cleanup_failure(error);
    }
    pub fn close(self: &Arc<Self>) -> Operation<()> {
        let mut state = self.state.lock().expect("spawner poisoned");
        if let Some(shutdown) = &state.shutdown {
            return shutdown.clone();
        }
        self.stop.cancel();
        let constructions = state.constructions.clone();
        let spawner = self.clone();
        let operation = Operation::start(async move {
            for construction in constructions {
                let _ = construction.join().await;
            }
            let rejected = spawner
                .state
                .lock()
                .expect("spawner poisoned")
                .rejected
                .clone();
            let mut result = Ok(());
            for rejection in rejected {
                super::operation::first(&mut result, rejection.join().await);
            }
            if let Some(error) = spawner
                .state
                .lock()
                .expect("spawner poisoned")
                .cleanup_failure
                .clone()
            {
                super::operation::first(&mut result, Err(error));
            }
            result
        });
        state.shutdown = Some(operation.clone());
        operation
    }
}
fn sanitize(name: &str) -> String {
    let mut output = String::new();
    for character in name.chars() {
        if character.is_ascii_alphanumeric() {
            output.push(character.to_ascii_lowercase());
        } else if !output.is_empty() && !output.ends_with('-') {
            output.push('-');
        }
    }
    output.trim_end_matches('-').to_owned()
}
fn select_name(requested: &str, id: &str, conflicts: usize) -> String {
    let basis = sanitize(requested);
    if basis.is_empty() {
        let suffix: String = id
            .chars()
            .rev()
            .take(6)
            .collect::<String>()
            .chars()
            .rev()
            .collect();
        format!("agent-{suffix}")
    } else if conflicts == 0 {
        basis
    } else {
        format!("{basis}-{}", conflicts + 1)
    }
}
