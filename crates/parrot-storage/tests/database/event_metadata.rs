use super::Fixture;
use parrot_domain::protocol::messages::parrot::{
    AgentStarted, Event, RequestUsageRecorded, ToolExecutionStarted,
};
use parrot_storage::database::{Connection, extract_usage_keys, record_agent_start};
use protobuf::Message;
use rusqlite::params;

fn start(id: &str, agent: &str, parent: &str, name: &str) -> Event {
    let mut event = Event {
        id: id.into(),
        agent_session_id: agent.into(),
        ..Default::default()
    };
    event.set_agent_started(AgentStarted {
        name: name.into(),
        parent_agent_session_id: parent.into(),
        ..Default::default()
    });
    event
}

#[test]
fn key_presence_is_oneof_specific_including_empty_keys() {
    let mut event = Event::new();
    assert_eq!(extract_usage_keys(&event), (None, None));
    event.set_request_usage_recorded(RequestUsageRecorded::new());
    assert_eq!(extract_usage_keys(&event), (Some(""), None));
    event.set_tool_execution_started(ToolExecutionStarted {
        request_id: "not-a-request-fact".into(),
        ..Default::default()
    });
    assert_eq!(extract_usage_keys(&event), (None, Some("")));
    event.set_agent_started(AgentStarted::new());
    assert_eq!(extract_usage_keys(&event), (None, None));
}

#[test]
fn first_agent_start_fixes_lineage_during_writes_and_survives_reopen() {
    let fixture = Fixture::new();
    let path = fixture.0.join("session.db");
    let mut connection = Connection::open(&path).unwrap();
    connection
        .execute_batch(include_str!("../../src/database/schema.sql"))
        .unwrap();
    connection
        .execute_batch(include_str!("../../src/database/lineage.sql"))
        .unwrap();
    let transaction = connection.transaction().unwrap();
    for event in [
        start("original", "child", "old-parent", "old"),
        start("later", "child", "new-parent", "new"),
    ] {
        transaction
            .execute(
                "INSERT INTO event(id,agent_session,payload,created_at) VALUES(?1,?2,?3,'')",
                params![
                    event.id,
                    event.agent_session_id,
                    event.write_to_bytes().unwrap()
                ],
            )
            .unwrap();
        record_agent_start(&transaction, transaction.last_insert_rowid(), &event).unwrap();
    }
    transaction.commit().unwrap();
    drop(connection);

    let reopened = Connection::open(&path).unwrap();
    let lineage: (String, String, String, i64) = reopened
        .query_row(
            "SELECT agent_session,parent_agent_session,name,event_sequence FROM agent_lineage",
            [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
        )
        .unwrap();
    assert_eq!(
        lineage,
        ("child".into(), "old-parent".into(), "old".into(), 1)
    );
}
