use super::{Fixture, active};
use parrot_storage::database::{Connection, DatabaseError, SessionDatabase};
use rusqlite::OpenFlags;

#[test]
fn legacy_fixture_is_rejected_without_mutating_its_database_or_journal_mode() {
    let fixture = Fixture::new();
    let path = fixture.0.join("session.db");
    std::fs::write(
        &path,
        include_bytes!("fixtures/legacy-canonical-before.sqlite"),
    )
    .unwrap();
    let before = std::fs::read(&path).unwrap();
    let connection = Connection::open_with_flags(&path, OpenFlags::SQLITE_OPEN_READ_ONLY).unwrap();
    let mode: String = connection
        .query_row("PRAGMA journal_mode", [], |row| row.get(0))
        .unwrap();
    drop(connection);

    assert!(matches!(
        SessionDatabase::open(fixture.root()),
        Err(DatabaseError::UnsupportedSchema { version: 0 })
    ));
    assert_eq!(std::fs::read(&path).unwrap(), before);
    let connection = Connection::open_with_flags(&path, OpenFlags::SQLITE_OPEN_READ_ONLY).unwrap();
    let after: String = connection
        .query_row("PRAGMA journal_mode", [], |row| row.get(0))
        .unwrap();
    assert_eq!(after, mode);
    assert!(!fixture.0.join("session.db-journal").exists());
    assert!(!fixture.0.join("session.db-wal").exists());
    assert!(!fixture.0.join("session.db-shm").exists());
}

#[test]
fn unversioned_tables_and_views_are_rejected_without_writes() {
    for sql in [
        "CREATE TABLE retained(value TEXT); INSERT INTO retained VALUES ('kept');",
        "CREATE VIEW retained AS SELECT 'kept' AS value;",
        "CREATE TABLE sqlitefoo(value TEXT);",
    ] {
        let fixture = Fixture::new();
        let path = fixture.0.join("session.db");
        let connection = Connection::open(&path).unwrap();
        connection.execute_batch(sql).unwrap();
        drop(connection);
        let before = std::fs::read(&path).unwrap();

        assert!(matches!(
            SessionDatabase::open(fixture.root()),
            Err(DatabaseError::UnsupportedSchema { version: 0 })
        ));
        assert_eq!(std::fs::read(&path).unwrap(), before);
        assert!(!fixture.0.join("session.db-journal").exists());
    }
}

#[test]
fn unknown_versions_are_rejected_without_writes() {
    for version in [1, 2, 3, 4, 6, 7] {
        let fixture = Fixture::new();
        let path = fixture.0.join("session.db");
        let connection = Connection::open(&path).unwrap();
        connection
            .execute_batch(
                "CREATE TABLE retained(value TEXT); INSERT INTO retained VALUES ('kept');",
            )
            .unwrap();
        connection
            .pragma_update(None, "user_version", version)
            .unwrap();
        drop(connection);
        let before = std::fs::read(&path).unwrap();

        assert!(matches!(
            SessionDatabase::open(fixture.root()),
            Err(DatabaseError::UnsupportedSchema { version: found }) if found == version
        ));
        assert_eq!(std::fs::read(&path).unwrap(), before);
        assert!(!fixture.0.join("session.db-journal").exists());
    }
}

#[tokio::test]
async fn empty_unversioned_file_initializes_the_current_format() {
    let fixture = Fixture::new();
    std::fs::write(fixture.0.join("session.db"), b"").unwrap();
    let database = SessionDatabase::open(fixture.root()).unwrap();
    let state = database
        .admit("empty-file", &active(), |tx| {
            let version: i64 = tx.query_row("PRAGMA user_version", [], |row| row.get(0))?;
            let tables: i64 = tx.query_row("SELECT count(*) FROM sqlite_schema WHERE type='table' AND name NOT GLOB 'sqlite_*'", [], |row| row.get(0))?;
            Ok((version,tables))
        })
        .unwrap()
        .into_completion()
        .await
        .unwrap()
        .into_value()
        .into_output();
    assert_eq!(state, (5, 9));
    database.close().await.unwrap();
}

