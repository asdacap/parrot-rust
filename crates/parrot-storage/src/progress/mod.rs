mod files;

use files::ProgressDirectory;
use parrot_domain::{
    contracts::{
        async_ops::BoxFuture,
        progress::{ProgressStore, ProgressStoreError, ProgressStoreErrorKind},
    },
    values::progress::{PROGRESS_VERSION, ProgressSnapshot, ProgressValidationError},
};
use serde::Deserialize;
use std::{io, path::Path, sync::Arc};
use tokio::sync::Mutex;

pub struct FileProgressStore {
    state: Arc<Mutex<State>>,
}
struct State {
    directory: ProgressDirectory,
    owner_session_id: String,
}
impl FileProgressStore {
    /// Opens an existing private agent directory without creating progress state.
    pub fn open(
        agent_directory: &Path,
        owner_session_id: String,
    ) -> BoxFuture<'static, Result<Self, ProgressStoreError>> {
        let path = agent_directory.to_owned();
        Box::pin(async move {
            tokio::task::spawn_blocking(move || {
                let directory = ProgressDirectory::open(&path).map_err(path_error)?;
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
impl ProgressStore for FileProgressStore {
    fn load(&self) -> BoxFuture<'_, Result<Option<ProgressSnapshot>, ProgressStoreError>> {
        Box::pin(async move {
            let state = self.state.clone().lock_owned().await;
            tokio::task::spawn_blocking(move || state.read_snapshot())
                .await
                .map_err(worker_error)?
        })
    }

    fn replace(&self, snapshot: ProgressSnapshot) -> BoxFuture<'_, Result<(), ProgressStoreError>> {
        Box::pin(async move {
            let state = self.state.clone().lock_owned().await;
            tokio::task::spawn_blocking(move || {
                state.validate(&snapshot)?;
                let _ = state.read_snapshot()?;
                let bytes = serde_json::to_vec(&snapshot).map_err(corrupt)?;
                state.directory.replace(&bytes)
            })
            .await
            .map_err(worker_error)?
        })
    }
}
impl State {
    fn read_snapshot(&self) -> Result<Option<ProgressSnapshot>, ProgressStoreError> {
        let Some(bytes) = self.directory.read().map_err(path_error)? else {
            return Ok(None);
        };
        #[derive(Deserialize)]
        struct Version {
            schema_version: u64,
        }
        let version: Version = serde_json::from_slice(&bytes).map_err(corrupt)?;
        if version.schema_version != PROGRESS_VERSION {
            return Err(ProgressStoreError::new(
                ProgressStoreErrorKind::UnsupportedVersion,
                format!(
                    "unsupported progress snapshot version {}",
                    version.schema_version
                ),
            ));
        }
        let snapshot = serde_json::from_slice(&bytes).map_err(corrupt)?;
        self.validate(&snapshot)?;
        Ok(Some(snapshot))
    }

    fn validate(&self, snapshot: &ProgressSnapshot) -> Result<(), ProgressStoreError> {
        snapshot.validate().map_err(|error| {
            ProgressStoreError::new(
                if matches!(error, ProgressValidationError::UnsupportedVersion(_)) {
                    ProgressStoreErrorKind::UnsupportedVersion
                } else {
                    ProgressStoreErrorKind::Corrupt
                },
                error.to_string(),
            )
        })?;
        if snapshot.owner_session_id != self.owner_session_id {
            return Err(ProgressStoreError::new(
                ProgressStoreErrorKind::OwnerMismatch,
                "progress snapshot belongs to a different agent session",
            ));
        }
        Ok(())
    }
}

fn corrupt(error: serde_json::Error) -> ProgressStoreError {
    ProgressStoreError::new(
        ProgressStoreErrorKind::Corrupt,
        format!("invalid progress snapshot: {error}"),
    )
}
fn path_error(error: io::Error) -> ProgressStoreError {
    ProgressStoreError::new(
        if matches!(
            error.kind(),
            io::ErrorKind::InvalidInput | io::ErrorKind::InvalidData
        ) {
            ProgressStoreErrorKind::InvalidPath
        } else {
            ProgressStoreErrorKind::Io
        },
        error.to_string(),
    )
}
fn worker_error(error: tokio::task::JoinError) -> ProgressStoreError {
    ProgressStoreError::new(ProgressStoreErrorKind::Io, error.to_string())
}

#[cfg(test)]
mod tests;
