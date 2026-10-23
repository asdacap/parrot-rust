mod observer;

use parrot_domain::contracts::agent_state::{AgentStateErrorKind, AgentStateStore};
use parrot_domain::values::{
    agent_state::*,
    agent_tasks::{ExecutionStatus, Task, TaskPayload},
    identifiers::Identifier,
};
use parrot_foundation::paths::AgentScratchDirectory;
use parrot_storage::agent_state::FileAgentStateStore;
use std::{
    fs,
    path::{Path, PathBuf},
    sync::Arc,
};

struct Fixture {
    root: PathBuf,
    agent: PathBuf,
}
impl Fixture {
    fn new() -> Self {
        let root = std::env::temp_dir().join(format!(
            "parrot-agent-state-{}",
            Identifier::event_id().unwrap()
        ));
        let agent = root.join("agent");
        AgentScratchDirectory::new(&agent, &root).unwrap();
        Self { root, agent }
    }
    async fn store(&self) -> Arc<dyn AgentStateStore> {
        Arc::new(
            FileAgentStateStore::open(&self.agent, "owner".into())
                .await
                .unwrap(),
        )
    }
    fn path(&self) -> PathBuf {
        self.agent.join(AGENT_STATE_FILE_NAME)
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        fs::remove_dir_all(&self.root).unwrap();
    }
}
fn selection() -> SelectionSnapshot {
    SelectionSnapshot {
        profile_id: "worker".into(),
        model_selector: "medium_llm".into(),
    }
}
fn task(state: ExecutionStatus) -> Task {
    Task {
        name: format!("task-{state:?}"),
        dependencies: vec!["first".into()],
        description: "do work".into(),
        payload: TaskPayload::Instruction("instructions".into()),
        acceptance_criteria: "tests pass".into(),
        model: Some("medium_llm".into()),
        hidden: true,
        state,
        result: Some("saved result".into()),
        failure: Some("saved failure".into()),
    }
}
fn entry(state: ExecutionStatus) -> TaskEntrySnapshot {
    let task = task(state);
    TaskEntrySnapshot {
        task: task.clone(),
        diagnostic_id: "diagnostic-1".into(),
        selection: selection(),
        fork_boundary: ForkBoundarySnapshot::BeforeToolBatch {
            assistant_sequence: 42,
            tool_call_id: "tool-call-1".into(),
        },
        worker_session_id: Some("child".into()),
        runner: Some(TaskRunnerCheckpoint {
            original_task: task.clone(),
            current_task: task,
            feedback: vec!["fix response".into()],
            carried_result: Some("carried result".into()),
            attempt: 2,
            repair_attempt: 1,
            phase: TaskRunnerPhase::AwaitingResponse,
            execution_id: "execution-1".into(),
            prompt_message_id: "prompt-1".into(),
            response_message_id: Some("response-1".into()),
        }),
        terminal_notification: Some(notification()),
    }
}
fn notification() -> NotificationCheckpoint {
    NotificationCheckpoint {
        message_id: "notification-1".into(),
        admitted: true,
        delivery_error: None,
    }
}
fn snapshot() -> AgentStateSnapshot {
    let mut snapshot = AgentStateSnapshot::new(
        "owner".into(),
        AgentDescriptor {
            parent_session_id: Some("parent".into()),
            name: "worker".into(),
            selection: selection(),
            local_scope: "only M1".into(),
            completion_delivery: CompletionDeliverySnapshot::RetainedOnly,
        },
    );
    snapshot.revision = 9;
    snapshot.children.push(RetainedChild {
        name: "child".into(),
        session_id: "child-session-1".into(),
    });
    snapshot.tasks = TaskGraphSnapshot {
        entries: [
            ExecutionStatus::Pending,
            ExecutionStatus::Running,
            ExecutionStatus::Succeeded,
            ExecutionStatus::Failed,
            ExecutionStatus::Canceled,
        ]
        .into_iter()
        .map(entry)
        .collect(),
        revision: 6,
        complete: true,
        all_resolved: true,
        completion_notification: Some(notification()),
    };
    snapshot.tasks.entries[0].task.payload =
        TaskPayload::Tasks(vec![task(ExecutionStatus::Succeeded)]);
    snapshot.execution = Some(ExecutionCheckpoint {
        execution_id: "execution-1".into(),
        initial_message_id: "prompt-1".into(),
        initial_parts: vec![ExecutionInputPart::Text {
            text: "question".into(),
        }],
        history_watermark: 12,
        phase: ExecutionPhase::Completed,
        terminal: Some(ExecutionTerminal {
            event_id: "execution-1.finished".into(),
            history_sequence: 93,
            outcome: ExecutionOutcome::Failed("failure".into()),
        }),
        parent_notification: Some(notification()),
    });
    snapshot
}

