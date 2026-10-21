//! Session routes borrow local transport and own adopted remote connections.

use crate::grpc::{
    address::TransportAddress,
    transport::{AttachError, BearerInterceptor, GrpcTransportClient},
};
use parrot_domain::{
    contracts::async_ops::{BoxFuture, Cancellation},
    protocol::{bindings::parrot::parrot_client::ParrotClient, messages::parrot as wire},
    values::{identifiers::UserSessionId, project_workspace::ProjectWorkspace},
};
use parrot_foundation::paths::{ApplicationRoots, UserSessionResources};
use parrot_storage::diagnostics::FileDiagnosticLog;
use std::{
    collections::{HashMap, HashSet},
    sync::{Arc, Mutex},
};
use tonic::{Status, service::interceptor::InterceptedService, transport::Channel};

pub type WebClient = ParrotClient<InterceptedService<Channel, BearerInterceptor>>;

/// The CLI owner supplies its actual LocalChatStartup binding. Web fixtures do
/// not establish production startup, command, or application lifetime wiring.
pub trait WebStartup: Send + Sync {
    fn open<'a>(
        &'a self,
        cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<WebOpenedSession, Status>>;
}

pub struct WebOpenedSession {
    pub session: wire::UserSession,
    pub remote: Option<GrpcTransportClient>,
}

pub struct WebSessionRouter {
    local: WebClient,
    roots: ApplicationRoots,
    workspace: ProjectWorkspace,
    startup: Arc<dyn WebStartup>,
    diagnostics: Arc<FileDiagnosticLog>,
    remotes: Mutex<HashMap<String, Remote>>,
    activities: Mutex<HashMap<String, TurnActivity>>,
}

struct Remote {
    client: WebClient,
    _owner: GrpcTransportClient,
}

impl WebSessionRouter {
    pub fn new(
        local: WebClient,
        roots: ApplicationRoots,
        workspace: ProjectWorkspace,
        startup: Arc<dyn WebStartup>,
        diagnostics: Arc<FileDiagnosticLog>,
    ) -> Self {
        Self {
            local,
            roots,
            workspace,
            startup,
            diagnostics,
            remotes: Mutex::new(HashMap::new()),
            activities: Mutex::new(HashMap::new()),
        }
    }

    pub fn client_for(&self, user_session_id: &str) -> WebClient {
        self.remotes
            .lock()
            .expect("web routes poisoned")
            .get(user_session_id)
            .map_or_else(|| self.local.clone(), |remote| remote.client.clone())
    }

    pub fn is_remote(&self, user_session_id: &str) -> bool {
        self.remotes
            .lock()
            .expect("web routes poisoned")
            .contains_key(user_session_id)
    }

    pub fn observe(&self, user_session_id: &str, published: &wire::Event) {
        self.activities
            .lock()
            .expect("web activities poisoned")
            .entry(user_session_id.to_owned())
            .or_default()
            .observe(published);
    }

    pub fn is_busy(&self, user_session_id: &str) -> bool {
        self.activities
            .lock()
            .expect("web activities poisoned")
            .get(user_session_id)
            .is_some_and(|activity| activity.busy)
    }

    pub async fn open_default(
        &self,
        cancellation: &dyn Cancellation,
    ) -> Result<wire::UserSession, Status> {
        let opened = self.startup.open(cancellation).await?;
        if let Some(remote) = opened.remote {
            self.adopt(&opened.session.id, remote)?;
        }
        Ok(opened.session)
    }

    pub async fn attach_remote(
        &self,
        request: wire::AttachSessionRequest,
        cancellation: &dyn Cancellation,
    ) -> Result<wire::UserSession, Status> {
        let id = UserSessionId::parse(&request.user_session_id)
            .map_err(|error| Status::invalid_argument(error.to_string()))?;
        let selected = self
            .roots
            .discover_session(&id, false, self.workspace.launch_directory())
            .map_err(|error| Status::internal(error.to_string()))?
            .ok_or_else(|| Status::not_found("no user session"))?;
        let resources = UserSessionResources::new(selected, id, self.workspace.clone())
            .map_err(|error| Status::internal(error.to_string()))?;
        let mut connection = GrpcTransportClient::connect(
            &TransportAddress::Unix(resources.socket_path()),
            None,
            None,
            self.diagnostics.clone(),
        )
        .map_err(|error| Status::internal(error.to_string()))?;
        let session = connection
            .attach(request, cancellation)
            .await
            .map_err(attach_status)?;
        self.adopt(&session.id, connection)?;
        Ok(session)
    }

    pub fn forget(&self, user_session_id: &str) {
        self.remotes
            .lock()
            .expect("web routes poisoned")
            .remove(user_session_id);
    }

    fn adopt(&self, user_session_id: &str, mut owner: GrpcTransportClient) -> Result<(), Status> {
        let client = owner
            .client()
            .map_err(|error| Status::internal(error.to_string()))?
            .clone();
        self.remotes.lock().expect("web routes poisoned").insert(
            user_session_id.to_owned(),
            Remote {
                client,
                _owner: owner,
            },
        );
        Ok(())
    }
}

fn attach_status(error: AttachError) -> Status {
    match error {
        AttachError::Rpc(status) => status,
        AttachError::Cancelled => Status::cancelled("operation cancelled"),
        AttachError::Timeout => Status::deadline_exceeded(error.to_string()),
        AttachError::DifferentSession | AttachError::Disposed => {
            Status::internal(error.to_string())
        }
    }
}

#[derive(Default)]
struct TurnActivity {
    subagents: HashSet<String>,
    busy: bool,
}

impl TurnActivity {
    fn observe(&mut self, published: &wire::Event) {
        use wire::event::Payload;
        if matches!(&published.payload, Some(Payload::AgentStarted(started)) if !started.parent_agent_session_id.is_empty())
        {
            self.subagents.insert(published.agent_session_id.clone());
        } else if !self.subagents.contains(&published.agent_session_id) {
            match published.payload {
                Some(Payload::TurnStarted(_)) => self.busy = true,
                Some(Payload::ModeTurnCompleted(_) | Payload::TurnFailed(_)) => self.busy = false,
                _ => {}
            }
        }
    }
}

/// A dropped generated handler future owns cancellation of its native call.
/// No independent task or runtime outlives that observation.
pub(crate) struct HandlerCancellation;
impl Cancellation for HandlerCancellation {
    fn is_cancelled(&self) -> bool {
        false
    }
    fn cancelled(&self) -> BoxFuture<'_, ()> {
        Box::pin(std::future::pending())
    }
}
