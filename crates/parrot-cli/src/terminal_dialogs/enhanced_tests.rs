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
        Box::new(io::sink()),
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
