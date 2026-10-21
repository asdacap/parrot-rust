use super::*;
use crate::ownership::{OwnershipRoots, ProcessIdentity, ProcessIdentityStatus, RuntimeIdentity};
use std::fs;
use std::sync::atomic::AtomicUsize;

struct Fixture {
    root: PathBuf,
    workspace: PathBuf,
    admissions: Arc<SessionAdmissions>,
    runtime: RuntimeIdentity,
}
impl Fixture {
    fn new() -> Self {
        let root = std::env::temp_dir().join(format!(
            "t21-session-store-{}",
            super::super::runtime::random_id().unwrap()
        ));
        let workspace = root.join("workspace");
        fs::create_dir_all(&workspace).unwrap();
        let runtime = RuntimeIdentity::new(
            "host".into(),
            "boot".into(),
            123,
            "start".into(),
            "runtime".into(),
            Arc::new(|_| ProcessIdentity {
                status: ProcessIdentityStatus::Found,
                process_start_token: Some("start".into()),
            }),
        )
        .with_proven_comparable_process_start_tokens();
        let admissions = Arc::new(SessionAdmissions::new(
            OwnershipRoots::new(root.join("new"), root.join("old")).unwrap(),
            runtime.clone(),
            true,
        ));
        Self {
            root,
            workspace,
            admissions,
            runtime,
        }
    }
    fn store(&self, factory: Arc<Factory>) -> SessionStore {
        *factory.log_path.lock().unwrap() = Some(self.root.join("logs/parrot-global.log"));
        SessionStore::new(
            self.admissions.clone(),
            self.workspace.clone(),
            "default".into(),
            factory,
            Arc::new(Router),
            Arc::new(Workspace),
            Arc::new(
                FileDiagnosticLog::open_global(self.root.join("logs"), "global", |_| {}).unwrap(),
            ),
        )
        .with_clock(Arc::new(|| "2026-10-17T00:00:00.0000000+00:00".into()))
    }
    fn id(&self) -> UserSessionId {
        UserSessionId::parse("user-session-test").unwrap()
    }
    fn admission(&self) -> SessionAdmission {
        self.admissions
            .create_fresh(&self.workspace, &self.id())
            .unwrap()
    }
    fn global(&self) -> String {
        fs::read_to_string(self.root.join("logs/parrot-global.log")).unwrap()
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}
struct Workspace;
impl SessionWorkspaceResolver for Workspace {
    fn resolve(&self, path: &Path) -> Result<PathBuf, SessionStoreError> {
        Ok(path.to_path_buf())
    }
}
struct Router;
impl SessionModelRouter for Router {
    fn resolve(&self, selector: &str) -> Result<SessionModelSelection, SessionStoreError> {
        Ok(SessionModelSelection {
            provider_id: "provider".into(),
            canonical_model: "provider/model".into(),
            selector: selector.into(),
        })
    }
}
fn model() -> SessionModelSelection {
    SessionModelSelection {
        provider_id: "provider".into(),
        canonical_model: "provider/model".into(),
        selector: "provider/model".into(),
    }
}
#[derive(Default)]
struct Factory {
    fail: AtomicBool,
    cancel: AtomicBool,
    block_publication: AtomicBool,
    fail_cleanup: AtomicBool,
    close_own_lease: AtomicBool,
    disposed: Arc<AtomicUsize>,
    lease: Mutex<Option<Arc<SessionResourceLease>>>,
    log_path: Mutex<Option<PathBuf>>,
    pending_log: Mutex<String>,
    session_id: Mutex<Option<String>>,
}
impl StoredUserSessionFactory for Factory {
    fn create(
        &self,
        lease: Arc<SessionResourceLease>,
        initialization: SessionInitialization,
    ) -> BoxFuture<'_, Result<Box<dyn StoredUserSession>, SessionStoreError>> {
        Box::pin(async move {
            assert!(lease.selected_root().path().join("session.db").is_file());
            assert!(!lease.is_cancelled());
            *self.lease.lock().unwrap() = Some(lease.clone());
            *self.session_id.lock().unwrap() = Some(initialization.id.value().to_owned());
            if let Some(path) = self.log_path.lock().unwrap().as_ref() {
                *self.pending_log.lock().unwrap() = fs::read_to_string(path).unwrap();
            }
            if self.cancel.load(Ordering::Acquire) {
                lease.confirm_producers_settled();
                return Err(SessionStoreError::Cancelled);
            }
            if self.fail.swap(false, Ordering::AcqRel) {
                lease.confirm_producers_settled();
                return Err(SessionStoreError::failure(std::io::Error::other(
                    "private-construction-failure",
                )));
            }
            if self.block_publication.swap(false, Ordering::AcqRel) {
                fs::create_dir(lease.selected_root().path().join("meta.json")).unwrap();
            }
            Ok(Box::new(Session {
                initialization,
                disposed: self.disposed.clone(),
                fail_cleanup: self.fail_cleanup.load(Ordering::Acquire),
                lease,
                close_own_lease: self.close_own_lease.load(Ordering::Acquire),
            }) as Box<dyn StoredUserSession>)
        })
    }
}
struct Session {
    initialization: SessionInitialization,
    disposed: Arc<AtomicUsize>,
    fail_cleanup: bool,
    lease: Arc<SessionResourceLease>,
    close_own_lease: bool,
}
impl StoredUserSession for Session {
    fn model(&self) -> SessionModelSelection {
        self.initialization.model.clone()
    }
    fn mode(&self) -> String {
        self.initialization.mode.clone()
    }
    fn dispose(&mut self) -> BoxFuture<'_, Result<(), SessionStoreError>> {
        Box::pin(async move {
            tokio::time::sleep(Duration::from_millis(1)).await;
            self.disposed.fetch_add(1, Ordering::AcqRel);
            self.lease.confirm_producers_settled();
            if self.close_own_lease {
                self.lease.close().await?;
            }
            if self.fail_cleanup {
                Err(SessionStoreError::failure(std::io::Error::other(
                    "cleanup failed",
                )))
            } else {
                Ok(())
            }
        })
    }
}

#[tokio::test]
async fn initializes_before_publication_then_closes_real_resources_and_fenced_activation() {
    let fixture = Fixture::new();
    let factory = Arc::new(Factory::default());
    let store = fixture.store(factory.clone());
    let mut opened = store
        .open_admitted(
            fixture.workspace.clone(),
            fixture.admission(),
            Some(model()),
            "query",
            true,
        )
        .await
        .unwrap();
    assert!(!opened.loaded);
    assert_eq!(opened.initialization.root_agent_name, "main");
    assert!(opened.initialization.interactive_permissions);
    let root = opened.resources.selected_root();
    assert_eq!(opened.resources.database.selected_root(), root);
    assert_eq!(opened.resources.events.database().selected_root(), root);
    assert_eq!(opened.resources.images.store().selected_root(), root);
    assert_eq!(
        opened.resources.images.store().directory(),
        root.path().join("artifacts")
    );
    let metadata = opened.resources.index.find().unwrap().unwrap();
    assert_eq!(metadata.mode.as_deref(), Some("query"));
    assert_eq!(metadata.created_at, metadata.last_opened_at);
    let log = root.path().join("session.log");
    opened.close().await.unwrap();
    assert_eq!(factory.disposed.load(Ordering::Acquire), 1);
    assert!(opened.resources.is_cancelled());
    let before = fs::read_to_string(&log).unwrap();
    assert!(before.contains("event=\"closed\""));
    opened.resources.write_diagnostic(&DiagnosticEvent::new(
        "test",
        "after-close",
        DiagnosticSeverity::Information,
    ));
    assert_eq!(fs::read_to_string(log).unwrap(), before);
    let retry = fixture
        .admissions
        .resume(&fixture.workspace, &fixture.id(), false)
        .unwrap();
    assert!(retry.admission.activation_lease.is_some());
    let global = fixture.global();
    for stage in ["workspace", "acquire"] {
        assert!(!global.contains(&format!("{stage}_start")));
    }
    for stage in [
        "resources",
        "database",
        "metadata_read",
        "initialize",
        "metadata_publish",
    ] {
        assert!(global.contains(&format!("event=\"{stage}_start\"")));
        assert!(global.contains(&format!("event=\"{stage}_complete\"")));
    }
}

#[tokio::test]
async fn publication_failure_disposes_session_cancels_lifetime_and_preserves_original_failure() {
    let fixture = Fixture::new();
    let factory = Arc::new(Factory::default());
    factory.block_publication.store(true, Ordering::Release);
    factory.fail_cleanup.store(true, Ordering::Release);
    let store = fixture.store(factory.clone());
    let error = store
        .open_admitted(
            fixture.workspace.clone(),
            fixture.admission(),
            Some(model()),
            "default",
            false,
        )
        .await
        .err()
        .unwrap();
    assert!(!error.to_string().contains("cleanup failed"));
    assert_eq!(factory.disposed.load(Ordering::Acquire), 1);
    let lease = factory.lease.lock().unwrap().clone().unwrap();
    assert!(lease.is_cancelled());
    let root = lease.selected_root().path();
    let log = fs::read_to_string(root.join("session.log")).unwrap();
    assert!(log.contains("event=\"open\""));
    assert!(log.contains("event=\"cleanup\""));
    assert!(log.contains("outcome=\"failed\""));
    fs::remove_dir(root.join("meta.json")).unwrap();
    factory.fail_cleanup.store(false, Ordering::Release);
    let mut retry = store.open(model(), "default", false).await.unwrap();
    assert_eq!(retry.initialization.id, fixture.id());
    retry.close().await.unwrap();
    let global = fixture.global();
    assert!(global.contains("metadata_publish_failure"));
    assert!(!global.contains("cleanup failed"));
    assert!(!global.contains(fixture.workspace.to_str().unwrap()));
}

