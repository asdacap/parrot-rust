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
fn failed_read_shows_full_request_yaml_not_raw_json() {
    let call = ToolCallPresentation::new("read", r#"{"path":"src/App.cs","offset":12,"limit":3}"#);
    for terminal in [
        ToolTerminalPresentation {
            result_present: true,
            result: "error: no such file or directory".into(),
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
        assert_eq!(
            rendered(&call, &terminal, false).join("\n"),
            format!(
                "✗ read src/App.cs\n  path: \"src/App.cs\"\n  offset: 12\n  limit: 3\n  ---\n  {expected}"
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
            "⠋ read src/App.cs"
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
        vec!["✓ read README.md"]
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
        vec!["■ read README.md"]
    );
    assert_eq!(
        ReadToolPresenter.metadata().style,
        ToolPresentationStyle::Muted
    );
    let colored = rendered(&call, &Default::default(), true);
    assert!(colored[0].contains('\u{1b}'));
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
