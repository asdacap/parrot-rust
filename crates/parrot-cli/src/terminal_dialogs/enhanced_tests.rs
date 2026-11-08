//! Focused driver assertions for source-owned frame and sink-failure predicates.
//! These use the real FrameView, T73 presenter, and T71 renderer; no key is read.

use super::*;
use crate::terminal_frames::ScrollbackRenderContext;
use std::sync::atomic::AtomicUsize;

#[test]
fn enhanced_run_future_preserves_existing_send_contract() {
    fn assert_send<T: Send>(_: T) {}
    fn check(cli: EnhancedCli, cancellation: &dyn Cancellation) {
        assert_send(cli.run(cancellation));
    }
    let _ = check;
}

#[derive(Clone, Default)]
struct Capture(Arc<Mutex<Vec<u8>>>);

impl Capture {
    fn text(&self) -> String {
        String::from_utf8(self.0.lock().unwrap().clone()).unwrap()
    }
}

impl Write for Capture {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

struct UnusedFrameInput(Arc<AtomicUsize>);

impl crate::terminal_dialogs::TerminalRead for UnusedFrameInput {
    fn read(
        &mut self,
        _: &mut [u8],
        _: &AtomicBool,
    ) -> io::Result<crate::terminal_input::ReadResult> {
        self.0.fetch_add(1, Ordering::SeqCst);
        Err(io::Error::other("frame-only input must not be started"))
    }

    fn restore(&mut self) -> io::Result<()> {
        Ok(())
    }
}

fn frame_view(
    columns: usize,
    input: Vec<Arc<dyn LiveBufferItem>>,
) -> (FrameView, Capture, Arc<AtomicUsize>) {
    let capture = Capture::default();
    let reads = Arc::new(AtomicUsize::new(0));
    let terminal = EnhancedTerminal::with_input(
        Box::new(UnusedFrameInput(reads.clone())),
        Box::new(capture.clone()),
        Box::new(capture.clone()),
        Arc::new(move || (columns as isize, 30)),
        false,
    );
    let frame = FrameView {
        renderer: TerminalFrameRenderer::new(terminal.output, false, 1, 1, true),
        state: Mutex::new(FrameState {
            body: Vec::new(),
            pending: VecDeque::new(),
            input,
        }),
        dimensions: terminal.dimensions,
        error: terminal.error,
    };
    (frame, capture, reads)
}

fn prompt(prefix: &str, text: &str, cursor: isize) -> Arc<dyn LiveBufferItem> {
    Arc::new(PromptValue(PromptState {
        prefix: prefix.into(),
        text: text.into(),
        cursor,
    }))
}

#[test]
fn frame_copies_owned_input_and_places_modeline_before_prompt() {
    // EnhancedRenderingSessionTests.Frame_copies_owned_input_and_places_modeline_before_prompt.
    let mut original = vec![prompt("first> ", "draft", 0)];
    let (view, output, reads) = frame_view(80, original.clone());
    original[0] = prompt("changed> ", "not owned", 0);
    let mut presenter = EnhancedSessionPresenter::new(80, false, false);
    view.apply(presenter.set_session("build", "provider/model").unwrap())
        .unwrap();

    let frame = output.text();
    let modeline = frame.find("mode: build").expect("modeline must be drawn");
    let first_prompt = frame
        .find("first> draft")
        .expect("the copied input must be drawn");
    assert!(first_prompt > modeline, "modeline precedes owned prompt");
    assert!(!frame.contains("changed>"));
    assert!(!frame.contains("not owned"));
    assert_eq!(reads.load(Ordering::SeqCst), 0);
}

#[test]
fn question_countdown_preserves_input_and_clears_without_another_key() {
    // EnhancedRenderingSessionTests.Question_countdown_preserves_input_and_clears_without_another_key.
    // Both original Arguments rows are retained, not replaced with a wide-only case.
    for columns in [20, 80] {
        let (view, output, reads) = frame_view(columns, vec![prompt("> ", "typed answer", 4)]);
        let mut presenter = EnhancedSessionPresenter::new(columns, false, false);
        presenter.set_session("build", "provider/model").unwrap();
        view.apply(presenter.set_question_countdown(Some(61)).unwrap())
            .unwrap();
        assert!(
            output.text().contains("Auto-return in 1:01"),
            "width {columns}"
        );
        assert!(output.text().contains("typed answer"), "width {columns}");

        view.apply(presenter.set_question_countdown(Some(60)).unwrap())
            .unwrap();
        assert!(
            output.text().contains("Auto-return in 1:00"),
            "width {columns}"
        );
        let before_clear = output.text().len();
        view.apply(presenter.set_question_countdown(None).unwrap())
            .unwrap();
        let after_clear = output.text();
        let cleared = &after_clear[before_clear..];
        assert!(!cleared.contains("Auto-return in"), "width {columns}");
        assert!(cleared.contains("typed answer"), "width {columns}");
        assert_eq!(reads.load(Ordering::SeqCst), 0, "no key at width {columns}");
    }
}

struct CommitItem {
    text: &'static str,
    fail_first: bool,
    rendered: AtomicUsize,
}

impl CommitItem {
    fn new(text: &'static str, fail_first: bool) -> Arc<Self> {
        Arc::new(Self {
            text,
            fail_first,
            rendered: AtomicUsize::new(0),
        })
    }
}

impl ScrollbackItem for CommitItem {
    fn is_completed(&self) -> bool {
        true
    }

