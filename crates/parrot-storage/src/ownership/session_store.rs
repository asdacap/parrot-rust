//! Selected-root session opening and explicit resource lifetime ownership.
use super::{ClaimDisposition, SessionActivationLease, SessionAdmission, SessionAdmissions};
use crate::artifacts::{ImageArtifactRepository, ImageArtifactStore};
use crate::database::{DatabaseError, SessionDatabase};
use crate::diagnostics::{DiagnosticEvent, DiagnosticSeverity, FileDiagnosticLog};
use crate::events::{EventRepository, RepositoryContext};
use crate::history_files::{SessionCatalogEntry, SessionIndex, SessionMeta};
use parrot_domain::contracts::async_ops::{BoxFuture, Cancellation};
use parrot_domain::contracts::paths::SelectedSessionRoot;
use parrot_domain::values::identifiers::UserSessionId;
use std::path::{Path, PathBuf};
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, Ordering},
};
use std::time::{Duration, Instant, SystemTime};

#[derive(Debug)]
pub enum SessionStoreError {
    Admission(ClaimDisposition),
    InvalidMetadata,
    Cancelled,
    Failure(Box<dyn std::error::Error + Send + Sync>),
}
impl std::fmt::Display for SessionStoreError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Admission(disposition) => write!(f, "session admission: {disposition:?}"),
            Self::InvalidMetadata => f.write_str("session metadata is unavailable or invalid"),
            Self::Cancelled => f.write_str("session opening cancelled"),
            Self::Failure(error) => error.fmt(f),
        }
    }
}
impl std::error::Error for SessionStoreError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Failure(error) => Some(error.as_ref()),
            _ => None,
        }
    }
}
impl SessionStoreError {
    pub fn failure(error: impl std::error::Error + Send + Sync + 'static) -> Self {
        Self::Failure(Box::new(error))
    }
    fn code(&self) -> &'static str {
        match self {
            Self::Cancelled => "cancelled",
            Self::Admission(_) | Self::InvalidMetadata => "invalid_operation",
            Self::Failure(error) => classify_failure(error.as_ref()),
        }
    }
}

fn classify_failure(error: &(dyn std::error::Error + 'static)) -> &'static str {
    if let Some(error) = error.downcast_ref::<SharedCleanupError>() {
        return error.0.code();
    }
    if let Some(error) = error.downcast_ref::<std::io::Error>() {
        return if error.kind() == std::io::ErrorKind::PermissionDenied {
            "access_denied"
        } else {
            "io"
        };
    }
    if let Some(error) = error.downcast_ref::<DatabaseError>() {
        return match error {
            DatabaseError::Io(error) => classify_failure(error),
            _ => "unexpected",
        };
    }
    if let Some(error) = error.downcast_ref::<crate::history_files::Error>() {
        use crate::history_files::ErrorKind;
        return match error.kind {
            ErrorKind::Io => std::error::Error::source(error).map_or("io", classify_failure),
            ErrorKind::InvalidInput => "argument",
            ErrorKind::InvalidOperation | ErrorKind::Unowned => "invalid_operation",
            ErrorKind::InvalidData | ErrorKind::Database => "unexpected",
        };
    }
    "unexpected"
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SessionModelSelection {
    pub provider_id: String,
    pub canonical_model: String,
    pub selector: String,
}
pub trait SessionModelRouter: Send + Sync {
    fn resolve(&self, selector: &str) -> Result<SessionModelSelection, SessionStoreError>;
}
/// Later application wiring supplies actual project-workspace discovery.
pub trait SessionWorkspaceResolver: Send + Sync {
    fn resolve(&self, launch_directory: &Path) -> Result<PathBuf, SessionStoreError>;
}
#[derive(Clone, Debug)]
pub struct SessionInitialization {
    pub id: UserSessionId,
    pub root_agent_name: String,
    pub model: SessionModelSelection,
    pub mode: String,
    pub interactive_permissions: bool,
}
pub trait StoredUserSession: Send + Sync {
    fn model(&self) -> SessionModelSelection;
    fn mode(&self) -> String;
    fn dispose(&mut self) -> BoxFuture<'_, Result<(), SessionStoreError>>;
}
pub trait StoredUserSessionFactory: Send + Sync {
    fn create(
        &self,
        lease: Arc<SessionResourceLease>,
        initialization: SessionInitialization,
    ) -> BoxFuture<'_, Result<Box<dyn StoredUserSession>, SessionStoreError>>;
}

/// One database actor, repositories, diagnostic sink and fenced activation.
/// Cleanup runs independently of the caller's executor once requested or dropped.
pub struct SessionResourceLease {
    root: SelectedSessionRoot,
    pub database: SessionDatabase,
    pub events: EventRepository,
    pub images: ImageArtifactRepository,
    cleanup: ResourceCleanup,
    metadata_gate: Mutex<()>,
    index: SessionIndex,
}

// The worker owns activation before any database can be opened. It remains alive
// through caller-future cancellation and Tokio runtime shutdown. If the worker
// panics or cannot establish database settlement, activation is deliberately not
// released: a live process must not advertise the session as safe to reacquire.
struct ResourceCleanup {
    sender: Mutex<tokio::sync::mpsc::UnboundedSender<CleanupCommand>>,
    shared: Arc<CleanupShared>,
    handle: tokio::runtime::Handle,
}
struct CleanupShared {
    diagnostics: Arc<Mutex<Option<FileDiagnosticLog>>>,
    terminal: tokio::sync::watch::Sender<CleanupTerminal>,
    producers_settled: tokio::sync::watch::Sender<bool>,
    initialized: AtomicBool,
    opened: AtomicBool,
    close_started: AtomicBool,
    cancelled: AtomicBool,
}
#[derive(Clone)]
enum CleanupTerminal {
    Pending,
    Closed(Option<Arc<SessionStoreError>>),
    Unsettled(Arc<SessionStoreError>),
}
#[derive(Debug)]
struct SharedCleanupError(Arc<SessionStoreError>);
impl std::fmt::Display for SharedCleanupError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(f)
    }
}
impl std::error::Error for SharedCleanupError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(self.0.as_ref())
    }
}
type InitializationResult =
    Result<OpenedSession, (SessionStoreError, Option<Box<dyn StoredUserSession>>)>;