#[tokio::test]
async fn database_open_failure_closes_diagnostics_and_releases_activation() {
    let fixture = Fixture::new();
    let factory = Arc::new(Factory::default());
    let store = fixture.store(factory);
    let admission = fixture.admission();
    let root = admission.selected_root.as_ref().unwrap().path().to_owned();
    fs::create_dir_all(root.join("session.db")).unwrap();
    assert!(
        store
            .open_admitted(
                fixture.workspace.clone(),
                admission,
                Some(model()),
                "default",
                false
            )
            .await
            .is_err()
    );
    let log = fs::read_to_string(root.join("session.log")).unwrap();
    assert!(log.contains("outcome=\"failed\""));
    fs::OpenOptions::new()
        .write(true)
        .open(root.join("session.log"))
        .unwrap();
    assert!(
        fixture
            .admissions
            .resume(&fixture.workspace, &fixture.id(), false)
            .unwrap()
            .admission
            .activation_lease
            .is_some()
    );
    assert!(fixture.global().contains("database_failure"));
}

#[tokio::test]
async fn construction_failure_releases_admission_for_retry_and_reports_stage_without_private_text()
{
    let fixture = Fixture::new();
    let factory = Arc::new(Factory::default());
    factory.fail.store(true, Ordering::Release);
    let store = fixture.store(factory.clone());
    let error = store
        .open_admitted(
            fixture.workspace.clone(),
            fixture.admission(),
            Some(model()),
            "default",
            false,
        )
        .await
        .err()
        .unwrap();
    assert_eq!(error.to_string(), "private-construction-failure");
    assert!(
        factory
            .lease
            .lock()
            .unwrap()
            .as_ref()
            .unwrap()
            .is_cancelled()
    );
    assert_eq!(factory.disposed.load(Ordering::Acquire), 0);
    let global = fixture.global();
    assert!(global.contains("initialize_failure"));
    assert!(!global.contains("private-construction-failure"));
    let mut retry = store.open(model(), "default", false).await.unwrap();
    assert_eq!(retry.initialization.id, fixture.id());
    retry.close().await.unwrap();
}

#[tokio::test]
async fn legacy_resume_preserves_selected_root_and_metadata_and_uses_selector_fallback() {
    let fixture = Fixture::new();
    let factory = Arc::new(Factory::default());
    let store = fixture.store(factory);
    let id = fixture.id();
    let root = SelectedSessionRoot::new(
        fixture.root.join("old/sessions").join(id.value()),
        parrot_domain::contracts::paths::RootOrigin::Legacy,
    )
    .unwrap();
    let index = SessionIndex::new(root.clone(), id.clone(), &fixture.workspace).unwrap();
    let mut metadata = SessionMeta::new(
        id.value(),
        fixture.workspace.to_str().unwrap(),
        "provider",
        "model",
    );
    metadata.created_at = Some("2026-01-01T00:00:00Z".into());
    metadata.root_agent_name = Some("legacy-root".into());
    metadata.mode = Some("query".into());
    metadata.selector = None;
    index.publish(&metadata).unwrap();
    let stabilized = crate::ownership::WorkingDirectoryClaim::new(
        fixture.root.join("old"),
        fixture.runtime.clone(),
    )
    .create_fresh(&fixture.workspace, &id)
    .unwrap();
    stabilized.activation_lease.unwrap().release().unwrap();
    let mut opened = store.resume(&id, true, false).await.unwrap();
    assert!(opened.loaded);
    assert_eq!(opened.resources.selected_root(), &root);
    assert_eq!(opened.initialization.root_agent_name, "legacy-root");
    assert_eq!(opened.initialization.mode, "query");
    assert_eq!(opened.initialization.model.selector, "provider/model");
    let published = index.find().unwrap().unwrap();
    assert_eq!(published.created_at, metadata.created_at);
    assert_eq!(published.working_directory, metadata.working_directory);
    assert_eq!(published.root_agent_name, metadata.root_agent_name);
    assert!(!fixture.root.join("new/sessions").exists());
    let prior = published.last_opened_at;
    opened.clock = Arc::new(|| "later-success".into());
    assert_eq!(index.find().unwrap().unwrap().last_opened_at, prior);
    opened.record_opened().unwrap();
    assert_eq!(
        index.find().unwrap().unwrap().last_opened_at.as_deref(),
        Some("later-success")
    );
    opened.close().await.unwrap();
}

#[tokio::test]
async fn explicit_resume_without_metadata_never_constructs_or_publishes_and_releases_claim() {
    let fixture = Fixture::new();
    let factory = Arc::new(Factory::default());
    let store = fixture.store(factory.clone());
    fixture
        .admission()
        .admission
        .activation_lease
        .unwrap()
        .release()
        .unwrap();
    assert!(matches!(
        store.resume(&fixture.id(), false, false).await,
        Err(SessionStoreError::InvalidMetadata)
    ));
    assert!(factory.lease.lock().unwrap().is_none());
    assert!(
        !fixture
            .root
            .join("new/sessions")
            .join(fixture.id().value())
            .join("meta.json")
            .exists()
    );
    let retry = fixture
        .admissions
        .resume(&fixture.workspace, &fixture.id(), false)
        .unwrap();
    assert!(retry.admission.activation_lease.is_some());
    assert!(fixture.global().contains("metadata_read_failure"));
}

#[tokio::test]
async fn cancelled_initialization_uses_information_stage_and_releases_resources() {
    let fixture = Fixture::new();
    let factory = Arc::new(Factory::default());
    factory.cancel.store(true, Ordering::Release);
    let store = fixture.store(factory.clone());
    assert!(matches!(
        store
            .open_admitted(
                fixture.workspace.clone(),
                fixture.admission(),
                Some(model()),
                "default",
                false
            )
            .await,
        Err(SessionStoreError::Cancelled)
    ));
    let global = fixture.global();
    let stage = global
        .lines()
        .find(|line| line.contains("initialize_failure"))
        .unwrap();
    assert!(stage.contains("INFO"));
    assert!(stage.contains("outcome=\"cancelled\""));
    assert!(stage.contains("error=\"cancelled\""));
    assert!(
        factory
            .lease
            .lock()
            .unwrap()
            .as_ref()
            .unwrap()
            .is_cancelled()
    );
}

