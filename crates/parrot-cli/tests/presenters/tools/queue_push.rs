use parrot_cli::presenters::{tools::queue_push::QueuePushToolPresenter, *};
use parrot_cli::terminal_frames::{
    LiveBufferItem, LiveBufferRenderContext, ScrollbackItem, ScrollbackRenderContext,
};
fn rows(arguments: &str, result: &str, columns: usize) -> Vec<String> {
    let item = QueuePushToolPresenter
        .present_terminal(
            &ToolCallPresentation::new("queue_push", arguments),
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
            columns,
            color: false,
            inline_diff: true,
            previous_packing: None,
        },
    )
    .unwrap()
}
#[test]
fn items_and_source_path_render_open_closed_without_file_reads() {
    assert_eq!(
        rows(r#"{"name":"work","items":["first","second"]}"#, "{}", 512).join("|"),
        "✓ Push to queue work · open|  first|  second"
    );
    for (arguments, expected) in [
        (
            r#"{"name":"work","items":["final"],"close":true}"#,
            "Push to queue work · closed|  final",
        ),
        (
            r#"{"name":"work","source_file":"tasks/items.txt","close":true}"#,
            "Push to queue work · closed|  source_file: tasks/items.txt",
        ),
    ] {
        assert_eq!(
            rows(arguments, "{}", 512).join("|"),
            format!("✓ {expected}")
        );
        let item = QueuePushToolPresenter
            .present_live(&ToolCallPresentation::new("queue_push", arguments), 0)
            .unwrap();
        let live = LiveBufferItem::render(
            &item,
            LiveBufferRenderContext {
                columns: 512,
                color: false,
            },
        )
        .unwrap();
        assert_eq!(
            live.lines
                .iter()
                .map(|line| line.text().to_owned())
                .collect::<Vec<_>>()
                .join("|"),
            format!("⠋ {expected}")
        );
    }
}
#[test]
fn failure_hides_source_file_details_and_preserves_closed_state() {
    let failed = rows(
        r#"{"name":"work","source_file":"private.txt"}"#,
        "error: access denied",
        512,
    );
    assert_eq!(failed[0], "✗ Push to queue work · open");
    assert!(failed.join("|").contains("error: access denied"));
    assert!(!failed.join("|").contains("private.txt"));
    assert_eq!(
        rows(
            r#"{"name":"work","items":["again"]}"#,
            "error: queue: 'work' is closed",
            512
        ),
        vec![
            "✗ Push to queue work · closed",
            "  error: queue: 'work' is closed"
        ]
    );
}
#[test]
fn thirty_rows_sanitization_and_utf8_byte_bound() {
    let items = (1..=40)
        .map(|i| format!("item {i}\u{1b}[2J"))
        .collect::<Vec<_>>();
    let text = rows(
        &serde_json::json!({"name":"work","items":items}).to_string(),
        "{}",
        512,
    );
    assert_eq!(text.len(), 30);
    assert!(text[0].contains("open"));
    assert!(text.last().unwrap().contains("lines truncated."));
    assert!(!text.join("|").contains('\u{1b}'));
    let large = rows(
        &serde_json::json!({"name":"work","items":["界".repeat(8000)]}).to_string(),
        "{}",
        32768,
    );
    assert!(large.concat().len() <= (16 * 1024) + 64);
}
#[test]
fn malformed_arguments_are_errors_for_safe_registry_fallback() {
    for arguments in [
        r#"{"name":"work"}"#,
        r#"{"name":"work","items":[],"source_file":"items.txt"}"#,
        r#"{"name":"work","source_file":42}"#,
        r#"{"name":"work","items":[1]}"#,
        r#"{"name":"work","items":[],"close":null}"#,
        r#"{"name":"","items":[]}"#,
        "{",
        "[]",
    ] {
        let call = ToolCallPresentation::new("queue_push", arguments);
        assert!(QueuePushToolPresenter.present_live(&call, 0).is_err());
        assert!(
            QueuePushToolPresenter
                .present_terminal(&call, &Default::default())
                .is_err()
        );
    }
}