type CloseOperation = Box<dyn FnOnce() -> BoxFuture<'static, ()> + Send>;
type CloseCompletion = tokio::sync::oneshot::Sender<Result<(), tokio::task::JoinError>>;
enum CleanupCommand {
    StartClose(CloseOperation, CloseCompletion),
    Database(SessionDatabase),
    Initialize(
        BoxFuture<'static, InitializationResult>,
        tokio::sync::oneshot::Sender<Result<OpenedSession, SessionStoreError>>,
    ),
    Close(Option<Box<dyn StoredUserSession>>),
}
struct CleanupWorker {
    activation: Option<std::mem::ManuallyDrop<SessionActivationLease>>,
    database: Option<SessionDatabase>,
    session: Option<Box<dyn StoredUserSession>>,
    close_task: Option<(tokio::task::JoinHandle<()>, CloseCompletion)>,
    shared: Arc<CleanupShared>,
}
impl CleanupWorker {
    async fn run(
        &mut self,
        receiver: &mut tokio::sync::mpsc::UnboundedReceiver<CleanupCommand>,
    ) -> bool {
        while let Some(command) = receiver.recv().await {
            match command {
                CleanupCommand::StartClose(operation, completion) => {
                    self.close_task =
                        Some((tokio::spawn(async move { operation().await }), completion));
                }
                CleanupCommand::Database(database) => self.database = Some(database),
                CleanupCommand::Initialize(mut initialize, response) => {
                    let result = loop {
                        tokio::select! {
                            result = &mut initialize => break result,
                            command = receiver.recv() => match command {
                                Some(CleanupCommand::Close(session)) => {
                                    debug_assert!(session.is_none());
                                }
                                Some(CleanupCommand::Database(database)) => self.database = Some(database),
                                Some(CleanupCommand::Initialize(..)) => unreachable!("one initialization per lease"),
                                Some(CleanupCommand::StartClose(..)) => unreachable!("close builder admitted only after opening"),
                                None => break initialize.await,
                            }
                        }
                    };
                    match result {
                        Ok(mut opened) if self.shared.cancelled.load(Ordering::Acquire) => {
                            self.session = opened.session.take();
                            drop(opened);
                            let settled = self.finish().await;
                            let _ = response.send(Err(SessionStoreError::Cancelled));
                            return settled;
                        }
                        Ok(opened) => {
                            self.shared.opened.store(true, Ordering::Release);
                            // Both failed send and a later dropped receiver drop the existing
                            // ownership envelope, which returns its unique session to us.
                            let _ = response.send(Ok(opened));
                        }
                        Err((error, session)) => {
                            self.session = session;
                            let settled = self.finish().await;
                            let _ = response.send(Err(error));
                            return settled;
                        }
                    }
                }
                CleanupCommand::Close(session) => {
                    if session.is_some() {
                        self.session = session;
                        return self.finish().await;
                    }
                    if !self.shared.initialized.load(Ordering::Acquire) {
                        return self.finish().await;
                    }
                    // The opened owner still holds the unique disposer. A resource
                    // close request cannot seize it or bypass its producer joins.
                }
            }
        }
        self.finish().await
    }
    async fn join_close_task(&mut self) {
        if let Some((task, completion)) = self.close_task.take() {
            let result = task.await;
            let _ = completion.send(result);
        }
    }
    async fn finish(&mut self) -> bool {
        let session_result = match self.session.as_mut() {
            Some(session) => session.dispose().await,
            None => Ok(()),
        };
        if let Err(error) = &session_result {
            write_diagnostic(&self.shared.diagnostics, &failure_event("cleanup", error));
        }
        if !*self.shared.producers_settled.borrow() {
            let error = session_result.err().unwrap_or_else(|| {
                SessionStoreError::failure(std::io::Error::other(
                    "host producer settlement was not confirmed",
                ))
            });
            self.shared
                .terminal
                .send_replace(CleanupTerminal::Unsettled(Arc::new(error)));
            return false;
        }
        let resources = close_resources(self.database.as_ref(), &self.shared.diagnostics).await;
        if let Err(error) = resources {
            self.shared
                .terminal
                .send_replace(CleanupTerminal::Unsettled(Arc::new(
                    session_result.err().unwrap_or(error),
                )));
            return false;
        }
        let release =
            std::mem::ManuallyDrop::into_inner(self.activation.take().expect("activation owned"))
                .release()
                .map_err(SessionStoreError::failure);
        let result = session_result.and(release);
        self.shared
            .terminal
            .send_replace(CleanupTerminal::Closed(result.err().map(Arc::new)));
        true
    }
}
impl ResourceCleanup {
    fn new(
        activation: SessionActivationLease,
        diagnostics: FileDiagnosticLog,
    ) -> Result<Self, SessionStoreError> {
        let (sender, mut receiver) = tokio::sync::mpsc::unbounded_channel();
        let (terminal, _) = tokio::sync::watch::channel(CleanupTerminal::Pending);
        let (producers_settled, _) = tokio::sync::watch::channel(true);
        let shared = Arc::new(CleanupShared {
            diagnostics: Arc::new(Mutex::new(Some(diagnostics))),
            terminal,
            producers_settled,
            initialized: AtomicBool::new(false),
            opened: AtomicBool::new(false),
            close_started: AtomicBool::new(false),
            cancelled: AtomicBool::new(false),
        });
        let worker_shared = shared.clone();
        let (startup, ready) = std::sync::mpsc::sync_channel(1);
        std::thread::Builder::new()
            .name("session-resource-cleanup".into())
            .spawn(move || {
                let mut worker = CleanupWorker {
                    activation: Some(std::mem::ManuallyDrop::new(activation)),
                    database: None,
                    session: None,
                    close_task: None,
                    shared: worker_shared,
                };
                let runtime = match tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                {
                    Ok(runtime) => runtime,
                    Err(error) => {
                        let _ = startup.send(Err(error));
                        return;
                    }
                };
                let _ = startup.send(Ok(runtime.handle().clone()));
                let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    runtime.block_on(async {
                        let settled = worker.run(&mut receiver).await;
                        // ResourceClosed releases nested opened.close(), but the
                        // driver still owns the outer close and its publication.
                        worker.join_close_task().await;
                        settled
                    })
                }));
                if !matches!(outcome, Ok(true)) {
                    if outcome.is_err() {
                        worker
                            .shared
                            .terminal
                            .send_replace(CleanupTerminal::Unsettled(Arc::new(
                                SessionStoreError::failure(DatabaseError::WorkerStopped),
                            )));
                        runtime.block_on(worker.join_close_task());
                    }
                    // Unknown joins are not a closed session. Keep the actual driver,
                    // database, disposer and fenced activation alive, without spinning.
                    runtime.block_on(std::future::pending::<()>());
                }
            })
            .map_err(SessionStoreError::failure)?;
        let handle = ready
            .recv()
            .map_err(|_| SessionStoreError::failure(DatabaseError::WorkerStopped))?
            .map_err(SessionStoreError::failure)?;
        Ok(Self {
            sender: Mutex::new(sender),
            shared,
            handle,
        })
    }
    fn attach_database(&self, database: SessionDatabase) -> Result<(), SessionStoreError> {
        self.sender
            .lock()
            .expect("session cleanup poisoned")
            .send(CleanupCommand::Database(database))
            .map_err(|_| SessionStoreError::failure(DatabaseError::WorkerStopped))
    }
    fn initialize(
        &self,
        initialize: BoxFuture<'static, InitializationResult>,
    ) -> Result<
        tokio::sync::oneshot::Receiver<Result<OpenedSession, SessionStoreError>>,
        SessionStoreError,
    > {
        let sender = self.sender.lock().expect("session cleanup poisoned");
        if self.shared.cancelled.load(Ordering::Acquire) {
            return Err(SessionStoreError::Cancelled);
        }
        let (response, result) = tokio::sync::oneshot::channel();
        self.shared.initialized.store(true, Ordering::Release);
        self.shared.producers_settled.send_replace(false);
        sender
            .send(CleanupCommand::Initialize(initialize, response))
            .map_err(|_| SessionStoreError::failure(DatabaseError::WorkerStopped))?;
        Ok(result)
    }
    fn spawn_close(
        &self,
        operation: CloseOperation,
    ) -> Result<tokio::sync::oneshot::Receiver<Result<(), tokio::task::JoinError>>, SessionStoreError>
    {
        let sender = self.sender.lock().expect("session cleanup poisoned");
        if !self.shared.opened.load(Ordering::Acquire)
            || self.shared.cancelled.load(Ordering::Acquire)
            || self.shared.close_started.load(Ordering::Acquire)
            || !matches!(*self.shared.terminal.borrow(), CleanupTerminal::Pending)
        {
            return Err(SessionStoreError::failure(DatabaseError::Closed));
        }
        let (completion, receipt) = tokio::sync::oneshot::channel();
        self.shared.close_started.store(true, Ordering::Release);
        let admitted = sender.send(CleanupCommand::StartClose(operation, completion));
        drop(sender);
        admitted.map_err(|_| SessionStoreError::failure(DatabaseError::WorkerStopped))?;
        Ok(receipt)
    }
    fn request_close(&self, session: Option<Box<dyn StoredUserSession>>) {
        let sender = self.sender.lock().unwrap_or_else(|e| e.into_inner());
        self.shared.cancelled.store(true, Ordering::Release);
        let _ = sender.send(CleanupCommand::Close(session));
    }
    async fn wait_closed(&self) -> Result<(), SessionStoreError> {
        let mut terminal = self.shared.terminal.subscribe();
        loop {
            match terminal.borrow_and_update().clone() {
                CleanupTerminal::Pending => {}
                CleanupTerminal::Closed(None) => return Ok(()),
                CleanupTerminal::Closed(Some(error)) | CleanupTerminal::Unsettled(error) => {
                    return Err(SessionStoreError::failure(SharedCleanupError(error)));
                }
            }
            terminal
                .changed()
                .await
                .map_err(|_| SessionStoreError::failure(DatabaseError::WorkerStopped))?;
        }
    }
}
impl Drop for ResourceCleanup {
    fn drop(&mut self) {
        self.request_close(None);
    }
}
// Database close is idempotent and awaits actual actor settlement. Keeping this
// separate from the worker's session-disposal wait permits a disposer to close
// its own resource lease without awaiting itself. Only the worker releases activation.
async fn close_resources(
    database: Option<&SessionDatabase>,
    diagnostics: &Mutex<Option<FileDiagnosticLog>>,
) -> Result<(), SessionStoreError> {
    let result = match database {
        Some(database) => database.close().await.map_err(SessionStoreError::failure),
        None => Ok(()),
    };
    if result.is_ok() {
        if let Some(log) = diagnostics.lock().unwrap_or_else(|e| e.into_inner()).take() {
            if database.is_some() && result.is_ok() {
                let mut event =
                    DiagnosticEvent::new("session", "closed", DiagnosticSeverity::Information);
                event.outcome = Some("success".into());
                log.write(&event);
            }
        }
    }
    result
}
fn write_diagnostic(diagnostics: &Mutex<Option<FileDiagnosticLog>>, event: &DiagnosticEvent) {
    if let Some(log) = diagnostics
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .as_ref()
    {
        log.write(event);
    }
}
impl SessionResourceLease {
    pub async fn open(
        index: SessionIndex,
        activation: SessionActivationLease,
        diagnostics: FileDiagnosticLog,
    ) -> Result<Arc<Self>, SessionStoreError> {
        let root = index.selected_root().clone();
        let cleanup = ResourceCleanup::new(activation, diagnostics)?;
        let opened = (|| {
            let database =
                SessionDatabase::open(root.clone()).map_err(SessionStoreError::failure)?;
            cleanup.attach_database(database.clone())?;
            let store = ImageArtifactStore::for_session(&root, "artifacts")
                .map_err(SessionStoreError::failure)?;
            let images = ImageArtifactRepository::new(database.clone(), store)
                .map_err(SessionStoreError::failure)?;
            Ok::<_, SessionStoreError>((database, images))
        })();
        let (database, images) = match opened {
            Ok(opened) => opened,
            Err(error) => {
                write_diagnostic(&cleanup.shared.diagnostics, &failure_event("open", &error));
                cleanup.request_close(None);
                let _ = cleanup.wait_closed().await;
                return Err(error);
            }
        };
        Self::initialize(root, index, cleanup, database, images).await
    }
    async fn initialize(
        root: SelectedSessionRoot,
        index: SessionIndex,
        cleanup: ResourceCleanup,
        database: SessionDatabase,
        images: ImageArtifactRepository,
    ) -> Result<Arc<Self>, SessionStoreError> {
        let lease = Arc::new(Self {
            events: EventRepository::new(database.clone()),
            database,
            images,
            root,
            cleanup,
            metadata_gate: Mutex::new(()),
            index,
        });
        if let Err(error) = lease
            .images
            .remove_stale_unreferenced_before(
                SystemTime::now() - Duration::from_secs(86400),
                &NeverCancelled,
            )
            .await
        {
            let error = SessionStoreError::failure(error);
            lease.write_diagnostic(&failure_event("open", &error));
            let _ = lease.close().await;
            return Err(error);
        }
        Ok(lease)
    }
    pub fn selected_root(&self) -> &SelectedSessionRoot {
        &self.root
    }
    pub fn is_cancelled(&self) -> bool {
        self.cleanup.shared.cancelled.load(Ordering::Acquire)
    }
    pub fn write_diagnostic(&self, event: &DiagnosticEvent) {
        write_diagnostic(&self.cleanup.shared.diagnostics, event);
    }
    /// Closes the database and diagnostic sink. A session disposer may await
    /// this on its own lease; fenced activation remains owned by the cleanup
    /// worker until the entire session disposal has also finished.
    pub async fn close(&self) -> Result<(), SessionStoreError> {
        self.cleanup.request_close(None);
        if !self.cleanup.shared.initialized.load(Ordering::Acquire) {
            return self.cleanup.wait_closed().await;
        }
        if !matches!(
            *self.cleanup.shared.terminal.borrow(),
            CleanupTerminal::Pending
        ) {
            return self.cleanup.wait_closed().await;
        }
        let mut settled = self.cleanup.shared.producers_settled.subscribe();
        loop {
            if *settled.borrow_and_update() {
                return close_resources(Some(&self.database), &self.cleanup.shared.diagnostics)
                    .await;
            }
            tokio::select! {
                result = self.cleanup.wait_closed() => return result,
                result = settled.changed() => result.map_err(|_| SessionStoreError::failure(DatabaseError::WorkerStopped))?,
            }
        }
    }
    /// The lease's continuously driven runtime for admitted session work.
    /// Host close must use spawn_close instead: raw handle-bound close observers
    /// can outlive resource settlement and are not kept alive by this handle.
    pub fn runtime_handle(&self) -> tokio::runtime::Handle {
        self.cleanup.handle.clone()
    }
    /// Admits the one host close builder before it can run. The worker creates
    /// its future on the lease runtime and owns its actual JoinHandle through
    /// final publication, even when the external observer drops its receipt.
    /// Observe this receipt outside the closing runtime; the close operation
    /// must not await its own receipt. Repeated host close reuses its existing
    /// result/receipt; duplicate or late admissions reject without polling.
    pub fn spawn_close<F, Fut>(
        &self,
        operation: F,
    ) -> Result<tokio::sync::oneshot::Receiver<Result<(), tokio::task::JoinError>>, SessionStoreError>
    where
        F: FnOnce() -> Fut + Send + 'static,
        Fut: std::future::Future<Output = ()> + Send + 'static,
    {
        self.cleanup
            .spawn_close(Box::new(move || Box::pin(operation())))
    }
    /// Trusted host-owner attestation, after every admitted producer and resource
    /// observer has joined. Unknown joins must not confirm or call nested close.
    pub fn confirm_producers_settled(&self) {
        self.cleanup.shared.producers_settled.send_replace(true);
    }
    pub fn update_selection_metadata(
        &self,
        provider_id: &str,
        model: &str,
        selector: &str,
        mode: &str,
    ) -> Result<(), SessionStoreError> {
        let _guard = self
            .metadata_gate
            .lock()
            .expect("session metadata poisoned");
        let mut metadata = self
            .index
            .find()
            .map_err(SessionStoreError::failure)?
            .ok_or(SessionStoreError::InvalidMetadata)?;
        metadata.provider_id = Some(provider_id.to_owned());
        metadata.model = Some(model.to_owned());
        metadata.selector = Some(selector.to_owned());
        metadata.mode = Some(mode.to_owned());
        self.index
            .publish(&metadata)
            .map_err(SessionStoreError::failure)
    }
    async fn close_with_session(
        &self,
        session: Option<Box<dyn StoredUserSession>>,
    ) -> Result<(), SessionStoreError> {
        self.cleanup.request_close(session);
        self.cleanup.wait_closed().await
    }
}
struct OpeningResources(Option<Arc<SessionResourceLease>>);
impl Drop for OpeningResources {
    fn drop(&mut self) {
        if let Some(lease) = self.0.take() {
            lease.cleanup.request_close(None);
        }
    }
}

