//! Literal T17-owned StorageInvariant assertions; no T18/T21 API acceptance.
use super::Fixture;
use parrot_domain::protocol::messages::parrot::{AgentStarted, AgentStatisticsUpdatedEvent, Event};
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
fn statistics(id: &str, agent: &str, value: AgentStatisticsUpdatedEvent) -> Event {
    let mut event = envelope(id, agent);
    event.set_agent_statistics_updated(value);
    event
}
// Identity creation belongs to T21. Bind the source fixture's selected main
// agent directly; this is not an implementation of SessionState/discovery.
async fn seed_main(repository: &EventRepository) -> String {
    repository.transaction(|tx| {
        tx.execute("INSERT INTO session_state (user_session, agent_session, mode) VALUES ('user', 'root', 'build')", [])?;
        Ok(())
    }).await.unwrap();
    "root".into()
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
            [
                "event",
                "message",
                "conversation_item",
                "conversation_part",
                "agent_history",
            ]
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
    assert_eq!(projection_counts(&repository).await, vec![2, 1, 1, 1, 1]);
    repository.database().close().await.unwrap();
}

#[tokio::test]
async fn usage_projection_replaces_agent_totals_and_aggregates_agents() {
    let fixture = Fixture::new();
    let repository = fixture.repository();
    let root = seed_main(&repository).await;
    repository
        .append(
            statistics(
                "root-1",
                &root,
                AgentStatisticsUpdatedEvent {
                    input_tokens: 10,
                    cached_input_tokens: 3,
                    output_tokens: 4,
                    context_size: 8,
                    context_limit: 128,
                    input_cost: 1.0,
                    output_cost: 2.0,
                    ..Default::default()
                },
            ),
            None,
        )
        .await
        .unwrap();
    repository
        .append(
            statistics(
                "child-1",
                "child",
                AgentStatisticsUpdatedEvent {
                    input_tokens: 7,
                    cached_input_tokens: 2,
                    output_tokens: 5,
                    context_size: 99,
                    context_limit: 999,
                    input_cost: 0.5,
                    output_cost: 0.25,
                    ..Default::default()
                },
            ),
            None,
        )
        .await
        .unwrap();
    let usage = repository
        .append(
            statistics(
                "root-2",
                &root,
                AgentStatisticsUpdatedEvent {
                    input_tokens: 15,
                    cached_input_tokens: 5,
                    output_tokens: 6,
                    context_size: 9,
                    context_limit: 128,
                    input_cost: 1.5,
                    output_cost: 2.5,
                    ..Default::default()
                },
            ),
            None,
        )
        .await
        .unwrap()
        .usage
        .expect("statistics did not project usage");
    let revision = usage.revision;
    // source_assertion: Usage_projection_replaces_agent_totals_and_aggregates_agents:134
    assert_eq!(usage.input_tokens, 22);
    // source_assertion: Usage_projection_replaces_agent_totals_and_aggregates_agents:135
    assert_eq!(usage.cached_input_tokens, 7);
    // source_assertion: Usage_projection_replaces_agent_totals_and_aggregates_agents:136
    assert_eq!(usage.output_tokens, 11);
    // source_assertion: Usage_projection_replaces_agent_totals_and_aggregates_agents:137
    assert_eq!(usage.context_size, 9);
    // source_assertion: Usage_projection_replaces_agent_totals_and_aggregates_agents:138
    assert_eq!(usage.context_limit, 128);
    // source_assertion: Usage_projection_replaces_agent_totals_and_aggregates_agents:139
    assert_eq!(usage.input_cost, 2.0);
    // source_assertion: Usage_projection_replaces_agent_totals_and_aggregates_agents:140
    assert_eq!(usage.output_cost, 2.75);
    repository.database().close().await.unwrap();
    drop(repository);
    let reopened = fixture.repository();
    let restored = reopened.usage().await.unwrap();
    // source_assertion: Usage_projection_replaces_agent_totals_and_aggregates_agents:145
    assert_eq!(restored.revision, revision);
    // source_assertion: Usage_projection_replaces_agent_totals_and_aggregates_agents:146
    assert_eq!(restored.input_tokens, 22);
    // source_assertion: Usage_projection_replaces_agent_totals_and_aggregates_agents:147
    assert_eq!(restored.output_tokens, 11);
    assert_eq!(restored, usage);
    reopened.database().close().await.unwrap();
}

