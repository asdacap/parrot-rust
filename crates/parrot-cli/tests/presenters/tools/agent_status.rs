use parrot_cli::presenters::{tools::agent_status::AgentStatusToolPresenter, *};
use parrot_cli::terminal_frames::{
    LiveBufferItem, LiveBufferRenderContext, ScrollbackItem, ScrollbackRenderContext,
};
use std::sync::Arc;

#[test]
fn resolves_agent_name_and_preserves_all_status_lines() {
    let mut call = ToolCallPresentation::new("agent_status", r#"{"name":"agent-session-opaque"}"#);
    call.agent_reference_resolver = Arc::new(|reference| {
        if reference == "agent-session-opaque" {
            "scout".into()
        } else {
            reference.into()
        }
    });
    let result = "Agent status\nName: scout\nLifecycle: running\nRequest session duration: 12.0s\nProvider request duration: none\nCurrent activity: none\nActive direct subagents: none\nActive processes: none\nRecent entries:\n- assistant message (1.0s ago): one\n- assistant message (0.5s ago): two\n- assistant message (10ms ago): three";
    let live = AgentStatusToolPresenter.present_live(&call, 0).unwrap();
    assert_eq!(
        LiveBufferItem::render(
            &live,
            LiveBufferRenderContext {
                columns: 512,
                color: false
            }
        )
        .unwrap()
        .lines[0]
            .text(),
        "⠋ Agent status · scout"
    );
    let terminal = ToolTerminalPresentation {
        result_present: true,
        result: result.into(),
        ..Default::default()
    };
    let item = AgentStatusToolPresenter
        .present_terminal(&call, &terminal)
        .unwrap()
        .unwrap();
    let rows = ScrollbackItem::render(
        &item,
        ScrollbackRenderContext {
            columns: 512,
            color: false,
            inline_diff: true,
            previous_packing: None,
        },
    )
    .unwrap();
    assert_eq!(rows[0], "✓ Agent status · scout");
    assert_eq!(rows.len(), 13);
    assert_eq!(
        rows.last().unwrap(),
        "  - assistant message (10ms ago): three"
    );
    assert_eq!(
        rows[1..].join("\n"),
        result
            .split('\n')
            .map(|line| format!("  {line}"))
            .collect::<Vec<_>>()
            .join("\n")
    );
}
#[test]
fn reported_errors_and_bad_arguments() {
    let call = ToolCallPresentation::new("agent_status", r#"{"name":"scout"}"#);
    let item = AgentStatusToolPresenter
        .present_terminal(
            &call,
            &ToolTerminalPresentation {
                result_present: true,
                result: "error: unavailable".into(),
                ..Default::default()
            },
        )
        .unwrap()
        .unwrap();
    let Presentation::Tool(report) = item else {
        panic!("expected report");
    };
    assert_eq!(report.label, "Agent status · scout");
    assert_eq!(report.status, Some(ToolTerminalStatus::ReportedFailure));
    assert_eq!(report.block.kind, ToolBlockKind::Error);
    for arguments in ["{", "[]", "{}", r#"{"name":null}"#, r#"{"name":4}"#] {
        assert!(
            AgentStatusToolPresenter
                .present_live(&ToolCallPresentation::new("agent_status", arguments), 0)
                .is_err()
        );
    }
}
