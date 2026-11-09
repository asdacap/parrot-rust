use std::io::Read;
use std::time::{SystemTime, UNIX_EPOCH};

use parrot_domain::contracts::async_ops::Cancellation;
use rusqlite::{OptionalExtension, params};

use super::image_store::ImageArtifactStore;
use super::{Error, ErrorKind, ImageArtifactMetadata, Result};
use crate::database::{DatabaseError, SessionDatabase, Transaction};
use parrot_domain::contracts::storage::AdmissionError;

/// Image bytes and upload identities bound to one selected session resource.
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

    #[allow(clippy::too_many_arguments)]
    pub async fn persist<R: Read + Send + 'static>(
        &self,
        mut source: R,
        upload_id: &str,
        display_name: &str,
        origin: &str,
        declared_media_type: Option<&str>,
        declared_byte_length: Option<i64>,
        cancellation: &dyn Cancellation,
    ) -> Result<ImageArtifactMetadata> {
        if upload_id.trim().is_empty() {
            return Err(Error::invalid_input("upload_id cannot be empty"));
        }
        let artifact = self.store.persist(
            &mut source,
            display_name,
            origin,
            declared_media_type,
            declared_byte_length,
            cancellation,
        )?;
        let upload_id = upload_id.to_owned();
        execute(
            &self.database,
            format!("image-upload:{upload_id}"),
            cancellation,
            move |transaction| record_artifact(transaction, artifact, &upload_id),
        )
        .await
    }

    pub async fn resolve(
        &self,
        artifact_id: &str,
        cancellation: &dyn Cancellation,
    ) -> Result<Option<ImageArtifactMetadata>> {
        if artifact_id.trim().is_empty() {
            return Err(Error::invalid_input("artifact_id cannot be empty"));
        }
        let artifact_id = artifact_id.to_owned();
        let selected_id = artifact_id.clone();
        let labels = execute(
            &self.database,
            format!("image-resolve:{artifact_id}"),
            cancellation,
            move |tx| read_labels(tx, &selected_id).map_err(Error::from_sqlite),
        )
        .await?;
        let Some((display_name, origin)) = labels else {
            return Ok(None);
        };
        self.store
            .inspect(&artifact_id, &display_name, &origin, cancellation)
    }

    pub async fn open(
        &self,
        artifact_id: &str,
        cancellation: &dyn Cancellation,
    ) -> Result<std::fs::File> {
        if self.resolve(artifact_id, cancellation).await?.is_none() {
            return Err(Error::new(
                ErrorKind::NotFound,
                "The image artifact does not exist.",
            ));
        }
        self.store.open(artifact_id)
    }

    /// Removes expired unreferenced hash files and their upload bindings.
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
        let artifacts = execute(&self.database, format!("image-cleanup:{}", roundtrip_timestamp(before)), cancellation, move |tx| {
            let mut artifacts = Vec::new();
            for artifact_id in candidates {
                let retained: bool = tx.query_row(
                    "SELECT EXISTS (SELECT 1 FROM input_part WHERE artifact_id = ?1) OR EXISTS (SELECT 1 FROM conversation_part WHERE artifact_id = ?1)",
                    [&artifact_id], |row| row.get(0),
                )?;
                if !retained {
                    tx.execute("DELETE FROM image_upload WHERE artifact_id = ?1", [&artifact_id])?;
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

fn record_artifact(
    tx: &Transaction<'_>,
    mut artifact: ImageArtifactMetadata,
    upload_id: &str,
) -> Result<ImageArtifactMetadata> {
    let existing: Option<String> = tx
        .query_row(
            "SELECT artifact_id FROM image_upload WHERE upload_id = ?1",
            [upload_id],
            |row| row.get(0),
        )
        .optional()?;
    if let Some(existing) = existing {
        if existing != artifact.artifact_id {
            return Err(Error::new(
                ErrorKind::Conflict,
                format!("image upload {upload_id} was already recorded with different content"),
            ));
        }
    } else {
        tx.execute(
            "INSERT INTO image_upload (upload_id, artifact_id, display_name, origin, created_at) VALUES (?1, ?2, ?3, ?4, ?5)",
            params![upload_id, artifact.artifact_id, artifact.display_name, artifact.origin, current_timestamp()],
        )?;
    }
    let (display_name, origin) = read_labels(tx, &artifact.artifact_id)?
        .ok_or_else(|| Error::invalid_data("The image upload was not recorded."))?;
    artifact.display_name = display_name;
    artifact.origin = origin;
    Ok(artifact)
}

fn read_labels(
    tx: &Transaction<'_>,
    artifact_id: &str,
) -> rusqlite::Result<Option<(String, String)>> {
    tx.query_row(
        "SELECT display_name, origin FROM image_upload WHERE artifact_id = ?1 ORDER BY rowid LIMIT 1",
        [artifact_id], |row| Ok((row.get(0)?, row.get(1)?)),
    ).optional()
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
        .admit(key, cancellation, move |transaction| {
            operation(transaction).map_err(rollback)
        })
        .map_err(|error| match error {
            AdmissionError::Cancelled => Error::cancelled(),
            AdmissionError::Rejected(error) => database_error(error),
        })?;
    let outcome = ticket.into_completion().await.map_err(database_error)?;
    let receipt = outcome.into_value();
    Ok(receipt.into_output())
}

fn rollback(error: Error) -> DatabaseError {
    DatabaseError::Sqlite(rusqlite::Error::ToSqlConversionFailure(Box::new(error)))
}

fn database_error(error: DatabaseError) -> Error {
    match error {
        DatabaseError::Sqlite(rusqlite::Error::ToSqlConversionFailure(error)) => {
            match error.downcast::<Error>() {
                Ok(error) => *error,
                Err(error) => {
                    let sqlite = rusqlite::Error::ToSqlConversionFailure(error);
                    Error::new(ErrorKind::Database, sqlite.to_string()).with_source(sqlite)
                }
            }
        }
        error => Error::new(ErrorKind::Database, error.to_string()).with_source(error),
    }
}

fn current_timestamp() -> String {
    roundtrip_timestamp(SystemTime::now())
}

fn roundtrip_timestamp(time: SystemTime) -> String {
    let duration = time.duration_since(UNIX_EPOCH).unwrap_or_default();
    let seconds = duration.as_secs() as i64;
    let days = seconds.div_euclid(86_400);
    let day_seconds = seconds.rem_euclid(86_400);
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 }.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let mut year = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = mp + if mp < 10 { 3 } else { -9 };
    year += i64::from(month <= 2);
    let hour = day_seconds / 3600;
    let minute = (day_seconds % 3600) / 60;
    let second = day_seconds % 60;
    let fraction = duration.subsec_nanos() / 100;
    format!("{year:04}-{month:02}-{day:02}T{hour:02}:{minute:02}:{second:02}.{fraction:07}+00:00")
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