#[tokio::test]
async fn usage_projection_backfills_latest_legacy_statistics_once() {
    let fixture = Fixture::new();
    let repository = fixture.repository();
    let root = seed_main(&repository).await;
    repository
        .append(
            statistics(
                "old-root",
                &root,
                AgentStatisticsUpdatedEvent {
                    input_tokens: 4,
                    cached_input_tokens: 1,
                    output_tokens: 2,
                    context_size: 3,
                    context_limit: 100,
                    input_cost: 0.4,
                    output_cost: 0.2,
                    ..Default::default()
                },
            ),
            None,
        )
        .await
        .unwrap();
    repository
        .append(
            statistics(
                "new-root",
                &root,
                AgentStatisticsUpdatedEvent {
                    input_tokens: 9,
                    cached_input_tokens: 2,
                    output_tokens: 5,
                    context_size: 6,
                    context_limit: 100,
                    input_cost: 0.9,
                    output_cost: 0.5,
                    ..Default::default()
                },
            ),
            None,
        )
        .await
        .unwrap();
    repository
        .append(
            statistics(
                "child",
                "child",
                AgentStatisticsUpdatedEvent {
                    input_tokens: 3,
                    cached_input_tokens: 1,
                    output_tokens: 1,
                    context_size: 80,
                    context_limit: 200,
                    input_cost: 0.3,
                    output_cost: 0.1,
                    ..Default::default()
                },
            ),
            None,
        )
        .await
        .unwrap();
    repository
        .transaction(|tx| {
            tx.execute_batch("DELETE FROM agent_usage; DELETE FROM projection_version;")?;
            Ok(())
        })
        .await
        .unwrap();
    let backfilled = EventRepository::new(repository.database().clone())
        .usage()
        .await
        .unwrap();
    // source_assertion: Usage_projection_backfills_latest_legacy_statistics_once:218
    assert_eq!(backfilled.input_tokens, 12);
    // source_assertion: Usage_projection_backfills_latest_legacy_statistics_once:219
    assert_eq!(backfilled.cached_input_tokens, 3);
    // source_assertion: Usage_projection_backfills_latest_legacy_statistics_once:220
    assert_eq!(backfilled.output_tokens, 6);
    // source_assertion: Usage_projection_backfills_latest_legacy_statistics_once:221
    assert_eq!(backfilled.context_size, 6);
    // source_assertion: Usage_projection_backfills_latest_legacy_statistics_once:222
    assert_eq!(backfilled.context_limit, 100);
    // source_assertion: Usage_projection_backfills_latest_legacy_statistics_once:223
    assert_eq!(backfilled.input_cost, 1.2);
    // source_assertion: Usage_projection_backfills_latest_legacy_statistics_once:224
    assert_eq!(backfilled.output_cost, 0.6);
    let second = EventRepository::new(repository.database().clone())
        .usage()
        .await
        .unwrap();
    // source_assertion: Usage_projection_backfills_latest_legacy_statistics_once:227
    assert_eq!(second, backfilled);
    repository.database().close().await.unwrap();
    drop(repository);
    let reopened = fixture.repository();
    assert_eq!(reopened.usage().await.unwrap(), backfilled);
    reopened.database().close().await.unwrap();
}