#[tokio::test]
async fn round_trip_preserves_complete_snapshot_and_reopen() {
    let fixture = Fixture::new();
    let store = fixture.store().await;
    let expected = snapshot();
    store.initialize(expected.clone()).await.unwrap();
    assert_eq!(store.load().await.unwrap(), Some(expected.clone()));
    assert_eq!(fixture.store().await.load().await.unwrap(), Some(expected));
    assert!(!fixture.agent.join(AGENT_STATE_STAGING_FILE_NAME).exists());
    let mut entries: Vec<_> = fs::read_dir(&fixture.agent)
        .unwrap()
        .map(|entry| entry.unwrap().file_name())
        .collect();
    entries.sort();
    assert_eq!(entries, ["agent-state.json", "blobs", "scratch"]);
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            fs::metadata(fixture.path()).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }
}

#[tokio::test]
async fn checkpoint_variants_round_trip() {
    fn round_trip<T>(value: T)
    where
        T: serde::Serialize + serde::de::DeserializeOwned + PartialEq + std::fmt::Debug,
    {
        let wire = serde_json::to_vec(&value).unwrap();
        assert_eq!(value, serde_json::from_slice(&wire).unwrap());
    }
    for phase in [
        TaskRunnerPhase::PromptIntent,
        TaskRunnerPhase::AwaitingResponse,
        TaskRunnerPhase::RepairingResponse,
        TaskRunnerPhase::AssessingResponse,
        TaskRunnerPhase::RetryIntent,
        TaskRunnerPhase::Completed,
    ] {
        round_trip(phase);
    }
    for phase in [
        ExecutionPhase::AdmissionIntent,
        ExecutionPhase::Active,
        ExecutionPhase::Suspended,
        ExecutionPhase::Completed,
        ExecutionPhase::Canceled,
    ] {
        round_trip(phase);
    }
    round_trip(ForkBoundarySnapshot::AfterCompletedHistory);
    round_trip(ForkBoundarySnapshot::AfterSafeHistoryPrefix);
    round_trip(CompletionDeliverySnapshot::Automatic);
    round_trip(ExecutionOutcome::Succeeded);
    round_trip(ExecutionOutcome::Canceled);
}

