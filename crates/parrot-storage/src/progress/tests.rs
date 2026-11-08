use super::*;
use parrot_domain::values::{identifiers::Identifier, progress::*};
use parrot_foundation::paths::AgentScratchDirectory;
use std::{fs, path::PathBuf};

struct Fixture {
    root: PathBuf,
    agent: PathBuf,
}
impl Fixture {
    fn new() -> Self {
        let root = std::env::temp_dir().join(format!(
            "parrot-progress-{}",
            Identifier::event_id().unwrap()
        ));
        let agent = root.join("agent");
        AgentScratchDirectory::new(&agent, &root).unwrap();
        Self { root, agent }
    }
    async fn store(&self) -> Arc<dyn ProgressStore> {
        Arc::new(
            FileProgressStore::open(&self.agent, "owner".into())
                .await
                .unwrap(),
        )
    }
    fn path(&self) -> PathBuf {
        self.agent.join(PROGRESS_FILE_NAME)
    }
    fn snapshot(&self) -> ProgressSnapshot {
        ProgressSnapshot {
            schema_version: PROGRESS_VERSION,
            owner_session_id: "owner".into(),
            revision: 1,
            progress: Some(Progress {
                steps: u64::MAX - 1,
                total_steps: u64::MAX,
                current_step: Some("Testing 🦜\nwith a label".into()),
            }),
        }
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        fs::remove_dir_all(&self.root).unwrap();
    }
}

#[tokio::test]
async fn absent_state_does_not_create_a_snapshot_or_staging_file() {
    let fixture = Fixture::new();
    let store = fixture.store().await;
    assert_eq!(store.load().await.unwrap(), None);
    assert_eq!(fixture.store().await.load().await.unwrap(), None);
    assert!(!fixture.path().exists());
    assert!(!fixture.agent.join(PROGRESS_STAGING_FILE_NAME).exists());
    assert!(!fixture.agent.join("agent-state.json").exists());
}

#[tokio::test]
async fn round_trip_replanning_close_and_reopen_preserve_exact_values() {
    let fixture = Fixture::new();
    let store = fixture.store().await;
    let mut snapshot = fixture.snapshot();
    store.replace(snapshot.clone()).await.unwrap();
    assert_eq!(
        fixture.store().await.load().await.unwrap(),
        Some(snapshot.clone())
    );
    let json = fs::read_to_string(fixture.path()).unwrap();
    assert!(json.contains("18446744073709551614"));
    assert!(json.contains("18446744073709551615"));
    assert_eq!(
        serde_json::from_str::<ProgressSnapshot>(&json).unwrap(),
        snapshot
    );

    for update in [
        ProgressUpdate::Replace(Progress {
            steps: 0,
            total_steps: 3,
            current_step: None,
        }),
        ProgressUpdate::Close,
        ProgressUpdate::Replace(Progress {
            steps: 1,
            total_steps: 1,
            current_step: Some(String::new()),
        }),
        ProgressUpdate::Close,
    ] {
        snapshot.apply(update).unwrap();
        store.replace(snapshot.clone()).await.unwrap();
        assert_eq!(
            fixture.store().await.load().await.unwrap(),
            Some(snapshot.clone())
        );
        assert!(!fixture.agent.join(PROGRESS_STAGING_FILE_NAME).exists());
    }
    assert_eq!(snapshot.revision, 5);
    assert!(
        fs::read_to_string(fixture.path())
            .unwrap()
            .contains("\"progress\":null")
    );
}