#[tokio::test]
async fn failed_event_insert_does_not_change_usage_projection() {
    let fixture = Fixture::new();
    let repository = fixture.repository();
    let root = seed_main(&repository).await;
    repository
        .append(
            statistics(
                "same",
                &root,
                AgentStatisticsUpdatedEvent {
                    input_tokens: 4,
                    cached_input_tokens: 1,
                    output_tokens: 2,
                    context_size: 3,
                    context_limit: 100,
                    input_cost: 0.4,
                    output_cost: 0.2,
                    ..Default::default()
                },
            ),
            None,
        )
        .await
        .unwrap();
    let before = repository.usage().await.unwrap();
    let before_raw = repository.replay_raw().await.unwrap();
    let error = repository
        .append(
            statistics(
                "same",
                &root,
                AgentStatisticsUpdatedEvent {
                    input_tokens: 40,
                    cached_input_tokens: 10,
                    output_tokens: 20,
                    context_size: 30,
                    context_limit: 100,
                    input_cost: 4.0,
                    output_cost: 2.0,
                    ..Default::default()
                },
            ),
            None,
        )
        .await
        .unwrap_err();
    // source_assertion: Failed_event_insert_does_not_change_usage_projection:253
    assert!(sqlite_constraint(error, 2067).contains("event.id"));
    let usage = repository.usage().await.unwrap();
    // source_assertion: Failed_event_insert_does_not_change_usage_projection:273
    assert_eq!(usage.input_tokens, 4);
    // source_assertion: Failed_event_insert_does_not_change_usage_projection:274
    assert_eq!(usage.output_tokens, 2);
    assert_eq!(usage, before);
    assert_eq!(repository.replay_raw().await.unwrap(), before_raw);
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
    assert_eq!(projection_counts(&reopened).await, vec![1, 1, 1, 1, 1]);
    reopened.database().close().await.unwrap();
}

#[tokio::test]
async fn opening_legacy_database_keeps_original_usage_identity_and_first_lineage() {
    let fixture = Fixture::new();
    // Independent .NET-generated exact six-event/five-column source fixture.
    std::fs::write(
        fixture.0.join("session.db"),
        include_bytes!("../database/fixtures/legacy-canonical-before.sqlite"),
    )
    .unwrap();
    let original = {
        let connection = rusqlite::Connection::open(fixture.0.join("session.db")).unwrap();
        let mut read = connection
            .prepare("SELECT id,payload FROM event ORDER BY sequence")
            .unwrap();
        read.query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, Vec<u8>>(1)?))
        })
        .unwrap()
        .collect::<Result<Vec<_>, _>>()
        .unwrap()
    };
    let repository = fixture.repository();
    let raw = repository.replay_raw().await.unwrap();
    assert_eq!(
        raw.iter()
            .map(|row| (row.event.id.clone(), row.raw.clone()))
            .collect::<Vec<_>>(),
        original
    );
    let mut duplicate = raw
        .iter()
        .find(|row| row.event.id == "request")
        .unwrap()
        .event
        .clone();
    duplicate.id = "duplicate-request".into();
    // source_assertion: Opening_a_legacy_database_migrates_usage_keys_and_agent_lineage:393
    assert!(
        matches!(repository.append_usage_fact(duplicate).await, Err(EventError::Invalid(message)) if message == "A request or tool execution already has a usage fact with another event identity.")
    );
    // source_assertion: Opening_a_legacy_database_migrates_usage_keys_and_agent_lineage:394
    assert_eq!(
        repository
            .find_request_usage("agent".into(), "call".into())
            .await
            .unwrap()
            .map(|request| request.request_id),
        Some("request".into())
    );
    let mut late = envelope("late-start", "late");
    late.set_agent_started(AgentStarted {
        parent_agent_session_id: "child".into(),
        name: "late".into(),
        ..Default::default()
    });
    repository.append(late, None).await.unwrap();
    let lineage = repository
        .agent_lineage()
        .await
        .unwrap()
        .into_iter()
        .map(|record| {
            format!(
                "{}<{}:{}",
                record.agent_session_id, record.parent_agent_session_id, record.name
            )
        })
        .collect::<Vec<_>>()
        .join(",");
    // source_assertion: Opening_a_legacy_database_migrates_usage_keys_and_agent_lineage:400
    assert_eq!(lineage, "agent<:main,child<agent:worker,late<child:late");
    let before = repository.replay_raw().await.unwrap();
    repository.database().close().await.unwrap();
    drop(repository);
    let reopened = fixture.repository();
    assert_eq!(reopened.replay_raw().await.unwrap(), before);
    assert_eq!(
        reopened
            .find_request_usage("agent".into(), "call".into())
            .await
            .unwrap()
            .unwrap()
            .request_id,
        "request"
    );
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
                "SELECT COUNT(*) FROM message WHERE content = ?1",
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
