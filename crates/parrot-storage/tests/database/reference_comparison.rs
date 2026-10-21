use super::{Fixture, active, observe, scenario};
use parrot_storage::database::{Connection, DatabaseError, SessionDatabase};
use serde_json::json;

const EVENT_CONTENT: &str =
    "SELECT sequence,id,agent_session,payload,created_at FROM event ORDER BY sequence";
const LINEAGE: &str = "SELECT agent_session,parent_agent_session,name,event_sequence FROM agent_lineage ORDER BY event_sequence";

#[tokio::test]
async fn historical_wire_payloads_keys_lineage_and_indexes_survive_upgrade_reopen() {
    let fixture = Fixture::new();
    let connection = scenario::legacy(&fixture.0);
    let original = observe::query(&connection, EVENT_CONTENT).unwrap();
    drop(connection);

    for _ in 0..2 {
        let database = SessionDatabase::open(fixture.root()).unwrap();
        let original = original.clone();
        database
            .admit("check migration", &active(), move |tx| {
                assert_eq!(observe::query(tx, EVENT_CONTENT)?, original);
                assert_eq!(
                    observe::query(tx, "SELECT id,request_id,tool_call_id FROM event ORDER BY sequence")?,
                    vec![
                        json!({"id":"agent-start","request_id":null,"tool_call_id":null}),
                        json!({"id":"child-start","request_id":null,"tool_call_id":null}),
                        json!({"id":"child-restart","request_id":null,"tool_call_id":null}),
                        json!({"id":"request","request_id":"request","tool_call_id":null}),
                        json!({"id":"tool","request_id":null,"tool_call_id":"call"}),
                        json!({"id":"text","request_id":null,"tool_call_id":null}),
                    ]
                );
                assert_eq!(
                    observe::query(tx, LINEAGE)?,
                    vec![
                        json!({"agent_session":"agent","parent_agent_session":"","name":"main","event_sequence":1}),
                        json!({"agent_session":"child","parent_agent_session":"agent","name":"worker","event_sequence":2}),
                    ]
                );
                for (name, columns, partial) in [
                    ("event_by_session", ["agent_session", "sequence"], 0),
                    ("event_request", ["agent_session", "request_id"], 1),
                    ("event_request_by_sequence", ["agent_session", "sequence"], 1),
                    ("event_tool_call", ["agent_session", "tool_call_id"], 1),
                ] {
                    let mut query = tx.prepare("SELECT name FROM pragma_index_info(?1) ORDER BY seqno")?;
                    let actual: Vec<String> = query
                        .query_map([name], |row| row.get(0))?
                        .collect::<Result<_, _>>()?;
                    assert_eq!(actual, columns);
                    let actual: i64 = tx.query_row(
                        "SELECT partial FROM pragma_index_list('event') WHERE name=?1",
                        [name],
                        |row| row.get(0),
                    )?;
                    assert_eq!(actual, partial);
                }
                assert!(tx.execute("INSERT INTO event(id,agent_session,payload,created_at) VALUES('request','agent',X'','')", []).is_err());
                assert!(tx.execute("INSERT INTO agent_lineage VALUES('orphan','','',999)", []).is_err());
                assert!(observe::query(tx, "PRAGMA foreign_key_check")?.is_empty());
                assert_eq!(observe::query(tx, "PRAGMA integrity_check")?, vec![json!({"integrity_check":"ok"})]);
                Ok(())
            })
            .unwrap()
            .into_completion()
            .await
            .unwrap();
        database.close().await.unwrap();
        assert_eq!(
            observe::sidecars(&fixture.0.join("session.db")),
            json!({"wal_exists":false,"shm_exists":false})
        );
    }
}

