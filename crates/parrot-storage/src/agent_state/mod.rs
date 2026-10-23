//! Agent-local recovery snapshots. One shared store belongs to each agent scope.

mod files;

use files::AgentStateDirectory;
use parrot_domain::{
    contracts::{
        agent_state::{AgentStateError, AgentStateErrorKind, AgentStateStore},
        async_ops::BoxFuture,
    },
    values::agent_state::{AGENT_STATE_VERSION, AgentStateSnapshot, AgentStateUpdate},
};
use serde::Deserialize;
use std::{io, path::Path, sync::Arc};
use tokio::sync::Mutex;

pub struct FileAgentStateStore {
    state: Arc<Mutex<State>>,
}
struct State {
    directory: AgentStateDirectory,
    owner_session_id: String,
}
impl FileAgentStateStore {
    /// Opens an existing private agent directory on a blocking worker without
    /// creating a snapshot. Share the returned store across section owners.
    pub fn open(
        agent_directory: &Path,
        owner_session_id: String,
    ) -> BoxFuture<'static, Result<Self, AgentStateError>> {
        let path = agent_directory.to_owned();
        Box::pin(async move {
            tokio::task::spawn_blocking(move || {
                let directory = AgentStateDirectory::open(&path).map_err(path_error)?;
                Ok(Self {
                    state: Arc::new(Mutex::new(State {
                        directory,
                        owner_session_id,
                    })),
                })
            })
            .await
            .map_err(worker_error)?
        })
    }
}
impl AgentStateStore for FileAgentStateStore {
    fn load(&self) -> BoxFuture<'_, Result<Option<AgentStateSnapshot>, AgentStateError>> {
        Box::pin(async move {
            let state = self.state.clone().lock_owned().await;
            tokio::task::spawn_blocking(move || state.read_snapshot())
                .await
                .map_err(worker_error)?
        })
    }

    fn initialize(
        &self,
        snapshot: AgentStateSnapshot,
    ) -> BoxFuture<'_, Result<(), AgentStateError>> {
        Box::pin(async move {
            let state = self.state.clone().lock_owned().await;
            tokio::task::spawn_blocking(move || {
                if state.directory.exists().map_err(path_error)? {
                    return Err(AgentStateError::new(
                        AgentStateErrorKind::AlreadyExists,
                        "agent snapshot already exists; initialization cannot replace it",
                    ));
                }
                state.validate(&snapshot)?;
                state.write_snapshot(&snapshot)
            })
            .await
            .map_err(worker_error)?
        })
    }

    fn update(
        &self,
        update: AgentStateUpdate,
    ) -> BoxFuture<'_, Result<AgentStateSnapshot, AgentStateError>> {
        Box::pin(async move {
            let state = self.state.clone().lock_owned().await;
            tokio::task::spawn_blocking(move || {
                let mut snapshot = state.read_snapshot()?.ok_or_else(|| {
                    AgentStateError::new(AgentStateErrorKind::NotFound, "agent snapshot is absent")
                })?;
                match update {
                    AgentStateUpdate::Descriptor(value) => snapshot.descriptor = value,
                    AgentStateUpdate::Children(value) => snapshot.children = value,
                    AgentStateUpdate::Tasks(value) => snapshot.tasks = value,
                    AgentStateUpdate::Execution(value) => snapshot.execution = value,
                }
                snapshot.revision = snapshot.revision.checked_add(1).ok_or_else(|| {
                    AgentStateError::new(
                        AgentStateErrorKind::Corrupt,
                        "agent snapshot revision overflow",
                    )
                })?;
                state.write_snapshot(&snapshot)?;
                Ok(snapshot)
            })
            .await
            .map_err(worker_error)?
        })
    }
}
impl State {
    fn read_snapshot(&self) -> Result<Option<AgentStateSnapshot>, AgentStateError> {
        let Some(bytes) = self.directory.read().map_err(path_error)? else {
            return Ok(None);
        };
        #[derive(Deserialize)]
        struct Version {
            schema_version: u64,
        }
        let version: Version = serde_json::from_slice(&bytes).map_err(corrupt)?;
        if version.schema_version != AGENT_STATE_VERSION {
            return Err(AgentStateError::new(
                AgentStateErrorKind::UnsupportedVersion,
                format!(
                    "unsupported agent snapshot version {}",
                    version.schema_version
                ),
            ));
        }
        let snapshot = serde_json::from_slice(&bytes).map_err(corrupt)?;
        self.validate(&snapshot)?;
        Ok(Some(snapshot))
    }

    fn validate(&self, snapshot: &AgentStateSnapshot) -> Result<(), AgentStateError> {
        if snapshot.schema_version != AGENT_STATE_VERSION {
            return Err(AgentStateError::new(
                AgentStateErrorKind::UnsupportedVersion,
                format!(
                    "unsupported agent snapshot version {}",
                    snapshot.schema_version
                ),
            ));
        }
        if snapshot.owner_session_id != self.owner_session_id {
            return Err(AgentStateError::new(
                AgentStateErrorKind::OwnerMismatch,
                "agent snapshot belongs to a different agent session",
            ));
        }
        Ok(())
    }

    fn write_snapshot(&self, snapshot: &AgentStateSnapshot) -> Result<(), AgentStateError> {
        let bytes = serde_json::to_vec(snapshot).map_err(corrupt)?;
        self.directory.replace(&bytes).map_err(path_error)
    }
}

fn corrupt(error: serde_json::Error) -> AgentStateError {
    AgentStateError::new(
        AgentStateErrorKind::Corrupt,
        format!("invalid agent snapshot: {error}"),
    )
}
fn path_error(error: io::Error) -> AgentStateError {
    AgentStateError::new(
        if matches!(
            error.kind(),
            io::ErrorKind::InvalidInput | io::ErrorKind::InvalidData
        ) {
            AgentStateErrorKind::InvalidPath
        } else {
            AgentStateErrorKind::Io
        },
        format!("agent snapshot storage: {error}"),
    )
}
fn worker_error(error: tokio::task::JoinError) -> AgentStateError {
    AgentStateError::new(
        AgentStateErrorKind::Io,
        format!("agent snapshot worker failed: {error}"),
    )
}