    fn continues(&self, _: &dyn ScrollbackItem) -> bool {
        false
    }

    fn render(&self, _: ScrollbackRenderContext) -> io::Result<Vec<String>> {
        let invocation = self.rendered.fetch_add(1, Ordering::SeqCst);
        if self.fail_first && invocation == 0 {
            return Err(io::Error::other("injected scrollback sink render failure"));
        }
        Ok(vec![self.text.into()])
    }
}

#[test]
fn deferred_commits_wait_for_publication_and_survive_clear() {
    let (view, output, _) = frame_view(80, vec![prompt("> ", "draft", 0)]);
    let first = CommitItem::new("deferred-first", false);
    let second = CommitItem::new("deferred-second", false);
    for item in [&first, &second] {
        view.enqueue(PresentationUpdate {
            commits: vec![item.clone()],
            ..Default::default()
        })
        .unwrap();
    }
    assert!(output.text().is_empty());
    assert_eq!(first.rendered.load(Ordering::SeqCst), 0);
    assert_eq!(second.rendered.load(Ordering::SeqCst), 0);
    view.replace_input(vec![prompt("> ", "responsive", 0)])
        .unwrap();
    assert!(output.text().contains("responsive"));
    assert!(!output.text().contains("deferred-first"));
    view.clear().unwrap();
    assert_eq!(output.text().matches("deferred-first").count(), 1);
    assert_eq!(output.text().matches("deferred-second").count(), 1);
    assert!(view.state.lock().unwrap().pending.is_empty());
}

#[test]
fn failed_commit_retains_same_arc_and_does_not_replay_successful_prefix() {
    let (view, output, reads) = frame_view(80, vec![prompt("> ", "draft", 0)]);
    let first = CommitItem::new("commit-first", false);
    let second = CommitItem::new("commit-second", true);
    let third = CommitItem::new("commit-third", false);
    let second_item: Arc<dyn ScrollbackItem> = second.clone();
    let third_item: Arc<dyn ScrollbackItem> = third.clone();
    let result = view.apply(PresentationUpdate {
        commits: vec![first.clone(), second_item.clone(), third_item.clone()],
        ..Default::default()
    });
    assert_eq!(
        result.unwrap_err().to_string(),
        "injected scrollback sink render failure"
    );
    assert_eq!(first.rendered.load(Ordering::SeqCst), 1);
    assert_eq!(second.rendered.load(Ordering::SeqCst), 1);
    assert_eq!(third.rendered.load(Ordering::SeqCst), 0);
    {
        let state = view.state.lock().unwrap();
        assert_eq!(state.pending.len(), 2);
        assert!(Arc::ptr_eq(&state.pending[0], &second_item));
        assert!(Arc::ptr_eq(&state.pending[1], &third_item));
    }
    assert_eq!(output.text().matches("commit-first").count(), 1);
    assert!(!output.text().contains("commit-second"));
    assert!(!output.text().contains("commit-third"));

    // clear invokes the actual retained-batch retry path, without observing an
    // event again or reconstructing any item that the presenter already drained.
    view.clear().unwrap();
    assert!(view.state.lock().unwrap().pending.is_empty());
    assert_eq!(first.rendered.load(Ordering::SeqCst), 1);
    assert_eq!(second.rendered.load(Ordering::SeqCst), 2);
    assert_eq!(third.rendered.load(Ordering::SeqCst), 1);
    let output = output.text();
    for text in ["commit-first", "commit-second", "commit-third"] {
        assert_eq!(
            output.matches(text).count(),
            1,
            "committed exactly once: {text}"
        );
    }
    assert_eq!(reads.load(Ordering::SeqCst), 0);
}

#[test]
fn deferred_failures_flush_queued_reasoning_before_errors_and_remove_the_live_row() {
    let (view, output, reads) = frame_view(80, vec![prompt("> ", "draft", 0)]);
    let mut presenter = EnhancedSessionPresenter::new(80, false, false);
    view.apply(
        presenter
            .observe(&wire::Event {
                agent_session_id: "root".into(),
                payload: Some(wire::event::Payload::TurnStarted(
                    wire::TurnStarted::default(),
                )),
                ..Default::default()
            })
            .unwrap(),
    )
    .unwrap();
    view.apply(
        presenter
            .observe(&wire::Event {
                agent_session_id: "root".into(),
                payload: Some(wire::event::Payload::ReasoningChunk(wire::ReasoningChunk {
                    kind: wire::ReasoningKind::REASONING_KIND_SUMMARY.into(),
                    fragment: "Planning".into(),
                    completed: true,
                    ..Default::default()
                })),
                ..Default::default()
            })
            .unwrap(),
    )
    .unwrap();
    assert!(output.text().contains("✦ Planning"));
    output.0.lock().unwrap().clear();
    view.enqueue(PresentationUpdate {
        commits: vec![CommitItem::new("queued prefix", false)],
        ..Default::default()
    })
    .unwrap();
    assert!(output.text().is_empty());
    let mut failed = presenter
        .apply_event(&wire::Event {
            agent_session_id: "root".into(),
            payload: Some(wire::event::Payload::TurnFailed(wire::TurnFailed {
                message: "reported failure".into(),
                ..Default::default()
            })),
            ..Default::default()
        })
        .unwrap();
    assert!(failed.live.is_empty());
    let error = failed.errors[0].clone();
    failed.live = presenter.snapshot().unwrap();
    view.enqueue(failed).unwrap();
    let flushed = output.text();
    assert_eq!(flushed.matches("queued prefix").count(), 1);
    assert_eq!(flushed.matches("✦ Planning").count(), 1);
    assert_eq!(flushed.matches(&error).count(), 1);
    assert!(flushed.find("queued prefix").unwrap() < flushed.find("✦ Planning").unwrap());
    assert!(flushed.find("✦ Planning").unwrap() < flushed.find(&error).unwrap());
    {
        let state = view.state.lock().unwrap();
        assert!(state.pending.is_empty());
        for item in &state.body {
            let rendered = item
                .render(LiveBufferRenderContext {
                    columns: 80,
                    color: false,
                })
                .unwrap();
            assert!(
                rendered
                    .lines
                    .iter()
                    .all(|line| !line.text().contains("Planning"))
            );
        }
    }
    let before_input = output.text().len();
    view.replace_input(vec![prompt("> ", "still responsive", 0)])
        .unwrap();
    assert!(!output.text()[before_input..].contains("Planning"));
    assert_eq!(reads.load(Ordering::SeqCst), 0);
}

#[test]
fn pending_reasoning_is_published_live_then_committed_once_before_dialog_output() {
    let (view, output, reads) = frame_view(80, vec![prompt("> ", "draft", 0)]);
    let mut presenter = EnhancedSessionPresenter::new(80, false, false);
    view.enqueue(
        presenter
            .apply_event(&wire::Event {
                agent_session_id: "root".into(),
                payload: Some(wire::event::Payload::TurnStarted(
                    wire::TurnStarted::default(),
                )),
                ..Default::default()
            })
            .unwrap(),
    )
    .unwrap();
    for fragment in ["A", "B"] {
        let update = presenter
            .apply_event(&wire::Event {
                agent_session_id: "root".into(),
                payload: Some(wire::event::Payload::ReasoningChunk(wire::ReasoningChunk {
                    kind: wire::ReasoningKind::REASONING_KIND_SUMMARY.into(),
                    fragment: fragment.into(),
                    completed: true,
                    ..Default::default()
                })),
                ..Default::default()
            })
            .unwrap();
        assert!(update.commits.is_empty());
        assert!(update.live.is_empty());
        view.enqueue(update).unwrap();
    }
    assert!(output.text().is_empty());
    view.apply(PresentationUpdate {
        live: presenter.snapshot().unwrap(),
        ..Default::default()
    })
    .unwrap();
    assert!(output.text().contains("✦ A ✦ B"));
    assert!(output.text().contains("> draft"));
    output.0.lock().unwrap().clear();
    let update = presenter
        .commit(Arc::new(
            crate::presenters::ImmediateScrollbackValue::trusted(vec!["dialog output".into()]),
        ))
        .unwrap();
    view.apply(update).unwrap();
    let flushed = output.text();
    assert_eq!(flushed.matches("✦ A ✦ B").count(), 1);
    assert_eq!(flushed.matches("dialog output").count(), 1);
    assert!(flushed.find("✦ A ✦ B").unwrap() < flushed.find("dialog output").unwrap());
    assert!(flushed.contains("> draft"));
    {
        let state = view.state.lock().unwrap();
        assert!(state.pending.is_empty());
        for item in &state.body {
            let rendered = item
                .render(LiveBufferRenderContext {
                    columns: 80,
                    color: false,
                })
                .unwrap();
            assert!(rendered.lines.iter().all(|line| !line.text().contains('✦')));
        }
    }
    let before_finish = output.text().len();
    view.apply(presenter.finish().unwrap()).unwrap();
    assert!(!output.text()[before_finish..].contains("✦ A ✦ B"));
    assert_eq!(reads.load(Ordering::SeqCst), 0);
}

#[test]
fn replacement_cleanup_commits_unfinished_reasoning_without_leaving_it_live() {
    let (view, output, reads) = frame_view(80, vec![prompt("> ", "draft", 0)]);
    let mut old = EnhancedSessionPresenter::new(80, false, false);
    view.apply(
        old.observe(&wire::Event {
            agent_session_id: "root".into(),
            payload: Some(wire::event::Payload::TurnStarted(
                wire::TurnStarted::default(),
            )),
            ..Default::default()
        })
        .unwrap(),
    )
    .unwrap();
    view.apply(
        old.observe(&wire::Event {
            agent_session_id: "root".into(),
            payload: Some(wire::event::Payload::ReasoningChunk(wire::ReasoningChunk {
                kind: wire::ReasoningKind::REASONING_KIND_SUMMARY.into(),
                fragment: "Old planning".into(),
                ..Default::default()
            })),
            ..Default::default()
        })
        .unwrap(),
    )
    .unwrap();
    assert!(output.text().contains("✦ Old planning"));
    output.0.lock().unwrap().clear();
    view.apply(old.cancel().unwrap()).unwrap();
    assert!(old.cancel().unwrap().commits.is_empty());
    let mut replacement = EnhancedSessionPresenter::new(80, false, false);
    view.apply(replacement.set_session("build", "replacement").unwrap())
        .unwrap();
    assert_eq!(output.text().matches("✦ Old planning").count(), 1);
    assert!(!replacement.snapshot().unwrap().iter().any(|item| {
        item.render(LiveBufferRenderContext {
            columns: 80,
            color: false,
        })
        .unwrap()
        .lines
        .iter()
        .any(|line| line.text().contains("Old planning"))
    }));
    assert_eq!(reads.load(Ordering::SeqCst), 0);
}

#[derive(Default)]
struct CountedLiveValue {
    rendered: AtomicUsize,
    tick: AtomicUsize,
}
impl LiveBufferItem for CountedLiveValue {
    fn render(&self, context: LiveBufferRenderContext) -> io::Result<LiveBufferRender> {
        self.rendered.fetch_add(1, Ordering::SeqCst);
        Ok(LiveBufferRender {
            lines: vec![crate::terminal_frames::TerminalLine::new(
                format!(
                    "tick-{} width-{}",
                    self.tick.load(Ordering::SeqCst),
                    context.columns
                ),
                crate::terminal_frames::TerminalStyle::default(),
            )],
            caret: None,
            retention: crate::terminal_frames::LiveBufferRetention::Fixed,
        })
    }
}

#[test]
fn body_render_cache_survives_input_changes_and_commits_but_invalidates_on_resize() {
    let (mut view, _, reads) = frame_view(80, vec![prompt("> ", "draft", 0)]);
    let columns = Arc::new(AtomicUsize::new(80));
    let dimensions = columns.clone();
    view.dimensions = Arc::new(move || (dimensions.load(Ordering::SeqCst) as isize, 30));
    let body = Arc::new(CountedLiveValue::default());
    view.apply(PresentationUpdate {
        live: vec![body.clone()],
        commits: vec![CommitItem::new("first", false)],
        ..Default::default()
    })
    .unwrap();
    assert_eq!(body.rendered.load(Ordering::SeqCst), 1);
    view.replace_input(vec![prompt("> ", "draft changed", 3)])
        .unwrap();
    view.enqueue(PresentationUpdate {
        commits: vec![CommitItem::new("second", false)],
        ..Default::default()
    })
    .unwrap();
    view.flush_commits(&mut view.state.lock().unwrap()).unwrap();
    assert_eq!(body.rendered.load(Ordering::SeqCst), 1);

    columns.store(40, Ordering::SeqCst);
    view.replace_input(vec![prompt("> ", "draft changed", 4)])
        .unwrap();
    assert_eq!(body.rendered.load(Ordering::SeqCst), 2);
    view.replace_input(vec![prompt("> ", "draft changed", 5)])
        .unwrap();
    assert_eq!(body.rendered.load(Ordering::SeqCst), 2);
    view.apply(PresentationUpdate {
        live: vec![body.clone()],
        ..Default::default()
    })
    .unwrap();
    assert_eq!(body.rendered.load(Ordering::SeqCst), 3);
    assert_eq!(reads.load(Ordering::SeqCst), 0);
}

#[test]
fn fresh_body_publication_refreshes_time_dependent_item_even_with_the_same_arc() {
    let (view, output, _) = frame_view(80, vec![prompt("> ", "draft", 0)]);
    let body = Arc::new(CountedLiveValue::default());
    view.apply(PresentationUpdate {
        live: vec![body.clone()],
        ..Default::default()
    })
    .unwrap();
    assert!(output.text().contains("tick-0"));

    body.tick.store(1, Ordering::SeqCst);
    view.replace_input(vec![prompt("> ", "typed", 0)]).unwrap();
    assert_eq!(body.rendered.load(Ordering::SeqCst), 1);
    assert!(!output.text().contains("tick-1"));
    view.apply(PresentationUpdate {
        live: vec![body.clone()],
        ..Default::default()
    })
    .unwrap();
    assert_eq!(body.rendered.load(Ordering::SeqCst), 2);
    assert!(output.text().contains("tick-1"));
}

#[test]
fn progress_snapshot_redraw_preserves_modeline_and_owned_input_without_a_key() {
    use parrot_domain::values::progress::{PROGRESS_INVENTORY_KIND, Progress, ProgressSnapshot};
    let (view, output, reads) = frame_view(80, vec![prompt("> ", "draft", 2)]);
    let mut presenter = EnhancedSessionPresenter::new(80, false, false);
    view.apply(presenter.set_session("build", "provider/model").unwrap())
        .unwrap();
    view.apply(
        presenter
            .observe(&wire::Event {
                agent_session_id: "root".into(),
                payload: Some(wire::event::Payload::AgentStarted(wire::AgentStarted {
                    name: "main".into(),
                    ..Default::default()
                })),
                ..Default::default()
            })
            .unwrap(),
    )
    .unwrap();
    let mut snapshot = ProgressSnapshot {
        revision: 1,
        progress: Some(Progress {
            steps: 2,
            total_steps: 5,
            current_step: Some("Running tests".into()),
        }),
        ..ProgressSnapshot::new("root".into())
    };
    for closed in [false, true] {
        if closed {
            snapshot.revision = 2;
            snapshot.progress = None;
        }
        let before = output.text().len();
        view.apply(
            presenter
                .observe(&wire::Event {
                    agent_session_id: "root".into(),
                    payload: Some(wire::event::Payload::ToolSnapshot(wire::ToolSnapshot {
                        kind: PROGRESS_INVENTORY_KIND.into(),
                        owner_agent_session_id: "root".into(),
                        inventory_instance_id: "scope".into(),
                        revision: snapshot.revision,
                        final_chunk: true,
                        data: serde_json::to_vec(&snapshot).unwrap(),
                        ..Default::default()
                    })),
                    ..Default::default()
                })
                .unwrap(),
        )
        .unwrap();
        let rendered = output.text();
        let redraw = &rendered[before..];
        assert!(redraw.contains("mode: build"));
        assert!(redraw.contains("> draft"));
        assert_eq!(redraw.contains("2/5"), !closed);
        assert_eq!(redraw.contains("40% Running tests"), !closed);
    }
    assert_eq!(reads.load(Ordering::SeqCst), 0);
}
