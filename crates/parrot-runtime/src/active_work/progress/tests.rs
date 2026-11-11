use super::*;
use parrot_domain::contracts::{async_ops::Cancellation, events::EventSubscription};
use parrot_domain::values::progress::{PROGRESS_FILE_NAME, Progress};
use parrot_foundation::paths::AgentScratchDirectory;
use parrot_storage::progress::FileProgressStore;
use std::{
    collections::BTreeMap, fs, future::Future, path::PathBuf, sync::Mutex as SyncMutex,
    time::Duration,
};
use tokio::sync::{Notify, Semaphore};

struct Fixture {
    root: PathBuf,
    agent: PathBuf,
}
impl Fixture {
    fn new() -> Self {
        let root = std::env::temp_dir().join(format!(
            "parrot-runtime-progress-{}",
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
    async fn progress(&self) -> Arc<AgentProgress> {
        Arc::new(
            AgentProgress::new(self.store().await, "owner".into())
                .await
                .unwrap(),
        )
    }
    fn read(&self) -> ProgressSnapshot {
        serde_json::from_slice(&fs::read(self.agent.join(PROGRESS_FILE_NAME)).unwrap()).unwrap()
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        fs::remove_dir_all(&self.root).unwrap();
    }
}
async fn bounded<T>(future: impl Future<Output = T>) -> T {
    tokio::time::timeout(Duration::from_secs(10), future)
        .await
        .expect("progress test timed out")
}
fn update(steps: u64, total_steps: u64) -> ProgressUpdate {
    ProgressUpdate::Replace(Progress {
        steps,
        total_steps,
        current_step: Some("current work".into()),
    })
}
fn templates() -> Arc<PromptTemplateCatalog> {
    Arc::new(
        PromptTemplateCatalog::from_content(
            "",
            &BTreeMap::from([("HOME".into(), "/synthetic-progress-home".into())]),
        )
        .unwrap()
        .1,
    )
}
fn decode(event: &Event) -> ProgressSnapshot {
    let envelope = event.tool_snapshot();
    assert_eq!(envelope.kind, PROGRESS_INVENTORY_KIND);
    assert!(!envelope.inventory_instance_id.is_empty());
    assert_eq!(envelope.chunk_index, 0);
    assert!(envelope.final_chunk);
    let snapshot: ProgressSnapshot = serde_json::from_slice(&envelope.data).unwrap();
    assert_eq!(snapshot.owner_session_id, envelope.owner_agent_session_id);
    assert_eq!(snapshot.owner_session_id, event.agent_session_id);
    assert_eq!(snapshot.revision, envelope.revision);
    snapshot.validate().unwrap();
    snapshot
}

#[tokio::test]
async fn saved_progress_restores_and_only_explicit_close_clears_full_progress() {
    let fixture = Fixture::new();
    let progress = fixture.progress().await;
    assert!(!progress.update(ProgressUpdate::Close).await.unwrap());
    assert!(!fixture.agent.join(PROGRESS_FILE_NAME).exists());
    progress.update(update(2, 5)).await.unwrap();
    assert_eq!(progress.capture_snapshot().unwrap(), fixture.read());
    progress.update(update(5, 5)).await.unwrap();
    assert!(progress.capture_snapshot().unwrap().progress.is_some());
    progress.stop_publication().await.unwrap();
    let restored = fixture.progress().await;
    assert_eq!(restored.capture_snapshot().unwrap(), fixture.read());
    let blocker = ProgressBlocker {
        progress: restored.clone(),
        templates: templates(),
    };
    assert!(
        blocker
            .observe()
            .await
            .unwrap()
            .unwrap()
            .reminder
            .unwrap()
            .contains("progress")
    );
    restored.update(update(1, 3)).await.unwrap();
    assert!(restored.update(ProgressUpdate::Close).await.unwrap());
    assert_eq!(blocker.observe().await.unwrap(), None);
    assert_eq!(fixture.read().progress, None);
    assert!(!restored.update(ProgressUpdate::Close).await.unwrap());
    assert_eq!(
        fixture
            .progress()
            .await
            .capture_snapshot()
            .unwrap()
            .progress,
        None
    );
}

struct OtherBlocker;
impl ActiveWorkBlocker for OtherBlocker {
    fn observe(&self) -> BoxFuture<'_, Result<Option<BlockerResult>, TurnError>> {
        Box::pin(async {
            Ok(Some(BlockerResult {
                work_section: None,
                reminder: Some("other owned work".into()),
            }))
        })
    }
}
#[tokio::test]
async fn closing_progress_leaves_other_blockers_and_zero_or_full_counts_stay_blocking() {
    let fixture = Fixture::new();
    let progress = fixture.progress().await;
    let reminders = super::super::blockers::ActiveWorkReminder {
        blockers: vec![
            Arc::new(ProgressBlocker {
                progress: progress.clone(),
                templates: templates(),
            }),
            Arc::new(OtherBlocker),
        ],
        templates: templates(),
    };
    for steps in [0, 2, 5] {
        progress.update(update(steps, 5)).await.unwrap();
        let text = reminders.build().await.unwrap().unwrap();
        assert!(text.contains("progress"));
        assert!(text.contains("other owned work"));
    }
    progress.update(ProgressUpdate::Close).await.unwrap();
    assert_eq!(
        reminders.build().await.unwrap().as_deref(),
        Some("other owned work")
    );
}

#[tokio::test]
async fn owners_are_independent_and_restored_publishers_use_a_new_instance() {
    let fixture = Fixture::new();
    let root = fixture.progress().await;
    let child_path = fixture.agent.join("child");
    AgentScratchDirectory::new(&child_path, &fixture.root).unwrap();
    let child = Arc::new(
        AgentProgress::new(
            Arc::new(
                FileProgressStore::open(&child_path, "child".into())
                    .await
                    .unwrap(),
            ),
            "child".into(),
        )
        .await
        .unwrap(),
    );
    root.update(update(1, 2)).await.unwrap();
    child.update(update(2, 3)).await.unwrap();
    child.update(ProgressUpdate::Close).await.unwrap();
    assert!(root.capture_snapshot().unwrap().progress.is_some());
    assert_eq!(child.capture_snapshot().unwrap().progress, None);
    let broker = Arc::new(EventBroker::new());
    let old = ProgressSnapshotPublisher::new(root.clone(), broker.clone())
        .capture()
        .unwrap();
    root.stop_publication().await.unwrap();
    let restored = fixture.progress().await;
    let new = ProgressSnapshotPublisher::new(restored, broker)
        .capture()
        .unwrap();
    assert_eq!(decode(&old[0]), decode(&new[0]));
    assert_ne!(
        old[0].tool_snapshot().inventory_instance_id,
        new[0].tool_snapshot().inventory_instance_id
    );
}

#[tokio::test]
async fn publication_subscribes_before_first_poll_and_shutdown_preserves_durable_state() {
    let fixture = Fixture::new();
    let progress = fixture.progress().await;
    let broker = Arc::new(EventBroker::new());
    let listener = broker.subscribe();
    let publisher = ProgressSnapshotPublisher::new(progress.clone(), broker);
    let operation = publisher.start().unwrap();
    progress.update(update(2, 5)).await.unwrap();
    let event = bounded(async {
        loop {
            let event = listener.next(&NeverCancelled).await.unwrap().unwrap();
            if event.tool_snapshot().revision == 1 {
                break event;
            }
        }
    })
    .await;
    assert_eq!(decode(&event), fixture.read());
    let bytes = fs::read(fixture.agent.join(PROGRESS_FILE_NAME)).unwrap();
    progress.stop_publication().await.unwrap();
    bounded(operation.join()).await.unwrap();
    let terminal = listener.next(&NeverCancelled).await.unwrap().unwrap();
    assert!(terminal.tool_snapshot().removed);
    assert_eq!(decode(&terminal), decode(&event));
    assert_eq!(
        fs::read(fixture.agent.join(PROGRESS_FILE_NAME)).unwrap(),
        bytes
    );
    assert!(matches!(
        progress.update(ProgressUpdate::Close).await,
        Err(ProgressError::Stopped)
    ));
}

#[tokio::test]
async fn initial_capture_after_live_update_returns_current_state_without_a_new_tool_call() {
    let fixture = Fixture::new();
    let progress = fixture.progress().await;
    progress.update(update(1, 4)).await.unwrap();
    let broker = Arc::new(EventBroker::new());
    let publisher = Arc::new(ProgressSnapshotPublisher::new(
        progress.clone(),
        broker.clone(),
    ));
    let (release, captured) = tokio::sync::oneshot::channel();
    let subscription = broker
        .listen(None, |_| async { Ok::<_, ChildError>(vec![]) }, {
            let publisher = publisher.clone();
            move || async move {
                captured.await.unwrap();
                publisher.capture()
            }
        })
        .await
        .unwrap();
    let reading = subscription.next(&NeverCancelled);
    tokio::pin!(reading);
    assert!(
        tokio::select! { biased; _ = &mut reading => false, _ = std::future::ready(()) => true }
    );
    progress.update(update(3, 4)).await.unwrap();
    release.send(()).unwrap();
    let event = bounded(reading).await.unwrap().unwrap();
    assert_eq!(decode(&event).progress.unwrap().steps, 3);
    progress.stop_publication().await.unwrap();
}

#[derive(Clone, Copy)]
enum Fault {
    None,
    BeforeCommit,
    AfterCommit,
    FailedReconciliation,
}
struct Pause {
    entered: Notify,
    release: Semaphore,
}
struct ControlledStore {
    inner: Arc<dyn ProgressStore>,
    fault: SyncMutex<Fault>,
    pause: SyncMutex<Option<Arc<Pause>>>,
}
impl ControlledStore {
    fn new(inner: Arc<dyn ProgressStore>) -> Self {
        Self {
            inner,
            fault: SyncMutex::new(Fault::None),
            pause: SyncMutex::new(None),
        }
    }
}
impl ProgressStore for ControlledStore {
    fn load(&self) -> BoxFuture<'_, Result<Option<ProgressSnapshot>, ProgressStoreError>> {
        Box::pin(async move {
            if matches!(*self.fault.lock().unwrap(), Fault::FailedReconciliation) {
                return Err(ProgressStoreError::new(
                    ProgressStoreErrorKind::Io,
                    "controlled reload failure",
                ));
            }
            self.inner.load().await
        })
    }
    fn replace(&self, snapshot: ProgressSnapshot) -> BoxFuture<'_, Result<(), ProgressStoreError>> {
        Box::pin(async move {
            let pause = self.pause.lock().unwrap().take();
            if let Some(pause) = pause {
                pause.entered.notify_one();
                pause.release.acquire().await.unwrap().forget();
            }
            let fault = *self.fault.lock().unwrap();
            if matches!(fault, Fault::BeforeCommit) {
                return Err(ProgressStoreError::new(
                    ProgressStoreErrorKind::Io,
                    "controlled pre-commit failure",
                ));
            }
            self.inner.replace(snapshot).await?;
            if matches!(fault, Fault::AfterCommit | Fault::FailedReconciliation) {
                return Err(ProgressStoreError::new(
                    ProgressStoreErrorKind::UncertainCommit,
                    "controlled directory-sync failure",
                ));
            }
            Ok(())
        })
    }
}

