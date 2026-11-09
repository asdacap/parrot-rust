//! Historical schemas are explicit SQL, independent of the current schema creator.
use parrot_storage::database::Connection;
use rusqlite::params;
use std::path::Path;

pub fn legacy(root: &Path) -> Connection {
    let connection = Connection::open(root.join("session.db")).unwrap();
    connection
        .execute_batch(
            "CREATE TABLE event (
                sequence INTEGER PRIMARY KEY AUTOINCREMENT,
                id TEXT NOT NULL UNIQUE,
                agent_session TEXT NOT NULL,
                payload BLOB NOT NULL,
                created_at TEXT NOT NULL
            );",
        )
        .unwrap();
    let events: &[(&str, &str, &[u8])] = &[
        (
            "agent-start",
            "agent",
            include_bytes!("fixtures/agent-start.pb"),
        ),
        (
            "child-start",
            "child",
            include_bytes!("fixtures/child-start.pb"),
        ),
        (
            "child-restart",
            "child",
            include_bytes!("fixtures/child-restart.pb"),
        ),
        ("request", "agent", include_bytes!("fixtures/request.pb")),
        ("tool", "agent", include_bytes!("fixtures/tool.pb")),
        ("text", "agent", include_bytes!("fixtures/text.pb")),
    ];
    for (id, agent, payload) in events {
        connection
            .execute(
                "INSERT INTO event(id,agent_session,payload,created_at) VALUES(?1,?2,?3,'')",
                params![id, agent, payload],
            )
            .unwrap();
    }
    connection
}

pub fn add_usage_columns(connection: &Connection) {
    connection
        .execute_batch(
            "ALTER TABLE event ADD COLUMN request_id TEXT;
             ALTER TABLE event ADD COLUMN tool_call_id TEXT;",
        )
        .unwrap();
}

pub fn add_lineage_table(connection: &Connection) {
    connection
        .execute_batch(
            "CREATE TABLE agent_lineage (
                agent_session TEXT PRIMARY KEY,
                parent_agent_session TEXT NOT NULL,
                name TEXT NOT NULL,
                event_sequence INTEGER NOT NULL REFERENCES event(sequence)
            );",
        )
        .unwrap();
}

pub fn add_event_indexes(connection: &Connection) {
    connection
        .execute_batch(
            "CREATE INDEX event_by_session ON event(agent_session, sequence);
             CREATE INDEX event_request ON event(agent_session, request_id) WHERE request_id IS NOT NULL;
             CREATE INDEX event_request_by_sequence ON event(agent_session, sequence) WHERE request_id IS NOT NULL;
             CREATE INDEX event_tool_call ON event(agent_session, tool_call_id) WHERE tool_call_id IS NOT NULL;",
        )
        .unwrap();
}
