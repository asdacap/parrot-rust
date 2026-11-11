use std::sync::Arc;

use parrot_cli::presenters::*;
use parrot_cli::terminal_frames::{
    LiveBufferItem, LiveBufferRenderContext, ScrollbackItem, ScrollbackRenderContext,
};

fn live(call: &ToolCallPresentation, columns: usize) -> Vec<String> {
    LiveBufferItem::render(
        &ToolPresenterRegistry::standard_with_skill_read_paths(
            vec![],
            Arc::new(tools::read::SkillReadPathFormatter::default()),
        )
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
        &ToolPresenterRegistry::standard_with_skill_read_paths(
            vec![],
            Arc::new(tools::read::SkillReadPathFormatter::default()),
        )
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
fn ask_shows_question_while_waiting_and_response_after_completion() {
    let mut call =
        ToolCallPresentation::new("ask", r#"{"name":"opaque","message":"Is this safe?"}"#);
    call.agent_reference_resolver = Arc::new(|_| "reviewer".into());
    assert_eq!(
        live(&call, 160),
        ["⠋ Ask reviewer", "  Question:", "  Is this safe?"]
    );
    assert_eq!(
        terminal(
            &call,
            &ToolTerminalPresentation {
                result_present: true,
                result: "The regression test passes.".into(),
                ..Default::default()
            },
            160,
        ),
        [
            "✓ Ask reviewer",
            "  Question:",
            "  Is this safe?",
            "  Response:",
            "  The regression test passes.",
        ]
    );
}

#[test]
fn ask_preserves_opaque_responses_and_distinguishes_absent_and_empty_results() {
    let call = ToolCallPresentation::new("ask", r#"{"name":"reviewer","message":"Ready?"}"#);
    for result in [
        "Ask to reviewer timed out without a reply.",
        "Custom response template: {not json}",
        "",
    ] {
        let rows = terminal(
            &call,
            &ToolTerminalPresentation {
                result_present: true,
                result: result.into(),
                ..Default::default()
            },
            160,
        );
        assert_eq!(rows[0], "✓ Ask reviewer");
        assert_eq!(rows[3], "  Response:");
        let expected = if result.is_empty() {
            String::new()
        } else {
            format!("  {result}")
        };
        assert_eq!(rows.last().unwrap(), &expected);
    }
    assert_eq!(
        terminal(&call, &ToolTerminalPresentation::default(), 160),
        ["✓ Ask reviewer", "  Question:", "  Ready?"]
    );
}

#[test]
fn ask_preserves_failure_diagnostics_and_cancellation_without_success_content() {
    let call = ToolCallPresentation::new("ask", r#"{"name":"reviewer","message":"Ready?"}"#);
    for (status, result_present, result, error, expected) in [
        (
            ToolTerminalStatus::Errored,
            false,
            "",
            "agent unavailable",
            vec!["✗ Ask reviewer", "  agent unavailable"],
        ),
        (
            ToolTerminalStatus::ReportedFailure,
            true,
            "rejected",
            "",
            vec!["✗ Ask reviewer", "  rejected"],
        ),
        (
            ToolTerminalStatus::Succeeded,
            true,
            "error: stale request",
            "",
            vec!["✗ Ask reviewer", "  error: stale request"],
        ),
        (
            ToolTerminalStatus::Cancelled,
            false,
            "",
            "",
            vec!["■ Ask reviewer"],
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
fn ask_uses_shared_sanitization_wrapping_and_truncation() {
    let call = ToolCallPresentation::new(
        "ask",
        serde_json::json!({"name":"reviewer", "message":"First 🦜\nNext\tline\u{7}"}).to_string(),
    );
    let result = ToolTerminalPresentation {
        result_present: true,
        result: "Yes 🦜\nDone\u{7}".into(),
        ..Default::default()
    };
    assert_eq!(
        terminal(&call, &result, 160),
        [
            "✓ Ask reviewer",
            "  Question:",
            "  First 🦜",
            "  Next    line",
            "  Response:",
            "  Yes 🦜",
            "  Done",
        ]
    );
    for rows in [live(&call, 20), terminal(&call, &result, 20)] {
        assert!(
            rows.iter()
                .all(|row| { parrot_cli::terminal_layout::TerminalText::width(row) <= 20 })
        );
        assert!(!rows.join("\n").contains('\u{7}'));
    }
    let long = (1..=20)
        .map(|line| format!("line {line}"))
        .collect::<Vec<_>>()
        .join("\n");
    for (question, response) in [(&long[..], "short response"), ("short question", &long[..])] {
        let call = ToolCallPresentation::new(
            "ask",
            serde_json::json!({"name":"reviewer", "message":question}).to_string(),
        );
        let result = ToolTerminalPresentation {
            result_present: true,
            result: response.into(),
            ..Default::default()
        };
        let rows = terminal(&call, &result, 160);
        assert!(rows.len() <= 10);
        assert!(rows.last().unwrap().contains("lines truncated."));
        assert!(!rows.join("\n").contains("line 20"));
        if question == long {
            let rows = live(&call, 160);
            assert!(rows.len() <= 10);
            assert!(rows.last().unwrap().contains("lines truncated."));
        }
    }
}

#[test]
fn ask_rejects_invalid_json_before_specialized_rendering() {
    use parrot_cli::presenters::tools::ask::AskToolPresenter;

    let call = ToolCallPresentation::new("ask", "not json");
    assert!(AskToolPresenter.present_live(&call, 0).is_err());
    assert!(
        AskToolPresenter
            .present_terminal(&call, &Default::default())
            .is_err()
    );
}
