//! Literal T17-owned StorageInvariant assertions; no T18/T21 API acceptance.
use super::Fixture;
use parrot_domain::protocol::messages::parrot::Event;
use parrot_storage::database::DatabaseError;
use parrot_storage::events::{ConversationMessage, EventError, EventRepository, MessageRole};
use rusqlite::params;

fn envelope(id: &str, agent: &str) -> Event {
    Event {
        id: id.into(),
        agent_session_id: agent.into(),
        ..Default::default()
    }
}
fn sqlite_constraint(error: EventError, extended: i32) -> String {
    match error {
        EventError::Database(DatabaseError::Sqlite(rusqlite::Error::SqliteFailure(
            code,
            message,
        ))) => {
            assert_eq!(code.code, rusqlite::ErrorCode::ConstraintViolation);
            assert_eq!(code.extended_code, extended);
            message.expect("SQLite constraint diagnostic")
        }
        other => panic!("expected actual SQLite constraint failure, got {other:?}"),
    }
}
async fn projection_counts(repository: &EventRepository) -> Vec<i64> {
    repository
        .transaction(|tx| {
            ["event", "conversation_item", "conversation_part"]
                .into_iter()
                .map(|table| {
                    Ok(
                        tx.query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |row| {
                            row.get(0)
                        })?,
                    )
                })
                .collect()
        })
        .await
        .unwrap()
}

#[tokio::test]
async fn an_event_and_its_projection_commit_together() {
    let fixture = Fixture::new();
    let repository = fixture.repository();
    repository
        .append(
            envelope("e1", "agent"),
            Some(ConversationMessage::text(MessageRole::User, "the prompt")),
        )
        .await
        .unwrap();
    repository
        .append(envelope("e2", "agent"), None)
        .await
        .unwrap();
    // source_assertion: An_event_and_its_projection_commit_together:60
    assert_eq!(repository.replay().await.unwrap().len(), 2);
    // Concrete projection observations, not a replacement T18 Messages API.
    assert_eq!(projection_counts(&repository).await, vec![2, 1, 1]);
    repository.database().close().await.unwrap();
}

#[tokio::test]
async fn events_survive_reopening_the_database() {
    let fixture = Fixture::new();
    let repository = fixture.repository();
    repository
        .append(
            envelope("kept", "agent"),
            Some(ConversationMessage::text(MessageRole::User, "durable")),
        )
        .await
        .unwrap();
    let before = repository.replay_raw().await.unwrap();
    repository.database().close().await.unwrap();
    drop(repository);
    let reopened = fixture.repository();
    // source_assertion: Events_survive_reopening_the_database:292
    assert_eq!(reopened.replay().await.unwrap()[0].id, "kept");
    assert_eq!(reopened.replay_raw().await.unwrap(), before);
    assert_eq!(projection_counts(&reopened).await, vec![1, 1, 1]);
    reopened.database().close().await.unwrap();
}

#[tokio::test]
async fn ordinary_append_projection_trigger_rolls_back_event_and_all_partial_projection_rows() {
    let fixture = Fixture::new();
    let repository = fixture.repository();
    repository
        .append(
            envelope("existing", "agent"),
            Some(ConversationMessage::text(MessageRole::User, "kept")),
        )
        .await
        .unwrap();
    repository.transaction(|tx| { tx.execute_batch("CREATE TRIGGER reject_part BEFORE INSERT ON conversation_part BEGIN SELECT RAISE(ABORT, 'projection rejected'); END;")?; Ok(()) }).await.unwrap();
    let before_raw = repository.replay_raw().await.unwrap();
    let before_counts = projection_counts(&repository).await;
    let error = repository
        .append(
            envelope("failed", "agent"),
            Some(ConversationMessage::text(
                MessageRole::Assistant,
                "must roll back",
            )),
        )
        .await
        .unwrap_err();
    assert_eq!(sqlite_constraint(error, 1811), "projection rejected");
    assert_eq!(repository.replay_raw().await.unwrap(), before_raw);
    assert_eq!(projection_counts(&repository).await, before_counts);
    repository
        .transaction(|tx| {
            let missing: i64 = tx.query_row(
                "SELECT COUNT(*) FROM conversation_part WHERE kind = 'text' AND text = ?1",
                params!["must roll back"],
                |row| row.get(0),
            )?;
            assert_eq!(missing, 0);
            tx.execute_batch("DROP TRIGGER reject_part;")?;
            Ok(())
        })
        .await
        .unwrap();
    repository
        .append(
            envelope("retry", "agent"),
            Some(ConversationMessage::text(
                MessageRole::Assistant,
                "committed",
            )),
        )
        .await
        .unwrap();
    repository.database().close().await.unwrap();
}
