use super::{Connection, DatabaseError, Transaction};
use parrot_domain::protocol::messages::parrot::Event;
use rusqlite::{OpenFlags, TransactionBehavior, params};
use std::path::Path;
use std::time::Duration;

const SCHEMA_VERSION: i64 = 1;

#[cfg(test)]
#[path = "../../tests/database/current_schema_crash.rs"]
mod current_schema_crash;

pub(super) fn open(path: &Path) -> Result<Connection, DatabaseError> {
    if let Some(parent) = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
    {
        std::fs::create_dir_all(parent)?;
    }
    let existing = path.try_exists()?;
    let mut connection = if existing {
        Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY)?
    } else {
        Connection::open(path)?
    };
    let timeout = Duration::from_secs(30);
    connection.busy_timeout(timeout)?;
    let version: i64 = connection.pragma_query_value(None, "user_version", |row| row.get(0))?;
    let initialize = match version {
        0 => {
            let populated: bool = connection.query_row(
                "SELECT EXISTS (SELECT 1 FROM sqlite_schema WHERE name NOT GLOB 'sqlite_*');",
                [],
                |row| row.get(0),
            )?;
            if populated {
                return Err(DatabaseError::UnsupportedSchema { version });
            }
            true
        }
        SCHEMA_VERSION => false,
        _ => return Err(DatabaseError::UnsupportedSchema { version }),
    };
    if existing {
        drop(connection);
        connection = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_WRITE)?;
        connection.busy_timeout(timeout)?;
    }
    // TRUNCATE is required on shared filesystems; WAL/SHM are never enabled.
    connection.execute_batch("PRAGMA journal_mode=TRUNCATE; PRAGMA foreign_keys=ON;")?;
    if initialize {
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        transaction.execute_batch(include_str!("schema.sql"))?;
        transaction.execute_batch(include_str!("event_indexes.sql"))?;
        transaction.pragma_update(None, "user_version", SCHEMA_VERSION)?;
        #[cfg(test)]
        current_schema_crash::wait_before_commit(&transaction);
        transaction.commit()?;
    }
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