#[tokio::test]
async fn persistence_precedes_capture_and_serialized_stop_joins_the_admitted_update() {
    let fixture = Fixture::new();
    let store = Arc::new(ControlledStore::new(fixture.store().await));
    let progress = Arc::new(
        AgentProgress::new(store.clone(), "owner".into())
            .await
            .unwrap(),
    );
    let pause = Arc::new(Pause {
        entered: Notify::new(),
        release: Semaphore::new(0),
    });
    *store.pause.lock().unwrap() = Some(pause.clone());
    let updating = tokio::spawn({
        let progress = progress.clone();
        async move { progress.update(update(1, 2)).await }
    });
    bounded(pause.entered.notified()).await;
    assert_eq!(progress.capture_snapshot().unwrap().progress, None);
    assert!(!fixture.agent.join(PROGRESS_FILE_NAME).exists());
    let stopping = progress.stop_publication();
    tokio::pin!(stopping);
    assert!(
        tokio::select! { biased; _ = &mut stopping => false, _ = std::future::ready(()) => true }
    );
    pause.release.add_permits(1);
    assert!(bounded(updating).await.unwrap().unwrap());
    bounded(stopping).await.unwrap();
    assert_eq!(progress.capture_snapshot().unwrap(), fixture.read());
    assert!(fixture.read().progress.is_some());
}