struct NeverCancelled;
impl Cancellation for NeverCancelled {
    fn is_cancelled(&self) -> bool {
        false
    }
    fn cancelled(&self) -> BoxFuture<'_, ()> {
        Box::pin(std::future::pending())
    }
}

pub struct OpenedSession {
    session: Option<Box<dyn StoredUserSession>>,
    pub loaded: bool,
    pub initialization: SessionInitialization,
    pub resources: Arc<SessionResourceLease>,
    clock: Arc<dyn Fn() -> String + Send + Sync>,
}
impl OpenedSession {
    pub fn session(&self) -> &dyn StoredUserSession {
        self.session.as_deref().expect("session is closed")
    }
    pub fn session_mut(&mut self) -> &mut (dyn StoredUserSession + 'static) {
        self.session.as_deref_mut().expect("session is closed")
    }
    /// Explicit successful-open publication; updating a session does not call it.
    pub fn record_opened(&self) -> Result<(), SessionStoreError> {
        let _guard = self
            .resources
            .metadata_gate
            .lock()
            .expect("session metadata poisoned");
        let mut metadata = self
            .resources
            .index
            .find()
            .map_err(SessionStoreError::failure)?
            .ok_or(SessionStoreError::InvalidMetadata)?;
        metadata.last_opened_at = Some((self.clock)());
        self.resources
            .index
            .publish(&metadata)
            .map_err(SessionStoreError::failure)
    }
    pub async fn close(&mut self) -> Result<(), SessionStoreError> {
        self.resources.close_with_session(self.session.take()).await
    }
}
impl Drop for OpenedSession {
    fn drop(&mut self) {
        self.resources.cleanup.request_close(self.session.take());
    }
}

