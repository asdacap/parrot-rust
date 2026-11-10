use super::{SessionIndex, SessionMeta};
use crate::{conversation::PendingStatus, ownership::SessionModelSelection};
use parrot_domain::{
    contracts::paths::{RootOrigin, SelectedSessionRoot},
    values::{identifiers::UserSessionId, lineage::AgentLineageRecord},
};
use std::{
    fs,
    path::PathBuf,
    sync::atomic::{AtomicU64, Ordering},
};

static NEXT: AtomicU64 = AtomicU64::new(0);

struct Fixture {
    directory: PathBuf,
    index: SessionIndex,
}

impl Fixture {
    fn new() -> Self {
        let directory = std::env::temp_dir().join(format!(
            "session-identity-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed),
        ));
        let workspace = directory.join("workspace");
        fs::create_dir_all(&workspace).unwrap();
        let root =
            SelectedSessionRoot::new(directory.join("session"), RootOrigin::Current).unwrap();
        let index =
            SessionIndex::new(root, UserSessionId::parse("session").unwrap(), &workspace).unwrap();
        let mut metadata =
            SessionMeta::new("session", workspace.to_string_lossy(), "provider", "model");
        metadata.root_agent_name = Some("main".into());
        metadata.mode = Some("query".into());
        index.initialize(metadata, "root".into()).unwrap();
        Self { directory, index }
    }

    fn reopen(&self) -> SessionIndex {
        SessionIndex::new(
            self.index.selected_root().clone(),
            UserSessionId::parse("session").unwrap(),
            self.directory.join("workspace"),
        )
        .unwrap()
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.directory);
    }
}

#[test]
fn canonical_identity_cannot_be_reinitialized_and_survives_reopen() {
    let fixture = Fixture::new();
    let original = fs::read(fixture.index.metadata_path()).unwrap();
    let state = fixture.index.read_session_state().unwrap();
    assert_eq!(state.root_agent_session_id, "root");
    assert_eq!(state.root_agent_name, "main");
    assert_eq!(
        state.mode,
        PendingStatus {
            mode: "query".into(),
            version: 0
        }
    );
    let metadata = fixture.index.find().unwrap().unwrap();
    assert!(
        fixture
            .index
            .initialize(metadata, "replacement".into())
            .is_err()
    );
    assert_eq!(fs::read(fixture.index.metadata_path()).unwrap(), original);
    assert_eq!(fixture.reopen().read_session_state().unwrap(), state);
}

#[test]
fn permanent_membership_is_idempotent_immutable_and_ordered_after_reopen() {
    let fixture = Fixture::new();
    let first = AgentLineageRecord::new("child-one", "root", "worker");
    fixture.index.remember_agent(first.clone()).unwrap();
    let after_first = fs::read(fixture.index.metadata_path()).unwrap();
    fixture.index.remember_agent(first.clone()).unwrap();
    assert_eq!(
        fs::read(fixture.index.metadata_path()).unwrap(),
        after_first
    );
    assert!(
        fixture
            .index
            .remember_agent(AgentLineageRecord::new("child-one", "root", "other"))
            .is_err()
    );
    assert!(
        fixture
            .index
            .remember_agent(AgentLineageRecord::new("child-one", "unknown", "worker"))
            .is_err()
    );
    assert_eq!(
        fs::read(fixture.index.metadata_path()).unwrap(),
        after_first
    );
    let second = AgentLineageRecord::new("child-two", "root", "worker");
    fixture.index.remember_agent(second.clone()).unwrap();
    assert_eq!(
        fixture.reopen().read_agent_lineage().unwrap(),
        vec![AgentLineageRecord::new("root", "", "main"), first, second,]
    );
    assert!(
        !fixture
            .index
            .selected_root()
            .path()
            .join("session.db")
            .exists()
    );
}

#[test]
fn lineage_rejects_unknown_parents_and_unsafe_names_without_publishing() {
    let fixture = Fixture::new();
    let original = fs::read(fixture.index.metadata_path()).unwrap();
    for record in [
        AgentLineageRecord::new("child", "missing", "worker"),
        AgentLineageRecord::new("child", "root", "../worker"),
        AgentLineageRecord::new("root-two", "", "other-root"),
        AgentLineageRecord::new("child", "child", "worker"),
    ] {
        assert!(fixture.index.remember_agent(record).is_err());
        assert_eq!(fs::read(fixture.index.metadata_path()).unwrap(), original);
    }
}

