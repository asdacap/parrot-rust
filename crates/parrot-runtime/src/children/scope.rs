use super::{
    operation::{first, stopped},
    *,
};
use crate::agent_turn::services::SelectionSource;
use parrot_domain::{
    contracts::security::RestrictableAccessSnapshot, protocol::messages::parrot::Event,
    values::accounting::RetainedAgentReservation,
};
use parrot_storage::conversation::AgentHistoryView;
use std::{
    any::{Any, TypeId},
    collections::HashMap,
    sync::{Arc, Mutex},
};

pub struct ScopeConfig<S: RestrictableAccessSnapshot + 'static> {
    pub authority: Arc<AgentRegistry<S>>,
    pub identity: Arc<AgentIdentity>,
    pub history: AgentHistoryView,
    pub selection: Arc<SelectionCell<S>>,
    pub parent: Option<Arc<Scope<S>>>,
    pub delivery: CompletionDelivery,
    pub retention: Option<RetainedAgentReservation>,
    pub parts: ScopeParts<S>,
}
struct ScopeState {
    publications: Vec<Operation<()>>,
    publication_started: bool,
    close: Option<Operation<()>>,
}
pub struct Scope<S: RestrictableAccessSnapshot + 'static> {
    identity: Arc<AgentIdentity>,
    history: AgentHistoryView,
    selection: Arc<SelectionCell<S>>,
    parts: ScopeParts<S>,
    services: HashMap<TypeId, Arc<dyn Any + Send + Sync>>,
    children: Arc<ChildRegistry<S>>,
    parent: Arc<ParentScope<S>>,
    spawner: Arc<AgentSpawner<S>>,
    retention: Option<RetainedAgentReservation>,
    state: Mutex<ScopeState>,
    diagnostics: Arc<dyn ChildDiagnostics>,
}
impl<S: RestrictableAccessSnapshot + 'static> Scope<S> {
    /// Own construction cleanup even when its observer is immediately dropped.
    pub fn create(config: ScopeConfig<S>) -> Operation<Arc<Self>> {
        Operation::start(async move {
            let mut services = HashMap::new();
            let mut failure = None;
            for service in &config.parts.services {
                if services
                    .insert(service.key, service.value.clone())
                    .is_some()
                {
                    failure = Some(ChildError::registry("service type is already registered"));
                }
            }
            let selected: Arc<dyn SelectionSource<S>> = config.selection.clone();
            if !Arc::ptr_eq(&config.identity, config.parts.session.identity())
                || !Arc::ptr_eq(&selected, config.parts.session.selection())
            {
                failure = Some(ChildError::registry(
                    "session does not match scope identity or selection source",
                ));
            }
            if config.history.agent_session_id() != config.identity.session.session_id {
                failure = Some(ChildError::registry(
                    "history does not match scope identity",
                ));
            }
            let children = ChildRegistry::new(
                config.identity.clone(),
                config.authority.config.validate_child.clone(),
            );
            let scope = Arc::new_cyclic(|owner| {
                let parent = ParentScope::bind(
                    config.identity.clone(),
                    Arc::downgrade(&config.authority),
                    owner.clone(),
                    children.clone(),
                    config.parent.as_ref().map(Arc::downgrade),
                    config.delivery,
                    config.authority.config.ids.clone(),
                );
                let spawner = AgentSpawner::new(
                    owner.clone(),
                    Arc::downgrade(&config.authority),
                    config.authority.lifetime(),
                );
                Self {
                    identity: config.identity,
                    history: config.history,
                    selection: config.selection,
                    parts: config.parts,
                    services,
                    children,
                    parent,
                    spawner,
                    retention: config.retention,
                    diagnostics: config.authority.config.diagnostics.clone(),
                    state: Mutex::new(ScopeState {
                        publications: Vec::new(),
                        publication_started: false,
                        close: None,
                    }),
                }
            });
            if let Err(error) = scope.parent.validate(scope.identity()) {
                failure = Some(error);
            }
            if let Some(error) = failure {
                let cleanup = scope.close_resources(false).join().await;
                return Err(cleanup.err().unwrap_or(error));
            }
            scope.record_diagnostic("created", None);
            Ok(scope)
        })
    }
    pub fn identity(&self) -> &Arc<AgentIdentity> {
        &self.identity
    }
    pub fn history(&self) -> &AgentHistoryView {
        &self.history
    }
    pub fn selection(&self) -> &Arc<SelectionCell<S>> {
        &self.selection
    }
    pub fn session(&self) -> &Arc<dyn ScopedSession<S>> {
        &self.parts.session
    }
    pub fn children(&self) -> &Arc<ChildRegistry<S>> {
        &self.children
    }
    pub fn parent_scope(&self) -> &Arc<ParentScope<S>> {
        &self.parent
    }
    pub fn spawner(&self) -> &Arc<AgentSpawner<S>> {
        &self.spawner
    }
    pub fn get_service<T: ?Sized + Send + Sync + 'static>(&self) -> ChildResult<Arc<T>> {
        self.services
            .get(&TypeId::of::<T>())
            .and_then(|value| value.downcast_ref::<Arc<T>>())
            .cloned()
            .ok_or_else(|| ChildError::registry("service type is not registered"))
    }
    pub fn publish_snapshots(&self) -> ChildResult<()> {
        let mut state = self.state.lock().expect("scope poisoned");
        if state.close.is_some() {
            return Err(stopped());
        }
        if state.publication_started {
            return Ok(());
        }
        for publisher in &self.parts.publishers {
            state.publications.push(publisher.start()?);
        }
        state.publication_started = true;
        Ok(())
    }
    pub fn capture_snapshot_events(&self) -> ChildResult<Vec<Event>> {
        let mut events = Vec::new();
        for publisher in &self.parts.publishers {
            events.extend(publisher.capture()?);
        }
        Ok(events)
    }
    pub async fn settle_work(&self) -> ChildResult<()> {
        let mut result = Ok(());
        for owner in &self.parts.work {
            first(&mut result, owner.settle().await);
        }
        result
    }
    fn record_diagnostic(&self, event: &'static str, error: Option<&ChildError>) {
        self.diagnostics.record(ChildDiagnostic {
            event,
            session_id: self.identity.session.session_id.clone(),
            correlation_id: String::new(),
            duration_milliseconds: None,
            outcome: None,
            error_code: error.map(ChildError::code),
        });
    }
    pub fn close(self: &Arc<Self>) -> Operation<()> {
        self.close_resources(true)
    }
    fn close_resources(self: &Arc<Self>, lifecycle_diagnostics: bool) -> Operation<()> {
        let mut state = self.state.lock().expect("scope poisoned");
        if let Some(close) = &state.close {
            return close.clone();
        }
        let scope = self.clone();
        let publications = state.publications.clone();
        let close = Operation::start(async move {
            let mut result = scope.settle_work().await;
            // Nested source finally stages override earlier failures; collection loops retain first.
            if let Err(error) = scope.parts.session.close().await {
                result = Err(error);
            }
            if let Err(error) = scope.spawner.close().join().await {
                result = Err(error);
            }
            if let Err(error) = scope.children.close().join().await {
                result = Err(error);
            }
            let mut resources = Ok(());
            for resource in &scope.parts.resources {
                first(&mut resources, resource.close().await);
            }
            if let Err(error) = resources {
                result = Err(error);
            }
            let mut final_resources = Ok(());
            for resource in &scope.parts.final_resources {
                let outcome = resource.close().await;
                if let Err(error) = &outcome {
                    scope.record_diagnostic("cleanup_failed", Some(error));
                }
                first(&mut final_resources, outcome);
            }
            if let Err(error) = final_resources {
                result = Err(error);
            }
            if let Some(retention) = &scope.retention {
                retention.release();
            }
            let mut published = Ok(());
            for publication in publications {
                let outcome = publication.join().await;
                if let Err(error) = &outcome {
                    scope.record_diagnostic("cleanup_failed", Some(error));
                }
                first(&mut published, outcome);
            }
            if let Err(error) = published {
                result = Err(error);
            }
            if lifecycle_diagnostics {
                scope.record_diagnostic(
                    if result.is_ok() {
                        "closed"
                    } else {
                        "close_failed"
                    },
                    result.as_ref().err(),
                );
            }
            result
        });
        state.close = Some(close.clone());
        close
    }
}
