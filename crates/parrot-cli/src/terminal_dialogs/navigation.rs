//! Exact workspace-session navigation and transactional terminal rebinding.
//!
//! Local navigation consumes an already composed native-client opener. It does
//! not construct a security/filesystem factory or turn workspace roots into grants.

use std::sync::{Arc, Mutex};

use parrot_domain::{
    contracts::async_ops::{BoxFuture, Cancellation},
    protocol::messages::parrot as wire,
    values::{identifiers::UserSessionId, project_workspace::ProjectWorkspace},
};
use parrot_foundation::paths::{ApplicationRoots, UserSessionResources};
use parrot_host::{
    composition::factory::user::ordinary_workspace_resolver,
    grpc::{
        address::TransportAddress,
        transport::{AttachError, GrpcTransportClient},
    },
    web::router::WebClient,
};
use parrot_storage::diagnostics::FileDiagnosticLog;
use tonic::{Code, Status};

use crate::{
    dispatch::startup::OpenLocalClient,
    slash_sessions::{NavigationFailure, WorkspaceNavigation, native_call},
};

/// The generated client is borrowed from the opener or retained native owner.
/// Closing a target closes only its owned transport, once. The application owns
/// its executor and must retain it until terminal work has joined.
pub struct TerminalSessionTarget {
    pub client: WebClient,
    pub session: wire::UserSession,
    owner: Option<GrpcTransportClient>,
}

impl TerminalSessionTarget {
    pub fn new(
        client: WebClient,
        session: wire::UserSession,
        owner: Option<GrpcTransportClient>,
    ) -> Self {
        Self {
            client,
            session,
            owner,
        }
    }

    pub async fn close(&mut self) {
        if let Some(mut owner) = self.owner.take() {
            owner.close();
        }
    }
}

pub trait TerminalSessionNavigation: Send + Sync {
    fn list<'a>(
        &'a self,
        current: &'a wire::UserSession,
        cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<wire::ListSessionsResponse, NavigationFailure>>;

    fn open<'a>(
        &'a self,
        current: &'a wire::UserSession,
        user_session_id: &'a str,
        cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<TerminalSessionTarget, NavigationFailure>>;
}

pub struct ExplicitRemoteTerminalSessionNavigation {
    client: WebClient,
}

impl ExplicitRemoteTerminalSessionNavigation {
    pub fn new(client: WebClient) -> Self {
        Self { client }
    }

    fn workspace(current: &wire::UserSession) -> Result<&str, NavigationFailure> {
        if current.working_directory.trim().is_empty() {
            return Err(NavigationFailure::Recoverable(
                "workspace session selection is unavailable on this server: the session workspace is missing".into(),
            ));
        }
        Ok(&current.working_directory)
    }
}

impl TerminalSessionNavigation for ExplicitRemoteTerminalSessionNavigation {
    fn list<'a>(
        &'a self,
        current: &'a wire::UserSession,
        cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<wire::ListSessionsResponse, NavigationFailure>> {
        Box::pin(async move {
            let workspace = Self::workspace(current)?;
            Ok(native_call(
                self.client
                    .clone()
                    .list_sessions(wire::ListSessionsRequest {
                        working_directory: workspace.into(),
                        ..Default::default()
                    }),
                cancellation,
            )
            .await?)
        })
    }

    fn open<'a>(
        &'a self,
        current: &'a wire::UserSession,
        user_session_id: &'a str,
        cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<TerminalSessionTarget, NavigationFailure>> {
        Box::pin(async move {
            let workspace = Self::workspace(current)?;
            let attached = native_call(
                self.client
                    .clone()
                    .attach_session(wire::AttachSessionRequest {
                        user_session_id: user_session_id.into(),
                        working_directory: workspace.into(),
                        ..Default::default()
                    }),
                cancellation,
            )
            .await;
            let selected = match attached {
                Ok(selected) => selected,
                Err(crate::slash_sessions::SlashFailure::Rpc(status))
                    if status.code() == Code::NotFound =>
                {
                    native_call(
                        self.client
                            .clone()
                            .resume_session(wire::ResumeSessionRequest {
                                user_session_id: user_session_id.into(),
                                working_directory: workspace.into(),
                                interactive_permissions: true,
                                ..Default::default()
                            }),
                        cancellation,
                    )
                    .await?
                }
                Err(failure) => return Err(failure.into()),
            };
            validate_id(&selected, user_session_id)?;
            Ok(TerminalSessionTarget::new(
                self.client.clone(),
                selected,
                None,
            ))
        })
    }
}

