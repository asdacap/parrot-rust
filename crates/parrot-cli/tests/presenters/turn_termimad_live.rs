use std::sync::{Arc, Mutex};

use parrot_cli::{presenters::EnhancedTurnRenderer, terminal_layout::MarkdownRenderer};

use super::{RuntimeCancellation, child, commits, completed, ended, live, start, text};

#[derive(Clone, Copy, PartialEq)]
enum Ending {
    Complete,
    Eof,
    Cancel,
    Error,
}

async fn buffered_list_case(ending: Ending) {
    let mut renderer = EnhancedTurnRenderer::new(80, false);
    let token = Arc::new(RuntimeCancellation::default());
    let after_token = token.clone();
    let source = "1. first\n1. second\n";
    let mut fragment = text("root", source);
    fragment.id = "fragment".into();
    let mut events = vec![Ok(start("root")), Ok(fragment)];
    match ending {
        Ending::Complete => events.extend([Ok(ended("root")), Ok(completed("root"))]),
        Ending::Error => events.push(Err(tonic::Status::aborted("stream failed"))),
        Ending::Eof | Ending::Cancel => {}
    }
    let mut stream = futures_util::stream::iter(events);
    let output = Arc::new(Mutex::new(String::new()));
    let previews = Arc::new(Mutex::new(String::new()));
    let captured = output.clone();
    let captured_previews = previews.clone();
    let result = renderer
        .render_session_turn(
            &mut stream,
            |_, _| Box::pin(async { Ok(()) }),
            move |published, _| {
                if ending == Ending::Cancel && published.id == "fragment" {
                    after_token.cancel();
                }
                Box::pin(async { Ok(()) })
            },
            move |items, _| {
                captured_previews
                    .lock()
                    .unwrap()
                    .push_str(&live(&items, 80));
                Box::pin(async { Ok(()) })
            },
            move |item, _, cancellation| {
                assert!(!cancellation.is_cancelled());
                captured.lock().unwrap().push_str(&commits(&[item], 80));
                Box::pin(async { Ok(()) })
            },
            |errors, _| {
                assert!(errors.is_empty());
                Box::pin(async { Ok(()) })
            },
            || false,
            token.as_ref(),
        )
        .await;
    match ending {
        Ending::Complete => assert!(result.unwrap()),
        Ending::Eof | Ending::Cancel => assert!(!result.unwrap()),
        Ending::Error => {
            let error = result.unwrap_err();
            assert_eq!(error.code(), tonic::Code::Aborted);
            assert_eq!(error.message(), "stream failed");
        }
    }
    assert!(previews.lock().unwrap().contains("1. second"));
    let expected = MarkdownRenderer::render("● ", source, 80, false).join("\r\n") + "\r\n";
    assert_eq!(*output.lock().unwrap(), expected);
    assert!(renderer.snapshot().is_empty());
    assert!(renderer.finish().unwrap().commits.is_empty());
}

#[tokio::test(flavor = "current_thread")]
async fn buffered_lists_commit_once_on_response_completion() {
    buffered_list_case(Ending::Complete).await;
}

#[tokio::test(flavor = "current_thread")]
async fn buffered_lists_commit_once_on_eof() {
    buffered_list_case(Ending::Eof).await;
}

#[tokio::test(flavor = "current_thread")]
async fn buffered_lists_commit_once_on_cancellation() {
    buffered_list_case(Ending::Cancel).await;
}

#[tokio::test(flavor = "current_thread")]
async fn buffered_lists_commit_once_on_stream_error() {
    buffered_list_case(Ending::Error).await;
}

#[test]
fn child_completion_keeps_the_foreground_response_pending() {
    let mut renderer = EnhancedTurnRenderer::new(8, false);
    for published in [start("root"), child("root"), start("child"), ended("child")] {
        let update = renderer.observe(&published, false).unwrap();
        assert_eq!(update.completion, None);
        assert!(update.commits.is_empty());
    }
    let pending = renderer
        .observe(&text("root", "main continues"), false)
        .unwrap();
    assert!(pending.commits.is_empty());
    let committed = renderer.observe(&ended("root"), false).unwrap();
    assert_eq!(committed.completion, None);
    let expected = MarkdownRenderer::render("● ", "main continues", 8, false).join("\r\n") + "\r\n";
    assert_eq!(commits(&committed.commits, 80), expected);
    let done = renderer.observe(&completed("root"), false).unwrap();
    assert_eq!(done.completion, Some(true));
    assert!(done.errors.is_empty());
    assert!(done.commits.is_empty());
    assert!(done.live.is_empty());
}

#[test]
fn buffered_list_resize_uses_the_new_content_width() {
    let mut renderer = EnhancedTurnRenderer::new(80, false);
    renderer.observe(&start("root"), false).unwrap();
    let source = "1. abcdefghij\n1. klmnopqrst\n";
    let pending = renderer.observe(&text("root", source), false).unwrap();
    assert!(pending.commits.is_empty());
    renderer.resize(8);
    let update = renderer.observe(&completed("root"), false).unwrap();
    let expected = MarkdownRenderer::render("● ", source, 8, false).join("\r\n") + "\r\n";
    assert_eq!(commits(&update.commits, 80), expected);
    assert!(update.live.is_empty());
    assert_eq!(update.completion, Some(true));
    assert!(renderer.finish().unwrap().commits.is_empty());
}