#[tokio::test]
async fn current_format_reopens_without_schema_repair() {
    let fixture = Fixture::new();
    let database = SessionDatabase::open(fixture.root()).unwrap();
    database.close().await.unwrap();
    let connection = Connection::open(fixture.0.join("session.db")).unwrap();
    connection
        .execute_batch("DROP INDEX conversation_owned_result;")
        .unwrap();
    drop(connection);

    let reopened = SessionDatabase::open(fixture.root()).unwrap();
    let state = reopened
        .admit("reopen", &active(), |tx| {
            let version: i64 = tx.query_row("PRAGMA user_version", [], |row| row.get(0))?;
            let tables: i64 = tx.query_row("SELECT count(*) FROM sqlite_schema WHERE type='table' AND name NOT GLOB 'sqlite_*'", [], |row| row.get(0))?;
            let index: bool = tx.query_row("SELECT EXISTS (SELECT 1 FROM sqlite_schema WHERE name='conversation_owned_result')", [], |row| row.get(0))?;
            Ok((version, tables, index))
        })
        .unwrap()
        .into_completion()
        .await
        .unwrap()
        .into_value()
        .into_output();
    assert_eq!(state, (5, 9, false));
    reopened.close().await.unwrap();
}

#[tokio::test]
async fn owned_messages_enforce_uniqueness_and_survive_owner_deletion() {
    let fixture = Fixture::new();
    let database = SessionDatabase::open(fixture.root()).unwrap();
    database.admit("ownership", &active(), |tx| {
        tx.execute_batch(
            "INSERT INTO conversation_item(sequence,agent_session,origin,role,tool_call_id,created_at,tool_owner_sequence) VALUES
             (1,'agent','model','assistant','','at',NULL),
             (2,'agent','model','assistant','','at',NULL),
             (3,'agent','tool','tool','call','at',1),
             (4,'agent','tool','user','','at',1);
             INSERT INTO conversation_part (item_sequence,position,kind,text,artifact_id,media_type,display_name) VALUES(3,0,'text','kept','','','');"
        )?;
        assert!(tx.execute("INSERT INTO conversation_item(agent_session,origin,role,tool_call_id,created_at,tool_owner_sequence) VALUES('agent','tool','tool','call','at',2)", []).is_err());
        assert!(tx.execute("INSERT INTO conversation_item(agent_session,origin,role,tool_call_id,created_at,tool_owner_sequence) VALUES('agent','tool','user','','at',1)", []).is_err());
        assert!(tx.execute("INSERT INTO conversation_item(agent_session,origin,role,tool_call_id,created_at,tool_owner_sequence) VALUES('agent','model','assistant','','at',2)", []).is_err());
        assert!(tx.execute("INSERT INTO conversation_item(sequence,agent_session,origin,role,tool_call_id,created_at,tool_owner_sequence) VALUES(99,'agent','tool','tool','self','at',99)", []).is_err());
        assert!(tx.execute("INSERT INTO conversation_item(agent_session,origin,role,tool_call_id,created_at,tool_owner_sequence) VALUES('agent','tool','tool','orphan','at',999)", []).is_err());
        tx.execute("DELETE FROM conversation_item WHERE sequence=1", [])?;
        let mut read = tx.prepare("SELECT sequence,tool_owner_sequence FROM conversation_item WHERE sequence IN (3,4) ORDER BY sequence")?;
        let children = read.query_map([], |row| Ok((row.get::<_, i64>(0)?,row.get::<_, Option<i64>>(1)?)))?.collect::<Result<Vec<_>, _>>()?;
        assert_eq!(children, [(3,None),(4,None)]);
        let text: String = tx.query_row("SELECT text FROM conversation_part WHERE item_sequence=3", [], |row| row.get(0))?;
        assert_eq!(text, "kept");
        Ok(())
    }).unwrap().into_completion().await.unwrap();
    database.close().await.unwrap();
}