pub struct LocalTerminalSessionNavigation {
    roots: ApplicationRoots,
    workspace: ProjectWorkspace,
    diagnostics: Arc<FileDiagnosticLog>,
    open_local_client: OpenLocalClient,
    legacy_path_read_fallback: bool,
}

impl LocalTerminalSessionNavigation {
    pub fn new(
        roots: ApplicationRoots,
        workspace: ProjectWorkspace,
        diagnostics: Arc<FileDiagnosticLog>,
        open_local_client: OpenLocalClient,
    ) -> Self {
        Self {
            roots,
            workspace,
            diagnostics,
            open_local_client,
            legacy_path_read_fallback: true,
        }
    }

    pub fn with_legacy_path_read_fallback(mut self, enabled: bool) -> Self {
        self.legacy_path_read_fallback = enabled;
        self
    }

    fn validate(&self, selected: &wire::UserSession, id: &str) -> Result<(), NavigationFailure> {
        validate_id(selected, id)?;
        if !selected.working_directory.is_empty() {
            let selected_workspace =
                ordinary_workspace_resolver()(std::path::Path::new(&selected.working_directory))
                    .map_err(|failure| NavigationFailure::Recoverable(failure.message))?;
            if selected_workspace != self.workspace {
                return Err(NavigationFailure::Recoverable(
                    "the user session belongs to another workspace".into(),
                ));
            }
        }
        Ok(())
    }

    async fn attach(
        &self,
        id: &str,
        cancellation: &dyn Cancellation,
    ) -> Result<TerminalSessionTarget, NavigationFailure> {
        check(cancellation)?;
        let id_value = UserSessionId::parse(id)
            .map_err(|failure| NavigationFailure::Recoverable(failure.to_string()))?;
        // Select an existing session in place, preferring current over legacy.
        // This is root selection only: no ownership lease or grant is acquired.
        let selected = match self
            .roots
            .discover_session(
                &id_value,
                self.legacy_path_read_fallback,
                self.workspace.launch_directory(),
            )
            .map_err(io_failure)?
        {
            Some(selected) => selected,
            None => self
                .roots
                .select_new(&id_value, self.workspace.launch_directory())
                .map_err(io_failure)?,
        };
        let resources = UserSessionResources::new(selected, id_value, self.workspace.clone())
            .map_err(io_failure)?;
        let mut owner = GrpcTransportClient::connect(
            &TransportAddress::Unix(resources.socket_path()),
            None,
            None,
            self.diagnostics.clone(),
        )
        .map_err(|failure| NavigationFailure::Recoverable(failure.to_string()))?;
        let attached = owner
            .attach(
                wire::AttachSessionRequest {
                    user_session_id: id.into(),
                    working_directory: self
                        .workspace
                        .launch_directory()
                        .to_string_lossy()
                        .into_owned(),
                    ..Default::default()
                },
                cancellation,
            )
            .await;
        let selected = match attached {
            Ok(selected) => selected,
            Err(failure) => {
                owner.close();
                return Err(attach_failure(failure));
            }
        };
        if let Err(failure) = self.validate(&selected, id) {
            owner.close();
            return Err(failure);
        }
        let client = owner
            .client()
            .map_err(|failure| NavigationFailure::Other(failure.to_string()))?
            .clone();
        Ok(TerminalSessionTarget::new(client, selected, Some(owner)))
    }
}

impl TerminalSessionNavigation for LocalTerminalSessionNavigation {
    fn list<'a>(
        &'a self,
        _: &'a wire::UserSession,
        cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<wire::ListSessionsResponse, NavigationFailure>> {
        Box::pin(async move {
            check(cancellation)?;
            let mut client = (self.open_local_client)(cancellation).await?;
            Ok(native_call(
                client.list_sessions(wire::ListSessionsRequest {
                    working_directory: self
                        .workspace
                        .launch_directory()
                        .to_string_lossy()
                        .into_owned(),
                    ..Default::default()
                }),
                cancellation,
            )
            .await?)
        })
    }

