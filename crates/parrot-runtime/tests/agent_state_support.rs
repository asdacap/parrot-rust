//! Opens an initialized per-test agent recovery store in an existing-style directory.
use parrot_domain::contracts::agent_state::AgentStateStore;
use parrot_domain::values::agent_state::{
    AgentDescriptor, AgentStateSnapshot, CompletionDeliverySnapshot, SelectionSnapshot,
};
use parrot_storage::agent_state::FileAgentStateStore;
use std::path::Path;
use std::sync::Arc;

/// Opens a file-backed recovery store under `directory`, initializing it when absent.
pub async fn test_agent_state_store(directory: &Path, owner: &str) -> Arc<dyn AgentStateStore> {
    use parrot_foundation::paths::AgentScratchDirectory;
    std::fs::create_dir_all(directory).unwrap();
    AgentScratchDirectory::new(directory, directory).unwrap();
    let store = Arc::new(
        FileAgentStateStore::open(directory, owner.into())
            .await
            .unwrap(),
    );
    if store.load().await.unwrap().is_none() {
        store
            .initialize(AgentStateSnapshot::new(
                owner.into(),
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
            ))
            .await
            .unwrap();
    }
    store
}