// SessionStoreTests.cs:87-137: all fresh/resumed and success/failure combinations.
#[tokio::test]
async fn startup_stages_keep_selected_session_and_one_resource_correlation() {
    for resume in [false, true] {
        for fail in [false, true] {
            let fixture = Fixture::new();
            let factory = Arc::new(Factory::default());
            factory.fail.store(fail, Ordering::Release);
            let store = fixture.store(factory.clone());
            if resume {
                fixture
                    .admission()
                    .admission
                    .activation_lease
                    .unwrap()
                    .release()
                    .unwrap();
                let root = SelectedSessionRoot::new(
                    fixture.root.join("new/sessions").join(fixture.id().value()),
                    parrot_domain::contracts::paths::RootOrigin::Current,
                )
                .unwrap();
                let index = SessionIndex::new(root, fixture.id(), &fixture.workspace).unwrap();
                index
                    .publish(&SessionMeta::new(
                        fixture.id().value(),
                        fixture.workspace.to_str().unwrap(),
                        "provider",
                        "model",
                    ))
                    .unwrap();
            }
            let result = if resume {
                store.resume(&fixture.id(), false, false).await
            } else {
                store.create_fresh(model(), "default", false).await
            };
            let id = factory.session_id.lock().unwrap().clone().unwrap();
            if fail {
                assert_eq!(
                    result.err().unwrap().to_string(),
                    "private-construction-failure"
                );
            } else {
                let mut opened = result.unwrap();
                assert_eq!(opened.initialization.id.value(), id);
                opened.close().await.unwrap();
            }
            if resume {
                assert_eq!(id, fixture.id().value());
            }
            let pending = factory.pending_log.lock().unwrap().clone();
            for stage in [
                "workspace_complete",
                "acquire_complete",
                "resources_complete",
                "database_complete",
                "initialize_start",
            ] {
                assert!(pending.contains(&format!("event=\"{stage}\"")));
            }
            assert!(!pending.contains("event=\"initialize_complete\""));
            assert!(!pending.contains("event=\"initialize_failure\""));
            let log = fixture.global();
            let initialization: Vec<_> = log
                .lines()
                .filter(|line| line.contains("event=\"initialize_"))
                .collect();
            assert_eq!(initialization.len(), 2);
            for line in &initialization {
                assert!(line.contains(&format!("session=\"{id}\"")));
                assert!(!diagnostic_field(line, "correlation").is_empty());
            }
            assert!(initialization[1].contains(if fail {
                "event=\"initialize_failure\""
            } else {
                "event=\"initialize_complete\""
            }));
            assert!(initialization[1].contains("duration_ms="));
            assert!(initialization[1].contains(if fail {
                "outcome=\"failed\""
            } else {
                "outcome=\"success\""
            }));
            assert!(initialization[1].contains(if fail { " ERROR " } else { " INFO " }));
            let correlation = diagnostic_field(initialization[0], "correlation");
            let resource_lines: Vec<_> = log
                .lines()
                .filter(|line| {
                    [
                        "resources",
                        "database",
                        "metadata_read",
                        "initialize",
                        "metadata_publish",
                    ]
                    .iter()
                    .any(|stage| line.contains(&format!("event=\"{stage}_")))
                })
                .collect();
            assert_eq!(resource_lines.len(), if fail { 8 } else { 10 });
            for line in resource_lines {
                assert_eq!(diagnostic_field(line, "session"), id);
                assert_eq!(diagnostic_field(line, "correlation"), correlation);
            }
            let workspace_start = log
                .lines()
                .find(|line| line.contains("event=\"workspace_start\""))
                .unwrap();
            let workspace_complete = log
                .lines()
                .find(|line| line.contains("event=\"workspace_complete\""))
                .unwrap();
            let acquire_start = log
                .lines()
                .find(|line| line.contains("event=\"acquire_start\""))
                .unwrap();
            let acquire_complete = log
                .lines()
                .find(|line| line.contains("event=\"acquire_complete\""))
                .unwrap();
            let workspace_correlation = diagnostic_field(workspace_start, "correlation");
            let acquire_correlation = diagnostic_field(acquire_start, "correlation");
            assert_eq!(
                diagnostic_field(workspace_complete, "correlation"),
                workspace_correlation
            );
            assert_eq!(
                diagnostic_field(acquire_complete, "correlation"),
                acquire_correlation
            );
            assert_ne!(workspace_correlation, acquire_correlation);
            assert_ne!(workspace_correlation, correlation);
            assert_ne!(acquire_correlation, correlation);
            assert_eq!(diagnostic_field(acquire_complete, "session"), id);
            for line in [workspace_start, workspace_complete] {
                if resume {
                    assert_eq!(diagnostic_field(line, "session"), id);
                } else {
                    assert!(!line.contains(" session="));
                }
            }
            assert_eq!(diagnostic_field(acquire_start, "session"), id);
            assert!(!log.contains("private-construction-failure"));
            assert!(!log.contains(fixture.workspace.to_str().unwrap()));
            assert!(
                factory
                    .lease
                    .lock()
                    .unwrap()
                    .as_ref()
                    .unwrap()
                    .is_cancelled()
            );
        }
    }
}

fn diagnostic_field<'a>(line: &'a str, name: &str) -> &'a str {
    let prefix = format!(" {name}=\"");
    let (_, tail) = line.split_once(&prefix).unwrap();
    tail.split_once('"').unwrap().0
}

#[test]
fn legacy_selector_fallback_is_ordinal_and_avoids_double_prefix() {
    let mut metadata = SessionMeta::new(
        "user-session-test",
        "/workspace",
        "Provider",
        "Provider/model",
    );
    metadata.selector = None;
    assert_eq!(stored_selector(&metadata), "Provider/model");
    metadata.model = Some("provider/model".into());
    assert_eq!(stored_selector(&metadata), "Provider/provider/model");
    metadata.selector = Some("alias".into());
    assert_eq!(stored_selector(&metadata), "alias");
}

// Real actor work remains admitted after its ticket is dropped. The release is
// also sent on assertion failure so a failing regression does not hang a worker.
struct BlockedTransaction {
    entered: std::sync::mpsc::Receiver<()>,
    release: Option<std::sync::mpsc::Sender<()>>,
}
impl BlockedTransaction {
    fn admit(database: &SessionDatabase) -> Self {
        let (entered_tx, entered) = std::sync::mpsc::channel();
        let (release, release_rx) = std::sync::mpsc::channel();
        let ticket = database
            .admit("blocked-startup", &NeverCancelled, move |tx| {
                tx.execute_batch("CREATE TABLE IF NOT EXISTS t21_settlement(value INTEGER);")?;
                entered_tx.send(()).unwrap();
                release_rx
                    .recv_timeout(Duration::from_secs(10))
                    .map_err(|error| DatabaseError::Io(std::io::Error::other(error)))?;
                tx.execute("INSERT INTO t21_settlement VALUES (1)", [])?;
                Ok(())
            })
            .unwrap();
        drop(ticket);
        Self {
            entered,
            release: Some(release),
        }
    }
    fn wait_entered(&self) {
        self.entered.recv_timeout(Duration::from_secs(5)).unwrap();
    }
    fn release(&mut self) {
        if let Some(release) = self.release.take() {
            let _ = release.send(());
        }
    }
}
impl Drop for BlockedTransaction {
    fn drop(&mut self) {
        self.release();
    }
}

#[derive(Default)]
struct PendingFactory {
    lease: Mutex<Option<Arc<SessionResourceLease>>>,
    blocked: Mutex<Option<BlockedTransaction>>,
}
impl StoredUserSessionFactory for PendingFactory {
    fn create(
        &self,
        lease: Arc<SessionResourceLease>,
        _: SessionInitialization,
    ) -> BoxFuture<'_, Result<Box<dyn StoredUserSession>, SessionStoreError>> {
        Box::pin(async move {
            *self.blocked.lock().unwrap() = Some(BlockedTransaction::admit(&lease.database));
            *self.lease.lock().unwrap() = Some(lease.clone());
            lease
                .database
                .admit("constructor-rollback", &NeverCancelled, |_| Ok(()))
                .unwrap()
                .into_completion()
                .await
                .unwrap();
            lease.confirm_producers_settled();
            Err(SessionStoreError::Cancelled)
        })
    }
}
fn pending_store(fixture: &Fixture, factory: Arc<PendingFactory>) -> SessionStore {
    SessionStore::new(
        fixture.admissions.clone(),
        fixture.workspace.clone(),
        "default".into(),
        factory,
        Arc::new(Router),
        Arc::new(Workspace),
        Arc::new(
            FileDiagnosticLog::open_global(fixture.root.join("logs"), "global", |_| {}).unwrap(),
        ),
    )
}
fn assert_live(fixture: &Fixture) {
    for take_over in [false, true] {
        let admission = fixture
            .admissions
            .resume(&fixture.workspace, &fixture.id(), take_over)
            .unwrap();
        assert_eq!(admission.admission.disposition, ClaimDisposition::Live);
        assert!(admission.admission.activation_lease.is_none());
    }
}
fn assert_settled(fixture: &Fixture, root: &SelectedSessionRoot) {
    let connection = crate::database::Connection::open(root.path().join("session.db")).unwrap();
    let count: i64 = connection
        .query_row("SELECT COUNT(*) FROM t21_settlement", [], |row| row.get(0))
        .unwrap();
    assert_eq!(count, 1);
    assert!(
        fs::read_to_string(root.path().join("session.log"))
            .unwrap()
            .contains("event=\"closed\"")
    );
    let retry = fixture
        .admissions
        .resume(&fixture.workspace, &fixture.id(), false)
        .unwrap();
    assert!(retry.admission.activation_lease.is_some());
}

#[tokio::test]
async fn dropped_initializer_retains_activation_until_real_admitted_transaction_settles() {
    let fixture = Fixture::new();
    let factory = Arc::new(PendingFactory::default());
    let store = pending_store(&fixture, factory.clone());
    let mut opening = Box::pin(store.open_admitted(
        fixture.workspace.clone(),
        fixture.admission(),
        Some(model()),
        "query",
        false,
    ));
    let deadline = Instant::now() + Duration::from_secs(5);
    std::future::poll_fn(|cx| {
        assert!(Instant::now() < deadline, "initializer was not reached");
        assert!(opening.as_mut().poll(cx).is_pending());
        if factory.lease.lock().unwrap().is_some() {
            std::task::Poll::Ready(())
        } else {
            cx.waker().wake_by_ref();
            std::task::Poll::Pending
        }
    })
    .await;
    let lease = factory.lease.lock().unwrap().clone().unwrap();
    let mut blocked = factory.blocked.lock().unwrap().take().unwrap();
    blocked.wait_entered();
    drop(opening);
    // Factory retains another Arc: startup ownership cannot rely on last-Arc Drop.
    assert!(lease.is_cancelled());
    assert!(matches!(
        *lease.cleanup.shared.terminal.borrow(),
        CleanupTerminal::Pending
    ));
    assert_live(&fixture);
    blocked.release();
    lease.cleanup.wait_closed().await.unwrap();
    assert_settled(&fixture, lease.selected_root());
    let before = fs::read(lease.selected_root().path().join("session.log")).unwrap();
    lease.write_diagnostic(&DiagnosticEvent::new(
        "test",
        "after-close",
        DiagnosticSeverity::Information,
    ));
    assert_eq!(
        fs::read(lease.selected_root().path().join("session.log")).unwrap(),
        before
    );
}

