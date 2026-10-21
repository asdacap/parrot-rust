use parrot_cli::terminal_layout::{EnhancedFinalMessageRenderer, MarkdownLiveRenderer};

#[test]
fn resize_uses_new_width_for_pending_commit() {
    let mut live = MarkdownLiveRenderer::new(80);
    let update = live
        .append("message", "P ", "alpha beta gamma", false)
        .unwrap();
    assert_eq!(update.preview, ["alpha beta gamma"]);

    live.set_width(5);
    let committed = live.commit(false).unwrap();
    let expected =
        EnhancedFinalMessageRenderer::render("alpha beta gamma", "P ", 5, false, false).unwrap();
    let old_width =
        EnhancedFinalMessageRenderer::render("alpha beta gamma", "P ", 80, false, false).unwrap();
    assert_eq!(committed.scrollback, Some(expected));
    assert_ne!(committed.scrollback, Some(old_width));
}

#[test]
fn resize_preserves_live_message_identity_prefix_and_started_state() {
    let mut live = MarkdownLiveRenderer::new(80);
    let first = live.append("message", "P ", "first line\n", false).unwrap();
    assert!(first.scrollback.is_some());
    let pending = live
        .append("message", "P ", "second pending line", false)
        .unwrap();
    assert_eq!(pending.preview, ["second pending line"]);

    live.set_width(5);
    assert_eq!(
        live.append("other message", "P ", "ignored", false)
            .unwrap_err(),
        "Another assistant message is still streaming."
    );
    assert_eq!(
        live.append("message", "different prefix", "ignored", false)
            .unwrap_err(),
        "The stream message prefix changed."
    );

    let committed = live.commit(false).unwrap();
    let expected =
        EnhancedFinalMessageRenderer::render("second pending line", "  ", 5, false, false).unwrap();
    assert_eq!(committed.scrollback, Some(expected));
}

#[test]
fn setter_clamps_zero_width_like_constructor() {
    let mut resized = MarkdownLiveRenderer::new(80);
    resized.append("message", "", "a b", false).unwrap();
    resized.set_width(0);

    let mut constructed_at_one = MarkdownLiveRenderer::new(1);
    constructed_at_one
        .append("message", "", "a b", false)
        .unwrap();

    assert_eq!(
        resized.commit(false).unwrap().scrollback,
        constructed_at_one.commit(false).unwrap().scrollback
    );
}
