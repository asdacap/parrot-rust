use super::Fixture;
use parrot_domain::protocol::messages::parrot::{
    AgentStarted, Event, RequestUsageRecorded, ToolExecutionStarted,
};
use parrot_storage::database::{Connection, extract_usage_keys};
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
fn agent_start_facts_keep_their_original_identity_after_reopen() {
    let fixture = Fixture::new();
    let path = fixture.0.join("session.db");
    let mut connection = Connection::open(&path).unwrap();
    connection
        .execute_batch(include_str!("../../src/database/schema.sql"))
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
    }
    transaction.commit().unwrap();
    drop(connection);

    let reopened = Connection::open(&path).unwrap();
    let mut read = reopened
        .prepare("SELECT payload FROM event ORDER BY sequence")
        .unwrap();
    let starts = read
        .query_map([], |row| row.get::<_, Vec<u8>>(0))
        .unwrap()
        .map(|row| Event::parse_from_bytes(&row.unwrap()).unwrap())
        .collect::<Vec<_>>();
    assert_eq!(
        starts,
        [
            start("original", "child", "old-parent", "old"),
            start("later", "child", "new-parent", "new"),
        ]
    );
}
