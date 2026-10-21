use super::{Fixture, active};
use parrot_domain::protocol::messages::parrot::{
    AgentStarted, Event, RequestUsageRecorded, ToolExecutionStarted,
};
use parrot_storage::database::{
    Connection, DatabaseError, SessionDatabase, extract_usage_keys, record_agent_start,
};
use protobuf::Message;
use rusqlite::params;

fn legacy(fixture: &Fixture, payloads: &[Event]) -> Vec<Vec<u8>> {
    let connection = Connection::open(fixture.0.join("session.db")).unwrap();
    connection.execute_batch("CREATE TABLE event (sequence INTEGER PRIMARY KEY AUTOINCREMENT, id TEXT NOT NULL UNIQUE, agent_session TEXT NOT NULL, payload BLOB NOT NULL, created_at TEXT NOT NULL);").unwrap();
    payloads
        .iter()
        .map(|event| {
            let bytes = event.write_to_bytes().unwrap();
            connection
                .execute(
                    "INSERT INTO event(id,agent_session,payload,created_at) VALUES(?1,?2,?3,'')",
                    params![event.id, event.agent_session_id, bytes],
                )
                .unwrap();
            bytes
        })
        .collect()
}
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

#[tokio::test]
async fn legacy_usage_keys_first_lineage_and_raw_bytes_survive_upgrade_reopen() {
    let fixture = Fixture::new();
    let mut request = Event {
        id: "request".into(),
        agent_session_id: "agent".into(),
        ..Default::default()
    };
    request.set_request_usage_recorded(RequestUsageRecorded {
        request_id: "request".into(),
        ..Default::default()
    });
    let mut tool = Event {
        id: "tool".into(),
        agent_session_id: "agent".into(),
        ..Default::default()
    };
    tool.set_tool_execution_started(ToolExecutionStarted {
        request_id: "ignored".into(),
        tool_call_id: "call".into(),
        ..Default::default()
    });
    let original = legacy(
        &fixture,
        &[
            start("agent-start", "agent", "", "main"),
            start("child-start", "child", "agent", "worker"),
            start("child-restart", "child", "agent", "renamed"),
            request,
            tool,
            Event {
                id: "text".into(),
                agent_session_id: "agent".into(),
                ..Default::default()
            },
        ],
    );
    for _ in 0..2 {
        let database = SessionDatabase::open(fixture.root()).unwrap();
        let expected = original.clone();
        database.admit("check", &active(), move |tx| {
            let keys: String = tx.query_row("SELECT group_concat(id || ':' || COALESCE(request_id, '-') || ':' || COALESCE(tool_call_id, '-'), ',') FROM event ORDER BY sequence", [], |r| r.get(0))?;
            assert_eq!(keys,"agent-start:-:-,child-start:-:-,child-restart:-:-,request:request:-,tool:-:call,text:-:-");
            let mut query = tx.prepare("SELECT payload FROM event ORDER BY sequence")?;
            let bytes: Vec<Vec<u8>> = query.query_map([], |r| r.get(0))?.collect::<Result<_,_>>()?; assert_eq!(bytes, expected);
            let mut query = tx.prepare("SELECT agent_session,parent_agent_session,name,event_sequence FROM agent_lineage ORDER BY event_sequence")?;
            let lineage: Vec<(String,String,String,i64)> = query.query_map([], |r| Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?)))?.collect::<Result<_,_>>()?;
            assert_eq!(lineage, vec![("agent".into(),"".into(),"main".into(),1),("child".into(),"agent".into(),"worker".into(),2)]);
            Ok(())
        }).unwrap().into_completion().await.unwrap();
        database.close().await.unwrap();
    }
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
fn malformed_payload_rolls_back_added_usage_columns() {
    let fixture = Fixture::new();
    legacy(&fixture, &[]);
    let connection = Connection::open(fixture.0.join("session.db")).unwrap();
    connection
        .execute(
            "INSERT INTO event(id,agent_session,payload,created_at) VALUES('bad','agent',X'FF','')",
            [],
        )
        .unwrap();
    drop(connection);
    assert!(matches!(
        SessionDatabase::open(fixture.root()),
        Err(DatabaseError::Protobuf(_))
    ));
    let connection = Connection::open(fixture.0.join("session.db")).unwrap();
    let columns: i64 = connection.query_row("SELECT count(*) FROM pragma_table_info('event') WHERE name IN ('request_id','tool_call_id')", [], |r| r.get(0)).unwrap();
    assert_eq!(columns, 0);
    let lineage: i64 = connection
        .query_row(
            "SELECT count(*) FROM sqlite_master WHERE name='agent_lineage'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(lineage, 0);
}

#[test]
fn malformed_payload_rolls_back_lineage_table_when_key_columns_already_exist() {
    let fixture = Fixture::new();
    legacy(&fixture, &[]);
    let connection = Connection::open(fixture.0.join("session.db")).unwrap();
    connection.execute_batch("ALTER TABLE event ADD COLUMN request_id TEXT; ALTER TABLE event ADD COLUMN tool_call_id TEXT; INSERT INTO event(id,agent_session,payload,created_at) VALUES('bad','agent',X'FF','');").unwrap();
    drop(connection);
    assert!(matches!(
        SessionDatabase::open(fixture.root()),
        Err(DatabaseError::Protobuf(_))
    ));
    let connection = Connection::open(fixture.0.join("session.db")).unwrap();
    let lineage: i64 = connection
        .query_row(
            "SELECT count(*) FROM sqlite_master WHERE name='agent_lineage'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(lineage, 0);
}

#[tokio::test]
async fn existing_lineage_is_not_rebuilt_and_record_start_ignores_later_start() {
    let fixture = Fixture::new();
    legacy(&fixture, &[start("original", "child", "old-parent", "old")]);
    let database = SessionDatabase::open(fixture.root()).unwrap();
    database
        .admit("late", &active(), |tx| {
            let later = start("later", "child", "new-parent", "new");
            tx.execute(
                "INSERT INTO event(id,agent_session,payload,created_at) VALUES(?1,?2,?3,'')",
                params![later.id, later.agent_session_id, later.write_to_bytes()?],
            )?;
            record_agent_start(tx, tx.last_insert_rowid(), &later)?;
            let name: String = tx.query_row(
                "SELECT name FROM agent_lineage WHERE agent_session='child'",
                [],
                |r| r.get(0),
            )?;
            assert_eq!(name, "old");
            tx.execute("UPDATE agent_lineage SET name='retained'", [])?;
            Ok(())
        })
        .unwrap()
        .into_completion()
        .await
        .unwrap();
    database.close().await.unwrap();
    let reopened = SessionDatabase::open(fixture.root()).unwrap();
    reopened
        .admit("read", &active(), |tx| {
            let name: String = tx.query_row("SELECT name FROM agent_lineage", [], |r| r.get(0))?;
            assert_eq!(name, "retained");
            Ok(())
        })
        .unwrap()
        .into_completion()
        .await
        .unwrap();
    reopened.close().await.unwrap();
}

#[test]
fn request_column_presence_is_source_sentinel_not_silent_partial_schema_repair() {
    let fixture = Fixture::new();
    legacy(&fixture, &[]);
    let connection = Connection::open(fixture.0.join("session.db")).unwrap();
    connection
        .execute_batch("ALTER TABLE event ADD COLUMN request_id TEXT;")
        .unwrap();
    drop(connection);
    let (error, message) = match SessionDatabase::open(fixture.root()) {
        Err(DatabaseError::Sqlite(rusqlite::Error::SqliteFailure(error, Some(message)))) => {
            (error, message)
        }
        Err(DatabaseError::Sqlite(rusqlite::Error::SqlInputError { error, msg, .. })) => {
            (error, msg)
        }
        _ => panic!("partial source schema must fail at the missing tool-call column"),
    };
    assert_eq!(error.extended_code, 1);
    assert_eq!(message, "no such column: tool_call_id");
    let connection = Connection::open(fixture.0.join("session.db")).unwrap();
    let tools: i64 = connection
        .query_row(
            "SELECT count(*) FROM pragma_table_info('event') WHERE name='tool_call_id'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(tools, 0);
}
