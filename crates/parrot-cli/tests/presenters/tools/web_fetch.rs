use parrot_cli::presenters::{tools::web_fetch::WebFetchToolPresenter, *};
use parrot_cli::terminal_frames::{
    LiveBufferItem, LiveBufferRenderContext, ScrollbackItem, ScrollbackRenderContext,
};
#[test]
fn schema_label_and_tool_errors_are_specific_and_muted() {
    let call = ToolCallPresentation::new("web_fetch", r#"{"url":"https://example.com/path"}"#);
    let presenter = WebFetchToolPresenter;
    let live = presenter.present_live(&call, 0).unwrap();
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
        "⠋ web fetch GET https://example.com/path"
    );
    let item = presenter
        .present_terminal(
            &call,
            &ToolTerminalPresentation {
                result_present: true,
                result: "error: tool failure".into(),
                ..Default::default()
            },
        )
        .unwrap()
        .unwrap();
    assert_eq!(
        ScrollbackItem::render(
            &item,
            ScrollbackRenderContext {
                columns: 32768,
                color: false,
                inline_diff: true,
                previous_packing: None
            }
        )
        .unwrap(),
        vec![
            "✗ web fetch GET https://example.com/path",
            "  error: tool failure"
        ]
    );
    assert_eq!(presenter.metadata().style, ToolPresentationStyle::Muted);
}
#[test]
fn method_is_uppercased_and_absent_or_invalid_url_is_empty() {
    for (arguments, label) in [
        (r#"{"url":"u","method":"head"}"#, "web fetch HEAD u"),
        (r#"{"url":"u","method":null}"#, "web fetch GET u"),
        ("[]", "web fetch "),
        (r#"{"url":1}"#, "web fetch "),
    ] {
        let Presentation::Tool(report) = WebFetchToolPresenter
            .present_live(&ToolCallPresentation::new("web_fetch", arguments), 0)
            .unwrap()
        else {
            panic!("expected tool");
        };
        assert_eq!(report.label, label);
    }
    let call = ToolCallPresentation::new("web_fetch", "{");
    assert!(WebFetchToolPresenter.present_live(&call, 0).is_err());
    assert!(
        WebFetchToolPresenter
            .present_terminal(&call, &Default::default())
            .is_err()
    );
}
