use std::io::Read;
use std::time::{SystemTime, UNIX_EPOCH};

use parrot_domain::contracts::async_ops::Cancellation;
use rusqlite::{OptionalExtension, params};

use super::image_store::ImageArtifactStore;
use super::{Error, ErrorKind, ImageArtifactMetadata, Result};
use crate::database::{DatabaseError, SessionDatabase, Transaction};
use parrot_domain::contracts::storage::AdmissionError;

const ARTIFACT_SELECT: &str = "SELECT artifact.artifact_id, artifact.media_type, artifact.byte_length, artifact.width, artifact.height, artifact.frame_count, artifact.aggregate_pixels, artifact.display_name, artifact.origin FROM image_artifact AS artifact";

/// Image bytes and upload metadata bound to one selected session DB and resource directory.
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
            move |transaction| record_artifact(transaction, &artifact, &upload_id),
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
        execute(
            &self.database,
            format!("image-resolve:{artifact_id}"),
            cancellation,
            move |tx| resolve_artifact(tx, &artifact_id).map_err(Error::from_sqlite),
        )
        .await
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

    /// Removes expired unreferenced metadata in one DB transaction, then its selected-root files.
    pub async fn remove_stale_unreferenced_before(
        &self,
        before: SystemTime,
        cancellation: &dyn Cancellation,
    ) -> Result<Vec<ImageArtifactMetadata>> {
        let before = roundtrip_timestamp(before);
        let artifacts = execute(&self.database, format!("image-cleanup:{before}"), cancellation, move |tx| {
            let mut statement = tx.prepare(&format!(
                "{ARTIFACT_SELECT} WHERE artifact.created_at < ?1 \
                 AND NOT EXISTS (SELECT 1 FROM input_part WHERE input_part.artifact_id = artifact.artifact_id) \
                 AND NOT EXISTS (SELECT 1 FROM conversation_part WHERE conversation_part.artifact_id = artifact.artifact_id)"
            ))?;
            let mut rows = statement.query([&before])?;
            let mut artifacts = Vec::new();
            while let Some(row) = rows.next()? { artifacts.push(read_metadata(row)?); }
            drop(rows);
            drop(statement);
            for artifact in &artifacts {
                tx.execute("DELETE FROM image_upload WHERE artifact_id = ?1", [&artifact.artifact_id])?;
                tx.execute("DELETE FROM image_artifact WHERE artifact_id = ?1", [&artifact.artifact_id])?;
            }
            Ok(artifacts)
        }).await?;
        for artifact in &artifacts {
            self.store.delete(artifact)?;
        }
        Ok(artifacts)
    }
}

fn record_artifact(
    tx: &Transaction<'_>,
    artifact: &ImageArtifactMetadata,
    upload_id: &str,
) -> Result<ImageArtifactMetadata> {
    if let Some(existing) = image_upload(tx, upload_id)? {
        return if existing.matches_content(artifact) {
            Ok(existing)
        } else {
            Err(Error::new(
                ErrorKind::Conflict,
                format!("image upload {upload_id} was already recorded with different content"),
            ))
        };
    }
    if let Some(existing) =
        resolve_artifact(tx, &artifact.artifact_id).map_err(Error::from_sqlite)?
    {
        if !existing.matches_content(artifact) {
            return Err(Error::new(
                ErrorKind::Conflict,
                format!(
                    "image artifact {} was already recorded with different metadata",
                    artifact.artifact_id
                ),
            ));
        }
    }
    let at = current_timestamp();
    tx.execute(
        "INSERT INTO image_artifact (artifact_id, media_type, byte_length, width, height, frame_count, aggregate_pixels, display_name, origin, created_at) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10) ON CONFLICT(artifact_id) DO NOTHING",
        params![artifact.artifact_id, artifact.media_type, artifact.byte_length, artifact.width, artifact.height, artifact.frame_count, artifact.aggregate_pixels, artifact.display_name, artifact.origin, at],
    ).map_err(Error::from_sqlite)?;
    let recorded = resolve_artifact(tx, &artifact.artifact_id)
        .map_err(Error::from_sqlite)?
        .ok_or_else(|| Error::invalid_data("The image artifact was not recorded."))?;
    if !recorded.matches_content(artifact) {
        return Err(Error::new(
            ErrorKind::Conflict,
            format!(
                "image artifact {} was already recorded with different metadata",
                artifact.artifact_id
            ),
        ));
    }
    tx.execute(
        "INSERT INTO image_upload (upload_id, artifact_id, created_at) VALUES (?1, ?2, ?3)",
        params![upload_id, artifact.artifact_id, current_timestamp()],
    )
    .map_err(Error::from_sqlite)?;
    Ok(recorded)
}

fn image_upload(tx: &Transaction<'_>, upload_id: &str) -> Result<Option<ImageArtifactMetadata>> {
    tx.query_row(
        &format!("{ARTIFACT_SELECT} JOIN image_upload AS upload ON upload.artifact_id = artifact.artifact_id WHERE upload.upload_id = ?1"),
        [upload_id], read_metadata,
    ).optional().map_err(Error::from_sqlite)
}

fn resolve_artifact(
    tx: &Transaction<'_>,
    artifact_id: &str,
) -> rusqlite::Result<Option<ImageArtifactMetadata>> {
    tx.query_row(
        &format!("{ARTIFACT_SELECT} WHERE artifact.artifact_id = ?1"),
        [artifact_id],
        read_metadata,
    )
    .optional()
}

fn read_metadata(row: &rusqlite::Row<'_>) -> rusqlite::Result<ImageArtifactMetadata> {
    let artifact_id: String = row.get(0)?;
    Ok(ImageArtifactMetadata {
        sha256: artifact_id.clone(),
        artifact_id,
        media_type: row.get(1)?,
        byte_length: row.get(2)?,
        width: row.get(3)?,
        height: row.get(4)?,
        frame_count: row.get(5)?,
        aggregate_pixels: row.get(6)?,
        display_name: row.get(7)?,
        origin: row.get(8)?,
    })
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