#[tokio::test]
async fn abandoned_actual_startup_image_cleanup_keeps_activation_until_queued_actor_work_settles() {
    let fixture = Fixture::new();
    let mut admission = fixture.admission();
    let root = admission.selected_root.take().unwrap();
    fs::create_dir_all(root.path()).unwrap();
    let diagnostics = FileDiagnosticLog::open_session(
        root.path().join("session.log"),
        "image-cancel",
        fixture.id().value(),
        |_| {},
    )
    .unwrap();
    let cleanup = ResourceCleanup::new(
        admission.admission.activation_lease.take().unwrap(),
        diagnostics,
    )
    .unwrap();
    let database = SessionDatabase::open(root.clone()).unwrap();
    cleanup.attach_database(database.clone()).unwrap();
    let mut blocked = BlockedTransaction::admit(&database);
    blocked.wait_entered();
    let images = ImageArtifactRepository::new(
        database.clone(),
        ImageArtifactStore::for_session(&root, "artifacts").unwrap(),
    )
    .unwrap();
    let mut settled = cleanup.shared.terminal.subscribe();
    let mut opening = Box::pin(SessionResourceLease::initialize(
        root.clone(),
        SessionIndex::new(root.clone(), fixture.id(), &fixture.workspace).unwrap(),
        cleanup,
        database,
        images,
    ));
    // The actual stale-image repository command is admitted behind the barrier.
    std::future::poll_fn(|cx| {
        assert!(opening.as_mut().poll(cx).is_pending());
        std::task::Poll::Ready(())
    })
    .await;
    drop(opening);
    assert!(matches!(*settled.borrow(), CleanupTerminal::Pending));
    assert_live(&fixture);
    blocked.release();
    while matches!(*settled.borrow_and_update(), CleanupTerminal::Pending) {
        settled.changed().await.unwrap();
    }
    assert_settled(&fixture, &root);
}

async fn bounded<T>(future: impl std::future::Future<Output = T>) -> T {
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut future = std::pin::pin!(future);
    std::future::poll_fn(|cx| {
        assert!(
            Instant::now() < deadline,
            "cleanup did not make bounded progress"
        );
        let result = future.as_mut().poll(cx);
        if result.is_pending() {
            cx.waker().wake_by_ref();
        }
        result
    })
    .await
}

#[tokio::test]
async fn disposer_can_close_its_own_lease_on_success_disposal_error_and_startup_failure() {
    for (fail_cleanup, fail_publication) in [(false, false), (true, false), (true, true)] {
        let fixture = Fixture::new();
        let factory = Arc::new(Factory::default());
        factory.close_own_lease.store(true, Ordering::Release);
        factory.fail_cleanup.store(fail_cleanup, Ordering::Release);
        factory
            .block_publication
            .store(fail_publication, Ordering::Release);
        let store = fixture.store(factory.clone());
        let result = bounded(store.open_admitted(
            fixture.workspace.clone(),
            fixture.admission(),
            Some(model()),
            "query",
            false,
        ))
        .await;
        if fail_publication {
            let error = result.err().expect("publication must fail");
            assert!(!error.to_string().contains("cleanup failed"));
        } else {
            let mut opened = result.unwrap();
            let closed = bounded(opened.close()).await;
            if fail_cleanup {
                assert!(closed.unwrap_err().to_string().contains("cleanup failed"));
            } else {
                closed.unwrap();
            }
        }
        assert_eq!(factory.disposed.load(Ordering::Acquire), 1);
        let lease = factory.lease.lock().unwrap().clone().unwrap();
        assert!(matches!(
            lease
                .database
                .admit("after-close", &NeverCancelled, |_| Ok(())),
            Err(parrot_domain::contracts::storage::AdmissionError::Rejected(
                DatabaseError::Closed
            ))
        ));
        let log_path = lease.selected_root().path().join("session.log");
        let before = fs::read_to_string(&log_path).unwrap();
        assert_eq!(before.matches("event=\"closed\"").count(), 1);
        lease.write_diagnostic(&DiagnosticEvent::new(
            "test",
            "after-close",
            DiagnosticSeverity::Information,
        ));
        assert_eq!(fs::read_to_string(log_path).unwrap(), before);
        assert!(
            fixture
                .admissions
                .resume(&fixture.workspace, &fixture.id(), false)
                .unwrap()
                .admission
                .activation_lease
                .is_some()
        );
    }
}

#[tokio::test]
async fn dropped_close_future_keeps_disposal_and_database_settlement_running() {
    let fixture = Fixture::new();
    let factory = Arc::new(Factory::default());
    factory.close_own_lease.store(true, Ordering::Release);
    let store = fixture.store(factory.clone());
    let mut opened = store
        .open_admitted(
            fixture.workspace.clone(),
            fixture.admission(),
            Some(model()),
            "query",
            false,
        )
        .await
        .unwrap();
    let lease = opened.resources.clone();
    let mut blocked = BlockedTransaction::admit(&lease.database);
    blocked.wait_entered();
    let mut closing = Box::pin(opened.close());
    std::future::poll_fn(|cx| {
        assert!(closing.as_mut().poll(cx).is_pending());
        std::task::Poll::Ready(())
    })
    .await;
    drop(closing);
    assert_live(&fixture);
    blocked.release();
    lease.cleanup.wait_closed().await.unwrap();
    opened.close().await.unwrap();
    assert_eq!(factory.disposed.load(Ordering::Acquire), 1);
    assert_settled(&fixture, lease.selected_root());
}

#[test]
fn returned_session_drop_finishes_cleanup_even_after_the_callers_tokio_runtime_shuts_down() {
    let fixture = Fixture::new();
    let factory = Arc::new(Factory::default());
    factory.close_own_lease.store(true, Ordering::Release);
    let store = fixture.store(factory.clone());
    let runtime = tokio::runtime::Builder::new_current_thread()
        .build()
        .unwrap();
    let opened = runtime
        .block_on(store.open_admitted(
            fixture.workspace.clone(),
            fixture.admission(),
            Some(model()),
            "query",
            false,
        ))
        .unwrap();
    let lease = opened.resources.clone();
    let mut blocked = BlockedTransaction::admit(&lease.database);
    blocked.wait_entered();
    drop(opened);
    drop(runtime);
    assert_live(&fixture);
    assert!(matches!(
        *lease.cleanup.shared.terminal.borrow(),
        CleanupTerminal::Pending
    ));
    blocked.release();
    tokio::runtime::Builder::new_current_thread()
        .build()
        .unwrap()
        .block_on(lease.cleanup.wait_closed())
        .unwrap();
    assert_eq!(factory.disposed.load(Ordering::Acquire), 1);
    assert_settled(&fixture, lease.selected_root());
}

struct PendingDisposal {
    started: Option<std::sync::mpsc::Sender<()>>,
    release: Option<tokio::sync::oneshot::Receiver<()>>,
    lease: Arc<SessionResourceLease>,
    resources_closed: Option<std::sync::mpsc::Sender<()>>,
}
impl StoredUserSession for PendingDisposal {
    fn model(&self) -> SessionModelSelection {
        model()
    }
    fn mode(&self) -> String {
        "query".into()
    }
    fn dispose(&mut self) -> BoxFuture<'_, Result<(), SessionStoreError>> {
        Box::pin(async move {
            self.started.take().unwrap().send(()).unwrap();
            if let Some(closed) = self.resources_closed.take() {
                self.lease.confirm_producers_settled();
                self.lease.close().await?;
                closed.send(()).unwrap();
            }
            self.release.take().unwrap().await.unwrap();
            self.lease.confirm_producers_settled();
            Ok(())
        })
    }
}

#[tokio::test]
async fn pending_session_disposal_remains_fail_closed_when_its_waiter_is_cancelled() {
    let fixture = Fixture::new();
    let mut admission = fixture.admission();
    let root = admission.selected_root.take().unwrap();
    fs::create_dir_all(root.path()).unwrap();
    let diagnostics = FileDiagnosticLog::open_session(
        root.path().join("session.log"),
        "dispose-cancel",
        fixture.id().value(),
        |_| {},
    )
    .unwrap();
    let lease = SessionResourceLease::open(
        SessionIndex::new(root, fixture.id(), &fixture.workspace).unwrap(),
        admission.admission.activation_lease.take().unwrap(),
        diagnostics,
    )
    .await
    .unwrap();
    let (started, entered) = std::sync::mpsc::channel();
    let (release, receiver) = tokio::sync::oneshot::channel();
    let session = PendingDisposal {
        started: Some(started),
        release: Some(receiver),
        lease: lease.clone(),
        resources_closed: None,
    };
    lease
        .cleanup
        .shared
        .initialized
        .store(true, Ordering::Release);
    lease.cleanup.shared.producers_settled.send_replace(false);
    let mut closing = Box::pin(lease.close_with_session(Some(Box::new(session))));
    std::future::poll_fn(|cx| {
        assert!(closing.as_mut().poll(cx).is_pending());
        std::task::Poll::Ready(())
    })
    .await;
    entered.recv_timeout(Duration::from_secs(5)).unwrap();
    drop(closing);
    assert!(matches!(
        *lease.cleanup.shared.terminal.borrow(),
        CleanupTerminal::Pending
    ));
    assert_live(&fixture);
    release.send(()).unwrap();
    lease.cleanup.wait_closed().await.unwrap();
    assert!(
        fixture
            .admissions
            .resume(&fixture.workspace, &fixture.id(), false)
            .unwrap()
            .admission
            .activation_lease
            .is_some()
    );
}

