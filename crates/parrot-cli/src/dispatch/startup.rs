//! Ordinary workspace startup, shared by the native CLI and web startup port.
//!
//! The caller supplies already selected roots/workspace and lazy local service
//! configuration. Workspace candidate roots are data, not filesystem grants.

use parrot_domain::{
    contracts::{
        async_ops::{BoxFuture, Cancellation},
        paths::{RootOrigin, SelectedSessionRoot},
    },
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
    web::router::{WebClient, WebOpenedSession, WebStartup},
};
use parrot_storage::{
    diagnostics::{DiagnosticEvent, DiagnosticSeverity, FileDiagnosticLog},
    ownership::{
        ClaimDisposition, OwnershipRoots, RuntimeIdentity, SessionAdmissions, WorkingDirectoryClaim,
    },
};
use std::{
    fmt,
    io::{self, Write},
    path::Path,
    sync::{Arc, Mutex},
    time::Instant,
};
use tonic::{Code, Status};

pub type OpenLocalClient = Arc<
    dyn for<'a> Fn(&'a dyn Cancellation) -> BoxFuture<'a, Result<WebClient, Status>> + Send + Sync,
>;
pub type ConfigureFresh = Arc<
    dyn for<'a> Fn(
            WebClient,
            &'a dyn Cancellation,
        ) -> BoxFuture<'a, Result<wire::CreateSessionRequest, Status>>
        + Send
        + Sync,
>;
pub type StartupProgress = Arc<Mutex<dyn Write + Send>>;

/// Owns the remote connection until the native caller retains it or the web
/// router adopts it. A cloned generated client alone is not that ownership.
pub struct OpenedLocalSession {
    pub client: WebClient,
    pub session: wire::UserSession,
    pub remote: Option<GrpcTransportClient>,
}

#[derive(Debug)]
pub enum StartupError {
    Rpc(Status),
    Io(io::Error),
    CorruptState,
}
impl fmt::Display for StartupError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Rpc(status) => status.fmt(formatter),
            Self::Io(error) => error.fmt(formatter),
            Self::CorruptState => formatter
                .write_str("cannot open the workspace's latest user session: corrupt state"),
        }
    }
}
impl std::error::Error for StartupError {}
impl From<Status> for StartupError {
    fn from(status: Status) -> Self {
        Self::Rpc(status)
    }
}
impl From<io::Error> for StartupError {
    fn from(error: io::Error) -> Self {
        Self::Io(error)
    }
}
impl StartupError {
    pub fn into_status(self) -> Status {
        match self {
            Self::Rpc(status) => status,
            other => Status::internal(other.to_string()),
        }
    }

    fn error_code(&self) -> String {
        match self {
            Self::Rpc(status) => format!("{:?}", status.code()),
            Self::Io(_) => "io".into(),
            Self::CorruptState => "invalid_operation".into(),
        }
    }
}