#[tokio::test]
async fn precommit_failures_preserve_state_and_uncertain_commits_reconcile_from_real_file() {
    let fixture = Fixture::new();
    let store = Arc::new(ControlledStore::new(fixture.store().await));
    let progress = AgentProgress::new(store.clone(), "owner".into())
        .await
        .unwrap();
    progress.update(update(1, 5)).await.unwrap();
    let before = progress.capture_snapshot().unwrap();
    *store.fault.lock().unwrap() = Fault::BeforeCommit;
    assert!(progress.update(update(2, 5)).await.is_err());
    assert_eq!(progress.capture_snapshot().unwrap(), before);
    assert_eq!(fixture.read(), before);
    *store.fault.lock().unwrap() = Fault::AfterCommit;
    assert!(matches!(
        progress.update(update(3, 5)).await,
        Err(ProgressError::Storage(ProgressStoreError {
            kind: ProgressStoreErrorKind::UncertainCommit,
            ..
        }))
    ));
    assert_eq!(progress.capture_snapshot().unwrap(), fixture.read());
    assert_eq!(fixture.read().progress.unwrap().steps, 3);
}

#[tokio::test]
async fn failed_reconciliation_fails_closed_and_does_not_leave_publisher_waiting() {
    let fixture = Fixture::new();
    let store = Arc::new(ControlledStore::new(fixture.store().await));
    let progress = Arc::new(
        AgentProgress::new(store.clone(), "owner".into())
            .await
            .unwrap(),
    );
    let publisher = ProgressSnapshotPublisher::new(progress.clone(), Arc::new(EventBroker::new()));
    let operation = publisher.start().unwrap();
    *store.fault.lock().unwrap() = Fault::FailedReconciliation;
    assert!(progress.update(update(1, 2)).await.is_err());
    assert!(progress.capture_snapshot().is_err());
    assert!(publisher.capture().is_err());
    assert!(bounded(operation.join()).await.is_err());
    assert!(progress.stop_publication().await.is_err());
    assert!(fixture.read().progress.is_some());
}

struct Cancelled;
impl Cancellation for Cancelled {
    fn is_cancelled(&self) -> bool {
        true
    }
    fn cancelled(&self) -> BoxFuture<'_, ()> {
        Box::pin(async {})
    }
}
#[tokio::test]
async fn cancelled_listener_does_not_clear_the_report_or_prevent_scope_shutdown() {
    let fixture = Fixture::new();
    let progress = fixture.progress().await;
    progress.update(update(1, 2)).await.unwrap();
    let broker = Arc::new(EventBroker::new());
    assert!(broker.subscribe().next(&Cancelled).await.is_err());
    let operation = ProgressSnapshotPublisher::new(progress.clone(), broker)
        .start()
        .unwrap();
    progress.stop_publication().await.unwrap();
    bounded(operation.join()).await.unwrap();
    assert!(fixture.read().progress.is_some());
}
