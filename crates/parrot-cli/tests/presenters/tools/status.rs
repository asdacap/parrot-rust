use parrot_cli::presenters::tools::generic::GenericToolPresenter;
use parrot_cli::presenters::tools::status::StatusToolPresenter;
use parrot_cli::presenters::*;
use parrot_cli::terminal_frames::{ScrollbackItem, ScrollbackRenderContext};
use parrot_cli::terminal_layout::TerminalText;
#[test]
fn status_output_preserves_all_lines_wrapping_and_sanitization() {
    let output = (1..=200)
        .map(|index| format!("Model {index}: {}\u{1b}", "x".repeat(100)))
        .collect::<Vec<_>>()
        .join("\n");
    let call = ToolCallPresentation::new("status", "{}");
    let terminal = ToolTerminalPresentation {
        result_present: true,
        result: output.clone(),
        ..Default::default()
    };
    for columns in [32, 512] {
        let context = ScrollbackRenderContext {
            columns,
            color: false,
            inline_diff: false,
            previous_packing: None,
        };
        let item = StatusToolPresenter
            .present_terminal(&call, &terminal)
            .unwrap()
            .unwrap();
        let rendered = ScrollbackItem::render(&item, context.clone()).unwrap();
        let mut expected = TerminalText::layout("✓ tool call status", columns);
        for line in output.replace('\u{1b}', "").split('\n') {
            expected.extend(
                TerminalText::layout(line, columns - 2)
                    .into_iter()
                    .map(|line| format!("  {line}")),
            );
        }
        assert_eq!(rendered, expected);
        assert!(rendered.len() > 200);
        let generic = GenericToolPresenter
            .present_terminal(&ToolCallPresentation::new("other", "{}"), &terminal)
            .unwrap()
            .unwrap();
        assert!(ScrollbackItem::render(&generic, context).unwrap().len() <= 10);
    }
}
#[test]
fn status_failure_uses_normal_error_block_not_unbounded_success_status() {
    let Presentation::Tool(report) = StatusToolPresenter
        .present_terminal(
            &ToolCallPresentation::new("status", "not JSON"),
            &ToolTerminalPresentation {
                result_present: true,
                result: "error: unavailable".into(),
                ..Default::default()
            },
        )
        .unwrap()
        .unwrap()
    else {
        panic!("tool")
    };
    assert_eq!(report.label, "tool call status");
    assert_eq!(report.status, Some(ToolTerminalStatus::ReportedFailure));
    assert_eq!(report.block.kind, ToolBlockKind::Error);
}
