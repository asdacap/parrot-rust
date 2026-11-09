use super::{Connection, DatabaseError};
use parrot_domain::protocol::messages::parrot::Event;
use rusqlite::{OpenFlags, TransactionBehavior};
use std::path::Path;
use std::time::Duration;

const SCHEMA_VERSION: i64 = 9;

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
    let version: i64 = match connection.pragma_query_value(None, "user_version", |row| row.get(0)) {
        Ok(version) => version,
        Err(rusqlite::Error::SqliteFailure(error, _))
            if error.extended_code == rusqlite::ffi::SQLITE_READONLY_ROLLBACK =>
        {
            // Immutable mode ignores WAL, so reserve it for this rollback-only error.
            drop(connection);
            let mut uri = String::from("file:");
            let hex = b"0123456789ABCDEF";
            for &byte in path.as_os_str().as_encoded_bytes() {
                uri.push('%');
                uri.push(char::from(hex[usize::from(byte >> 4)]));
                uri.push(char::from(hex[usize::from(byte & 15)]));
            }
            uri.push_str("?immutable=1");
            connection = Connection::open_with_flags(
                uri,
                OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_URI,
            )?;
            connection.busy_timeout(timeout)?;
            connection.pragma_query_value(None, "user_version", |row| row.get(0))?
        }
        Err(error) => return Err(error.into()),
    };
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
