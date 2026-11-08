use parrot_cli::presenters::{tools::read::ReadToolPresenter, *};
use parrot_cli::terminal_frames::{
    LiveBufferItem, LiveBufferRenderContext, ScrollbackItem, ScrollbackRenderContext,
};
fn rendered(
    call: &ToolCallPresentation,
    terminal: &ToolTerminalPresentation,
    color: bool,
) -> Vec<String> {
    let item = ReadToolPresenter
        .present_terminal(call, terminal)
        .unwrap()
        .unwrap();
    ScrollbackItem::render(
        &item,
        ScrollbackRenderContext {
            columns: 32768,
            color,
            inline_diff: true,
            previous_packing: None,
        },
    )
    .unwrap()
}
#[test]
fn missing_read_is_compact_and_keeps_failure_status() {
    for (arguments, label) in [
        (
            r#"{"path":"crates/parrot-domain/tests/protocol/fixtures/README.md"}"#,
            "read crates/parrot-domain/tests/protocol/fixtures/README.md",
        ),
        (
            r#"{"path":"missing-directory","offset":12,"limit":3}"#,
            "read L12-L14 missing-directory",
        ),
    ] {
        let call = ToolCallPresentation::new("read", arguments);
        let terminal = ToolTerminalPresentation {
            result_present: true,
            result: "error: no such file or directory".into(),
            ..Default::default()
        };
        assert_eq!(
            rendered(&call, &terminal, false),
            vec![format!("✗ {label} · not found")]
        );
        let Presentation::Tool(report) = ReadToolPresenter
            .present_terminal(&call, &terminal)
            .unwrap()
            .unwrap()
        else {
            panic!("tool")
        };
        assert_eq!(report.status, Some(ToolTerminalStatus::ReportedFailure));
        assert!(report.block.text.is_empty());
    }
}
#[test]
fn other_read_failures_show_full_request_yaml_not_raw_json() {
    let call = ToolCallPresentation::new("read", r#"{"path":"src/App.cs","offset":12,"limit":3}"#);
    for terminal in [
        ToolTerminalPresentation {
            result_present: true,
            result: "error: permission denied".into(),
            ..Default::default()
        },
        ToolTerminalPresentation {
            result_present: true,
            result: "error: no such file or directory\nadditional details".into(),
            ..Default::default()
        },
        ToolTerminalPresentation {
            status: ToolTerminalStatus::Errored,
            error: "unexpected failure".into(),
            ..Default::default()
        },
    ] {
        let expected = if terminal.result_present {
            &terminal.result
        } else {
            &terminal.error
        };
        let expected = expected.replace('\n', "\n  ");
        assert_eq!(
            rendered(&call, &terminal, false).join("\n"),
            format!(
                "✗ read L12-L14 src/App.cs\n  \"path\": \"src/App.cs\"\n  \"offset\": 12\n  \"limit\": 3\n  ---\n  {expected}"
            )
        );
        let live = ReadToolPresenter.present_live(&call, 0).unwrap();
        assert_eq!(
            LiveBufferItem::render(
                &live,
                LiveBufferRenderContext {
                    columns: 32768,
                    color: false
                }
            )
            .unwrap()
            .lines[0]
                .text(),
            "⠋ read L12-L14 src/App.cs"
        );
    }
}
#[test]
fn non_failed_read_is_compact_and_muted() {
    let call = ToolCallPresentation::new("read", r#"{"path":"README.md","offset":2,"limit":4}"#);
    assert_eq!(
        rendered(
            &call,
            &ToolTerminalPresentation {
                result_present: true,
                result: "1: heading\n2: body".into(),
                ..Default::default()
            },
            false
        ),
        vec!["✓ read L2-L5 README.md"]
    );
    assert_eq!(
        rendered(
            &call,
            &ToolTerminalPresentation {
                status: ToolTerminalStatus::Cancelled,
                ..Default::default()
            },
            false
        ),
        vec!["■ read L2-L5 README.md"]
    );
    assert_eq!(
        ReadToolPresenter.metadata().style,
        ToolPresentationStyle::Muted
    );
    let colored = rendered(&call, &Default::default(), true);
    assert!(colored[0].contains('\u{1b}'));
}
#[test]
fn read_range_labels_use_inclusive_bounds_and_tool_defaults() {
    for (arguments, label) in [
        (
            r#"{"path":"README.md","offset":12,"limit":3}"#,
            "read L12-L14 README.md",
        ),
        (
            r#"{"path":"README.md","offset":12}"#,
            "read L12-L2011 README.md",
        ),
        (r#"{"path":"README.md","limit":3}"#, "read L1-L3 README.md"),
        (
            r#"{"path":"README.md","offset":12,"limit":1}"#,
            "read L12-L12 README.md",
        ),
        (
            r#"{"path":"README.md","offset":2147483647,"limit":2000}"#,
            "read L2147483647-L2147485646 README.md",
        ),
        (r#"{"path":"src"}"#, "read src"),
        (r#"{"path":"src","offset":null,"limit":null}"#, "read src"),
    ] {
        let call = ToolCallPresentation::new("read", arguments);
        let Presentation::Tool(live) = ReadToolPresenter.present_live(&call, 0).unwrap() else {
            panic!("tool")
        };
        assert_eq!(live.label, label);
        assert!(live.block.text.is_empty());
        let Presentation::Tool(terminal) = ReadToolPresenter
            .present_terminal(&call, &Default::default())
            .unwrap()
            .unwrap()
        else {
            panic!("tool")
        };
        assert_eq!(terminal.label, label);
        assert!(terminal.block.text.is_empty());
    }
}
#[test]
fn malformed_arguments_return_errors() {
    let call = ToolCallPresentation::new("read", "{");
    assert!(ReadToolPresenter.present_live(&call, 0).is_err());
    assert!(
        ReadToolPresenter
            .present_terminal(&call, &Default::default())
            .is_err()
    );
}