pub struct LocalChatStartup {
    roots: ApplicationRoots,
    workspace: ProjectWorkspace,
    runtime: RuntimeIdentity,
    error: StartupProgress,
    diagnostics: Arc<FileDiagnosticLog>,
    open_local_client: OpenLocalClient,
    configure_fresh: ConfigureFresh,
    take_over: bool,
    legacy_path_read_fallback: bool,
}
impl LocalChatStartup {
    /// Use the same resolved workspace that is supplied to the composition
    /// factory and web router. This constructor does not resolve it again.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        roots: ApplicationRoots,
        workspace: ProjectWorkspace,
        runtime: RuntimeIdentity,
        error: StartupProgress,
        diagnostics: Arc<FileDiagnosticLog>,
        open_local_client: OpenLocalClient,
        configure_fresh: ConfigureFresh,
        take_over: bool,
    ) -> Self {
        Self {
            roots,
            workspace,
            runtime,
            error,
            diagnostics,
            open_local_client,
            configure_fresh,
            take_over,
            legacy_path_read_fallback: true,
        }
    }

    /// Bind the actual application config flag before sharing this startup.
    /// Default-enabled legacy discovery selects sessions in place, not grants.
    pub fn with_legacy_path_read_fallback(mut self, enabled: bool) -> Self {
        self.legacy_path_read_fallback = enabled;
        self
    }

    /// Ordinary discovery for callers that have not resolved the workspace yet.
    /// Reuse `workspace()` for subsequent factory/router construction.
    #[allow(clippy::too_many_arguments)]
    pub fn from_working_directory(
        roots: ApplicationRoots,
        working_directory: &Path,
        runtime: RuntimeIdentity,
        error: StartupProgress,
        diagnostics: Arc<FileDiagnosticLog>,
        open_local_client: OpenLocalClient,
        configure_fresh: ConfigureFresh,
        take_over: bool,
    ) -> Result<Self, Status> {
        let workspace = ordinary_workspace_resolver()(working_directory)
            .map_err(|failure| Status::invalid_argument(failure.message))?;
        Ok(Self::new(
            roots,
            workspace,
            runtime,
            error,
            diagnostics,
            open_local_client,
            configure_fresh,
            take_over,
        ))
    }

    pub fn workspace(&self) -> &ProjectWorkspace {
        &self.workspace
    }

    pub async fn open(
        &self,
        interactive_permissions: bool,
        cancellation: &dyn Cancellation,
    ) -> Result<OpenedLocalSession, StartupError> {
        let startup_id = FileDiagnosticLog::create_instance_id();
        let started = Instant::now();
        let mut start = DiagnosticEvent::new(
            "transport",
            "local.open.start",
            DiagnosticSeverity::Information,
        );
        start.correlation_id = Some(startup_id.clone());
        self.diagnostics.write(&start);
        let result = self
            .open_session(interactive_permissions, cancellation)
            .await;
        let cancelled =
            matches!(&result, Err(StartupError::Rpc(status)) if status.code() == Code::Cancelled);
        let mut complete = DiagnosticEvent::new(
            "transport",
            "local.open.complete",
            if result.is_ok() || cancelled {
                DiagnosticSeverity::Information
            } else {
                DiagnosticSeverity::Error
            },
        );
        complete.correlation_id = Some(startup_id);
        complete.duration_milliseconds =
            Some(i64::try_from(started.elapsed().as_millis()).unwrap_or(i64::MAX));
        complete.outcome = Some(
            if result.is_ok() {
                "success"
            } else if cancelled {
                "cancelled"
            } else {
                "failure"
            }
            .into(),
        );
        complete.error_code = result.as_ref().err().map(StartupError::error_code);
        self.diagnostics.write(&complete);
        result
    }

    async fn open_session(
        &self,
        interactive_permissions: bool,
        cancellation: &dyn Cancellation,
    ) -> Result<OpenedLocalSession, StartupError> {
        check(cancellation)?;
        self.progress(
            "looking for the workspace's latest user session...",
            cancellation,
        )?;
        let directory = self.workspace.launch_directory();
        let admissions = SessionAdmissions::new(
            OwnershipRoots::new(
                self.roots.current().state().to_owned(),
                self.roots.legacy().state().to_owned(),
            )?,
            self.runtime.clone(),
            self.legacy_path_read_fallback,
        );
        let candidate = admissions.discover_latest(directory)?;
        if candidate.admission.disposition == ClaimDisposition::Corrupt {
            return Err(StartupError::CorruptState);
        }
        let mut local = None;
        if let Some(id) = candidate.admission.session_id {
            let selected = candidate
                .selected_root
                .expect("discovered session has a selected root");
            let paths = match selected.origin() {
                RootOrigin::Current => self.roots.current(),
                RootOrigin::Legacy => self.roots.legacy(),
            };
            let claim = WorkingDirectoryClaim::new(paths.state(), self.runtime.clone());
            let unverified_owner = candidate.admission.disposition == ClaimDisposition::Live
                && !claim.is_active(&id, directory)?;
            let mut attached = if unverified_owner {
                None
            } else {
                self.try_attach(&id, &selected, false, cancellation).await?
            };
            if attached.is_none() {
                check(cancellation)?;
                let mut client = (self.open_local_client)(cancellation).await?;
                self.progress(
                    &format!("restoring existing user session {id}..."),
                    cancellation,
                )?;
                let request = wire::ResumeSessionRequest {
                    user_session_id: id.value().into(),
                    working_directory: directory.to_string_lossy().into_owned(),
                    interactive_permissions,
                    take_over: self.take_over,
                    ..Default::default()
                };
                let resumed = tokio::select! {
                    biased;
                    _ = cancellation.cancelled() => return Err(cancelled().into()),
                    result = client.resume_session(request) => result,
                };
                match resumed {
                    Ok(response) => {
                        let session = response.into_inner();
                        self.progress(
                            &format!("loaded existing user session {}", session.id),
                            cancellation,
                        )?;
                        return Ok(OpenedLocalSession {
                            client,
                            session,
                            remote: None,
                        });
                    }
                    Err(status) if status.code() == Code::AlreadyExists => check(cancellation)?,
                    Err(status) => return Err(status.into()),
                }
                local = Some(client);
                if unverified_owner && !self.take_over {
                    self.line(
                        &format!("user session {id} is held by a process parrot cannot verify (another host, or one it cannot inspect); run `parrot chat --take-over` to reclaim it; creating a new user session"),
                        false,
                        cancellation,
                    )?;
                } else {
                    attached = self.try_attach(&id, &selected, true, cancellation).await?;
                }
            }
            if let Some(opened) = attached {
                self.progress(
                    &format!("connected to existing user session {}", opened.session.id),
                    cancellation,
                )?;
                return Ok(opened);
            }
        }
        check(cancellation)?;
        let mut client = match local {
            Some(client) => client,
            None => (self.open_local_client)(cancellation).await?,
        };
        self.progress("creating a new user session...", cancellation)?;
        let mut request = (self.configure_fresh)(client.clone(), cancellation).await?;
        request.interactive_permissions = interactive_permissions;
        check(cancellation)?;
        let session = tokio::select! {
            biased;
            _ = cancellation.cancelled() => return Err(cancelled().into()),
            result = client.create_session(request) => result?.into_inner(),
        };
        self.progress(
            &format!("created user session {}", session.id),
            cancellation,
        )?;
        Ok(OpenedLocalSession {
            client,
            session,
            remote: None,
        })
    }

    async fn try_attach(
        &self,
        id: &UserSessionId,
        selected: &SelectedSessionRoot,
        report_failure: bool,
        cancellation: &dyn Cancellation,
    ) -> Result<Option<OpenedLocalSession>, StartupError> {
        match self.attach(id, selected, cancellation).await {
            Ok(opened) => Ok(Some(opened)),
            Err(failure) => {
                check(cancellation)?;
                if report_failure {
                    self.line(
                        &format!("unable to connect to existing user session {id}: {failure}; creating a new user session"),
                        false,
                        cancellation,
                    )?;
                }
                Ok(None)
            }
        }
    }

    async fn attach(
        &self,
        id: &UserSessionId,
        selected: &SelectedSessionRoot,
        cancellation: &dyn Cancellation,
    ) -> Result<OpenedLocalSession, StartupError> {
        check(cancellation)?;
        self.progress(
            &format!("connecting to existing user session {id}..."),
            cancellation,
        )?;
        let resources =
            UserSessionResources::new(selected.clone(), id.clone(), self.workspace.clone())?;
        let mut remote = GrpcTransportClient::connect(
            &TransportAddress::Unix(resources.socket_path()),
            None,
            None,
            self.diagnostics.clone(),
        )
        .map_err(|failure| Status::internal(failure.to_string()))?;
        let session = remote
            .attach(
                wire::AttachSessionRequest {
                    user_session_id: id.value().into(),
                    working_directory: self.workspace.launch_directory().to_string_lossy().into(),
                    ..Default::default()
                },
                cancellation,
            )
            .await
            .map_err(attach_status)?;
        let client = remote
            .client()
            .map_err(|failure| Status::internal(failure.to_string()))?
            .clone();
        Ok(OpenedLocalSession {
            client,
            session,
            remote: Some(remote),
        })
    }

    fn progress(&self, message: &str, cancellation: &dyn Cancellation) -> Result<(), StartupError> {
        self.line(message, true, cancellation)
    }

    fn line(
        &self,
        message: &str,
        flush: bool,
        cancellation: &dyn Cancellation,
    ) -> Result<(), StartupError> {
        check(cancellation)?;
        let mut error = self.error.lock().expect("startup progress poisoned");
        writeln!(error, "parrot: {message}")?;
        if flush {
            error.flush()?;
        }
        Ok(())
    }
}

impl WebStartup for LocalChatStartup {
    fn open<'a>(
        &'a self,
        cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<WebOpenedSession, Status>> {
        Box::pin(async move {
            let opened = LocalChatStartup::open(self, true, cancellation)
                .await
                .map_err(StartupError::into_status)?;
            Ok(WebOpenedSession {
                session: opened.session,
                remote: opened.remote,
            })
        })
    }
}

fn cancelled() -> Status {
    Status::cancelled("operation cancelled")
}
fn check(cancellation: &dyn Cancellation) -> Result<(), Status> {
    cancellation.check().map_err(|_| cancelled())
}
fn attach_status(error: AttachError) -> Status {
    match error {
        AttachError::Rpc(status) => status,
        AttachError::Cancelled => cancelled(),
        AttachError::Timeout => Status::deadline_exceeded(error.to_string()),
        AttachError::DifferentSession | AttachError::Disposed => {
            Status::internal(error.to_string())
        }
    }
}