#[tokio::test]
async fn reentrant_resource_close_settles_database_but_keeps_activation_until_disposer_finishes() {
    let fixture = Fixture::new();
    let mut admission = fixture.admission();
    let root = admission.selected_root.take().unwrap();
    fs::create_dir_all(root.path()).unwrap();
    let diagnostics = FileDiagnosticLog::open_session(
        root.path().join("session.log"),
        "reentrant-dispose",
        fixture.id().value(),
        |_| {},
    )
    .unwrap();
    let lease = SessionResourceLease::open(
        SessionIndex::new(root, fixture.id(), &fixture.workspace).unwrap(),
        admission.admission.activation_lease.take().unwrap(),
        diagnostics,
    )
    .await
    .unwrap();
    let mut blocked = BlockedTransaction::admit(&lease.database);
    blocked.wait_entered();
    let (started, entered) = std::sync::mpsc::channel();
    let (resources_closed, closed) = std::sync::mpsc::channel();
    let (release, receiver) = tokio::sync::oneshot::channel();
    let session = PendingDisposal {
        started: Some(started),
        release: Some(receiver),
        lease: lease.clone(),
        resources_closed: Some(resources_closed),
    };
    lease
        .cleanup
        .shared
        .initialized
        .store(true, Ordering::Release);
    lease.cleanup.shared.producers_settled.send_replace(false);
    let mut closing = Box::pin(lease.close_with_session(Some(Box::new(session))));
    std::future::poll_fn(|cx| {
        assert!(closing.as_mut().poll(cx).is_pending());
        std::task::Poll::Ready(())
    })
    .await;
    entered.recv_timeout(Duration::from_secs(5)).unwrap();
    drop(closing);
    assert!(matches!(
        closed.try_recv(),
        Err(std::sync::mpsc::TryRecvError::Empty)
    ));
    assert_live(&fixture);
    blocked.release();
    closed.recv_timeout(Duration::from_secs(5)).unwrap();
    assert!(matches!(
        *lease.cleanup.shared.terminal.borrow(),
        CleanupTerminal::Pending
    ));
    assert_live(&fixture);
    let connection =
        crate::database::Connection::open(lease.selected_root().path().join("session.db")).unwrap();
    let count: i64 = connection
        .query_row("SELECT COUNT(*) FROM t21_settlement", [], |row| row.get(0))
        .unwrap();
    assert_eq!(count, 1);
    let log_path = lease.selected_root().path().join("session.log");
    let before = fs::read_to_string(&log_path).unwrap();
    assert_eq!(before.matches("event=\"closed\"").count(), 1);
    lease.write_diagnostic(&DiagnosticEvent::new(
        "test",
        "after-resource-close",
        DiagnosticSeverity::Information,
    ));
    assert_eq!(fs::read_to_string(log_path).unwrap(), before);
    release.send(()).unwrap();
    bounded(lease.cleanup.wait_closed()).await.unwrap();
    assert_settled(&fixture, lease.selected_root());
}

#[test]
fn failure_codes_follow_concrete_database_history_and_io_causes() {
    use crate::history_files::{Error, ErrorKind};
    for (error, code) in [
        (
            SessionStoreError::failure(DatabaseError::Io(std::io::Error::from(
                std::io::ErrorKind::PermissionDenied,
            ))),
            "access_denied",
        ),
        (
            SessionStoreError::failure(DatabaseError::Io(std::io::Error::from(
                std::io::ErrorKind::NotFound,
            ))),
            "io",
        ),
        (
            SessionStoreError::failure(DatabaseError::Sqlite(rusqlite::Error::InvalidQuery)),
            "unexpected",
        ),
        (
            SessionStoreError::failure(DatabaseError::WorkerStopped),
            "unexpected",
        ),
        (
            SessionStoreError::failure(Error::new(ErrorKind::InvalidData, "invalid-json")),
            "unexpected",
        ),
        (
            SessionStoreError::failure(Error::io(
                "metadata",
                std::io::Error::from(std::io::ErrorKind::PermissionDenied),
            )),
            "access_denied",
        ),
        (
            SessionStoreError::failure(Error::io(
                "metadata",
                std::io::Error::from(std::io::ErrorKind::NotFound),
            )),
            "io",
        ),
        (
            SessionStoreError::failure(Error::new(ErrorKind::InvalidInput, "argument")),
            "argument",
        ),
        (
            SessionStoreError::failure(Error::new(ErrorKind::Unowned, "unowned")),
            "invalid_operation",
        ),
    ] {
        assert_eq!(error.code(), code);
        assert!(std::error::Error::source(&error).is_some());
    }
}

#[tokio::test]
async fn actual_sqlite_open_failure_and_malformed_metadata_are_unexpected_not_io() {
    for sqlite in [false, true] {
        let fixture = Fixture::new();
        let factory = Arc::new(Factory::default());
        let store = fixture.store(factory);
        let admission = fixture.admission();
        let root = admission.selected_root.as_ref().unwrap().clone();
        fs::create_dir_all(root.path()).unwrap();
        if sqlite {
            fs::create_dir(root.path().join("session.db")).unwrap();
        } else {
            fs::write(root.path().join("meta.json"), "not-json").unwrap();
        }
        let error = match store
            .open_admitted(
                fixture.workspace.clone(),
                admission,
                Some(model()),
                "query",
                false,
            )
            .await
        {
            Ok(_) => panic!("expected original opening failure"),
            Err(error) => error,
        };
        assert_eq!(error.code(), "unexpected");
        let cause = std::error::Error::source(&error).unwrap();
        if sqlite {
            assert!(matches!(
                cause.downcast_ref::<DatabaseError>(),
                Some(DatabaseError::Sqlite(_))
            ));
        } else {
            assert_eq!(
                cause
                    .downcast_ref::<crate::history_files::Error>()
                    .unwrap()
                    .kind,
                crate::history_files::ErrorKind::InvalidData
            );
        }
        assert!(
            fixture
                .global()
                .lines()
                .any(|line| line.contains(if sqlite {
                    "database_failure"
                } else {
                    "metadata_read_failure"
                }) && line.contains("error=\"unexpected\""))
        );
        assert!(
            fs::read_to_string(root.path().join("session.log"))
                .unwrap()
                .lines()
                .any(
                    |line| line.contains("event=\"open\"") && line.contains("error=\"unexpected\"")
                )
        );
    }
}

#[cfg(unix)]
#[tokio::test]
async fn actual_permission_denied_metadata_access_retains_original_io_cause_and_category_when_enforced()
 {
    use std::os::unix::fs::PermissionsExt;
    let fixture = Fixture::new();
    let factory = Arc::new(Factory::default());
    let store = fixture.store(factory);
    let admission = fixture.admission();
    let root = admission.selected_root.as_ref().unwrap();
    fs::create_dir_all(root.path()).unwrap();
    let path = root.path().join("meta.json");
    fs::write(
        &path,
        serde_json::to_vec(&SessionMeta::new(
            fixture.id().value(),
            fixture.workspace.to_str().unwrap(),
            "provider",
            "model",
        ))
        .unwrap(),
    )
    .unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o0)).unwrap();
    if fs::File::open(&path).is_ok() {
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
        println!(
            "T21_PERMISSION_DENIAL_NOT_ENFORCED: privileged executor; concrete wrapped PermissionDenied classifier covered separately, zero actual permission-denial claim"
        );
        return;
    }
    let error = match store
        .open_admitted(
            fixture.workspace.clone(),
            admission,
            Some(model()),
            "query",
            false,
        )
        .await
    {
        Ok(_) => panic!("expected denied metadata"),
        Err(error) => error,
    };
    fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
    assert_eq!(error.code(), "access_denied");
    let history = std::error::Error::source(&error)
        .unwrap()
        .downcast_ref::<crate::history_files::Error>()
        .unwrap();
    let io = std::error::Error::source(history)
        .unwrap()
        .downcast_ref::<std::io::Error>()
        .unwrap();
    assert_eq!(io.kind(), std::io::ErrorKind::PermissionDenied);
    assert!(
        fixture
            .global()
            .lines()
            .any(|line| line.contains("metadata_read_failure")
                && line.contains("error=\"access_denied\""))
    );
    println!(
        "T21_PERMISSION_DENIAL_ENFORCED: actual metadata read denied and original cause retained"
    );
}

