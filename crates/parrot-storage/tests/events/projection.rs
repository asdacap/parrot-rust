use parrot_storage::events::{projection::*, types::*};
use rusqlite::Connection;

fn database() -> Connection {
    let connection = Connection::open_in_memory().unwrap();
    connection.execute_batch("PRAGMA foreign_keys=ON;").unwrap();
    connection
        .execute_batch(include_str!("../../src/database/schema.sql"))
        .unwrap();
    connection
}

fn count(connection: &Connection, table: &str) -> i64 {
    connection
        .query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |row| {
            row.get(0)
        })
        .unwrap()
}

#[test]
fn project_writes_every_payload_component_and_source_text_only_legacy_content() {
    let mut connection = database();
    let tx = connection.transaction().unwrap();
    let message = ConversationMessage {
        origin: ConversationOrigin::Model,
        role: MessageRole::Assistant,
        parts: vec![
            ConversationPart::text("first\0"),
            ConversationPart::image("artifact", "image/webp", "name.webp"),
            ConversationPart::text("last"),
        ],
        tool_calls: vec![
            ToolCall {
                id: "call1".into(),
                name: "one".into(),
                arguments_json: "{\"n\":1}".into(),
            },
            ToolCall {
                id: "call2".into(),
                name: "two".into(),
                arguments_json: "{}".into(),
            },
        ],
        tool_call_id: "parent-call".into(),
        provider_content: ProviderContent {
            provider_id: "provider".into(),
            content: "{\"opaque\":true}".into(),
        },
    };
    let item = project(&tx, "agent", &message, "exact-at").unwrap();
    assert_eq!(item.message, message);
    let legacy: (String, String, String, String) = tx
        .query_row(
            "SELECT agent_session,role,content,created_at FROM message",
            [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
        )
        .unwrap();
    assert_eq!(
        legacy,
        (
            "agent".into(),
            "assistant".into(),
            "first\0last".into(),
            "exact-at".into()
        )
    );
    assert_eq!(
        read_parts(&tx, "conversation_part", "item_sequence", &item.sequence).unwrap(),
        message.parts
    );
    let calls: Vec<(i64, String, String, String)> = tx
        .prepare("SELECT position,id,name,arguments FROM conversation_tool_call ORDER BY position")
        .unwrap()
        .query_map([], |row| {
            Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?))
        })
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
    assert_eq!(
        calls,
        vec![
            (0, "call1".into(), "one".into(), "{\"n\":1}".into()),
            (1, "call2".into(), "two".into(), "{}".into())
        ]
    );
    assert_eq!(
        tx.query_row(
            "SELECT provider||':'||content FROM conversation_provider_content",
            [],
            |row| row.get::<_, String>(0)
        )
        .unwrap(),
        "provider:{\"opaque\":true}"
    );
    assert_eq!(
        tx.query_row(
            "SELECT conversation_sequence FROM agent_history",
            [],
            |row| row.get::<_, i64>(0)
        )
        .unwrap(),
        item.sequence
    );
    let empty = project(
        &tx,
        "agent",
        &ConversationMessage::text(MessageRole::User, ""),
        "next",
    )
    .unwrap();
    assert_eq!(count(&tx, "conversation_provider_content"), 1);
    assert_eq!(
        read_parts(&tx, "conversation_part", "item_sequence", &empty.sequence).unwrap(),
        vec![ConversationPart::text("")]
    );
    assert_eq!(count(&tx, "agent_history"), 2);
}

#[test]
fn part_roundtrip_legacy_fallback_and_unknown_kind_match_source_behavior() {
    let mut connection = database();
    let tx = connection.transaction().unwrap();
    tx.execute("INSERT INTO input (id,agent_session,message_id,content,delivery,status,created_at) VALUES ('input','agent','message','legacy','queue','pending','at')",[]).unwrap();
    assert_eq!(
        read_input_parts(&tx, "input", "legacy").unwrap(),
        vec![ConversationPart::text("legacy")]
    );
    let parts = vec![
        ConversationPart::image("id", "image/png", "display"),
        ConversationPart::text("second"),
    ];
    insert_parts(&tx, "input_part", "input_id", "input", &parts).unwrap();
    assert_eq!(read_input_parts(&tx, "input", "ignored").unwrap(), parts);
    tx.execute("INSERT INTO tool_execution_terminal (agent_session,tool_call_id,tool_name,status,message,created_at) VALUES ('agent','call','tool','finished','result','at')",[]).unwrap();
    let terminal = tx.last_insert_rowid();
    insert_parts(
        &tx,
        "tool_execution_result_part",
        "terminal_sequence",
        terminal,
        &parts,
    )
    .unwrap();
    assert_eq!(
        read_parts(
            &tx,
            "tool_execution_result_part",
            "terminal_sequence",
            &terminal
        )
        .unwrap(),
        parts
    );
    tx.execute("UPDATE input_part SET kind='unknown' WHERE position=0", [])
        .unwrap();
    assert_eq!(
        read_input_parts(&tx, "input", "ignored")
            .unwrap_err()
            .to_string(),
        "unknown conversation part kind unknown"
    );
    assert!(
        insert_parts(
            &tx,
            "input_part; DROP TABLE input",
            "input_id",
            "input",
            &[]
        )
        .is_err()
    );
    assert!(read_parts(&tx, "input_part", "not_input_id", &"input").is_err());
    assert_eq!(count(&tx, "input"), 1);
}

#[test]
fn projection_failure_leaves_every_write_in_callers_transaction_for_rollback() {
    let mut connection = database();
    connection.execute_batch("CREATE TRIGGER reject_part BEFORE INSERT ON conversation_part BEGIN SELECT RAISE(ABORT,'reject'); END;").unwrap();
    {
        let tx = connection.transaction().unwrap();
        assert!(
            project(
                &tx,
                "agent",
                &ConversationMessage::text(MessageRole::Assistant, "answer"),
                "at"
            )
            .is_err()
        );
    }
    for table in [
        "message",
        "conversation_item",
        "conversation_part",
        "conversation_tool_call",
        "conversation_provider_content",
        "agent_history",
        "projection_version",
    ] {
        assert_eq!(count(&connection, table), 0, "{table}");
    }
}
