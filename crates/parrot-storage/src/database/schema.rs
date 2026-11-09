use super::{Connection, DatabaseError, Transaction};
use parrot_domain::protocol::messages::parrot::Event;
use rusqlite::params;
use std::path::Path;

pub(super) fn open(path: &Path) -> Result<Connection, DatabaseError> {
    if let Some(parent) = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
    {
        std::fs::create_dir_all(parent)?;
    }
    let connection = Connection::open(path)?;
    // Microsoft.Data.Sqlite's unmodified connection/command timeout is 30s;
    // rusqlite otherwise uses a shorter five-second busy timeout.
    connection.busy_timeout(std::time::Duration::from_secs(30))?;
    // TRUNCATE is required on shared filesystems; WAL/SHM are never enabled.
    connection.execute_batch("PRAGMA journal_mode=TRUNCATE; PRAGMA foreign_keys=ON;")?;
    connection.execute_batch(include_str!("schema.sql"))?;
    connection.execute_batch(include_str!("event_indexes.sql"))?;
    Ok(connection)
}

/// Only request usage and tool-start facts carry their respective indexed key.
/// An empty protobuf string is still a present key, just as in the source.
pub fn extract_usage_keys(event: &Event) -> (Option<&str>, Option<&str>) {
    if event.has_request_usage_recorded() {
        (Some(&event.request_usage_recorded().request_id), None)
    } else if event.has_tool_execution_started() {
        (None, Some(&event.tool_execution_started().tool_call_id))
    } else {
        (None, None)
    }
}

/// The first start, in event-sequence order, fixes an agent's lineage.
pub fn record_agent_start(
    transaction: &Transaction<'_>,
    event_sequence: i64,
    event: &Event,
) -> Result<(), DatabaseError> {
    if event.has_agent_started() {
        let start = event.agent_started();
        transaction.execute(
            "INSERT OR IGNORE INTO agent_lineage (agent_session, parent_agent_session, name, event_sequence) VALUES (?1, ?2, ?3, ?4);",
            params![event.agent_session_id, start.parent_agent_session_id, start.name, event_sequence],
        )?;
    }
    Ok(())
}