// Real tasks below are spawned by the constructor on the lease worker's runtime.
// Their database write and join are the settlement boundary, not a cached flag.
struct DrivenFactory {
    lease: Mutex<Option<Arc<SessionResourceLease>>>,
    release: Mutex<Option<tokio::sync::oneshot::Receiver<()>>>,
    entered: Mutex<Option<std::sync::mpsc::Sender<()>>>,
    completed: Arc<AtomicBool>,
    disposed: Arc<AtomicUsize>,
    hold_constructor: bool,
    fail_constructor: bool,
    unknown: bool,
}
struct DrivenSession {
    lease: Arc<SessionResourceLease>,
    producer: Option<tokio::task::JoinHandle<()>>,
    disposed: Arc<AtomicUsize>,
}
impl StoredUserSession for DrivenSession {
    fn model(&self) -> SessionModelSelection {
        model()
    }
    fn mode(&self) -> String {
        "query".into()
    }
    fn dispose(&mut self) -> BoxFuture<'_, Result<(), SessionStoreError>> {
        Box::pin(async move {
            self.disposed.fetch_add(1, Ordering::AcqRel);
            if let Some(producer) = self.producer.take() {
                producer.await.map_err(SessionStoreError::failure)?;
            }
            self.lease.confirm_producers_settled();
            self.lease.close().await
        })
    }
}
impl StoredUserSessionFactory for DrivenFactory {
    fn create(
        &self,
        lease: Arc<SessionResourceLease>,
        _: SessionInitialization,
    ) -> BoxFuture<'_, Result<Box<dyn StoredUserSession>, SessionStoreError>> {
        Box::pin(async move {
            *self.lease.lock().unwrap() = Some(lease.clone());
            let release = self.release.lock().unwrap().take().unwrap();
            let entered = self.entered.lock().unwrap().take().unwrap();
            let completed = self.completed.clone();
            let database = lease.database.clone();
            let producer = tokio::spawn(async move {
                entered.send(()).unwrap();
                release.await.unwrap();
                database.admit("owned-producer", &NeverCancelled, |tx| {
                    tx.execute_batch("CREATE TABLE driven_producer(value INTEGER); INSERT INTO driven_producer VALUES (1);")?;
                    Ok(())
                }).unwrap().into_completion().await.unwrap();
                completed.store(true, Ordering::Release);
            });
            if self.unknown {
                // Deliberately lost join knowledge: an actual live producer remains.
                drop(producer);
                return Err(SessionStoreError::failure(std::io::Error::other(
                    "unknown constructor join",
                )));
            }
            let producer = if self.hold_constructor || self.fail_constructor {
                producer.await.map_err(SessionStoreError::failure)?;
                None
            } else {
                Some(producer)
            };
            if self.fail_constructor {
                lease.confirm_producers_settled();
                return Err(SessionStoreError::failure(std::io::Error::other(
                    "constructor rolled back",
                )));
            }
            Ok(Box::new(DrivenSession {
                lease,
                producer,
                disposed: self.disposed.clone(),
            }) as Box<dyn StoredUserSession>)
        })
    }
}
fn driven_factory(
    hold_constructor: bool,
    fail_constructor: bool,
    unknown: bool,
) -> (
    Arc<DrivenFactory>,
    tokio::sync::oneshot::Sender<()>,
    std::sync::mpsc::Receiver<()>,
) {
    let (release, receiver) = tokio::sync::oneshot::channel();
    let (entered, observed) = std::sync::mpsc::channel();
    (
        Arc::new(DrivenFactory {
            lease: Mutex::new(None),
            release: Mutex::new(Some(receiver)),
            entered: Mutex::new(Some(entered)),
            completed: Arc::new(AtomicBool::new(false)),
            disposed: Arc::new(AtomicUsize::new(0)),
            hold_constructor,
            fail_constructor,
            unknown,
        }),
        release,
        observed,
    )
}
fn driven_store(fixture: &Fixture, factory: Arc<DrivenFactory>) -> SessionStore {
    SessionStore::new(
        fixture.admissions.clone(),
        fixture.workspace.clone(),
        "default".into(),
        factory,
        Arc::new(Router),
        Arc::new(Workspace),
        Arc::new(
            FileDiagnosticLog::open_global(fixture.root.join("logs"), "global", |_| {}).unwrap(),
        ),
    )
}
fn caller_runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
}
#[test]
fn owned_constructor_and_rollback_survive_open_observer_and_caller_runtime_drop() {
    for fails in [false, true] {
        let fixture = Fixture::new();
        let (factory, release, entered) = driven_factory(true, fails, false);
        let store = driven_store(&fixture, factory.clone());
        let runtime = caller_runtime();
        let mut opening = Box::pin(store.open_admitted(
            fixture.workspace.clone(),
            fixture.admission(),
            Some(model()),
            "query",
            false,
        ));
        runtime.block_on(std::future::poll_fn(|cx| {
            assert!(opening.as_mut().poll(cx).is_pending());
            if factory.lease.lock().unwrap().is_some() {
                std::task::Poll::Ready(())
            } else {
                cx.waker().wake_by_ref();
                std::task::Poll::Pending
            }
        }));
        entered.recv_timeout(Duration::from_secs(5)).unwrap();
        let lease = factory.lease.lock().unwrap().clone().unwrap();
        drop(opening);
        drop(runtime);
        assert_live(&fixture);
        assert!(!factory.completed.load(Ordering::Acquire));
        release.send(()).unwrap();
        caller_runtime()
            .block_on(bounded(lease.cleanup.wait_closed()))
            .unwrap();
        assert!(factory.completed.load(Ordering::Acquire));
        assert_eq!(
            factory.disposed.load(Ordering::Acquire),
            usize::from(!fails)
        );
        assert!(
            fixture
                .admissions
                .resume(&fixture.workspace, &fixture.id(), false)
                .unwrap()
                .admission
                .activation_lease
                .is_some()
        );
    }
}
#[test]
fn owned_session_runtime_stays_driven_and_repeated_close_cannot_bypass_live_producer() {
    let fixture = Fixture::new();
    let (factory, release, entered) = driven_factory(false, false, false);
    let store = driven_store(&fixture, factory.clone());
    let runtime = caller_runtime();
    let opened = runtime
        .block_on(store.open_admitted(
            fixture.workspace.clone(),
            fixture.admission(),
            Some(model()),
            "query",
            false,
        ))
        .unwrap();
    let lease = opened.resources.clone();
    entered.recv_timeout(Duration::from_secs(5)).unwrap();
    drop(runtime);
    let (tick, observed) = std::sync::mpsc::channel();
    lease.runtime_handle().spawn(async move {
        tick.send(()).unwrap();
    });
    observed.recv_timeout(Duration::from_secs(5)).unwrap();
    drop(opened);
    let runtime = caller_runtime();
    runtime.block_on(async {
        let mut first = Box::pin(lease.close());
        let mut second = Box::pin(lease.close());
        std::future::poll_fn(|cx| {
            assert!(first.as_mut().poll(cx).is_pending());
            assert!(second.as_mut().poll(cx).is_pending());
            std::task::Poll::Ready(())
        })
        .await;
        assert_live(&fixture);
        lease
            .database
            .admit("still-open", &NeverCancelled, |_| Ok(()))
            .unwrap()
            .into_completion()
            .await
            .unwrap();
        release.send(()).unwrap();
        bounded(first).await.unwrap();
        bounded(second).await.unwrap();
        bounded(lease.cleanup.wait_closed()).await.unwrap();
    });
    assert_eq!(factory.disposed.load(Ordering::Acquire), 1);
    assert!(factory.completed.load(Ordering::Acquire));
    assert!(
        fixture
            .admissions
            .resume(&fixture.workspace, &fixture.id(), false)
            .unwrap()
            .admission
            .activation_lease
            .is_some()
    );
}
#[tokio::test]
async fn unknown_constructor_join_retains_driver_database_activation_and_stable_failure() {
    let fixture = Fixture::new();
    let (factory, release, entered) = driven_factory(false, false, true);
    let store = driven_store(&fixture, factory.clone());
    let error = store
        .open_admitted(
            fixture.workspace.clone(),
            fixture.admission(),
            Some(model()),
            "query",
            false,
        )
        .await
        .err()
        .unwrap();
    assert_eq!(error.to_string(), "unknown constructor join");
    entered.recv_timeout(Duration::from_secs(5)).unwrap();
    let lease = factory.lease.lock().unwrap().clone().unwrap();
    assert_live(&fixture);
    let first = bounded(lease.close()).await.unwrap_err().to_string();
    let second = bounded(lease.close()).await.unwrap_err().to_string();
    assert_eq!(first, second);
    assert!(first.contains("host producer settlement was not confirmed"));
    release.send(()).unwrap();
    bounded(async {
        while !factory.completed.load(Ordering::Acquire) {
            tokio::task::yield_now().await;
        }
    })
    .await;
    lease
        .database
        .admit("fenced-not-closed", &NeverCancelled, |_| Ok(()))
        .unwrap()
        .into_completion()
        .await
        .unwrap();
    assert_live(&fixture);
    assert_eq!(bounded(lease.close()).await.unwrap_err().to_string(), first);
    // The deliberately unsettled lease owns a parked live driver until process exit.
    // Preserve its physical fixture rather than removing a still-owned database.
    std::mem::forget(fixture);
}
struct DeliveryWake(AtomicUsize);
impl std::task::Wake for DeliveryWake {
    fn wake(self: Arc<Self>) {
        self.0.fetch_add(1, Ordering::AcqRel);
    }
    fn wake_by_ref(self: &Arc<Self>) {
        self.0.fetch_add(1, Ordering::AcqRel);
    }
}
#[test]
fn delivered_opened_envelope_is_disposed_once_when_receiver_drops_before_taking_it() {
    let fixture = Fixture::new();
    let (factory, release, entered) = driven_factory(true, false, false);
    let store = driven_store(&fixture, factory.clone());
    let runtime = caller_runtime();
    let mut opening = Box::pin(store.open_admitted(
        fixture.workspace.clone(),
        fixture.admission(),
        Some(model()),
        "query",
        false,
    ));
    let wake = Arc::new(DeliveryWake(AtomicUsize::new(0)));
    let waker = std::task::Waker::from(wake.clone());
    let mut context = std::task::Context::from_waker(&waker);
    runtime.block_on(async {
        while factory.lease.lock().unwrap().is_none() {
            assert!(opening.as_mut().poll(&mut context).is_pending());
            tokio::task::yield_now().await;
        }
    });
    entered.recv_timeout(Duration::from_secs(5)).unwrap();
    assert!(opening.as_mut().poll(&mut context).is_pending());
    let baseline = wake.0.load(Ordering::Acquire);
    release.send(()).unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    while wake.0.load(Ordering::Acquire) == baseline {
        assert!(Instant::now() < deadline, "owned result was not delivered");
        std::thread::yield_now();
    }
    // No poll after the result sender woke this observer: the envelope is queued.
    drop(opening);
    drop(runtime);
    let lease = factory.lease.lock().unwrap().clone().unwrap();
    caller_runtime()
        .block_on(bounded(lease.cleanup.wait_closed()))
        .unwrap();
    assert_eq!(factory.disposed.load(Ordering::Acquire), 1);
    assert!(
        fixture
            .admissions
            .resume(&fixture.workspace, &fixture.id(), false)
            .unwrap()
            .admission
            .activation_lease
            .is_some()
    );
}
#[tokio::test]
async fn same_owned_metadata_index_preserves_identity_timestamps_and_serializes_updates() {
    let fixture = Fixture::new();
    let factory = Arc::new(Factory::default());
    let store = fixture.store(factory);
    let mut opened = store
        .open_admitted(
            fixture.workspace.clone(),
            fixture.admission(),
            Some(model()),
            "query",
            false,
        )
        .await
        .unwrap();
    let original = opened.resources.index.find().unwrap().unwrap();
    let mut supported = original.clone();
    supported.root_agent_name = None;
    supported.created_at = None;
    supported.last_opened_at = Some("retained-time".into());
    opened.resources.index.publish(&supported).unwrap();
    opened
        .resources
        .update_selection_metadata("other", "other/model", "alias", "mode")
        .unwrap();
    let changed = opened.resources.index.find().unwrap().unwrap();
    assert_eq!(changed.id, supported.id);
    assert_eq!(changed.working_directory, supported.working_directory);
    assert_eq!(changed.root_agent_name, supported.root_agent_name);
    assert_eq!(changed.created_at, supported.created_at);
    assert_eq!(changed.last_opened_at, supported.last_opened_at);
    assert_eq!(changed.provider_id.as_deref(), Some("other"));
    assert_eq!(changed.model.as_deref(), Some("other/model"));
    assert_eq!(changed.selector.as_deref(), Some("alias"));
    assert_eq!(changed.mode.as_deref(), Some("mode"));
    opened.record_opened().unwrap();
    opened
        .resources
        .update_selection_metadata("latest", "latest/model", "latest", "latest-mode")
        .unwrap();
    let latest = opened.resources.index.find().unwrap().unwrap();
    assert_eq!(latest.last_opened_at, Some((opened.clock)()));
    assert_eq!(latest.provider_id.as_deref(), Some("latest"));
    opened.close().await.unwrap();
}

