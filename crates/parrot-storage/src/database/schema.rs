use super::{Connection, DatabaseError, Transaction};
use parrot_domain::protocol::messages::parrot::Event;
use protobuf::Message;
use rusqlite::params;
use std::path::Path;

#[cfg(test)]
#[path = "../../tests/database/process_kill.rs"]
mod g16_process_kill;

pub(super) fn open(path: &Path) -> Result<Connection, DatabaseError> {
    if let Some(parent) = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
    {
        std::fs::create_dir_all(parent)?;
    }
    let mut connection = Connection::open(path)?;
    // Microsoft.Data.Sqlite's unmodified connection/command timeout is 30s;
    // rusqlite otherwise uses a shorter five-second busy timeout.
    connection.busy_timeout(std::time::Duration::from_secs(30))?;
    // TRUNCATE is required on shared filesystems; WAL/SHM are never enabled.
    connection.execute_batch("PRAGMA journal_mode=TRUNCATE; PRAGMA foreign_keys=ON;")?;
    connection.execute_batch(include_str!("schema.sql"))?;
    migrate_usage_keys(&mut connection)?;
    migrate_lineage(&mut connection)?;
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

fn migrate_usage_keys(connection: &mut Connection) -> Result<(), DatabaseError> {
    let exists: bool = connection.query_row(
        "SELECT EXISTS (SELECT 1 FROM pragma_table_info('event') WHERE name = 'request_id');",
        [],
        |row| row.get(0),
    )?;
    // Match the source presence sentinel, including its deliberately narrow
    // behavior on malformed partially upgraded schemas.
    if exists {
        return Ok(());
    }
    #[cfg(test)]
    g16_process_kill::baseline(connection, "usage-before-commit");
    let transaction =
        connection.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
    transaction.execute_batch("ALTER TABLE event ADD COLUMN request_id TEXT; ALTER TABLE event ADD COLUMN tool_call_id TEXT;")?;
    let mut keys = Vec::new();
    {
        let mut statement = transaction.prepare("SELECT sequence, payload FROM event;")?;
        let mut rows = statement.query([])?;
        while let Some(row) = rows.next()? {
            let sequence: i64 = row.get(0)?;
            let payload: Vec<u8> = row.get(1)?;
            let event = Event::parse_from_bytes(&payload)?;
            let (request, tool) = extract_usage_keys(&event);
            if request.is_some() || tool.is_some() {
                keys.push((
                    sequence,
                    request.map(str::to_owned),
                    tool.map(str::to_owned),
                ));
            }
        }
    }
    for (sequence, request, tool) in keys {
        transaction.execute(
            "UPDATE event SET request_id = ?1, tool_call_id = ?2 WHERE sequence = ?3;",
            params![request, tool, sequence],
        )?;
    }
    #[cfg(test)]
    g16_process_kill::pause(&transaction, "usage-before-commit");
    transaction.commit()?;
    Ok(())
}

fn migrate_lineage(connection: &mut Connection) -> Result<(), DatabaseError> {
    let exists: bool = connection.query_row(
        "SELECT EXISTS (SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = 'agent_lineage');",
        [], |row| row.get(0),
    )?;
    if exists {
        return Ok(());
    }
    #[cfg(test)]
    g16_process_kill::baseline(connection, "lineage-before-commit");
    let transaction =
        connection.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
    transaction.execute_batch(include_str!("lineage.sql"))?;
    let mut starts = Vec::new();
    {
        let mut statement =
            transaction.prepare("SELECT sequence, payload FROM event ORDER BY sequence;")?;
        let mut rows = statement.query([])?;
        while let Some(row) = rows.next()? {
            let sequence: i64 = row.get(0)?;
            let payload: Vec<u8> = row.get(1)?;
            let event = Event::parse_from_bytes(&payload)?;
            if event.has_agent_started() {
                starts.push((sequence, event));
            }
        }
    }
    for (sequence, event) in starts {
        record_agent_start(&transaction, sequence, &event)?;
    }
    #[cfg(test)]
    g16_process_kill::pause(&transaction, "lineage-before-commit");
    transaction.commit()?;
    Ok(())
}
