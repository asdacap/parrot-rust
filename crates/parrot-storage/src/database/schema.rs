use super::{Connection, DatabaseError};
use parrot_domain::protocol::messages::parrot::Event;
use rusqlite::{OpenFlags, TransactionBehavior};
use std::path::Path;
use std::time::Duration;

const SCHEMA_VERSION: i64 = 12;

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
    connection.execute_batch(include_str!("conversation_indexes.sql"))?;
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

/// Every stored event names its payload variant so SQL can filter by kind,
/// which the serialized payload blob alone does not support. The name is the
/// proto oneof field name, which matches the variant's snake case. An event
/// with no payload set stores an empty kind.
pub fn extract_event_kind(event: &Event) -> &'static str {
    use parrot_domain::protocol::messages::parrot::event::Payload;
    match event.payload.as_ref() {
        Some(Payload::TurnStarted(_)) => "turn_started",
        Some(Payload::TextChunk(_)) => "text_chunk",
        Some(Payload::ReasoningChunk(_)) => "reasoning_chunk",
        Some(Payload::ToolCallChunk(_)) => "tool_call_chunk",
        Some(Payload::RetryNotice(_)) => "retry_notice",
        Some(Payload::TurnEnded(_)) => "turn_ended",
        Some(Payload::TurnFailed(_)) => "turn_failed",
        Some(Payload::InputAdmitted(_)) => "input_admitted",
        Some(Payload::InputPromoted(_)) => "input_promoted",
        Some(Payload::ToolStarted(_)) => "tool_started",
        Some(Payload::ToolFinished(_)) => "tool_finished",
        Some(Payload::ToolCancelled(_)) => "tool_cancelled",
        Some(Payload::ToolError(_)) => "tool_error",
        Some(Payload::AgentStarted(_)) => "agent_started",
        Some(Payload::AgentFinished(_)) => "agent_finished",
        Some(Payload::AgentFailed(_)) => "agent_failed",
        Some(Payload::AgentStatisticsUpdated(_)) => "agent_statistics_updated",
        Some(Payload::PlanCompleted(_)) => "plan_completed",
        Some(Payload::PermissionPending(_)) => "permission_pending",
        Some(Payload::SessionUsageSnapshot(_)) => "session_usage_snapshot",
        Some(Payload::CompactionStarted(_)) => "compaction_started",
        Some(Payload::CompactionFinished(_)) => "compaction_finished",
        Some(Payload::CompactionFailed(_)) => "compaction_failed",
        Some(Payload::ProviderCallUsage(_)) => "provider_call_usage",
        Some(Payload::SkillLoaded(_)) => "skill_loaded",
        Some(Payload::InputCanceled(_)) => "input_canceled",
        Some(Payload::ProviderRequestPhaseChanged(_)) => "provider_request_phase_changed",
        Some(Payload::RequestUsageRecorded(_)) => "request_usage_recorded",
        Some(Payload::ToolExecutionStarted(_)) => "tool_execution_started",
        Some(Payload::ToolRequestReceived(_)) => "tool_request_received",
        Some(Payload::ModeTurnCompleted(_)) => "mode_turn_completed",
        Some(Payload::AgentNotice(_)) => "agent_notice",
        Some(Payload::ToolSnapshot(_)) => "tool_snapshot",
        Some(Payload::EncryptedReasoningReceived(_)) => "encrypted_reasoning_received",
        Some(Payload::CompactionProgress(_)) => "compaction_progress",
        // The enum is non_exhaustive, so an event carrying a payload variant
        // this build does not know stores an empty kind, as does an event with
        // no payload set.
        _ => "",
    }
}
