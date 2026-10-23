use super::{AgentIdentity, AgentRegistry, ChildError, ChildResult, CompletionDelivery};
use crate::agent_turn::services::TurnSelection;
use parrot_domain::values::identifiers::UserSessionId;
use parrot_domain::{
    contracts::{
        agent_state::{AgentStateErrorKind, AgentStateStore},
        async_ops::BoxFuture,
        paths::SelectedSessionRoot,
        security::RestrictableAccessSnapshot,
    },
    values::agent_state::{
        AgentDescriptor, AgentStateSnapshot, AgentStateUpdate, CompletionDeliverySnapshot,
        SelectionSnapshot,
    },
};
use parrot_foundation::paths::UserSessionResources;
use parrot_storage::agent_state::FileAgentStateStore;
use std::sync::Arc;

/// Opens one agent's state beneath the selected user-session's agent directory.
pub trait AgentStatePreparer: Send + Sync {
    /// The expected identity binds the opened store; mismatched incarnations are not replaced.
    fn open<'a>(
        &'a self,
        name_path: &'a [String],
        session_id: &'a str,
    ) -> BoxFuture<'a, ChildResult<Arc<dyn AgentStateStore>>>;
}

pub struct AgentStateFiles {
    root: SelectedSessionRoot,
}
impl AgentStateFiles {
    pub fn new(root: SelectedSessionRoot) -> Self {
        Self { root }
    }
}
impl AgentStatePreparer for AgentStateFiles {
    fn open<'a>(
        &'a self,
        name_path: &'a [String],
        session_id: &'a str,
    ) -> BoxFuture<'a, ChildResult<Arc<dyn AgentStateStore>>> {
        Box::pin(async move {
            let id = self
                .root
                .path()
                .file_name()
                .and_then(|name| name.to_str())
                .ok_or_else(|| {
                    ChildError::registry("selected session root has no valid session name")
                })?;
            let id = UserSessionId::parse(id)
                .map_err(|error| ChildError::registry(error.to_string()))?;
            let resources = UserSessionResources::new(self.root.clone(), id, ())
                .map_err(|error| ChildError::failure("io", error.to_string()))?;
            let names: Vec<_> = name_path.iter().map(String::as_str).collect();
            let scratch = resources
                .agent_scratch(&names)
                .map_err(|error| ChildError::failure("io", error.to_string()))?;
            let store: Arc<dyn AgentStateStore> = Arc::new(
                FileAgentStateStore::open(scratch.root(), session_id.to_owned())
                    .await
                    .map_err(state_error)?,
            );
            Ok(store)
        })
    }
}

impl<S: RestrictableAccessSnapshot + 'static> AgentRegistry<S> {
    /// Initializes a new scope's descriptor before construction. Existing owner state is retained.
    pub async fn prepare_state(
        &self,
        identity: &AgentIdentity,
        selection: &TurnSelection<S>,
        local_scope: &str,
        delivery: CompletionDelivery,
    ) -> ChildResult<Arc<dyn AgentStateStore>> {
        let store = self
            .config
            .states
            .open(&identity.name_path, &identity.session.session_id)
            .await?;
        let descriptor = AgentDescriptor {
            parent_session_id: if identity.session.parent_session_id.is_empty() {
                None
            } else {
                Some(identity.session.parent_session_id.clone())
            },
            name: identity.session.name.clone(),
            selection: SelectionSnapshot {
                profile_id: selection.profile.id.clone(),
                model_selector: selection.model.requested_selector.value.clone(),
            },
            local_scope: local_scope.to_owned(),
            completion_delivery: match delivery {
                CompletionDelivery::Automatic => CompletionDeliverySnapshot::Automatic,
                CompletionDelivery::RetainedOnly => CompletionDeliverySnapshot::RetainedOnly,
            },
        };
        match store.load().await.map_err(state_error)? {
            None => store
                .initialize(AgentStateSnapshot::new(
                    identity.session.session_id.clone(),
                    descriptor,
                ))
                .await
                .map_err(state_error)?,
            Some(snapshot) => {
                if snapshot.descriptor.parent_session_id != descriptor.parent_session_id
                    || snapshot.descriptor.name != descriptor.name
                {
                    return Err(ChildError::registry(
                        "agent snapshot descriptor does not match scope identity",
                    ));
                }
                if snapshot.descriptor != descriptor {
                    store
                        .update(AgentStateUpdate::Descriptor(descriptor))
                        .await
                        .map_err(state_error)?;
                }
            }
        }
        Ok(store)
    }
}

pub(crate) fn state_error(
    error: parrot_domain::contracts::agent_state::AgentStateError,
) -> ChildError {
    if error.kind == AgentStateErrorKind::OwnerMismatch {
        ChildError::NameConflict(format!(
            "agent directory belongs to another retained incarnation: {error}"
        ))
    } else {
        ChildError::failure("io", error.to_string())
    }
}