#[tokio::test]
async fn invalid_existing_state_is_not_absent_and_cannot_be_overwritten() {
    let cases = [
        ("{", ProgressStoreErrorKind::Corrupt),
        (
            r#"{"schema_version":2}"#,
            ProgressStoreErrorKind::UnsupportedVersion,
        ),
        (
            r#"{"schema_version":1,"owner_session_id":"other","revision":1,"progress":null}"#,
            ProgressStoreErrorKind::OwnerMismatch,
        ),
        (
            r#"{"schema_version":1,"owner_session_id":"owner","revision":1}"#,
            ProgressStoreErrorKind::Corrupt,
        ),
        (
            r#"{"schema_version":1,"owner_session_id":"owner","revision":1,"progress":{"steps":0,"total_steps":0}}"#,
            ProgressStoreErrorKind::Corrupt,
        ),
        (
            r#"{"schema_version":1,"owner_session_id":"owner","revision":1,"progress":{"steps":2,"total_steps":1}}"#,
            ProgressStoreErrorKind::Corrupt,
        ),
        (
            r#"{"schema_version":1,"owner_session_id":"owner","revision":1,"progress":{"steps":-1,"total_steps":2}}"#,
            ProgressStoreErrorKind::Corrupt,
        ),
        (
            r#"{"schema_version":1,"owner_session_id":"owner","revision":1,"progress":{"steps":1.5,"total_steps":2}}"#,
            ProgressStoreErrorKind::Corrupt,
        ),
        (
            r#"{"schema_version":1,"owner_session_id":"owner","revision":1,"progress":{"steps":1,"total_steps":2,"current_step":4}}"#,
            ProgressStoreErrorKind::Corrupt,
        ),
        (
            r#"{"schema_version":1,"owner_session_id":"owner","revision":1,"progress":null,"extra":true}"#,
            ProgressStoreErrorKind::Corrupt,
        ),
        (
            r#"{"schema_version":1,"owner_session_id":"owner","revision":1,"progress":{"steps":1,"total_steps":2,"extra":true}}"#,
            ProgressStoreErrorKind::Corrupt,
        ),
        (
            r#"{"schema_version":1,"owner_session_id":" ","revision":1,"progress":null}"#,
            ProgressStoreErrorKind::Corrupt,
        ),
    ];
    for (json, kind) in cases {
        let fixture = Fixture::new();
        let store = fixture.store().await;
        fs::write(fixture.path(), json).unwrap();
        assert_eq!(store.load().await.unwrap_err().kind, kind, "{json}");
        assert_eq!(
            store.replace(fixture.snapshot()).await.unwrap_err().kind,
            kind,
            "{json}"
        );
        assert_eq!(fs::read_to_string(fixture.path()).unwrap(), json);
    }
}

#[tokio::test]
async fn invalid_replacements_preserve_the_previous_snapshot() {
    let fixture = Fixture::new();
    let store = fixture.store().await;
    let original = fixture.snapshot();
    store.replace(original.clone()).await.unwrap();
    let before = fs::read(fixture.path()).unwrap();
    let mut unsupported = original.clone();
    unsupported.schema_version = 2;
    let mut wrong_owner = original.clone();
    wrong_owner.owner_session_id = "other".into();
    let mut invalid_range = original.clone();
    invalid_range.progress.as_mut().unwrap().total_steps = 0;
    for (snapshot, kind) in [
        (unsupported, ProgressStoreErrorKind::UnsupportedVersion),
        (wrong_owner, ProgressStoreErrorKind::OwnerMismatch),
        (invalid_range, ProgressStoreErrorKind::Corrupt),
    ] {
        assert_eq!(store.replace(snapshot).await.unwrap_err().kind, kind);
        assert_eq!(fs::read(fixture.path()).unwrap(), before);
        assert_eq!(store.load().await.unwrap(), Some(original.clone()));
    }
}

#[tokio::test]
async fn staging_failure_preserves_previous_file_and_stale_regular_staging_is_discarded() {
    let fixture = Fixture::new();
    let store = fixture.store().await;
    let original = fixture.snapshot();
    store.replace(original.clone()).await.unwrap();
    let before = fs::read(fixture.path()).unwrap();
    let staging = fixture.agent.join(PROGRESS_STAGING_FILE_NAME);
    fs::create_dir(&staging).unwrap();
    let mut closed = original.clone();
    closed.apply(ProgressUpdate::Close).unwrap();
    assert_eq!(
        store.replace(closed.clone()).await.unwrap_err().kind,
        ProgressStoreErrorKind::InvalidPath
    );
    assert_eq!(fs::read(fixture.path()).unwrap(), before);
    assert_eq!(store.load().await.unwrap(), Some(original));
    fs::remove_dir(&staging).unwrap();
    fs::write(&staging, "uncommitted garbage").unwrap();
    store.replace(closed.clone()).await.unwrap();
    assert_eq!(store.load().await.unwrap(), Some(closed));
    assert!(!staging.exists());
}