#[tokio::test]
async fn metadata_updater_uses_record_opened_gate_and_preserves_failures() {
    let fixture = Fixture::new();
    let factory = Arc::new(Factory::default());
    let store = fixture.store(factory);
    let mut opened = store
        .open_admitted(
            fixture.workspace.clone(),
            fixture.admission(),
            Some(model()),
            "query",
            false,
        )
        .await
        .unwrap();
    std::thread::scope(|scope| {
        let guard = opened.resources.metadata_gate.lock().unwrap();
        let (entered, ready) = std::sync::mpsc::channel();
        let (completed, done) = std::sync::mpsc::channel();
        let record = &opened;
        let entered2 = entered.clone();
        let completed2 = completed.clone();
        scope.spawn(move || {
            entered.send(()).unwrap();
            record.record_opened().unwrap();
            completed.send(()).unwrap();
        });
        let lease = opened.resources.clone();
        scope.spawn(move || {
            entered2.send(()).unwrap();
            lease
                .update_selection_metadata(
                    "gate-provider",
                    "gate-model",
                    "gate-selector",
                    "gate-mode",
                )
                .unwrap();
            completed2.send(()).unwrap();
        });
        ready.recv_timeout(Duration::from_secs(5)).unwrap();
        ready.recv_timeout(Duration::from_secs(5)).unwrap();
        assert!(matches!(
            done.recv_timeout(Duration::from_millis(20)),
            Err(std::sync::mpsc::RecvTimeoutError::Timeout)
        ));
        drop(guard);
        done.recv_timeout(Duration::from_secs(5)).unwrap();
        done.recv_timeout(Duration::from_secs(5)).unwrap();
    });
    let latest = opened.resources.index.find().unwrap().unwrap();
    assert_eq!(latest.last_opened_at, Some((opened.clock)()));
    assert_eq!(latest.provider_id.as_deref(), Some("gate-provider"));
    let path = opened.resources.index.metadata_path();
    let bytes = fs::read(path).unwrap();
    let saved = path.with_file_name("saved-metadata-for-test");
    fs::rename(path, &saved).unwrap();
    assert!(matches!(
        opened
            .resources
            .update_selection_metadata("p", "m", "s", "q"),
        Err(SessionStoreError::InvalidMetadata)
    ));
    assert!(!path.exists());
    fs::rename(saved, path).unwrap();
    let mut foreign = latest.clone();
    foreign.id = Some("user-session-foreign".into());
    fs::write(path, serde_json::to_vec(&foreign).unwrap()).unwrap();
    assert!(
        opened
            .resources
            .update_selection_metadata("p", "m", "s", "q")
            .is_err()
    );
    assert_eq!(
        fs::read(path).unwrap(),
        serde_json::to_vec(&foreign).unwrap()
    );
    fs::write(path, &bytes).unwrap();
    fs::create_dir(path.with_file_name("meta.json.staging")).unwrap();
    assert!(
        opened
            .resources
            .update_selection_metadata("p", "m", "s", "q")
            .is_err()
    );
    assert_eq!(fs::read(path).unwrap(), bytes);
    opened.close().await.unwrap();
}
struct PanickingFactory(Mutex<Option<Arc<SessionResourceLease>>>);
impl StoredUserSessionFactory for PanickingFactory {
    fn create(
        &self,
        lease: Arc<SessionResourceLease>,
        _: SessionInitialization,
    ) -> BoxFuture<'_, Result<Box<dyn StoredUserSession>, SessionStoreError>> {
        Box::pin(async move {
            *self.0.lock().unwrap() = Some(lease);
            panic!("intentional owned-constructor panic");
        })
    }
}
#[tokio::test]
async fn worker_panic_keeps_runtime_and_activation_fenced_with_repeatable_failure() {
    let fixture = Fixture::new();
    let factory = Arc::new(PanickingFactory(Mutex::new(None)));
    let store = SessionStore::new(
        fixture.admissions.clone(),
        fixture.workspace.clone(),
        "default".into(),
        factory.clone(),
        Arc::new(Router),
        Arc::new(Workspace),
        Arc::new(
            FileDiagnosticLog::open_global(fixture.root.join("logs"), "global", |_| {}).unwrap(),
        ),
    );
    assert!(
        store
            .open_admitted(
                fixture.workspace.clone(),
                fixture.admission(),
                Some(model()),
                "query",
                false
            )
            .await
            .is_err()
    );
    let lease = factory.0.lock().unwrap().clone().unwrap();
    let first = bounded(lease.close()).await.unwrap_err().to_string();
    assert_eq!(bounded(lease.close()).await.unwrap_err().to_string(), first);
    let (tick, observed) = tokio::sync::oneshot::channel();
    lease.runtime_handle().spawn(async move {
        tick.send(()).unwrap();
    });
    bounded(observed).await.unwrap();
    lease
        .database
        .admit("panic-fenced", &NeverCancelled, |_| Ok(()))
        .unwrap()
        .into_completion()
        .await
        .unwrap();
    assert_live(&fixture);
    std::mem::forget(fixture);
}

