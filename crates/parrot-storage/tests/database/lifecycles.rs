use super::{Fixture, active};
use parrot_domain::protocol::messages::parrot::Event;
use parrot_storage::conversation::{CompactionSnapshot, ConversationRepository};
use parrot_storage::database::SessionDatabase;
use parrot_storage::events::{ConversationMessage, EventRepository, MessageRole};

#[tokio::test]
async fn repository_workflows_keep_current_schema_after_reopen() {
    let fixture = Fixture::new();
    let repository = ConversationRepository::new(EventRepository::new(
        SessionDatabase::open(fixture.root()).unwrap(),
    ));
    repository
        .events()
        .append(
            Event {
                id: "prompt".into(),
                agent_session_id: "agent".into(),
                ..Default::default()
            },
            Some(ConversationMessage::text(MessageRole::User, "the prompt")),
        )
        .await
        .unwrap();
    repository
        .save_compaction(
            "agent".into(),
            CompactionSnapshot {
                summary: "the summary".into(),
                watermark: 0,
            },
        )
        .await
        .unwrap();
    repository.agent_history("agent".into()).await.unwrap();
    repository.events().database().close().await.unwrap();

    let reopened = SessionDatabase::open(fixture.root()).unwrap();
    let (version, tables) = reopened
        .admit("schema-after-workflows", &active(), |tx| {
            let version: i64 = tx.query_row("PRAGMA user_version", [], |row| row.get(0))?;
            let mut read = tx.prepare("SELECT name FROM sqlite_schema WHERE type = 'table' AND name NOT GLOB 'sqlite_*' ORDER BY name")?;
            let tables = read
                .query_map([], |row| row.get::<_, String>(0))?
                .collect::<Result<Vec<_>, _>>()?;
            Ok((version, tables))
        })
        .unwrap()
        .into_completion()
        .await
        .unwrap()
        .into_value()
        .into_output();
    assert_eq!((version, tables.len()), (2, 14));
    assert_eq!(
        tables,
        [
            "agent_history",
            "agent_lineage",
            "conversation_item",
            "conversation_part",
            "conversation_tool_call",
            "event",
            "history_checkpoint",
            "image_artifact",
            "image_upload",
            "input",
            "input_part",
            "session_state",
            "tool_execution_result_part",
            "tool_execution_terminal",
        ]
    );
    reopened.close().await.unwrap();
}
