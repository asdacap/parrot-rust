use parrot_domain::protocol::messages::parrot::Event;
use parrot_storage::events::error::EventError;
use parrot_storage::events::projection::project;
use parrot_storage::events::tools::{
    append_tool_settlement, append_tool_synthetic, has_tool_synthetic, tool_terminals,
    tool_terminals_of_calls,
};
use parrot_storage::events::types::{
    ConversationMessage, ConversationOrigin, ConversationPart, MessageRole, ToolExecutionStatus,
    ToolExecutionTerminal,
};
use rusqlite::{Connection, Transaction};
const AT: &str = "2026-01-02T03:04:05.0000000+00:00";
fn clock() -> String {
    AT.into()
}
fn database() -> Connection {
    let connection = Connection::open_in_memory().unwrap();
    connection
        .execute_batch("PRAGMA foreign_keys = ON;")
        .unwrap();
    connection
        .execute_batch(include_str!("../../src/database/schema.sql"))
        .unwrap();
    connection
        .execute_batch(include_str!("../../src/database/event_indexes.sql"))
        .unwrap();
    connection
}
fn event(id: &str, agent: &str) -> Event {
    Event {
        id: id.into(),
        agent_session_id: agent.into(),
        ..Event::default()
    }
}
fn terminal(call: &str) -> ToolExecutionTerminal {
    ToolExecutionTerminal {
        tool_call_id: call.into(),
        tool_name: "shell".into(),
        status: ToolExecutionStatus::Finished,
        result_parts: vec![
            ConversationPart::text("answer"),
            super::Fixture::image("image-1", "image/png", "diagram"),
        ],
    }
}
fn assistant(tx: &Transaction<'_>, agent: &str) -> i64 {
    project(
        tx,
        agent,
        &ConversationMessage::text(MessageRole::Assistant, "request"),
        AT,
    )
    .unwrap()
    .sequence
}
fn count(connection: &Connection, table: &str) -> i64 {
    connection
        .query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |row| {
            row.get(0)
        })
        .unwrap()
}
#[test]
fn result_identity_is_agent_scoped_and_compares_metadata_and_ordered_content() {
    let mut connection = database();
    let tx = connection.transaction().unwrap();
    let owner = assistant(&tx, "agent");
    let first = terminal("call");
    assert!(
        append_tool_settlement(&tx, &event("e1", "agent"), owner, &first, &clock)
            .unwrap()
            .is_some()
    );
    assert!(
        append_tool_settlement(&tx, &event("ignored-id", "agent"), owner, &first, &clock)
            .unwrap()
            .is_none()
    );
    let mut conflicts = Vec::new();
    let mut changed = first.clone();
    changed.tool_name.push('x');
    conflicts.push(changed);
    let mut changed = first.clone();
    changed.status = ToolExecutionStatus::Cancelled;
    conflicts.push(changed);
    let mut changed = first.clone();
    changed.result_parts[0] = ConversationPart::text("changed answer");
    conflicts.push(changed);
    let mut changed = first.clone();
    changed.result_parts.reverse();
    conflicts.push(changed);
    let mut changed = first.clone();
    changed.result_parts.push(ConversationPart::text(""));
    conflicts.push(changed);
    for changed in conflicts {
        let error =
            append_tool_settlement(&tx, &event("conflict", "agent"), owner, &changed, &clock)
                .unwrap_err();
        assert!(matches!(error, EventError::Conflict(_)));
        assert_eq!(
            error.to_string(),
            "tool call call was already settled differently"
        );
    }
    let other = assistant(&tx, "other");
    assert!(
        append_tool_settlement(&tx, &event("e2", "other"), other, &first, &clock)
            .unwrap()
            .is_some()
    );
    assert_eq!(tool_terminals(&tx, "agent").unwrap(), vec![first.clone()]);
    assert_eq!(tool_terminals(&tx, "other").unwrap(), vec![first]);
    tx.commit().unwrap();
    assert_eq!(count(&connection, "event"), 2);
    assert_eq!(count(&connection, "conversation_item"), 4);
}
#[test]
fn settlement_stores_complete_content_once_and_retry_preserves_the_original_event() {
    let mut connection = database();
    let terminal = terminal("call");
    let tx = connection.transaction().unwrap();
    let owner = assistant(&tx, "agent");
    let item = append_tool_settlement(&tx, &event("original", "agent"), owner, &terminal, &clock)
        .unwrap()
        .unwrap();
    assert_eq!(item.message.origin, ConversationOrigin::Tool);
    assert_eq!(item.message.role, MessageRole::Tool);
    assert_eq!(item.message.parts, terminal.result_parts);
    assert_eq!(item.message.tool_call_id, "call");
    tx.commit().unwrap();
    let raw: Vec<u8> = connection
        .query_row("SELECT payload FROM event", [], |row| row.get(0))
        .unwrap();
    let tx = connection.transaction().unwrap();
    assert!(
        append_tool_settlement(
            &tx,
            &event("must-not-record", "agent"),
            owner,
            &terminal,
            &clock
        )
        .unwrap()
        .is_none()
    );
    assert_eq!(tool_terminals(&tx, "agent").unwrap(), vec![terminal]);
    assert_eq!(
        parrot_storage::conversation::reads::read_conversation_item(&tx, item.sequence).unwrap(),
        item
    );
    let copies: i64 = tx
        .query_row(
            "SELECT count(*) FROM conversation_part WHERE text = 'answer'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(copies, 1);
    tx.commit().unwrap();
    assert_eq!(count(&connection, "event"), 1);
    assert_eq!(count(&connection, "conversation_item"), 2);
    let after: Vec<u8> = connection
        .query_row("SELECT payload FROM event", [], |row| row.get(0))
        .unwrap();
    assert_eq!(raw, after);
}
#[test]
fn settlement_preserves_image_only_empty_text_and_empty_content() {
    let mut connection = database();
    let tx = connection.transaction().unwrap();
    let owner = assistant(&tx, "agent");
    for (index, parts) in [
        vec![super::Fixture::image("id", "image/webp", "image")],
        vec![ConversationPart::text("")],
        vec![],
    ]
    .into_iter()
    .enumerate()
    {
        let mut result = terminal(&format!("call-{index}"));
        result.result_parts = parts;
        let item = append_tool_settlement(
            &tx,
            &event(&format!("e{index}"), "agent"),
            owner,
            &result,
            &clock,
        )
        .unwrap()
        .unwrap();
        assert_eq!(item.message.parts, result.result_parts);
        assert_eq!(result.content(), "");
    }
    assert_eq!(tool_terminals(&tx, "agent").unwrap().len(), 3);
    tx.commit().unwrap();
    assert_eq!(count(&connection, "event"), 3);
}
#[test]
fn cross_assistant_conflict_preserves_the_original_result_and_owner() {
    let mut connection = database();
    let terminal = terminal("call");
    let tx = connection.transaction().unwrap();
    let first = assistant(&tx, "agent");
    let second = assistant(&tx, "agent");
    append_tool_settlement(&tx, &event("original", "agent"), first, &terminal, &clock).unwrap();
    tx.commit().unwrap();
    let tables = ["event", "conversation_item", "conversation_part"];
    let before: Vec<_> = tables
        .iter()
        .map(|table| count(&connection, table))
        .collect();
    let tx = connection.transaction().unwrap();
    assert!(matches!(
        append_tool_settlement(&tx, &event("ignored", "agent"), second, &terminal, &clock),
        Err(EventError::Conflict(_))
    ));
    tx.rollback().unwrap();
    let after: Vec<_> = tables
        .iter()
        .map(|table| count(&connection, table))
        .collect();
    assert_eq!(before, after);
    let stored: i64 = connection
        .query_row(
            "SELECT tool_owner_sequence FROM conversation_item WHERE tool_result_status IS NOT NULL",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(stored, first);
}
#[test]
fn missing_assistant_foreign_key_rolls_back_the_event_result_and_history() {
    let mut connection = database();
    let tx = connection.transaction().unwrap();
    assert!(
        append_tool_settlement(&tx, &event("e1", "agent"), 123, &terminal("call"), &clock).is_err()
    );
    tx.rollback().unwrap();
    for table in ["event", "conversation_item", "conversation_part"] {
        assert_eq!(count(&connection, table), 0, "{table}");
    }
}
#[test]
fn results_survive_owner_deletion_without_losing_identity() {
    let mut connection = database();
    let tx = connection.transaction().unwrap();
    let owner = assistant(&tx, "agent");
    let result = terminal("call");
    let item = append_tool_settlement(&tx, &event("e1", "agent"), owner, &result, &clock)
        .unwrap()
        .unwrap();
    tx.execute("DELETE FROM conversation_item WHERE sequence = ?1", [owner])
        .unwrap();
    assert_eq!(
        tool_terminals_of_calls(&tx, "agent", &["call".into()]).unwrap(),
        vec![result.clone()]
    );
    assert_eq!(
        parrot_storage::conversation::reads::read_conversation_item(&tx, item.sequence).unwrap(),
        item
    );
    let another = assistant(&tx, "agent");
    assert!(matches!(
        append_tool_settlement(&tx, &event("e2", "agent"), another, &result, &clock),
        Err(EventError::Conflict(_))
    ));
}
#[test]
fn synthetic_requires_images_is_idempotent_and_validates_before_duplicate_lookup() {
    let mut connection = database();
    let tx = connection.transaction().unwrap();
    let assistant = assistant(&tx, "agent");
    let images = vec![
        super::Fixture::image("one", "image/png", "first"),
        super::Fixture::image("two", "image/jpeg", "second"),
    ];
    let item = append_tool_synthetic(&tx, &event("e1", "agent"), assistant, &images, &clock)
        .unwrap()
        .unwrap();
    assert_eq!(item.message.origin, ConversationOrigin::Tool);
    assert_eq!(item.message.role, MessageRole::User);
    assert_eq!(item.message.parts, images);
    assert!(item.message.tool_call_id.is_empty());
    assert!(has_tool_synthetic(&tx, "agent", assistant).unwrap());
    assert!(!has_tool_synthetic(&tx, "other", assistant).unwrap());
    assert!(
        append_tool_synthetic(
            &tx,
            &event("ignored", "agent"),
            assistant,
            &[super::Fixture::image("different", "image/gif", "different")],
            &clock
        )
        .unwrap()
        .is_none()
    );
    for parts in [
        vec![],
        vec![ConversationPart::text("text")],
        vec![images[0].clone(), ConversationPart::text("mixed")],
    ] {
        let error =
            append_tool_synthetic(&tx, &event("invalid", "agent"), assistant, &parts, &clock)
                .unwrap_err();
        assert!(matches!(error, EventError::Argument(_)));
        assert_eq!(
            error.to_string(),
            "A tool synthetic message requires image artifact parts."
        );
    }
    tx.commit().unwrap();
    assert_eq!(count(&connection, "event"), 1);
    assert_eq!(connection.query_row("SELECT COUNT(*) FROM conversation_item WHERE tool_owner_sequence IS NOT NULL AND role = 'user' AND origin = 'tool'", [], |row| row.get::<_, i64>(0)).unwrap(), 1);
    assert_eq!(count(&connection, "conversation_item"), 2);
}
#[test]
fn result_reads_preserve_call_input_order_duplicates_and_all_statuses() {
    let mut connection = database();
    let tx = connection.transaction().unwrap();
    let owner = assistant(&tx, "agent");
    let statuses = [
        ToolExecutionStatus::Finished,
        ToolExecutionStatus::Cancelled,
        ToolExecutionStatus::Error,
        ToolExecutionStatus::ImageBudgetExceeded,
    ];
    let mut expected = Vec::new();
    for (index, status) in statuses.into_iter().enumerate() {
        let mut value = terminal(&format!("call-{index}"));
        value.status = status;
        append_tool_settlement(
            &tx,
            &event(&format!("e{index}"), "agent"),
            owner,
            &value,
            &clock,
        )
        .unwrap();
        expected.push(value);
    }
    assert_eq!(tool_terminals(&tx, "agent").unwrap(), expected);
    assert!(tool_terminals(&tx, "other").unwrap().is_empty());
    assert_eq!(
        tool_terminals_of_calls(
            &tx,
            "agent",
            &[
                "call-2".into(),
                "missing".into(),
                "call-0".into(),
                "call-2".into()
            ]
        )
        .unwrap(),
        vec![
            expected[2].clone(),
            expected[0].clone(),
            expected[2].clone()
        ]
    );
    tx.execute(
        "UPDATE conversation_item SET tool_result_status = 'unknown' WHERE tool_call_id = 'call-0'",
        [],
    )
    .unwrap();
    assert_eq!(
        tool_terminals(&tx, "agent").unwrap_err().to_string(),
        "unknown tool execution status unknown"
    );
    assert_eq!(
        tool_terminals_of_calls(&tx, "agent", &["call-0".into()])
            .unwrap_err()
            .to_string(),
        "unknown tool execution status unknown"
    );
}
#[test]
fn ordinary_tool_messages_are_not_completed_execution_results() {
    let mut connection = database();
    let tx = connection.transaction().unwrap();
    let mut message = ConversationMessage::text(MessageRole::Tool, "ordinary message");
    message.tool_call_id = "ordinary".into();
    project(&tx, "agent", &message, AT).unwrap();
    assert!(tool_terminals(&tx, "agent").unwrap().is_empty());
    assert!(
        tool_terminals_of_calls(&tx, "agent", &["ordinary".into()])
            .unwrap()
            .is_empty()
    );
}