#[derive(Clone)]
pub struct SessionStore {
    admissions: Arc<SessionAdmissions>,
    launch_directory: PathBuf,
    default_mode: String,
    factory: Arc<dyn StoredUserSessionFactory>,
    router: Arc<dyn SessionModelRouter>,
    workspace: Arc<dyn SessionWorkspaceResolver>,
    global: Arc<FileDiagnosticLog>,
    clock: Arc<dyn Fn() -> String + Send + Sync>,
}
impl SessionStore {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        admissions: Arc<SessionAdmissions>,
        launch_directory: PathBuf,
        default_mode: String,
        factory: Arc<dyn StoredUserSessionFactory>,
        router: Arc<dyn SessionModelRouter>,
        workspace: Arc<dyn SessionWorkspaceResolver>,
        global: Arc<FileDiagnosticLog>,
    ) -> Self {
        Self {
            admissions,
            launch_directory,
            default_mode,
            factory,
            router,
            workspace,
            global,
            clock: RepositoryContext::default().timestamp,
        }
    }
    pub fn with_clock(mut self, clock: Arc<dyn Fn() -> String + Send + Sync>) -> Self {
        self.clock = clock;
        self
    }
    pub fn discover_latest(&self) -> Result<SessionAdmission, SessionStoreError> {
        self.admissions
            .discover_latest(&self.launch_directory)
            .map_err(SessionStoreError::failure)
    }
    pub async fn open(
        &self,
        model: SessionModelSelection,
        mode: &str,
        interactive: bool,
    ) -> Result<OpenedSession, SessionStoreError> {
        let workspace = self.resolve_workspace(None)?;
        let admission = self.acquire(None, || self.admissions.open_default(&workspace))?;
        self.open_admitted(workspace, admission, Some(model), mode, interactive)
            .await
    }
    pub async fn create_fresh(
        &self,
        model: SessionModelSelection,
        mode: &str,
        interactive: bool,
    ) -> Result<OpenedSession, SessionStoreError> {
        let workspace = self.resolve_workspace(None)?;
        let id = UserSessionId::generate().map_err(SessionStoreError::failure)?;
        let admission =
            self.acquire(Some(&id), || self.admissions.create_fresh(&workspace, &id))?;
        self.open_admitted(workspace, admission, Some(model), mode, interactive)
            .await
    }
    pub fn resume_metadata(
        &self,
        id: &UserSessionId,
    ) -> Result<Option<SessionCatalogEntry>, SessionStoreError> {
        self.admissions
            .resume_metadata(id)
            .map_err(SessionStoreError::failure)
    }
    pub async fn resume(
        &self,
        id: &UserSessionId,
        interactive: bool,
        take_over: bool,
    ) -> Result<OpenedSession, SessionStoreError> {
        let workspace = self.resolve_workspace(Some(id))?;
        let admission = self.acquire(Some(id), || {
            self.admissions
                .resume(&self.launch_directory, id, take_over)
        })?;
        self.open_admitted(workspace, admission, None, &self.default_mode, interactive)
            .await
    }
    fn resolve_workspace(&self, id: Option<&UserSessionId>) -> Result<PathBuf, SessionStoreError> {
        let context = startup_context(id)?;
        let started = Instant::now();
        self.stage(&context, "workspace", "start", started, None);
        let result = self.workspace.resolve(&self.launch_directory);
        self.stage(
            &context,
            "workspace",
            if result.is_ok() {
                "complete"
            } else {
                "failure"
            },
            started,
            result.as_ref().err(),
        );
        result
    }
    fn acquire(
        &self,
        id: Option<&UserSessionId>,
        operation: impl FnOnce() -> std::io::Result<SessionAdmission>,
    ) -> Result<SessionAdmission, SessionStoreError> {
        let mut context = startup_context(id)?;
        let started = Instant::now();
        self.stage(&context, "acquire", "start", started, None);
        let result = operation()
            .map_err(SessionStoreError::failure)
            .and_then(|admission| {
                if admission.admission.activation_lease.is_none() {
                    Err(SessionStoreError::Admission(
                        admission.admission.disposition,
                    ))
                } else {
                    Ok(admission)
                }
            });
        if let Ok(admission) = &result {
            context.user_session_id = admission
                .admission
                .session_id
                .as_ref()
                .map(|id| id.value().to_owned());
        }
        self.stage(
            &context,
            "acquire",
            if result.is_ok() {
                "complete"
            } else {
                "failure"
            },
            started,
            result.as_ref().err(),
        );
        result
    }
    async fn open_admitted(
        &self,
        workspace: PathBuf,
        mut admission: SessionAdmission,
        model: Option<SessionModelSelection>,
        mode: &str,
        interactive: bool,
    ) -> Result<OpenedSession, SessionStoreError> {
        let id = admission
            .admission
            .session_id
            .take()
            .ok_or(SessionStoreError::Admission(
                admission.admission.disposition,
            ))?;
        let activation =
            admission
                .admission
                .activation_lease
                .take()
                .ok_or(SessionStoreError::Admission(
                    admission.admission.disposition,
                ))?;
        let started = Instant::now();
        let context = startup_context(Some(&id))?;
        self.stage(&context, "resources", "start", started, None);
        let resources = (|| {
            let root = admission
                .selected_root
                .ok_or(SessionStoreError::InvalidMetadata)?;
            std::fs::create_dir_all(root.path()).map_err(SessionStoreError::failure)?;
            let diagnostics = FileDiagnosticLog::open_session(
                root.path().join("session.log"),
                FileDiagnosticLog::create_instance_id(),
                id.value(),
                |_| {},
            )
            .map_err(SessionStoreError::failure)?;
            let index = SessionIndex::new(root.clone(), id.clone(), &workspace)
                .map_err(SessionStoreError::failure)?;
            Ok::<_, SessionStoreError>((root, diagnostics, index))
        })();
        self.stage(
            &context,
            "resources",
            if resources.is_ok() {
                "complete"
            } else {
                "failure"
            },
            started,
            resources.as_ref().err(),
        );
        let (_root, diagnostics, index) = resources?;
        let started = Instant::now();
        self.stage(&context, "database", "start", started, None);
        let lease = SessionResourceLease::open(index, activation, diagnostics).await;
        self.stage(
            &context,
            "database",
            if lease.is_ok() { "complete" } else { "failure" },
            started,
            lease.as_ref().err(),
        );
        let lease = lease?;
        let mut opening_resources = OpeningResources(Some(lease.clone()));
        let started = Instant::now();
        self.stage(&context, "metadata_read", "start", started, None);
        let selected = (|| {
            let existing = lease.index.find().map_err(SessionStoreError::failure)?;
            if existing.is_none() && (model.is_none() || lease.index.metadata_path().is_file()) {
                return Err(SessionStoreError::InvalidMetadata);
            }
            let initialization = SessionInitialization {
                id: id.clone(),
                root_agent_name: nonempty(
                    existing
                        .as_ref()
                        .and_then(|meta| meta.root_agent_name.as_deref()),
                )
                .unwrap_or("main")
                .to_owned(),
                model: if let Some(existing) = &existing {
                    self.router.resolve(&stored_selector(existing))?
                } else {
                    model.ok_or(SessionStoreError::InvalidMetadata)?
                },
                mode: if let Some(existing) = &existing {
                    nonempty(existing.mode.as_deref()).unwrap_or(&self.default_mode)
                } else {
                    mode
                }
                .to_owned(),
                interactive_permissions: interactive,
            };
            Ok::<_, SessionStoreError>((existing, initialization))
        })();
        let (existing, initialization) = match selected {
            Ok(selected) => {
                self.stage(&context, "metadata_read", "complete", started, None);
                selected
            }
            Err(error) => {
                self.stage(&context, "metadata_read", "failure", started, Some(&error));
                lease.write_diagnostic(&failure_event("open", &error));
                let _ = lease.close().await;
                return Err(error);
            }
        };
        let owned = self.clone();
        let owned_lease = lease.clone();
        let result = lease.cleanup.initialize(Box::pin(async move {
            owned
                .initialize_owned(owned_lease, workspace, existing, initialization, context)
                .await
        }))?;
        let opened = result
            .await
            .map_err(|_| SessionStoreError::failure(DatabaseError::WorkerStopped))??;
        opening_resources.0 = None;
        Ok(opened)
    }
    async fn initialize_owned(
        self,
        lease: Arc<SessionResourceLease>,
        workspace: PathBuf,
        existing: Option<SessionMeta>,
        initialization: SessionInitialization,
        context: DiagnosticEvent,
    ) -> InitializationResult {
        let mut stage = "initialize";
        let mut started = Instant::now();
        self.stage(&context, stage, "start", started, None);
        let mut session = None;
        let result = async {
            session = Some(
                self.factory
                    .create(lease.clone(), initialization.clone())
                    .await?,
            );
            self.stage(&context, stage, "complete", started, None);
            stage = "metadata_publish";
            started = Instant::now();
            self.stage(&context, stage, "start", started, None);
            let constructed = session.as_ref().expect("constructed session");
            let selected = constructed.model();
            let metadata = SessionMeta {
                id: Some(initialization.id.value().into()),
                working_directory: existing
                    .as_ref()
                    .and_then(|meta| meta.working_directory.clone())
                    .or_else(|| Some(workspace.to_string_lossy().into_owned())),
                root_agent_name: Some(initialization.root_agent_name.clone()),
                provider_id: Some(selected.provider_id),
                model: Some(selected.canonical_model),
                selector: Some(selected.selector),
                mode: Some(constructed.mode()),
                last_opened_at: Some((self.clock)()),
                created_at: existing
                    .as_ref()
                    .and_then(|meta| meta.created_at.clone())
                    .or_else(|| Some((self.clock)())),
            };
            {
                let _guard = lease
                    .metadata_gate
                    .lock()
                    .expect("session metadata poisoned");
                lease
                    .index
                    .publish(&metadata)
                    .map_err(SessionStoreError::failure)?;
            }
            self.stage(&context, stage, "complete", started, None);
            let mut event = DiagnosticEvent::new(
                "session",
                if existing.is_some() {
                    "resume"
                } else {
                    "start"
                },
                DiagnosticSeverity::Information,
            );
            event.outcome = Some("success".into());
            lease.write_diagnostic(&event);
            Ok::<_, SessionStoreError>(())
        }
        .await;
        match result {
            Ok(()) => Ok(OpenedSession {
                session,
                loaded: existing.is_some(),
                initialization,
                resources: lease,
                clock: self.clock,
            }),
            Err(error) => {
                self.stage(&context, stage, "failure", started, Some(&error));
                lease.write_diagnostic(&failure_event("open", &error));
                Err((error, session))
            }
        }
    }
    fn stage(
        &self,
        context: &DiagnosticEvent,
        stage: &str,
        phase: &str,
        started: Instant,
        error: Option<&SessionStoreError>,
    ) {
        let cancelled = matches!(error, Some(SessionStoreError::Cancelled));
        let mut event = context.clone();
        event.operation = format!("{stage}_{phase}");
        event.severity = if error.is_some() && !cancelled {
            DiagnosticSeverity::Error
        } else {
            DiagnosticSeverity::Information
        };
        if phase != "start" {
            event.duration_milliseconds =
                Some(started.elapsed().as_millis().min(i64::MAX as u128) as i64);
            event.outcome = Some(
                if cancelled {
                    "cancelled"
                } else if error.is_some() {
                    "failed"
                } else {
                    "success"
                }
                .into(),
            );
        }
        event.error_code = error.map(|error| error.code().into());
        self.global.write(&event);
    }
}
fn startup_context(id: Option<&UserSessionId>) -> Result<DiagnosticEvent, SessionStoreError> {
    let mut context = DiagnosticEvent::new("session", "", DiagnosticSeverity::Information);
    context.user_session_id = id.map(|id| id.value().to_owned());
    context.correlation_id = Some(super::runtime::random_id().map_err(SessionStoreError::failure)?);
    Ok(context)
}
fn nonempty(value: Option<&str>) -> Option<&str> {
    value.filter(|value| !value.is_empty())
}
fn stored_selector(meta: &SessionMeta) -> String {
    if let Some(selector) = nonempty(meta.selector.as_deref()) {
        return selector.to_owned();
    }
    let provider = meta.provider_id.as_deref().unwrap_or("");
    let model = meta.model.as_deref().unwrap_or("");
    if model.starts_with(&format!("{provider}/")) {
        model.into()
    } else {
        format!("{provider}/{model}")
    }
}
fn failure_event(operation: &str, error: &SessionStoreError) -> DiagnosticEvent {
    let mut event = DiagnosticEvent::new("session", operation, DiagnosticSeverity::Error);
    event.outcome = Some("failed".into());
    event.error_code = Some(error.code().into());
    event
}

#[cfg(test)]
#[path = "session_store_tests.rs"]
mod tests;
