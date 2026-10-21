use parrot_cli::presenters::tools::answer::AnswerToolPresenter;
use parrot_cli::presenters::*;
use parrot_cli::terminal_frames::{ScrollbackItem, ScrollbackRenderContext};
use std::sync::Arc;
#[test]
fn answer_formats_resolved_recipient_single_and_numbered_multiline_answers() {
    for (answers, expected) in [
        (serde_json::json!(["use postgres"]), "use postgres"),
        (
            serde_json::json!(["yes", "keep it\nbut rename"]),
            "1. yes\n2. keep it\n   but rename",
        ),
    ] {
        let mut call = ToolCallPresentation::new(
            "answer",
            serde_json::json!({"agent_name":"opaque","answers":answers}).to_string(),
        );
        call.agent_reference_resolver = Arc::new(|_| "scout".into());
        let Presentation::Tool(live) = AnswerToolPresenter.present_live(&call, 0).unwrap() else {
            panic!("expected tool")
        };
        assert_eq!(live.block.text, expected);
        assert!(live.label.starts_with("Answer · scout · "));
        let item = AnswerToolPresenter
            .present_terminal(
                &call,
                &ToolTerminalPresentation {
                    result_present: true,
                    result: "Answered the pending question from child agent scout.".into(),
                    ..Default::default()
                },
            )
            .unwrap()
            .unwrap();
        let Presentation::Tool(report) = &item else {
            panic!("expected tool")
        };
        assert_eq!(report.block.text, expected);
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
        assert!(lines[0].starts_with("✓ Answer · scout · "));
        assert!(!lines.join("|").contains("Answered the pending question"));
        let item = AnswerToolPresenter
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
            panic!("expected tool")
        };
        assert_eq!(report.status, Some(ToolTerminalStatus::ReportedFailure));
        assert_eq!(report.block.text, "error: unavailable");
    }
}
#[test]
fn answer_matches_empty_non_array_and_invalid_value_source_boundaries() {
    for input in [
        r#"{"agent_name":"scout"}"#,
        r#"{"agent_name":"scout","answers":true}"#,
    ] {
        let Presentation::Tool(report) = AnswerToolPresenter
            .present_live(&ToolCallPresentation::new("answer", input), 0)
            .unwrap()
        else {
            panic!("expected tool")
        };
        assert_eq!(report.label, "Answer · scout · 0 items");
        assert_eq!(report.block.kind, ToolBlockKind::None);
    }
    for input in [
        "{",
        r#"{"agent_name":5}"#,
        r#"{"agent_name":"scout","answers":[5]}"#,
    ] {
        assert!(
            AnswerToolPresenter
                .present_live(&ToolCallPresentation::new("answer", input), 0)
                .is_err()
        );
    }
}
