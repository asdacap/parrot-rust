use parrot_cli::presenters::tools::agent_send::AgentSendToolPresenter;
use parrot_cli::presenters::*;
use parrot_cli::terminal_frames::{ScrollbackItem, ScrollbackRenderContext};
use std::sync::Arc;
fn complete(call: &ToolCallPresentation, result: &str) -> (ToolReport, Vec<String>) {
    let item = AgentSendToolPresenter
        .present_terminal(
            call,
            &ToolTerminalPresentation {
                result_present: true,
                result: result.into(),
                ..Default::default()
            },
        )
        .unwrap()
        .unwrap();
    let lines = ScrollbackItem::render(
        &item,
        ScrollbackRenderContext {
            columns: 512,
            color: false,
            inline_diff: false,
            previous_packing: None,
        },
    )
    .unwrap();
    let Presentation::Tool(report) = item else {
        panic!("expected tool report")
    };
    (report, lines)
}
#[test]
fn agent_send_prefers_completed_recipient_and_falls_back_to_live_resolution() {
    let mut call = ToolCallPresentation::new(
        "agent_send",
        r#"{"name":"opaque","message":"inspect logs"}"#,
    );
    call.agent_reference_resolver = Arc::new(|_| "known-before-send".into());
    for (result, name) in [
        (
            r#"{"name":"resolved-by-core","status":"running"}"#,
            "resolved-by-core",
        ),
        ("not json", "known-before-send"),
        ("[]", "known-before-send"),
        (r#"{"name":""}"#, "known-before-send"),
    ] {
        let (report, lines) = complete(&call, result);
        assert_eq!(report.label, format!("Send to {name}"));
        assert_eq!(
            lines,
            vec![format!("✓ Send to {name}"), "  inspect logs".into()]
        );
    }
    let (report, lines) = complete(&call, "error: unavailable");
    assert_eq!(report.status, Some(ToolTerminalStatus::ReportedFailure));
    assert_eq!(
        lines,
        vec!["✗ Send to known-before-send", "  error: unavailable"]
    );
}
#[test]
fn agent_send_flushes_a_bounded_message() {
    let message = (1..=12)
        .map(|n| format!("line {n}"))
        .collect::<Vec<_>>()
        .join("\n");
    let args = serde_json::json!({"name":"scout","message":message}).to_string();
    let (_, lines) = complete(&ToolCallPresentation::new("agent_send", args), "{}");
    assert_eq!(lines[0], "✓ Send to scout");
    assert!(lines.len() <= 10);
    assert!(lines.join("|").contains("  line 1"));
    assert!(lines.last().unwrap().contains("lines truncated."));
    assert!(!lines.join("|").contains("line 12"));
}
#[test]
fn agent_send_live_resolves_name_and_parsing_errors_are_explicit() {
    let mut call = ToolCallPresentation::new(
        "agent_send",
        r#"{"name":"opaque","message":"inspect logs"}"#,
    );
    call.agent_reference_resolver = Arc::new(|_| "scout".into());
    let Presentation::Tool(report) = AgentSendToolPresenter.present_live(&call, 3).unwrap() else {
        panic!("expected tool")
    };
    assert_eq!(report.label, "Send to scout");
    assert_eq!(report.frame, 3);
    assert_eq!(report.block.kind, ToolBlockKind::None);
    for input in ["not json", "[]", r#"{"name":4}"#] {
        assert!(
            AgentSendToolPresenter
                .present_live(&ToolCallPresentation::new("agent_send", input), 0)
                .is_err()
        );
    }
    assert!(
        AgentSendToolPresenter
            .present_terminal(
                &ToolCallPresentation::new("agent_send", r#"{"name":"scout"}"#),
                &ToolTerminalPresentation::default()
            )
            .is_err()
    );
}