#[test]
fn registered_host_close_retains_driver_through_publication_and_actual_join() {
    let fixture = Fixture::new();
    let (factory, producer_release, entered) = driven_factory(false, false, false);
    let store = driven_store(&fixture, factory);
    let caller = caller_runtime();
    let opened = caller
        .block_on(store.open_admitted(
            fixture.workspace.clone(),
            fixture.admission(),
            Some(model()),
            "query",
            false,
        ))
        .unwrap();
    entered.recv_timeout(Duration::from_secs(5)).unwrap();
    let lease = opened.resources.clone();
    let (resources_closed, closed) = tokio::sync::oneshot::channel();
    let (publication_release, publication) = tokio::sync::oneshot::channel();
    let (published, observed) = tokio::sync::watch::channel(None);
    let duplicate_lease = lease.clone();
    let mut receipt = lease
        .spawn_close(move || {
            assert_eq!(
                std::thread::current().name(),
                Some("session-resource-cleanup")
            );
            async move {
                // Recursive registration rejects immediately rather than self-waiting.
                assert!(duplicate_lease.spawn_close(|| async {}).is_err());
                drop(duplicate_lease);
                let result = {
                    let mut opened = opened;
                    opened.close().await.map_err(|error| error.to_string())
                };
                resources_closed.send(()).unwrap();
                // The OpenedSession envelope is already dropped. ResourceClosed is
                // deliberately not the outer close/publication completion boundary.
                publication.await.unwrap();
                published.send(Some(result)).unwrap();
            }
        })
        .unwrap();
    drop(caller);
    producer_release.send(()).unwrap();
    caller_runtime().block_on(async {
        bounded(closed).await.unwrap();
        assert!(observed.borrow().is_none());
        assert!(matches!(
            receipt.try_recv(),
            Err(tokio::sync::oneshot::error::TryRecvError::Empty)
        ));
        lease.close().await.unwrap();
        assert!(lease.spawn_close(|| async {}).is_err());
        publication_release.send(()).unwrap();
        // This receipt carries the result of awaiting the ACTUAL Tokio task.
        bounded(receipt).await.unwrap().unwrap();
        assert_eq!(*observed.borrow(), Some(Ok(())));
        lease.close().await.unwrap();
    });
    assert!(
        fixture
            .admissions
            .resume(&fixture.workspace, &fixture.id(), false)
            .unwrap()
            .admission
            .activation_lease
            .is_some()
    );
}
#[test]
fn dropped_close_receipt_and_caller_runtime_do_not_drop_registered_publication() {
    let fixture = Fixture::new();
    let factory = Arc::new(Factory::default());
    let store = fixture.store(factory.clone());
    let caller = caller_runtime();
    let opened = caller
        .block_on(store.open_admitted(
            fixture.workspace.clone(),
            fixture.admission(),
            Some(model()),
            "query",
            false,
        ))
        .unwrap();
    let lease = opened.resources.clone();
    let (resources_closed, closed) = std::sync::mpsc::channel();
    let (release, publication) = tokio::sync::oneshot::channel();
    let (published, observed) = std::sync::mpsc::channel();
    let receipt = lease
        .spawn_close(move || async move {
            {
                let mut opened = opened;
                opened.close().await.unwrap();
            }
            resources_closed.send(()).unwrap();
            publication.await.unwrap();
            published.send(()).unwrap();
        })
        .unwrap();
    drop(receipt);
    drop(caller);
    closed.recv_timeout(Duration::from_secs(5)).unwrap();
    assert_eq!(factory.disposed.load(Ordering::Acquire), 1);
    release.send(()).unwrap();
    observed.recv_timeout(Duration::from_secs(5)).unwrap();
    caller_runtime().block_on(lease.close()).unwrap();
}
#[tokio::test]
async fn registered_close_reports_actual_task_panic_and_preserves_disposal_error() {
    for (panic_before_publish, panic_after_publish) in
        [(false, false), (true, false), (false, true)]
    {
        let panics = panic_before_publish || panic_after_publish;
        let fixture = Fixture::new();
        let factory = Arc::new(Factory::default());
        factory.fail_cleanup.store(!panics, Ordering::Release);
        let store = fixture.store(factory.clone());
        let opened = store
            .open_admitted(
                fixture.workspace.clone(),
                fixture.admission(),
                Some(model()),
                "query",
                false,
            )
            .await
            .unwrap();
        let lease = opened.resources.clone();
        let (published, observed) = tokio::sync::watch::channel(None);
        let receipt = lease
            .spawn_close(move || async move {
                let result = {
                    let mut opened = opened;
                    opened.close().await.map_err(|error| error.to_string())
                };
                assert!(
                    !panic_before_publish,
                    "intentional close panic before publication"
                );
                published.send(Some(result)).unwrap();
                assert!(
                    !panic_after_publish,
                    "intentional close panic after publication"
                );
            })
            .unwrap();
        let joined = bounded(receipt).await.unwrap();
        if panics {
            assert!(joined.unwrap_err().is_panic());
            if panic_before_publish {
                assert!(observed.borrow().is_none());
                assert!(observed.has_changed().is_err());
            } else {
                assert_eq!(*observed.borrow(), Some(Ok(())));
            }
            lease.close().await.unwrap();
        } else {
            joined.unwrap();
            let first = lease.close().await.unwrap_err().to_string();
            assert_eq!(first, "cleanup failed");
            assert_eq!(lease.close().await.unwrap_err().to_string(), first);
            assert_eq!(*observed.borrow(), Some(Err(first)));
        }
        assert_eq!(factory.disposed.load(Ordering::Acquire), 1);
        assert!(lease.spawn_close(|| async {}).is_err());
    }
}
#[tokio::test]
async fn late_close_builder_is_rejected_before_poll_while_external_close_is_pending() {
    let fixture = Fixture::new();
    let factory = Arc::new(Factory::default());
    let store = fixture.store(factory);
    let mut opened = store
        .open_admitted(
            fixture.workspace.clone(),
            fixture.admission(),
            Some(model()),
            "query",
            false,
        )
        .await
        .unwrap();
    let lease = opened.resources.clone();
    let mut barrier = BlockedTransaction::admit(&lease.database);
    barrier.wait_entered();
    let mut closing = Box::pin(opened.close());
    std::future::poll_fn(|cx| {
        assert!(closing.as_mut().poll(cx).is_pending());
        std::task::Poll::Ready(())
    })
    .await;
    let called = Arc::new(AtomicBool::new(false));
    let builder_called = called.clone();
    let failure = lease
        .spawn_close(move || {
            builder_called.store(true, Ordering::Release);
            async {}
        })
        .unwrap_err()
        .to_string();
    assert!(!called.load(Ordering::Acquire));
    barrier.release();
    bounded(closing).await.unwrap();
    assert_eq!(
        lease.spawn_close(|| async {}).unwrap_err().to_string(),
        failure
    );
    lease.close().await.unwrap();
}
struct PanickingDisposerFactory(Mutex<Option<Arc<SessionResourceLease>>>);
struct PanickingDisposer;
impl StoredUserSessionFactory for PanickingDisposerFactory {
    fn create(
        &self,
        lease: Arc<SessionResourceLease>,
        _: SessionInitialization,
    ) -> BoxFuture<'_, Result<Box<dyn StoredUserSession>, SessionStoreError>> {
        Box::pin(async move {
            *self.0.lock().unwrap() = Some(lease);
            Ok(Box::new(PanickingDisposer) as Box<dyn StoredUserSession>)
        })
    }
}
impl StoredUserSession for PanickingDisposer {
    fn model(&self) -> SessionModelSelection {
        model()
    }
    fn mode(&self) -> String {
        "query".into()
    }
    fn dispose(&mut self) -> BoxFuture<'_, Result<(), SessionStoreError>> {
        Box::pin(async { panic!("intentional disposer panic before settlement") })
    }
}
#[tokio::test]
async fn worker_disposer_panic_still_joins_registered_close_and_fences_resources() {
    let fixture = Fixture::new();
    let factory = Arc::new(PanickingDisposerFactory(Mutex::new(None)));
    let store = SessionStore::new(
        fixture.admissions.clone(),
        fixture.workspace.clone(),
        "default".into(),
        factory.clone(),
        Arc::new(Router),
        Arc::new(Workspace),
        Arc::new(
            FileDiagnosticLog::open_global(fixture.root.join("logs"), "global", |_| {}).unwrap(),
        ),
    );
    let opened = store
        .open_admitted(
            fixture.workspace.clone(),
            fixture.admission(),
            Some(model()),
            "query",
            false,
        )
        .await
        .unwrap();
    let lease = opened.resources.clone();
    let (published, observed) = tokio::sync::watch::channel(None);
    let receipt = lease
        .spawn_close(move || async move {
            let result = {
                let mut opened = opened;
                opened.close().await.map_err(|error| error.to_string())
            };
            published.send(Some(result)).unwrap();
        })
        .unwrap();
    bounded(receipt).await.unwrap().unwrap();
    let first = lease.close().await.unwrap_err().to_string();
    assert_eq!(*observed.borrow(), Some(Err(first.clone())));
    assert_eq!(lease.close().await.unwrap_err().to_string(), first);
    assert_live(&fixture);
    std::mem::forget(fixture);
}