#[test]
fn missing_or_corrupt_identity_never_generates_a_replacement() {
    let fixture = Fixture::new();
    let original = fs::read(fixture.index.metadata_path()).unwrap();
    for field in [
        "RootAgentSessionId",
        "RootAgentName",
        "Mode",
        "ModeRevision",
        "AgentLineage",
    ] {
        let mut metadata: serde_json::Value = serde_json::from_slice(&original).unwrap();
        metadata.as_object_mut().unwrap().remove(field);
        let bytes = serde_json::to_vec(&metadata).unwrap();
        fs::write(fixture.index.metadata_path(), &bytes).unwrap();
        assert!(
            fixture.reopen().read_session_state().is_err(),
            "missing {field}"
        );
        assert_eq!(fs::read(fixture.index.metadata_path()).unwrap(), bytes);
    }
    fs::remove_file(fixture.index.metadata_path()).unwrap();
    assert!(fixture.reopen().read_session_state().is_err());
    assert!(!fixture.index.metadata_path().exists());
}

#[tokio::test]
async fn same_mode_updates_advance_checked_revision_and_failed_publication_preserves_it() {
    let fixture = Fixture::new();
    fixture.index.update_mode("query").await.unwrap();
    fixture.index.update_mode("plan").await.unwrap();
    assert_eq!(
        fixture.reopen().read_session_state().unwrap().mode,
        PendingStatus {
            mode: "plan".into(),
            version: 2,
        }
    );
    let original = fs::read(fixture.index.metadata_path()).unwrap();
    let staging = fixture
        .index
        .metadata_path()
        .with_file_name("meta.json.staging");
    fs::create_dir(&staging).unwrap();
    assert!(fixture.index.update_mode("query").await.is_err());
    assert_eq!(fs::read(fixture.index.metadata_path()).unwrap(), original);
    fs::remove_dir(staging).unwrap();
    let mut metadata: serde_json::Value = serde_json::from_slice(&original).unwrap();
    metadata["ModeRevision"] = i64::MAX.into();
    let bytes = serde_json::to_vec(&metadata).unwrap();
    fs::write(fixture.index.metadata_path(), &bytes).unwrap();
    assert!(fixture.index.update_mode("query").await.is_err());
    assert_eq!(fs::read(fixture.index.metadata_path()).unwrap(), bytes);
}

#[test]
fn cloned_writers_preserve_each_others_canonical_fields() {
    let fixture = Fixture::new();
    std::thread::scope(|scope| {
        let index = fixture.index.clone();
        scope.spawn(move || index.update_task_limit(4).unwrap());
        let index = fixture.index.clone();
        scope.spawn(move || index.update_fast_mode(true).unwrap());
        let index = fixture.index.clone();
        scope.spawn(move || index.record_opened("opened").unwrap());
        let index = fixture.index.clone();
        scope.spawn(move || {
            index
                .remember_agent(AgentLineageRecord::new("child", "root", "worker"))
                .unwrap()
        });
        let index = fixture.index.clone();
        scope.spawn(move || {
            index
                .update_model_selection(&SessionModelSelection {
                    provider_id: "other".into(),
                    canonical_model: "other/model".into(),
                    selector: "alias".into(),
                })
                .unwrap()
        });
    });
    let metadata = fixture.reopen().find().unwrap().unwrap();
    assert_eq!(metadata.root_agent_session_id.as_deref(), Some("root"));
    assert_eq!(metadata.mode.as_deref(), Some("query"));
    assert_eq!(metadata.mode_revision, Some(0));
    assert_eq!(metadata.extra_concurrent_tasks, Some(4));
    assert_eq!(metadata.fast_mode, Some(true));
    assert_eq!(metadata.last_opened_at.as_deref(), Some("opened"));
    assert_eq!(metadata.provider_id.as_deref(), Some("other"));
    assert_eq!(metadata.agent_lineage.unwrap().len(), 2);
}
