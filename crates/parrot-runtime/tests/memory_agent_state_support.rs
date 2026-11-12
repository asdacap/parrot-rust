//! In-memory agent state for tests that never reopen the session.
use parrot_domain::contracts::agent_state::AgentStateStore;
use parrot_domain::values::agent_state::{
    AgentDescriptor, AgentStateSnapshot, AgentStateUpdate, CompletionDeliverySnapshot,
    SelectionSnapshot,
};

/// Starts initialized with an empty snapshot, matching a session with a store.
pub struct MemoryAgentStateStore(pub std::sync::Mutex<Option<AgentStateSnapshot>>);
impl Default for MemoryAgentStateStore {
    fn default() -> Self {
        Self(std::sync::Mutex::new(Some(AgentStateSnapshot::new(
            "test".into(),
            AgentDescriptor {
                parent_session_id: None,
                name: "test".into(),
                selection: SelectionSnapshot {
                    profile_id: "mode".into(),
                    model_selector: "scripted/model".into(),
                },
                local_scope: String::new(),
                completion_delivery: CompletionDeliverySnapshot::Automatic,
            },
        ))))
    }
}
impl AgentStateStore for MemoryAgentStateStore {
    fn load(
        &self,
    ) -> parrot_domain::contracts::async_ops::BoxFuture<
        '_,
        Result<Option<AgentStateSnapshot>, parrot_domain::contracts::agent_state::AgentStateError>,
    > {
        Box::pin(async { Ok(self.0.lock().unwrap().clone()) })
    }
    fn initialize(
        &self,
        snapshot: AgentStateSnapshot,
    ) -> parrot_domain::contracts::async_ops::BoxFuture<
        '_,
        Result<(), parrot_domain::contracts::agent_state::AgentStateError>,
    > {
        use parrot_domain::contracts::agent_state::{AgentStateError, AgentStateErrorKind};
        Box::pin(async move {
            let mut slot = self.0.lock().unwrap();
            if slot.is_some() {
                return Err(AgentStateError::new(
                    AgentStateErrorKind::AlreadyExists,
                    "snapshot already initialized",
                ));
            }
            *slot = Some(snapshot);
            Ok(())
        })
    }
    fn update(
        &self,
        update: AgentStateUpdate,
    ) -> parrot_domain::contracts::async_ops::BoxFuture<
        '_,
        Result<AgentStateSnapshot, parrot_domain::contracts::agent_state::AgentStateError>,
    > {
        use parrot_domain::contracts::agent_state::{AgentStateError, AgentStateErrorKind};
        Box::pin(async move {
            let mut slot = self.0.lock().unwrap();
            let snapshot = slot.as_mut().ok_or_else(|| {
                AgentStateError::new(AgentStateErrorKind::NotFound, "no initialized snapshot")
            })?;
            match update {
                AgentStateUpdate::Descriptor(descriptor) => snapshot.descriptor = descriptor,
                AgentStateUpdate::Children(children) => snapshot.children = children,
                AgentStateUpdate::Tasks(tasks) => snapshot.tasks = tasks,
                AgentStateUpdate::Execution(execution) => snapshot.execution = execution,
                AgentStateUpdate::Core(state) => snapshot.core = Some(state),
            }
            snapshot.revision += 1;
            Ok(snapshot.clone())
        })
    }
    fn retire(
        &self,
    ) -> parrot_domain::contracts::async_ops::BoxFuture<
        '_,
        Result<(), parrot_domain::contracts::agent_state::AgentStateError>,
    > {
        Box::pin(async {
            *self.0.lock().unwrap() = None;
            Ok(())
        })
    }
}
