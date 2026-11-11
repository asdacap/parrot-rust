use parrot_cli::{presenters::EnhancedTurnRenderer, terminal_layout::MarkdownRenderer};

use super::{child, commits, completed, ended, live, start, text};

fn buffered_list_case(complete: bool) {
    let mut renderer = EnhancedTurnRenderer::new(80, false);
    let source = "1. first\n1. second\n";
    let mut output = String::new();
    for published in [start("root"), text("root", source)] {
        let update = renderer.apply_event(&published, false).unwrap();
        output.push_str(&commits(&update.commits, 80));
    }
    assert!(live(&renderer.snapshot(), 80).contains("1. second"));
    let mut completion = None;
    if complete {
        for published in [ended("root"), completed("root")] {
            let update = renderer.apply_event(&published, false).unwrap();
            output.push_str(&commits(&update.commits, 80));
            completion = update.completion.or(completion);
        }
    } else {
        output.push_str(&commits(&renderer.finish().unwrap().commits, 80));
    }
    assert_eq!(completion, complete.then_some(true));
    let expected = MarkdownRenderer::render("● ", source, 80, false).join("\r\n") + "\r\n";
    assert_eq!(output, expected);
    assert!(renderer.snapshot().is_empty());
    assert!(renderer.finish().unwrap().commits.is_empty());
}

#[test]
fn buffered_lists_commit_once_on_response_completion() {
    buffered_list_case(true);
}

#[test]
fn buffered_lists_commit_once_on_finish() {
    buffered_list_case(false);
}

#[test]
fn child_completion_keeps_the_foreground_response_pending() {
    let mut renderer = EnhancedTurnRenderer::new(8, false);
    for published in [start("root"), child("root"), start("child"), ended("child")] {
        let update = renderer.apply_event(&published, false).unwrap();
        assert_eq!(update.completion, None);
        assert!(update.commits.is_empty());
    }
    let pending = renderer
        .apply_event(&text("root", "main continues"), false)
        .unwrap();
    assert!(pending.commits.is_empty());
    let committed = renderer.apply_event(&ended("root"), false).unwrap();
    assert_eq!(committed.completion, None);
    let expected = MarkdownRenderer::render("● ", "main continues", 8, false).join("\r\n") + "\r\n";
    assert_eq!(commits(&committed.commits, 80), expected);
    let done = renderer.apply_event(&completed("root"), false).unwrap();
    assert_eq!(done.completion, Some(true));
    assert!(done.errors.is_empty());
    assert!(done.commits.is_empty());
    assert!(renderer.snapshot().is_empty());
}

#[test]
fn buffered_list_resize_uses_the_new_content_width() {
    let mut renderer = EnhancedTurnRenderer::new(80, false);
    renderer.apply_event(&start("root"), false).unwrap();
    let source = "1. abcdefghij\n1. klmnopqrst\n";
    let pending = renderer.apply_event(&text("root", source), false).unwrap();
    assert!(pending.commits.is_empty());
    renderer.resize(8);
    let update = renderer.apply_event(&completed("root"), false).unwrap();
    let expected = MarkdownRenderer::render("● ", source, 8, false).join("\r\n") + "\r\n";
    assert_eq!(commits(&update.commits, 80), expected);
    assert!(renderer.snapshot().is_empty());
    assert_eq!(update.completion, Some(true));
    assert!(renderer.finish().unwrap().commits.is_empty());
}