#[tokio::test]
async fn absence_corruption_version_and_owner_have_distinct_results() {
    let fixture = Fixture::new();
    let store = fixture.store().await;
    assert_eq!(store.load().await.unwrap(), None);
    assert_eq!(
        store
            .update(AgentStateUpdate::Execution(None))
            .await
            .unwrap_err()
            .kind,
        AgentStateErrorKind::NotFound
    );
    fs::write(fixture.path(), "not json").unwrap();
    assert_eq!(
        store.load().await.unwrap_err().kind,
        AgentStateErrorKind::Corrupt
    );
    assert_eq!(
        store.initialize(snapshot()).await.unwrap_err().kind,
        AgentStateErrorKind::AlreadyExists
    );
    assert_eq!(fs::read_to_string(fixture.path()).unwrap(), "not json");
    fs::write(fixture.path(), r#"{"schema_version":99}"#).unwrap();
    assert_eq!(
        store.load().await.unwrap_err().kind,
        AgentStateErrorKind::UnsupportedVersion
    );
    let mut wrong_owner = snapshot();
    wrong_owner.owner_session_id = "different-owner".into();
    fs::write(fixture.path(), serde_json::to_vec(&wrong_owner).unwrap()).unwrap();
    assert_eq!(
        store.load().await.unwrap_err().kind,
        AgentStateErrorKind::OwnerMismatch
    );
    assert_eq!(
        store
            .update(AgentStateUpdate::Execution(None))
            .await
            .unwrap_err()
            .kind,
        AgentStateErrorKind::OwnerMismatch
    );
}

#[tokio::test]
async fn initialization_cannot_replace_existing_state_or_accept_wrong_identity() {
    let fixture = Fixture::new();
    let store = fixture.store().await;
    let mut invalid = snapshot();
    invalid.owner_session_id = "wrong-owner".into();
    assert_eq!(
        store.initialize(invalid).await.unwrap_err().kind,
        AgentStateErrorKind::OwnerMismatch
    );
    assert!(!fixture.path().exists());
    let mut invalid = snapshot();
    invalid.schema_version = 99;
    assert_eq!(
        store.initialize(invalid).await.unwrap_err().kind,
        AgentStateErrorKind::UnsupportedVersion
    );
    store.initialize(snapshot()).await.unwrap();
    assert_eq!(
        store.initialize(snapshot()).await.unwrap_err().kind,
        AgentStateErrorKind::AlreadyExists
    );
}

#[tokio::test]
async fn unknown_fields_and_revision_overflow_do_not_reset_state() {
    let fixture = Fixture::new();
    let store = fixture.store().await;
    let encoded = serde_json::to_string(&snapshot()).unwrap();
    fs::write(
        fixture.path(),
        encoded.replacen('{', "{\"unknown\":true,", 1),
    )
    .unwrap();
    assert_eq!(
        store.load().await.unwrap_err().kind,
        AgentStateErrorKind::Corrupt
    );
    let mut expected = snapshot();
    expected.revision = u64::MAX;
    fs::write(fixture.path(), serde_json::to_vec(&expected).unwrap()).unwrap();
    assert_eq!(
        store
            .update(AgentStateUpdate::Execution(None))
            .await
            .unwrap_err()
            .kind,
        AgentStateErrorKind::Corrupt
    );
    assert_eq!(store.load().await.unwrap(), Some(expected));
}

#[tokio::test]
async fn concurrent_section_updates_preserve_unrelated_fields() {
    let fixture = Fixture::new();
    let store = fixture.store().await;
    let expected = snapshot();
    store.initialize(expected.clone()).await.unwrap();
    let mut descriptor = expected.descriptor.clone();
    descriptor.local_scope = "changed scope".into();
    let children = vec![RetainedChild {
        name: "new-child".into(),
        session_id: "new-id".into(),
    }];
    let mut tasks = expected.tasks.clone();
    tasks.revision += 1;
    let updates = [
        AgentStateUpdate::Descriptor(descriptor.clone()),
        AgentStateUpdate::Children(children.clone()),
        AgentStateUpdate::Tasks(tasks.clone()),
        AgentStateUpdate::Execution(None),
    ];
    let barrier = Arc::new(tokio::sync::Barrier::new(updates.len()));
    let workers: Vec<_> = updates
        .into_iter()
        .map(|update| {
            let store = store.clone();
            let barrier = barrier.clone();
            tokio::spawn(async move {
                barrier.wait().await;
                store.update(update).await.unwrap();
            })
        })
        .collect();
    for worker in workers {
        worker.await.unwrap();
    }
    let actual = store.load().await.unwrap().unwrap();
    assert_eq!(actual.revision, expected.revision + 4);
    assert_eq!(actual.owner_session_id, expected.owner_session_id);
    assert_eq!(actual.descriptor, descriptor);
    assert_eq!(actual.children, children);
    assert_eq!(actual.tasks, tasks);
    assert_eq!(actual.execution, None);
}

#[tokio::test]
async fn failed_replacement_preserves_last_committed_snapshot() {
    let fixture = Fixture::new();
    let store = fixture.store().await;
    let expected = snapshot();
    store.initialize(expected.clone()).await.unwrap();
    let staging = fixture.agent.join(AGENT_STATE_STAGING_FILE_NAME);
    fs::create_dir(&staging).unwrap();
    assert!(
        store
            .update(AgentStateUpdate::Execution(None))
            .await
            .is_err()
    );
    assert_eq!(store.load().await.unwrap(), Some(expected.clone()));
    fs::remove_dir(staging).unwrap();
    fs::write(
        fixture.agent.join(AGENT_STATE_STAGING_FILE_NAME),
        "stale partial write",
    )
    .unwrap();
    let actual = store
        .update(AgentStateUpdate::Execution(None))
        .await
        .unwrap();
    assert_eq!(actual.revision, expected.revision + 1);
    assert!(!fixture.agent.join(AGENT_STATE_STAGING_FILE_NAME).exists());
}

#[cfg(unix)]
#[tokio::test]
async fn paths_and_symlinks_cannot_redirect_snapshot_io() {
    use std::os::unix::fs::{PermissionsExt, symlink};
    let fixture = Fixture::new();
    let outside = fixture.root.join("outside");
    AgentScratchDirectory::new(&outside, &fixture.root).unwrap();
    let linked = fixture.root.join("linked");
    symlink(&fixture.agent, &linked).unwrap();
    assert!(
        FileAgentStateStore::open(&linked, "owner".into())
            .await
            .is_err()
    );
    assert!(
        FileAgentStateStore::open(&linked.join("scratch"), "owner".into())
            .await
            .is_err()
    );
    assert!(
        FileAgentStateStore::open(Path::new("relative"), "owner".into())
            .await
            .is_err()
    );
    fs::set_permissions(&outside, fs::Permissions::from_mode(0o755)).unwrap();
    assert!(
        FileAgentStateStore::open(&outside, "owner".into())
            .await
            .is_err()
    );
    fs::set_permissions(&outside, fs::Permissions::from_mode(0o700)).unwrap();
    let sentinel = outside.join("sentinel");
    fs::write(&sentinel, "outside bytes").unwrap();
    let store = fixture.store().await;
    symlink(&sentinel, fixture.path()).unwrap();
    assert_eq!(
        store.load().await.unwrap_err().kind,
        AgentStateErrorKind::InvalidPath
    );
    assert_eq!(
        store.initialize(snapshot()).await.unwrap_err().kind,
        AgentStateErrorKind::AlreadyExists
    );
    fs::remove_file(fixture.path()).unwrap();
    symlink(&sentinel, fixture.agent.join(AGENT_STATE_STAGING_FILE_NAME)).unwrap();
    store.initialize(snapshot()).await.unwrap();
    assert_eq!(fs::read_to_string(&sentinel).unwrap(), "outside bytes");
}

#[cfg(unix)]
#[tokio::test]
async fn pinned_agent_directory_cannot_be_replaced_by_a_link() {
    use std::os::unix::fs::symlink;
    let fixture = Fixture::new();
    let store = fixture.store().await;
    store.initialize(snapshot()).await.unwrap();
    let moved = fixture.root.join("moved-agent");
    fs::rename(&fixture.agent, &moved).unwrap();
    let outside = fixture.root.join("outside");
    fs::create_dir(&outside).unwrap();
    symlink(&outside, &fixture.agent).unwrap();
    store
        .update(AgentStateUpdate::Execution(None))
        .await
        .unwrap();
    assert!(moved.join(AGENT_STATE_FILE_NAME).exists());
    assert_eq!(fs::read_dir(&outside).unwrap().count(), 0);
}
