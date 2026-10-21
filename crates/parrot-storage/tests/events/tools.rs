use parrot_domain::protocol::messages::parrot::Event;
use parrot_storage::events::error::EventError;
use parrot_storage::events::projection::project;
use parrot_storage::events::tools::{
    append_tool_settlement, append_tool_synthetic, append_tool_terminal, has_tool_synthetic,
    tool_terminals, tool_terminals_of_calls,
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
        .execute_batch(include_str!("../../src/database/lineage.sql"))
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
            ConversationPart::image("image-1", "image/png", "diagram"),
        ],
        message: "fallback".into(),
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
fn terminal_identity_is_agent_scoped_and_compares_every_field_without_duplicate_event() {
    let mut connection = database();
    let tx = connection.transaction().unwrap();
    let first = terminal("call");
    assert!(append_tool_terminal(&tx, &event("e1", "agent"), &first, &clock).unwrap());
    assert!(!append_tool_terminal(&tx, &event("ignored-id", "agent"), &first, &clock).unwrap());
    let mut conflicts = Vec::new();
    let mut changed = first.clone();
    changed.tool_name.push('x');
    conflicts.push(changed);
    let mut changed = first.clone();
    changed.message.push('x');
    conflicts.push(changed);
    let mut changed = first.clone();
    changed.status = ToolExecutionStatus::Cancelled;
    conflicts.push(changed);
    let mut changed = first.clone();
    changed.result_parts.reverse();
    conflicts.push(changed);
    let mut changed = first.clone();
    changed.result_parts.push(ConversationPart::text(""));
    conflicts.push(changed);
    for changed in conflicts {
        let error =
            append_tool_terminal(&tx, &event("conflict", "agent"), &changed, &clock).unwrap_err();
        assert!(matches!(error, EventError::Conflict(_)));
        assert_eq!(
            error.to_string(),
            "tool call call was already settled differently"
        );
    }
    assert!(append_tool_terminal(&tx, &event("e2", "other"), &first, &clock).unwrap());
    assert_eq!(tool_terminals(&tx, "agent").unwrap(), vec![first.clone()]);
    assert_eq!(tool_terminals(&tx, "other").unwrap(), vec![first]);
    tx.commit().unwrap();
    assert_eq!(count(&connection, "event"), 2);
    assert_eq!(count(&connection, "tool_execution_terminal"), 2);
    assert_eq!(count(&connection, "conversation_item"), 0);
}
#[test]
fn settlement_repairs_existing_terminal_and_projects_only_text_once() {
    let mut connection = database();
    let terminal = terminal("call");
    let tx = connection.transaction().unwrap();
    let assistant = assistant(&tx, "agent");
    assert!(append_tool_terminal(&tx, &event("original", "agent"), &terminal, &clock).unwrap());
    tx.commit().unwrap();
    let raw: Vec<u8> = connection
        .query_row("SELECT payload FROM event", [], |row| row.get(0))
        .unwrap();
    let tx = connection.transaction().unwrap();
    let item = append_tool_settlement(
        &tx,
        &event("must-not-record", "agent"),
        assistant,
        &terminal,
        &clock,
    )
    .unwrap()
    .unwrap();
    assert_eq!(item.message.origin, ConversationOrigin::Tool);
    assert_eq!(item.message.role, MessageRole::Tool);
    assert_eq!(item.message.parts, vec![ConversationPart::text("answer")]);
    assert_eq!(item.message.tool_call_id, "call");
    assert!(
        append_tool_settlement(
            &tx,
            &event("also-ignored", "agent"),
            assistant,
            &terminal,
            &clock
        )
        .unwrap()
        .is_none()
    );
    assert_eq!(
        tool_terminals(&tx, "agent").unwrap()[0].result_parts,
        terminal.result_parts
    );
    tx.commit().unwrap();
    assert_eq!(count(&connection, "event"), 1);
    assert_eq!(count(&connection, "message"), 2);
    assert_eq!(count(&connection, "tool_batch_result"), 1);
    let after: Vec<u8> = connection
        .query_row("SELECT payload FROM event", [], |row| row.get(0))
        .unwrap();
    assert_eq!(raw, after);
}
#[test]
fn settlement_without_text_uses_message_but_empty_text_is_not_replaced() {
    let mut connection = database();
    let tx = connection.transaction().unwrap();
    let assistant = assistant(&tx, "agent");
    let mut only_image = terminal("image");
    only_image.result_parts = vec![ConversationPart::image("id", "image/webp", "image")];
    let item = append_tool_settlement(&tx, &event("e1", "agent"), assistant, &only_image, &clock)
        .unwrap()
        .unwrap();
    assert_eq!(item.message.parts, vec![ConversationPart::text("fallback")]);
    let mut empty = terminal("empty");
    empty.result_parts = vec![ConversationPart::text("")];
    let item = append_tool_settlement(&tx, &event("e2", "agent"), assistant, &empty, &clock)
        .unwrap()
        .unwrap();
    assert_eq!(item.message.parts, vec![ConversationPart::text("")]);
    let mut no_parts = terminal("none");
    no_parts.result_parts.clear();
    let item = append_tool_settlement(&tx, &event("e3", "agent"), assistant, &no_parts, &clock)
        .unwrap()
        .unwrap();
    assert_eq!(item.message.parts, vec![ConversationPart::text("fallback")]);
    tx.commit().unwrap();
    assert_eq!(count(&connection, "event"), 3);
    assert_eq!(count(&connection, "tool_batch_result"), 3);
}
#[test]
fn cross_assistant_unique_conflict_rolls_back_projected_rows_instead_of_overwriting() {
    let mut connection = database();
    let terminal = terminal("call");
    let tx = connection.transaction().unwrap();
    let first = assistant(&tx, "agent");
    let second = assistant(&tx, "agent");
    append_tool_settlement(&tx, &event("original", "agent"), first, &terminal, &clock).unwrap();
    tx.commit().unwrap();
    let tables = [
        "event",
        "tool_execution_terminal",
        "tool_execution_result_part",
        "message",
        "conversation_item",
        "conversation_part",
        "agent_history",
        "tool_batch_result",
    ];
    let before: Vec<_> = tables
        .iter()
        .map(|table| count(&connection, table))
        .collect();
    let tx = connection.transaction().unwrap();
    assert!(
        append_tool_settlement(&tx, &event("ignored", "agent"), second, &terminal, &clock).is_err()
    );
    tx.rollback().unwrap();
    let after: Vec<_> = tables
        .iter()
        .map(|table| count(&connection, table))
        .collect();
    assert_eq!(before, after);
    let stored: i64 = connection
        .query_row(
            "SELECT assistant_sequence FROM tool_batch_result",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(stored, first);
}
#[test]
fn missing_assistant_foreign_key_rolls_back_new_terminal_event_and_projection() {
    let mut connection = database();
    let tx = connection.transaction().unwrap();
    assert!(
        append_tool_settlement(&tx, &event("e1", "agent"), 123, &terminal("call"), &clock).is_err()
    );
    tx.rollback().unwrap();
    for table in [
        "event",
        "tool_execution_terminal",
        "tool_execution_result_part",
        "message",
        "conversation_item",
        "conversation_part",
        "agent_history",
        "tool_batch_result",
        "projection_version",
    ] {
        assert_eq!(count(&connection, table), 0, "{table}");
    }
}
#[test]
fn synthetic_requires_images_is_idempotent_and_validates_before_duplicate_lookup() {
    let mut connection = database();
    let tx = connection.transaction().unwrap();
    let assistant = assistant(&tx, "agent");
    let images = vec![
        ConversationPart::image("one", "image/png", "first"),
        ConversationPart::image("two", "image/jpeg", "second"),
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
            &[ConversationPart::image(
                "different",
                "image/gif",
                "different"
            )],
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
    assert_eq!(count(&connection, "tool_batch_synthetic"), 1);
    assert_eq!(count(&connection, "conversation_item"), 2);
}
#[test]
fn terminal_reads_keep_sequence_call_input_order_duplicates_and_all_statuses() {
    let mut connection = database();
    let tx = connection.transaction().unwrap();
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
        append_tool_terminal(&tx, &event(&format!("e{index}"), "agent"), &value, &clock).unwrap();
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
        "UPDATE tool_execution_terminal SET status = 'unknown' WHERE tool_call_id = 'call-0'",
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
fn clock_reads_follow_each_source_callsite_and_duplicates_do_not_read_clock() {
    use std::cell::Cell;
    let mut connection = database();
    let tx = connection.transaction().unwrap();
    let assistant = assistant(&tx, "agent");
    let reads = Cell::new(0);
    let clock = || {
        let next = reads.get() + 1;
        reads.set(next);
        format!("2026-01-02T03:04:05.{next:07}+00:00")
    };
    let terminal = terminal("call");
    let item = append_tool_settlement(&tx, &event("e1", "agent"), assistant, &terminal, &clock)
        .unwrap()
        .unwrap();
    assert_eq!(reads.get(), 3);
    let event_at: String = tx
        .query_row("SELECT created_at FROM event WHERE id = 'e1'", [], |row| {
            row.get(0)
        })
        .unwrap();
    let terminal_at: String = tx
        .query_row(
            "SELECT created_at FROM tool_execution_terminal",
            [],
            |row| row.get(0),
        )
        .unwrap();
    let item_at: String = tx
        .query_row(
            "SELECT created_at FROM conversation_item WHERE sequence = ?1",
            [item.sequence],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(event_at, "2026-01-02T03:04:05.0000001+00:00");
    assert_eq!(terminal_at, "2026-01-02T03:04:05.0000002+00:00");
    assert_eq!(item_at, "2026-01-02T03:04:05.0000003+00:00");
    assert!(
        append_tool_settlement(
            &tx,
            &event("ignored", "agent"),
            assistant,
            &terminal,
            &clock
        )
        .unwrap()
        .is_none()
    );
    assert_eq!(reads.get(), 3);
    assert!(!append_tool_terminal(&tx, &event("ignored2", "agent"), &terminal, &clock).unwrap());
    assert_eq!(reads.get(), 3);
    append_tool_synthetic(
        &tx,
        &event("e2", "agent"),
        assistant,
        &[ConversationPart::image("image", "image/png", "name")],
        &clock,
    )
    .unwrap()
    .unwrap();
    assert_eq!(reads.get(), 5);
    let event_at: String = tx
        .query_row("SELECT created_at FROM event WHERE id = 'e2'", [], |row| {
            row.get(0)
        })
        .unwrap();
    assert_eq!(event_at, "2026-01-02T03:04:05.0000004+00:00");
}
