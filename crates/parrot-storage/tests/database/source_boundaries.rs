//! Low-level SQL counterparts: repository projection policy remains T17-owned.
use super::{Fixture, active};
use parrot_domain::protocol::messages::parrot::Event;
use parrot_storage::database::SessionDatabase;
use protobuf::Message;
use rusqlite::params;

#[tokio::test]
async fn fifty_event_message_transactions_leave_truncate_without_wal_or_shm() {
    let fixture = Fixture::new();
    let database = SessionDatabase::open(fixture.root()).unwrap();
    for index in 0..50 {
        database.admit(index,&active(),move|tx| {
            let event=Event{id:index.to_string(),agent_session_id:"agent".into(),..Default::default()};
            tx.execute("INSERT INTO event(id,agent_session,payload,created_at) VALUES(?1,'agent',?2,'')",params![event.id,event.write_to_bytes()?])?;
            tx.execute("INSERT INTO message(agent_session,role,content,created_at) VALUES('agent','user','hello','')",[])?;Ok(())
        }).unwrap().into_completion().await.unwrap();
        assert!(!fixture.0.join("session.db-wal").exists());
        assert!(!fixture.0.join("session.db-shm").exists());
    }
    let mode = database
        .admit("mode", &active(), |tx| {
            Ok(tx.query_row("PRAGMA journal_mode", [], |r| r.get::<_, String>(0))?)
        })
        .unwrap()
        .into_completion()
        .await
        .unwrap()
        .into_value()
        .into_output();
    assert_eq!(mode, "truncate");
    database.close().await.unwrap();
    assert!(!fixture.0.join("session.db-wal").exists());
    assert!(!fixture.0.join("session.db-shm").exists());
}

#[tokio::test]
async fn sql_event_and_message_are_durable_together_without_claiming_repository_policy() {
    let fixture = Fixture::new();
    let database = SessionDatabase::open(fixture.root()).unwrap();
    let original = Event {
        id: "kept".into(),
        agent_session_id: "agent".into(),
        ..Default::default()
    }
    .write_to_bytes()
    .unwrap();
    let payload = original.clone();
    database.admit("durable",&active(),move|tx| {
        tx.execute("INSERT INTO event(id,agent_session,payload,created_at) VALUES('kept','agent',?1,'')",[payload])?;
        tx.execute("INSERT INTO message(agent_session,role,content,created_at) VALUES('agent','user','durable','')",[])?;Ok(())
    }).unwrap().into_completion().await.unwrap();
    database.close().await.unwrap();
    let reopened = SessionDatabase::open(fixture.root()).unwrap();
    let stored = reopened
        .admit("read", &active(), |tx| {
            let payload: Vec<u8> =
                tx.query_row("SELECT payload FROM event WHERE id='kept'", [], |r| {
                    r.get(0)
                })?;
            let text: String = tx.query_row("SELECT content FROM message", [], |r| r.get(0))?;
            assert_eq!(text, "durable");
            Ok(payload)
        })
        .unwrap()
        .into_completion()
        .await
        .unwrap()
        .into_value()
        .into_output();
    assert_eq!(stored, original);
    assert_eq!(Event::parse_from_bytes(&stored).unwrap().id, "kept");
    reopened.close().await.unwrap();
}
