use parrot_cli::presenters::tools::clipboard::ClipboardToolPresenter;
use parrot_cli::presenters::*;
use parrot_cli::terminal_frames::ScrollbackLayout;

fn call(arguments: &str) -> ToolCallPresentation {
    ToolCallPresentation::new("clipboard_copy", arguments)
}

fn finished(status: ToolTerminalStatus, result: &str, error: &str) -> ToolTerminalPresentation {
    ToolTerminalPresentation {
        status,
        result_present: !result.is_empty(),
        result: result.to_owned(),
        error: error.to_owned(),
        ..Default::default()
    }
}

fn render_lines(update: Option<Presentation>) -> Vec<String> {
    match update {
        Some(Presentation::Lines { lines, layout }) => {
            assert_eq!(layout, ScrollbackLayout::Compact);
            lines
        }
        None => Vec::new(),
        other => panic!("expected dedicated lines, got {other:?}"),
    }
}

#[test]
fn live_label_counts_argument_bytes_without_echoing_the_text() {
    let presenter = ClipboardToolPresenter;
    let live = presenter
        .present_live(&call(r#"{"text":"Hello from parrot"}"#), 3)
        .unwrap();
    match live {
        Presentation::Tool(report) => {
            assert_eq!(
                report.label,
                "clipboard_copy · copying 17 bytes to the clipboard"
            );
            assert!(report.status.is_none());
            assert_eq!(report.frame, 3);
        }
        other => panic!("expected a live tool report, got {other:?}"),
    }
    let live = presenter.present_live(&call("{}"), 0).unwrap();
    match live {
        Presentation::Tool(report) => assert_eq!(report.label, "clipboard_copy"),
        other => panic!("expected a live tool report, got {other:?}"),
    }
}

#[test]
fn terminal_success_renders_one_muted_dedicated_line() {
    let presenter = ClipboardToolPresenter;
    let lines = render_lines(
        presenter
            .present_terminal(
                &call(r#"{"text":"Hello from parrot"}"#),
                &finished(
                    ToolTerminalStatus::Succeeded,
                    "Copied 17 bytes to the clipboard.",
                    "",
                ),
            )
            .unwrap(),
    );
    assert_eq!(
        lines,
        vec!["\x1b[2m↻ Copied 17 bytes to the clipboard\x1b[0m"]
    );
}

#[test]
fn terminal_failure_renders_the_failure_line_and_cancelled_renders_nothing() {
    let presenter = ClipboardToolPresenter;
    let lines = render_lines(
        presenter
            .present_terminal(
                &call(r#"{"text":"hello"}"#),
                &finished(
                    ToolTerminalStatus::Succeeded,
                    "error: text is 1048577 bytes; the clipboard cap is 1048576 bytes.",
                    "",
                ),
            )
            .unwrap(),
    );
    assert_eq!(
        lines,
        vec![
            "\x1b[2m↻ clipboard copy failed: error: text is 1048577 bytes; the clipboard cap is 1048576 bytes.\x1b[0m"
        ]
    );
    let lines = render_lines(
        presenter
            .present_terminal(
                &call(r#"{"text":"hello"}"#),
                &finished(
                    ToolTerminalStatus::Errored,
                    "",
                    "clipboard notice publication failed: broker closed",
                ),
            )
            .unwrap(),
    );
    assert_eq!(
        lines,
        vec![
            "\x1b[2m↻ clipboard copy failed: clipboard notice publication failed: broker closed\x1b[0m"
        ]
    );
    let update = presenter
        .present_terminal(
            &call(r#"{"text":"hello"}"#),
            &finished(ToolTerminalStatus::Cancelled, "", ""),
        )
        .unwrap();
    assert!(update.is_none());
}