#[tokio::test]
async fn root_and_child_store_their_own_progress() {
    let fixture = Fixture::new();
    let root = fixture.store().await;
    let child_path = fixture.agent.join("child");
    AgentScratchDirectory::new(&child_path, &fixture.root).unwrap();
    let child: Arc<dyn ProgressStore> = Arc::new(
        FileProgressStore::open(&child_path, "child".into())
            .await
            .unwrap(),
    );
    let root_snapshot = fixture.snapshot();
    let mut child_snapshot = ProgressSnapshot::new("child".into());
    child_snapshot
        .apply(ProgressUpdate::Replace(Progress {
            steps: 1,
            total_steps: 2,
            current_step: Some("child work".into()),
        }))
        .unwrap();
    root.replace(root_snapshot.clone()).await.unwrap();
    child.replace(child_snapshot.clone()).await.unwrap();
    assert_eq!(root.load().await.unwrap(), Some(root_snapshot));
    assert_eq!(child.load().await.unwrap(), Some(child_snapshot));
    assert!(fixture.path().is_file());
    assert!(child_path.join(PROGRESS_FILE_NAME).is_file());
}

#[tokio::test]
async fn nonregular_snapshot_is_rejected() {
    let fixture = Fixture::new();
    let store = fixture.store().await;
    fs::create_dir(fixture.path()).unwrap();
    assert_eq!(
        store.load().await.unwrap_err().kind,
        ProgressStoreErrorKind::InvalidPath
    );
    assert_eq!(
        store.replace(fixture.snapshot()).await.unwrap_err().kind,
        ProgressStoreErrorKind::InvalidPath
    );
    assert!(fixture.path().is_dir());
}

#[cfg(unix)]
#[tokio::test]
async fn symlink_destinations_and_staging_are_rejected_without_touching_targets() {
    use std::os::unix::fs::symlink;
    let fixture = Fixture::new();
    let store = fixture.store().await;
    let outside = fixture.root.join("outside");
    fs::write(&outside, "untouched").unwrap();
    symlink(&outside, fixture.path()).unwrap();
    assert_eq!(
        store.load().await.unwrap_err().kind,
        ProgressStoreErrorKind::InvalidPath
    );
    assert_eq!(
        store.replace(fixture.snapshot()).await.unwrap_err().kind,
        ProgressStoreErrorKind::InvalidPath
    );
    assert_eq!(fs::read_to_string(&outside).unwrap(), "untouched");
    fs::remove_file(fixture.path()).unwrap();
    store.replace(fixture.snapshot()).await.unwrap();
    let before = fs::read(fixture.path()).unwrap();
    symlink(&outside, fixture.agent.join(PROGRESS_STAGING_FILE_NAME)).unwrap();
    assert_eq!(
        store.replace(fixture.snapshot()).await.unwrap_err().kind,
        ProgressStoreErrorKind::InvalidPath
    );
    assert_eq!(fs::read(fixture.path()).unwrap(), before);
    assert_eq!(fs::read_to_string(outside).unwrap(), "untouched");
}

#[cfg(unix)]
#[tokio::test]
async fn files_are_private_and_directory_identity_is_pinned() {
    use std::os::unix::fs::{PermissionsExt, symlink};
    let fixture = Fixture::new();
    let store = fixture.store().await;
    let snapshot = fixture.snapshot();
    store.replace(snapshot.clone()).await.unwrap();
    assert_eq!(
        fs::metadata(fixture.path()).unwrap().permissions().mode() & 0o777,
        0o600
    );
    let moved = fixture.root.join("moved");
    fs::rename(&fixture.agent, &moved).unwrap();
    let outside = fixture.root.join("outside");
    AgentScratchDirectory::new(&outside, &fixture.root).unwrap();
    symlink(&outside, &fixture.agent).unwrap();
    let mut closed = snapshot;
    closed.apply(ProgressUpdate::Close).unwrap();
    store.replace(closed.clone()).await.unwrap();
    assert_eq!(store.load().await.unwrap(), Some(closed));
    assert!(moved.join(PROGRESS_FILE_NAME).is_file());
    assert!(!outside.join(PROGRESS_FILE_NAME).exists());
    assert_eq!(
        FileProgressStore::open(&fixture.agent, "owner".into())
            .await
            .err()
            .unwrap()
            .kind,
        ProgressStoreErrorKind::InvalidPath
    );
}
