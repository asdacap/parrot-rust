use std::io::Read;
use std::time::SystemTime;

use parrot_domain::contracts::async_ops::Cancellation;

use super::image_store::ImageArtifactStore;
use super::{Error, ErrorKind, ImageArtifactMetadata, Result};
use crate::database::{DatabaseError, SessionDatabase, Transaction};
use parrot_domain::contracts::storage::AdmissionError;

/// Image bytes and reference-aware cleanup bound to one selected session resource.
pub struct ImageArtifactRepository {
    database: SessionDatabase,
    store: ImageArtifactStore,
}

impl ImageArtifactRepository {
    pub fn new(database: SessionDatabase, store: ImageArtifactStore) -> Result<Self> {
        if database.selected_root() != store.selected_root() {
            return Err(Error::invalid_input(
                "image store and session database must use the same selected session root",
            ));
        }
        Ok(Self { database, store })
    }

    pub fn store(&self) -> &ImageArtifactStore {
        &self.store
    }

    pub async fn persist<R: Read + Send + 'static>(
        &self,
        mut source: R,
        display_name: &str,
        origin: &str,
        declared_media_type: Option<&str>,
        declared_byte_length: Option<i64>,
        cancellation: &dyn Cancellation,
    ) -> Result<ImageArtifactMetadata> {
        self.store.persist(
            &mut source,
            display_name,
            origin,
            declared_media_type,
            declared_byte_length,
            cancellation,
        )
    }

    pub async fn resolve(
        &self,
        artifact_id: &str,
        display_name: &str,
        origin: &str,
        cancellation: &dyn Cancellation,
    ) -> Result<Option<ImageArtifactMetadata>> {
        self.store
            .inspect(artifact_id, display_name, origin, cancellation)
    }

    pub async fn open(
        &self,
        artifact_id: &str,
        cancellation: &dyn Cancellation,
    ) -> Result<std::fs::File> {
        cancellation.check().map_err(|_| Error::task_cancelled())?;
        self.store.open(artifact_id)
    }

    /// Removes expired hash files that no input or conversation part references.
    pub async fn remove_stale_unreferenced_before(
        &self,
        before: SystemTime,
        cancellation: &dyn Cancellation,
    ) -> Result<Vec<String>> {
        cancellation.check().map_err(|_| Error::task_cancelled())?;
        let entries = match std::fs::read_dir(self.store.directory()) {
            Ok(entries) => Some(entries),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
            Err(error) => return Err(Error::io("could not read image artifact directory", error)),
        };
        let mut candidates = Vec::new();
        for entry in entries.into_iter().flatten() {
            cancellation.check().map_err(|_| Error::task_cancelled())?;
            let entry =
                entry.map_err(|error| Error::io("could not read image artifact entry", error))?;
            if !entry
                .file_type()
                .map_err(|error| Error::io("could not inspect image artifact entry", error))?
                .is_file()
            {
                continue;
            }
            let name = entry.file_name();
            let Some(name) = name.to_str() else { continue };
            if name.len() != 64
                || !name
                    .bytes()
                    .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
            {
                continue;
            }
            let modified = entry
                .metadata()
                .and_then(|metadata| metadata.modified())
                .map_err(|error| Error::io("could not read image artifact age", error))?;
            if modified < before {
                candidates.push(name.to_owned());
            }
        }
        let artifacts = execute(&self.database, format!("image-cleanup:{before:?}"), cancellation, move |tx| {
            let mut artifacts = Vec::new();
            for artifact_id in candidates {
                let retained: bool = tx.query_row(
                    "SELECT EXISTS (SELECT 1 FROM input_part WHERE artifact_id = ?1) OR EXISTS (SELECT 1 FROM conversation_part WHERE artifact_id = ?1)",
                    [&artifact_id], |row| row.get(0),
                )?;
                if !retained {
                    artifacts.push(artifact_id);
                }
            }
            Ok(artifacts)
        }).await?;
        for artifact_id in &artifacts {
            self.store.delete(artifact_id)?;
        }
        Ok(artifacts)
    }
}

async fn execute<T, F>(
    database: &SessionDatabase,
    key: String,
    cancellation: &dyn Cancellation,
    operation: F,
) -> Result<T>
where
    T: Send + 'static,
    F: FnOnce(&Transaction<'_>) -> Result<T> + Send + 'static,
{
    let ticket = database
        .admit_with_error(key, cancellation, operation)
        .map_err(|error| match error {
            AdmissionError::Cancelled => Error::cancelled(),
            AdmissionError::Rejected(error) => error,
        })?;
    let outcome = ticket.into_completion().await?;
    let receipt = outcome.into_value();
    Ok(receipt.into_output())
}

impl From<DatabaseError> for Error {
    fn from(error: DatabaseError) -> Self {
        Self::new(ErrorKind::Database, error.to_string()).with_source(error)
    }
}

impl From<rusqlite::Error> for Error {
    fn from(error: rusqlite::Error) -> Self {
        Self::from_sqlite(error)
    }
}

impl Error {
    fn from_sqlite(error: rusqlite::Error) -> Self {
        Error::new(ErrorKind::Database, error.to_string()).with_source(error)
    }
}