#[tokio::test]
async fn compaction_status_is_unique_and_clears_without_deleting_history() {
    let fixture = Fixture::new();
    let database = SessionDatabase::open(fixture.root()).unwrap();
    database.admit("compaction", &active(), |tx| {
        tx.execute_batch(
            "INSERT INTO conversation_item(sequence,agent_session,origin,role,tool_call_id,created_at) VALUES
             (1,'agent','system','system','','at'),(2,'agent','system','system','','at');
             INSERT INTO agent_history(agent_session,kind,conversation_sequence,summary,watermark,created_at,status_conversation_sequence) VALUES
             ('agent','compaction',NULL,'first',10,'at',1);"
        )?;
        assert!(tx.execute("INSERT INTO agent_history(agent_session,kind,conversation_sequence,summary,watermark,created_at,status_conversation_sequence) VALUES('agent','compaction',NULL,'duplicate',10,'at',2)", []).is_err());
        assert!(tx.execute("INSERT INTO agent_history(agent_session,kind,conversation_sequence,summary,watermark,created_at,status_conversation_sequence) VALUES('agent','compaction',NULL,'duplicate-status',11,'at',1)", []).is_err());
        assert!(tx.execute("INSERT INTO agent_history(agent_session,kind,conversation_sequence,summary,watermark,created_at,status_conversation_sequence) VALUES('agent','message',2,'',0,'at',1)", []).is_err());
        tx.execute("INSERT INTO agent_history(agent_session,kind,conversation_sequence,summary,watermark,created_at,status_conversation_sequence) VALUES('agent','compaction',NULL,'later',5,'at',2)", [])?;
        let current: (String,i64) = tx.query_row("SELECT summary,watermark FROM agent_history WHERE agent_session='agent' AND kind='compaction' ORDER BY sequence DESC LIMIT 1", [], |row| Ok((row.get(0)?,row.get(1)?)))?;
        assert_eq!(current, ("later".into(),5));
        tx.execute("DELETE FROM conversation_item WHERE sequence=1", [])?;
        let first: (String,Option<i64>) = tx.query_row("SELECT summary,status_conversation_sequence FROM agent_history WHERE watermark=10", [], |row| Ok((row.get(0)?,row.get(1)?)))?;
        assert_eq!(first, ("first".into(),None));
        let count: i64 = tx.query_row("SELECT count(*) FROM agent_history", [], |row| row.get(0))?;
        assert_eq!(count,2);
        Ok(())
    }).unwrap().into_completion().await.unwrap();
    database.close().await.unwrap();
}

#[tokio::test]
async fn mode_revisions_and_history_anchor_constraints_match_the_current_contract() {
    let fixture = Fixture::new();
    let database = SessionDatabase::open(fixture.root()).unwrap();
    database.admit("revisions", &active(), |tx| {
        tx.execute("INSERT INTO conversation_item(agent_session,origin,role,tool_call_id,created_at,mode_status_revision) VALUES('agent','system','system','','at',0)", [])?;
        let revision: Option<i64> = tx.query_row("SELECT mode_status_revision FROM conversation_item", [], |row| row.get(0))?;
        assert_eq!(revision,Some(0));
        assert!(tx.execute("INSERT INTO conversation_item(agent_session,origin,role,tool_call_id,created_at,mode_status_revision) VALUES('agent','system','system','','at',0)", []).is_err());
        assert!(tx.execute("UPDATE conversation_item SET mode_status_revision=-1", []).is_err());
        tx.execute("INSERT INTO conversation_item(agent_session,origin,role,tool_call_id,created_at) VALUES('agent','system','system','','at'),('agent','system','system','','at')", [])?;
        tx.execute("INSERT INTO event(id,agent_session,payload,created_at,history_anchor) VALUES('anchored','agent',X'','at',42)", [])?;
        let anchor: i64 = tx.query_row("SELECT history_anchor FROM event WHERE id='anchored'", [], |row| row.get(0))?;
        assert_eq!(anchor,42);
        assert!(tx.execute("UPDATE event SET history_anchor=-1", []).is_err());
        Ok(())
    }).unwrap().into_completion().await.unwrap();
    database.close().await.unwrap();
}
