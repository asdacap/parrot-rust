use std::sync::Arc;

use parrot_cli::presenters::*;
use parrot_cli::terminal_frames::{
    LiveBufferItem, LiveBufferRenderContext, ScrollbackItem, ScrollbackRenderContext,
};

fn live(call: &ToolCallPresentation, columns: usize) -> Vec<String> {
    LiveBufferItem::render(
        &ToolPresenterRegistry::standard(vec![])
            .present_live(call, 0)
            .unwrap(),
        LiveBufferRenderContext {
            columns,
            color: false,
        },
    )
    .unwrap()
    .lines
    .iter()
    .map(|line| line.text().to_owned())
    .collect()
}

fn terminal(
    call: &ToolCallPresentation,
    terminal: &ToolTerminalPresentation,
    columns: usize,
) -> Vec<String> {
    ScrollbackItem::render(
        &ToolPresenterRegistry::standard(vec![])
            .present_terminal(call, terminal)
            .unwrap()
            .unwrap(),
        ScrollbackRenderContext {
            columns,
            color: false,
            inline_diff: false,
            previous_packing: None,
        },
    )
    .unwrap()
}

#[test]
fn reply_shows_literal_request_id_and_outgoing_message() {
    let mut call = ToolCallPresentation::new(
        "reply",
        r#"{"request_id":"request-123","message":"The regression test passes."}"#,
    );
    call.agent_reference_resolver = Arc::new(|_| panic!("request IDs are not agent references"));
    assert_eq!(
        live(&call, 160),
        ["⠋ Reply to request-123", "  The regression test passes.",]
    );
    assert_eq!(
        terminal(
            &call,
            &ToolTerminalPresentation {
                result_present: true,
                result: "Replied to ask request-123.".into(),
                ..Default::default()
            },
            160,
        ),
        ["✓ Reply to request-123", "  The regression test passes.",]
    );
}

#[test]
fn reply_preserves_failure_diagnostics_and_cancellation_without_sent_message() {
    let call = ToolCallPresentation::new(
        "reply",
        r#"{"request_id":"request-123","message":"The regression test passes."}"#,
    );
    for (status, result_present, result, error, expected) in [
        (
            ToolTerminalStatus::Errored,
            false,
            "",
            "unknown request",
            vec!["✗ Reply to request-123", "  unknown request"],
        ),
        (
            ToolTerminalStatus::ReportedFailure,
            true,
            "reply rejected",
            "",
            vec!["✗ Reply to request-123", "  reply rejected"],
        ),
        (
            ToolTerminalStatus::Succeeded,
            true,
            "error: already settled",
            "",
            vec!["✗ Reply to request-123", "  error: already settled"],
        ),
        (
            ToolTerminalStatus::Cancelled,
            false,
            "",
            "",
            vec!["■ Reply to request-123"],
        ),
    ] {
        assert_eq!(
            terminal(
                &call,
                &ToolTerminalPresentation {
                    status,
                    result_present,
                    result: result.into(),
                    error: error.into(),
                    ..Default::default()
                },
                160,
            ),
            expected,
        );
    }
}

#[test]
fn reply_uses_shared_sanitization_wrapping_and_truncation() {
    let call = ToolCallPresentation::new(
        "reply",
        serde_json::json!({"request_id":"request-123", "message":"First 🦜\nNext\tline\u{7}"})
            .to_string(),
    );
    let result = ToolTerminalPresentation::default();
    assert_eq!(
        terminal(&call, &result, 160),
        ["✓ Reply to request-123", "  First 🦜", "  Next    line"]
    );
    for rows in [live(&call, 20), terminal(&call, &result, 20)] {
        assert!(
            rows.iter()
                .all(|row| { parrot_cli::terminal_layout::TerminalText::width(row) <= 20 })
        );
        assert!(!rows.join("\n").contains('\u{7}'));
    }
    let message = (1..=20)
        .map(|line| format!("line {line}"))
        .collect::<Vec<_>>()
        .join("\n");
    let call = ToolCallPresentation::new(
        "reply",
        serde_json::json!({"request_id":"request-123", "message":message}).to_string(),
    );
    for rows in [live(&call, 160), terminal(&call, &result, 160)] {
        assert!(rows.len() <= 10);
        assert!(rows.last().unwrap().contains("lines truncated."));
        assert!(!rows.join("\n").contains("line 20"));
    }
}

#[test]
fn reply_rejects_invalid_json_before_specialized_rendering() {
    use parrot_cli::presenters::tools::reply::ReplyToolPresenter;

    let call = ToolCallPresentation::new("reply", "not json");
    assert!(ReplyToolPresenter.present_live(&call, 0).is_err());
    assert!(
        ReplyToolPresenter
            .present_terminal(&call, &Default::default())
            .is_err()
    );
}