    fn open<'a>(
        &'a self,
        _: &'a wire::UserSession,
        user_session_id: &'a str,
        cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<TerminalSessionTarget, NavigationFailure>> {
        Box::pin(async move {
            check(cancellation)?;
            match self.attach(user_session_id, cancellation).await {
                Ok(target) => return Ok(target),
                Err(failure) if can_resume(&failure) => check(cancellation)?,
                Err(failure) => return Err(failure),
            }
            let mut client = (self.open_local_client)(cancellation).await?;
            let resumed = native_call(
                client.resume_session(wire::ResumeSessionRequest {
                    user_session_id: user_session_id.into(),
                    working_directory: self
                        .workspace
                        .launch_directory()
                        .to_string_lossy()
                        .into_owned(),
                    interactive_permissions: true,
                    take_over: true,
                    ..Default::default()
                }),
                cancellation,
            )
            .await;
            match resumed {
                Ok(selected) => {
                    self.validate(&selected, user_session_id)?;
                    Ok(TerminalSessionTarget::new(client, selected, None))
                }
                Err(crate::slash_sessions::SlashFailure::Rpc(status))
                    if status.code() == Code::AlreadyExists =>
                {
                    check(cancellation)?;
                    self.attach(user_session_id, cancellation).await
                }
                Err(failure) => Err(failure.into()),
            }
        })
    }
}

pub trait SessionReplacement: Send + Sync {
    /// Prepare the target's stream before stopping the previous binding. Call
    /// commit at the binding point; a later failure restores the old snapshot.
    fn replace<'a>(
        &'a self,
        session: wire::UserSession,
        client: WebClient,
        commit: Box<dyn FnOnce() + Send + 'a>,
        cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<(), NavigationFailure>>;
}

#[derive(Clone)]
pub struct TerminalSessionSnapshot {
    pub session: wire::UserSession,
    pub client: WebClient,
    pub revision: u64,
}

struct ControllerState {
    snapshot: TerminalSessionSnapshot,
    next_target: u64,
    targets: Vec<(u64, TerminalSessionTarget)>,
    closed: bool,
}

pub struct TerminalSessionController {
    navigation: Arc<dyn TerminalSessionNavigation>,
    replacement: Arc<dyn SessionReplacement>,
    state: Arc<Mutex<ControllerState>>,
    changing: tokio::sync::Mutex<()>,
}

impl TerminalSessionController {
    pub fn new(
        navigation: Arc<dyn TerminalSessionNavigation>,
        initial_client: WebClient,
        initial_session: wire::UserSession,
        replacement: Arc<dyn SessionReplacement>,
    ) -> Self {
        Self {
            navigation,
            replacement,
            state: Arc::new(Mutex::new(ControllerState {
                snapshot: TerminalSessionSnapshot {
                    session: initial_session,
                    client: initial_client,
                    revision: 0,
                },
                next_target: 0,
                targets: Vec::new(),
                closed: false,
            })),
            changing: tokio::sync::Mutex::new(()),
        }
    }

    pub fn snapshot(&self) -> TerminalSessionSnapshot {
        self.state
            .lock()
            .expect("terminal controller poisoned")
            .snapshot
            .clone()
    }
    pub fn current(&self) -> wire::UserSession {
        self.snapshot().session
    }
    pub fn client(&self) -> WebClient {
        self.snapshot().client
    }
    pub fn revision(&self) -> u64 {
        self.snapshot().revision
    }

    /// Synchronize a completed slash-session replacement (for example /clear).
    /// The caller serializes this with load/close and supplies the bound client.
    pub fn synchronize_current(&self, client: WebClient, session: wire::UserSession) {
        let mut state = self.state.lock().expect("terminal controller poisoned");
        if state.closed {
            return;
        }
        let revision = state
            .snapshot
            .revision
            .wrapping_add(u64::from(state.snapshot.session.id != session.id));
        state.snapshot = TerminalSessionSnapshot {
            client,
            session,
            revision,
        };
    }

    pub async fn close(&self) -> Result<(), NavigationFailure> {
        let _changing = self.changing.lock().await;
        let targets = {
            let mut state = self.state.lock().expect("terminal controller poisoned");
            state.closed = true;
            std::mem::take(&mut state.targets)
        };
        for (_, mut target) in targets {
            target.close().await;
        }
        Ok(())
    }

