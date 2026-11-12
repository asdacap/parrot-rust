//! Durable event/input repository on the selected session's whole-command actor.
//! Publication is a separate, post-commit boundary; replay never filters events.
pub mod error;
pub mod inputs;
pub mod projection;
pub mod repository;
mod runtime_statistics;
pub mod statistics;
pub mod terminal;
pub mod tools;
pub mod types;
pub use error::EventError;
pub use repository::AppendOutcome;
pub use terminal::{CloseTurnError, TurnClosure, TurnCommit, TurnKey};
pub use types::*;

use crate::database::{DatabaseError, SessionDatabase, Transaction, extract_usage_keys};
use parrot_domain::contracts::async_ops::Cancellation;
use parrot_domain::contracts::storage::{AdmissionError, CommitTicket, Committed};
use parrot_domain::protocol::messages::parrot::Event;
use protobuf::Message;
use std::sync::Arc;
use std::time::SystemTime;

/// Observes cumulative decoded event counts, including zero for an empty replay.
/// Counts do not indicate that statistics replay or restoration has completed.
pub type EventLoadProgress = Arc<dyn Fn(usize) + Send + Sync>;

struct RepositoryInner {
    database: SessionDatabase,
    runtime_statistics:
        tokio::sync::OnceCell<Arc<crate::diagnostics::statistics::UserSessionStatistics>>,
}
#[derive(Clone)]
pub struct EventRepository {
    inner: Arc<RepositoryInner>,
}
impl EventRepository {
    pub fn new(database: SessionDatabase) -> Self {
        Self {
            inner: Arc::new(RepositoryInner {
                database,
                runtime_statistics: tokio::sync::OnceCell::new(),
            }),
        }
    }
    pub fn database(&self) -> &SessionDatabase {
        &self.inner.database
    }

    /// Transfers one complete repository transaction, preserving validation errors.
    pub fn admit<K, T, F>(
        &self,
        key: K,
        cancellation: &dyn Cancellation,
        operation: F,
    ) -> Result<CommitTicket<K, T, EventError>, AdmissionError<EventError>>
    where
        K: Clone + Eq + Send + Sync + 'static,
        T: Send + 'static,
        F: FnOnce(&Transaction<'_>) -> Result<T, EventError> + Send + 'static,
    {
        self.inner
            .database
            .admit_with_error(key, cancellation, operation)
    }
    pub async fn transaction<T, F>(&self, operation: F) -> Result<T, EventError>
    where
        T: Send + 'static,
        F: FnOnce(&Transaction<'_>) -> Result<T, EventError> + Send + 'static,
    {
        self.transact(operation).await
    }

    /// Executes a transaction without changing the operation's domain error.
    pub async fn transact<T, E, F>(&self, operation: F) -> Result<T, E>
    where
        T: Send + 'static,
        E: From<DatabaseError> + Send + 'static,
        F: FnOnce(&Transaction<'_>) -> Result<T, E> + Send + 'static,
    {
        let ticket = self
            .inner
            .database
            .admit_with_error((), &NeverCancelled, operation)
            .map_err(|error| match error {
                AdmissionError::Rejected(error) => error,
                AdmissionError::Cancelled => unreachable!("noncancelled admission"),
            })?;
        Ok(ticket.into_completion().await?.into_value().into_output())
    }
    /// Every stored event, in SQLite sequence order, including historical duplicates.
    pub async fn replay_raw(&self) -> Result<Vec<StoredEvent>, EventError> {
        self.transaction(|tx| read_events(tx, None)).await
    }
    pub async fn replay(&self) -> Result<Vec<Event>, EventError> {
        Ok(self
            .replay_raw()
            .await?
            .into_iter()
            .map(|row| row.event)
            .collect())
    }
    /// Events after the first stored occurrence of the cursor id, or every
    /// stored event when the cursor id was never recorded.
    pub async fn replay_after(&self, cursor: &str) -> Result<Vec<Event>, EventError> {
        let cursor = cursor.to_string();
        Ok(self
            .transaction(move |tx| read_events_after(tx, &cursor))
            .await?
            .into_iter()
            .map(|row| row.event)
            .collect())
    }
    pub async fn notices(
        &self,
        agent: String,
        kind: String,
    ) -> Result<Vec<parrot_domain::protocol::messages::parrot::AgentNotice>, EventError> {
        if kind.is_empty() {
            return Err(EventError::Invalid("kind must not be empty".into()));
        }
        self.transaction(move |tx| {
            Ok(read_events(tx, Some(&agent))?
                .into_iter()
                .filter_map(|row| {
                    if row.event.has_agent_notice() && row.event.agent_notice().kind == kind {
                        Some(row.event.agent_notice().clone())
                    } else {
                        None
                    }
                })
                .collect())
        })
        .await
    }
}

pub use parrot_core::NeverCancelled;

/// Insert, never upsert/deduplicate.
pub fn record(tx: &Transaction<'_>, event: &Event, at: &str) -> Result<StoredEvent, EventError> {
    let bytes = event.write_to_bytes()?;
    let (request, call) = extract_usage_keys(event);
    tx.execute("INSERT INTO event (id, agent_session, payload, created_at, request_id, tool_call_id) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",rusqlite::params![event.id,event.agent_session_id,bytes,at,request,call])?;
    let sequence = tx.last_insert_rowid();
    Ok(StoredEvent {
        sequence,
        event: event.clone(),
        raw: bytes,
    })
}
pub fn read_events(
    tx: &Transaction<'_>,
    agent: Option<&str>,
) -> Result<Vec<StoredEvent>, EventError> {
    let mut read=tx.prepare("SELECT sequence, payload FROM event WHERE ?1 IS NULL OR agent_session = ?1 ORDER BY sequence")?;
    let rows = read.query_map([agent], |row| {
        Ok((row.get::<_, i64>(0)?, row.get::<_, Vec<u8>>(1)?))
    })?;
    rows.map(|row| {
        let (sequence, raw) = row?;
        Ok(StoredEvent {
            sequence,
            event: Event::parse_from_bytes(&raw)?,
            raw,
        })
    })
    .collect()
}

pub fn read_events_after(
    tx: &Transaction<'_>,
    cursor: &str,
) -> Result<Vec<StoredEvent>, EventError> {
    let start: Option<i64> = tx.query_row(
        "SELECT MIN(sequence) FROM event WHERE id = ?1",
        [cursor],
        |row| row.get(0),
    )?;
    let Some(start) = start else {
        return read_events(tx, None);
    };
    let mut read =
        tx.prepare("SELECT sequence, payload FROM event WHERE sequence > ?1 ORDER BY sequence")?;
    let rows = read.query_map([start], |row| {
        Ok((row.get::<_, i64>(0)?, row.get::<_, Vec<u8>>(1)?))
    })?;
    rows.map(|row| {
        let (sequence, raw) = row?;
        Ok(StoredEvent {
            sequence,
            event: Event::parse_from_bytes(&raw)?,
            raw,
        })
    })
    .collect()
}

pub(crate) fn timestamp() -> String {
    crate::diagnostics::format_timestamp(SystemTime::now())
}
