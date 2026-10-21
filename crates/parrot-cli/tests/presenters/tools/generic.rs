use parrot_cli::presenters::tools::generic::GenericToolPresenter;
use parrot_cli::presenters::*;
use parrot_cli::terminal_frames::{
    LiveBufferItem, LiveBufferRenderContext, ScrollbackItem, ScrollbackRenderContext,
};

fn terminal(result: &str) -> ToolTerminalPresentation {
    ToolTerminalPresentation {
        result_present: true,
        result: result.into(),
        ..Default::default()
    }
}
fn rendered(item: &Presentation) -> String {
    ScrollbackItem::render(
        item,
        ScrollbackRenderContext {
            columns: 32_768,
            color: false,
            inline_diff: false,
            previous_packing: None,
        },
    )
    .unwrap()
    .join("\n")
}

#[test]
fn generic_presenter_formats_json_inputs_and_results_as_yaml() {
    let presenter = GenericToolPresenter;
    let call = ToolCallPresentation::new(
        "unknown",
        r#"{"path":"src/App.cs","options":{"limit":2},"enabled":true,"ambiguous":"true"}"#,
    );
    let live = presenter.present_live(&call, 0).unwrap();
    let live = LiveBufferItem::render(
        &live,
        LiveBufferRenderContext {
            columns: 32_768,
            color: false,
        },
    )
    .unwrap()
    .lines
    .into_iter()
    .map(|line| line.text().to_owned())
    .collect::<Vec<_>>()
    .join("\n");
    let complete = rendered(
        &presenter
            .present_terminal(&call, &terminal(r#"[{"name":"first"},{"name":"second"}]"#))
            .unwrap()
            .unwrap(),
    );
    assert!(live.contains("path: \"src/App.cs\""));
    assert!(live.contains("limit: 2"));
    assert!(complete.contains("enabled: true"));
    assert!(complete.contains("ambiguous: \"true\""));
    assert!(complete.contains("  ---\n  - name: \"first\""));
    assert!(!complete.contains("{\""));
    let plain = rendered(
        &presenter
            .present_terminal(
                &ToolCallPresentation::new("unknown", "not json"),
                &terminal("plain result"),
            )
            .unwrap()
            .unwrap(),
    );
    assert_eq!(
        plain,
        "✓ tool call unknown\n  not json\n  ---\n  plain result"
    );
    let duplicate = presenter
        .present_live(
            &ToolCallPresentation::new("unknown", r#"{"value":1,"value":2}"#),
            0,
        )
        .unwrap();
    let Presentation::Tool(report) = duplicate else {
        panic!("expected tool report")
    };
    assert_eq!(report.block.text, r#"{"value":1,"value":2}"#);
}

#[test]
fn unknown_calls_retain_malformed_input_and_spill_notice() {
    let call = ToolCallPresentation::new("retired_tool", "not json");
    let notice = "Tool output exceeded 64 KiB and was saved to /tmp/output.";
    let text = rendered(
        &GenericToolPresenter
            .present_terminal(&call, &terminal(notice))
            .unwrap()
            .unwrap(),
    );
    assert!(text.contains("tool call retired_tool"));
    assert!(text.contains("not json"));
    assert!(text.contains(notice));
    let cancelled = ToolTerminalPresentation {
        status: ToolTerminalStatus::Cancelled,
        error: "reason".into(),
        ..Default::default()
    };
    let text = rendered(
        &GenericToolPresenter
            .present_terminal(&call, &cancelled)
            .unwrap()
            .unwrap(),
    );
    assert!(text.contains("tool call retired_tool cancelled"));
    assert!(text.contains("reason"));
}