#[tokio::test]
async fn preexisting_keys_and_lineage_are_not_recomputed_on_open_or_reopen() {
    let fixture = Fixture::new();
    let connection = scenario::legacy(&fixture.0);
    scenario::add_usage_columns(&connection);
    scenario::add_lineage_table(&connection);
    scenario::add_event_indexes(&connection);
    connection
        .execute_batch(
            "UPDATE event SET request_id='preserved-request',tool_call_id='preserved-call' WHERE id='request';
             INSERT INTO agent_lineage VALUES('child','preserved-parent','preserved-name',3);",
        )
        .unwrap();
    let events = observe::query(&connection, "SELECT * FROM event ORDER BY sequence").unwrap();
    let lineage = observe::query(&connection, LINEAGE).unwrap();
    drop(connection);

    for _ in 0..2 {
        let database = SessionDatabase::open(fixture.root()).unwrap();
        let events = events.clone();
        let lineage = lineage.clone();
        database
            .admit("check preserved metadata", &active(), move |tx| {
                assert_eq!(observe::query(tx, "SELECT * FROM event ORDER BY sequence")?, events);
                assert_eq!(observe::query(tx, LINEAGE)?, lineage);
                Ok(())
            })
            .unwrap()
            .into_completion()
            .await
            .unwrap();
        database.close().await.unwrap();
    }
}

#[test]
fn corrupt_payload_rolls_back_each_migration_and_remains_a_protobuf_error_on_retry() {
    for with_usage_columns in [false, true] {
        let fixture = Fixture::new();
        let connection = scenario::legacy(&fixture.0);
        if with_usage_columns {
            scenario::add_usage_columns(&connection);
            scenario::add_event_indexes(&connection);
        }
        connection
            .execute(
                "INSERT INTO event(id,agent_session,payload,created_at) VALUES('bad','agent',?1,'')",
                [include_bytes!("fixtures/malformed.pb").as_slice()],
            )
            .unwrap();
        let events = observe::query(&connection, "SELECT * FROM event ORDER BY sequence").unwrap();
        let event_schema = observe::query(
            &connection,
            "SELECT type,name,sql FROM sqlite_master WHERE tbl_name='event' ORDER BY type,name",
        )
        .unwrap();
        drop(connection);

        for _ in 0..2 {
            assert!(matches!(
                SessionDatabase::open(fixture.root()),
                Err(DatabaseError::Protobuf(_))
            ));
            let connection = Connection::open(fixture.0.join("session.db")).unwrap();
            assert_eq!(observe::query(&connection, "SELECT * FROM event ORDER BY sequence").unwrap(), events);
            assert_eq!(
                observe::query(&connection, "SELECT type,name,sql FROM sqlite_master WHERE tbl_name='event' ORDER BY type,name").unwrap(),
                event_schema
            );
            let lineage_tables: i64 = connection
                .query_row(
                    "SELECT count(*) FROM sqlite_master WHERE name='agent_lineage'",
                    [],
                    |row| row.get(0),
                )
                .unwrap();
            assert_eq!(lineage_tables, 0);
        }
    }
}

#[test]
fn partial_usage_schema_preserves_sentinel_and_committed_lineage_despite_index_error() {
    let fixture = Fixture::new();
    let connection = scenario::legacy(&fixture.0);
    connection
        .execute_batch(
            "ALTER TABLE event ADD COLUMN request_id TEXT;
             UPDATE event SET request_id='preserved-sentinel' WHERE id='request';",
        )
        .unwrap();
    let events = observe::query(&connection, "SELECT * FROM event ORDER BY sequence").unwrap();
    drop(connection);

    for _ in 0..2 {
        assert!(matches!(
            SessionDatabase::open(fixture.root()),
            Err(DatabaseError::Sqlite(_))
        ));
        let connection = Connection::open(fixture.0.join("session.db")).unwrap();
        assert_eq!(observe::query(&connection, "SELECT * FROM event ORDER BY sequence").unwrap(), events);
        let tools: i64 = connection
            .query_row(
                "SELECT count(*) FROM pragma_table_info('event') WHERE name='tool_call_id'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(tools, 0);
        assert_eq!(
            observe::query(&connection, LINEAGE).unwrap(),
            vec![
                json!({"agent_session":"agent","parent_agent_session":"","name":"main","event_sequence":1}),
                json!({"agent_session":"child","parent_agent_session":"agent","name":"worker","event_sequence":2}),
            ]
        );
    }
}
