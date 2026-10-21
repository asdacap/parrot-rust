use parrot_cli::presenters::tools::queue_take::QueueTakeToolPresenter;
use parrot_cli::presenters::*;
use parrot_cli::terminal_frames::{ScrollbackItem, ScrollbackRenderContext};
fn render(args: &str, result: &str, width: usize) -> Vec<String> {
    let item = QueueTakeToolPresenter
        .present_terminal(
            &ToolCallPresentation::new("queue_take", args),
            &ToolTerminalPresentation {
                result_present: true,
                result: result.into(),
                ..Default::default()
            },
        )
        .unwrap()
        .unwrap();
    ScrollbackItem::render(
        &item,
        ScrollbackRenderContext {
            columns: width,
            color: false,
            inline_diff: false,
            previous_packing: None,
        },
    )
    .unwrap()
}
#[test]
fn queue_take_renders_remaining_items_metadata_and_default_open_state() {
    let result = r#"{"path":"/ignored","name":"work","description":"release tasks","size":2,"closed":true,"items":["first","second"]}"#;
    assert_eq!(
        render(r#"{"name":"work","count":5}"#, result, 512).join("|"),
        "✓ Take from queue work · up to 5 items · release tasks · closed|  2 remaining|  first|  second"
    );
    assert_eq!(
        render(
            r#"{"name":"work"}"#,
            r#"{"name":"work","size":0,"closed":false,"items":[]}"#,
            512
        )
        .join("|"),
        "✓ Take from queue work · up to 1 item|  0 remaining"
    );
    let Presentation::Tool(live) = QueueTakeToolPresenter
        .present_live(
            &ToolCallPresentation::new("queue_take", r#"{"name":"work","count":5}"#),
            0,
        )
        .unwrap()
    else {
        panic!("tool")
    };
    assert_eq!(live.label, "Take from queue work · up to 5 items");
    assert_eq!(live.block.kind, ToolBlockKind::None);
}
#[test]
fn queue_take_errors_use_input_name_and_error_block() {
    assert_eq!(
        render(
            r#"{"name":"work","count":3}"#,
            "error: queue: 'work' is unavailable",
            512
        ),
        vec![
            "✗ Take from queue work · up to 3 items",
            "  error: queue: 'work' is unavailable"
        ]
    );
}
#[test]
fn queue_take_uses_30_rendered_lines_sanitization_and_utf8_bound() {
    let result=serde_json::json!({"name":"work","size":40,"closed":false,"items":(1..=40).map(|n|format!("item {n}\u{1b}[2J")).collect::<Vec<_>>()}).to_string();
    let lines = render(r#"{"name":"work"}"#, &result, 512);
    assert_eq!(lines.len(), 30);
    assert!(lines[1].contains("40 remaining"));
    assert!(lines.last().unwrap().contains("lines truncated."));
    assert!(!lines.join("|").contains('\u{1b}'));
    let result =
        serde_json::json!({"name":"work","size":1,"closed":false,"items":["界".repeat(8000)]})
            .to_string();
    let lines = render(r#"{"name":"work"}"#, &result, 32768);
    assert!(lines.concat().len() <= 16 * 1024 + 64);
}
#[test]
fn queue_take_invalid_counts_and_malformed_input_results_reach_registry_as_errors() {
    for count in [
        serde_json::json!(0),
        serde_json::json!(-1),
        serde_json::json!(1.5),
        serde_json::json!("2"),
        serde_json::json!(2147483648_i64),
    ] {
        let args = serde_json::json!({"name":"work","count":count}).to_string();
        let call = ToolCallPresentation::new("queue_take", args);
        assert!(QueueTakeToolPresenter.present_live(&call, 0).is_err());
        assert!(
            QueueTakeToolPresenter
                .present_terminal(&call, &ToolTerminalPresentation::default())
                .is_err()
        );
    }
    for (args, result) in [
        (r#"{"name":false}"#, ""),
        (r#"{"name":"work"}"#, r#"{"name":"work"}"#),
        (
            r#"{"name":"work"}"#,
            r#"{"name":"work","size":0,"closed":false,"items":[3]}"#,
        ),
        (
            r#"{"name":"work"}"#,
            r#"{"name":"work","size":0,"closed":false,"items":[],"description":3}"#,
        ),
    ] {
        assert!(
            QueueTakeToolPresenter
                .present_terminal(
                    &ToolCallPresentation::new("queue_take", args),
                    &ToolTerminalPresentation {
                        result_present: true,
                        result: result.into(),
                        ..Default::default()
                    }
                )
                .is_err()
        );
    }
}
