use parrot_core::{ConversationHistoryItem, ConversationState, ConversationSummary, Message, Role};

fn item(sequence: i64, text: &str, is_compaction_status: bool) -> ConversationHistoryItem {
    ConversationHistoryItem {
        sequence,
        tool_owner_sequence: None,
        is_compaction_status,
        message: Message::text(Role::System, text),
    }
}

#[test]
fn assembly_preserves_the_complete_summary_and_selects_its_associated_status() {
    let summary = Message {
        role: Role::Assistant,
        provider_id: "native".into(),
        provider_content: " { \"continuation\" : 1.00 } ".into(),
        ..Message::text(Role::Assistant, "previous summary")
    };
    let tail = item(11, "retained message", false);
    let old_status = item(12, "old status", true);
    let status = item(13, "current status", true);
    let state = ConversationState {
        summary: Some(ConversationSummary {
            message: summary.clone(),
            watermark: 10,
            status_sequence: Some(13),
        }),
        history: vec![tail.clone(), old_status, status.clone()],
        filter_watermark: 0,
    };
    assert_eq!(
        state.assemble_messages(),
        [summary, status.message, tail.message]
    );
}

#[test]
fn a_summary_without_status_does_not_inherit_an_older_status() {
    let summary = Message::text(Role::System, "summary without status");
    let tail = item(11, "retained message", false);
    let state = ConversationState {
        summary: Some(ConversationSummary {
            message: summary.clone(),
            watermark: 10,
            status_sequence: None,
        }),
        history: vec![tail.clone(), item(12, "old status", true)],
        filter_watermark: 0,
    };
    assert_eq!(state.assemble_messages(), [summary, tail.message]);
}

#[test]
fn uncompacted_history_retains_every_message_in_order() {
    let first = item(1, "first", false);
    let status = item(2, "status", true);
    let last = item(3, "last", false);
    let state = ConversationState {
        summary: None,
        history: vec![first.clone(), status.clone(), last.clone()],
        filter_watermark: 0,
    };
    assert_eq!(
        state.assemble_messages(),
        [first.message, status.message, last.message]
    );
}