    async fn load_selected(
        &self,
        id: &str,
        cancellation: &dyn Cancellation,
    ) -> Result<(), NavigationFailure> {
        if id.is_empty() {
            return Err(NavigationFailure::Other(
                "user session id cannot be empty".into(),
            ));
        }
        if id == self.current().id {
            return Ok(());
        }
        let _changing = tokio::select! {
            biased;
            _ = cancellation.cancelled() => return Err(cancelled()),
            guard = self.changing.lock() => guard,
        };
        let previous = {
            let state = self.state.lock().expect("terminal controller poisoned");
            if state.closed {
                return Err(NavigationFailure::Other(
                    "terminal session controller is closed".into(),
                ));
            }
            state.snapshot.clone()
        };
        if id == previous.session.id {
            return Ok(());
        }
        check(cancellation)?;
        let target = self
            .navigation
            .open(&previous.session, id, cancellation)
            .await?;
        let selected = target.session.clone();
        let selected_client = target.client.clone();
        let target_id = {
            let mut state = self.state.lock().expect("terminal controller poisoned");
            let id = state.next_target;
            state.next_target = state.next_target.wrapping_add(1);
            // Retain BEFORE awaiting the callback. Dropping a load observer
            // cannot orphan an opened owner; close() still settles it.
            state.targets.push((id, target));
            id
        };
        let rollback = SnapshotRollback {
            state: self.state.clone(),
            previous: Some(previous.clone()),
        };
        let state = self.state.clone();
        let revision = previous.revision.wrapping_add(1);
        let replacement_session = selected.clone();
        let replacement_client = selected_client.clone();
        let commit = Box::new(move || {
            state.lock().expect("terminal controller poisoned").snapshot =
                TerminalSessionSnapshot {
                    session: selected,
                    client: selected_client,
                    revision,
                };
        });
        let outcome = self
            .replacement
            .replace(
                replacement_session,
                replacement_client,
                commit,
                cancellation,
            )
            .await;
        if let Err(failure) = outcome {
            drop(rollback);
            let target = {
                let mut state = self.state.lock().expect("terminal controller poisoned");
                let index = state
                    .targets
                    .iter()
                    .position(|(id, _)| *id == target_id)
                    .expect("retained terminal target");
                state.targets.remove(index).1
            };
            let mut target = target;
            target.close().await;
            return Err(failure);
        }
        rollback.keep();
        Ok(())
    }
}

struct SnapshotRollback {
    state: Arc<Mutex<ControllerState>>,
    previous: Option<TerminalSessionSnapshot>,
}
impl SnapshotRollback {
    fn keep(mut self) {
        self.previous = None;
    }
}
impl Drop for SnapshotRollback {
    fn drop(&mut self) {
        if let Some(previous) = self.previous.take() {
            self.state
                .lock()
                .expect("terminal controller poisoned")
                .snapshot = previous;
        }
    }
}

impl WorkspaceNavigation for TerminalSessionController {
    fn list<'a>(
        &'a self,
        cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<wire::ListSessionsResponse, NavigationFailure>> {
        Box::pin(async move {
            let current = self.current();
            self.navigation.list(&current, cancellation).await
        })
    }
    fn load<'a>(
        &'a self,
        id: &'a str,
        cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<(), NavigationFailure>> {
        Box::pin(self.load_selected(id, cancellation))
    }
}

fn validate_id(selected: &wire::UserSession, id: &str) -> Result<(), NavigationFailure> {
    if selected.id != id {
        Err(NavigationFailure::Recoverable(
            "the server returned a different user session".into(),
        ))
    } else {
        Ok(())
    }
}
fn cancelled() -> NavigationFailure {
    NavigationFailure::Rpc(Status::cancelled("operation cancelled"))
}
fn check(cancellation: &dyn Cancellation) -> Result<(), NavigationFailure> {
    cancellation.check().map_err(|_| cancelled())
}
fn io_failure(failure: std::io::Error) -> NavigationFailure {
    NavigationFailure::Recoverable(failure.to_string())
}
fn can_resume(failure: &NavigationFailure) -> bool {
    matches!(failure, NavigationFailure::Rpc(status) if matches!(status.code(), Code::Unavailable | Code::NotFound | Code::DeadlineExceeded))
}
fn attach_failure(failure: AttachError) -> NavigationFailure {
    match failure {
        AttachError::Rpc(status) => NavigationFailure::Rpc(status),
        AttachError::Cancelled => cancelled(),
        AttachError::Timeout => {
            NavigationFailure::Rpc(Status::deadline_exceeded(failure.to_string()))
        }
        AttachError::DifferentSession | AttachError::Disposed => {
            NavigationFailure::Recoverable(failure.to_string())
        }
    }
}
