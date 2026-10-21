use std::fmt::Write as _;
use std::io::{self, Write};
use std::sync::{Arc, Condvar, Mutex, mpsc};
use std::thread;
use std::time::Duration;

use parrot_cli::terminal_frames::{
    CancellationToken, ItemIdentity, LiveBufferCaret, LiveBufferItem, LiveBufferRender,
    LiveBufferRenderContext, LiveBufferRetention, ScrollbackItem, ScrollbackLayout,
    ScrollbackRenderContext, TerminalCellStyleSpan, TerminalFrameRenderer, TerminalLine,
    TerminalStyle,
};

#[derive(Clone, Default)]
struct Output(Arc<Mutex<OutputState>>);
#[derive(Default)]
struct OutputState {
    bytes: Vec<u8>,
    flushes: usize,
    writes: usize,
    fail_write: bool,
    fail_flush: bool,
}
impl Output {
    fn take(&self) -> String {
        String::from_utf8(std::mem::take(&mut self.0.lock().unwrap().bytes)).unwrap()
    }
}
impl Write for Output {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        let mut state = self.0.lock().unwrap();
        state.writes += 1;
        if std::mem::take(&mut state.fail_write) {
            return Err(io::Error::other("write failed"));
        }
        state.bytes.extend_from_slice(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        let mut state = self.0.lock().unwrap();
        state.flushes += 1;
        if std::mem::take(&mut state.fail_flush) {
            return Err(io::Error::other("flush failed"));
        }
        Ok(())
    }
}

#[derive(Clone)]
struct Live(LiveBufferRender);
impl LiveBufferItem for Live {
    fn render(&self, _: LiveBufferRenderContext) -> io::Result<LiveBufferRender> {
        Ok(self.0.clone())
    }
}
fn live(
    text: &[&str],
    retention: LiveBufferRetention,
    caret: Option<(isize, isize)>,
) -> Arc<dyn LiveBufferItem> {
    Arc::new(Live(LiveBufferRender {
        lines: text
            .iter()
            .map(|text| TerminalLine::new(*text, TerminalStyle::default()))
            .collect(),
        caret: caret.map(|(row, cells)| LiveBufferCaret { row, cells }),
        retention,
    }))
}
fn frame(text: &[&str]) -> Vec<Arc<dyn LiveBufferItem>> {
    vec![live(
        text,
        LiveBufferRetention::Fixed,
        Some((text.len() as isize - 1, 1)),
    )]
}

#[test]
fn draw_and_clear_exact_bytes_and_no_alternate_screen() {
    let output = Output::default();
    let renderer = TerminalFrameRenderer::new(output.clone(), false, 10, 12, true);
    let cancel = CancellationToken::new();
    renderer
        .draw(&frame(&["a\x1b[2J\nb", "界x"]), 5, 100, &cancel)
        .unwrap();
    assert_eq!(
        output.take(),
        "\x1b[?25l\x1b[?7l\r\n\x1b[1A\r\x1b[2Ka[2Jb\r\n\x1b[2K界x  \r\x1b[1C\x1b[?7h\x1b[?25h"
    );
    renderer.clear(&cancel).unwrap();
    assert_eq!(
        output.take(),
        "\x1b[?25l\x1b[?7l\r\x1b[1A\x1b[2K\r\n\x1b[2K\x1b[1A\r\x1b[?7h\x1b[?25h"
    );
    renderer.clear(&cancel).unwrap();
    assert_eq!(output.take(), "");
    assert_eq!(output.0.lock().unwrap().flushes, 3);
}

#[test]
fn identical_draw_is_silent_caret_only_moves_and_resize_repaints_all() {
    let output = Output::default();
    let renderer = TerminalFrameRenderer::new(output.clone(), false, 10, 12, true);
    let cancel = CancellationToken::new();
    let items = frame(&["one", "two"]);
    renderer.draw(&items, 8, 100, &cancel).unwrap();
    output.take();
    renderer.draw(&items, 8, 100, &cancel).unwrap();
    assert_eq!(output.take(), "");
    assert_eq!(output.0.lock().unwrap().flushes, 1);
    renderer
        .draw(
            &[live(
                &["one", "two"],
                LiveBufferRetention::Fixed,
                Some((0, 3)),
            )],
            8,
            100,
            &cancel,
        )
        .unwrap();
    let caret = output.take();
    assert!(!caret.contains("\x1b[2K"));
    assert!(caret.ends_with("\x1b[3C\x1b[?7h\x1b[?25h"));
    renderer.draw(&items, 5, 100, &cancel).unwrap();
    assert_eq!(output.take().matches("\x1b[2K").count(), 2);
}

#[test]
fn grow_and_shrink_preserve_unchanged_prefix_and_delete_rows() {
    let output = Output::default();
    let renderer = TerminalFrameRenderer::new(output.clone(), false, 10, 12, true);
    let cancel = CancellationToken::new();
    renderer
        .draw(&frame(&["one", "two"]), 8, 100, &cancel)
        .unwrap();
    output.take();
    renderer
        .draw(&frame(&["one", "two", "three"]), 8, 100, &cancel)
        .unwrap();
    let growth = output.take();
    assert_eq!(growth.matches("\x1b[2K").count(), 1);
    assert!(growth.contains("\x1b[2Kthree   "));
    renderer.draw(&frame(&["one"]), 8, 100, &cancel).unwrap();
    let shrink = output.take();
    assert!(!shrink.contains("\x1b[2K"));
    assert!(shrink.contains("\x1b[B\r\x1b[2M"));
}

#[test]
fn full_cell_styles_cover_combining_wide_zwj_and_padding() {
    let output = Output::default();
    let renderer = TerminalFrameRenderer::new(output.clone(), true, 10, 12, true);
    let cancel = CancellationToken::new();
    let base = TerminalStyle::new("\x1b[48;5;236m\x1b[38;5;252m");
    let red = TerminalStyle::new("\x1b[48;5;236m\x1b[31m");
    for (text, start, length, selected, suffix) in [
        ("e\u{301}x", 0, 1, "e\u{301}", "x"),
        ("界x", 0, 2, "界", "x"),
        ("👨‍👩‍👧‍👦ab", 0, 2, "👨‍👩‍👧‍👦", "ab"),
    ] {
        renderer.clear(&cancel).unwrap();
        output.take();
        let item = Arc::new(Live(LiveBufferRender {
            lines: vec![TerminalLine::with_spans(
                text,
                base.clone(),
                vec![TerminalCellStyleSpan::new(start, length, red.clone())],
            )],
            caret: Some(LiveBufferCaret { row: 0, cells: 0 }),
            retention: LiveBufferRetention::Fixed,
        }));
        renderer.draw(&[item], 6, 100, &cancel).unwrap();
        let rendered = output.take();
        assert!(
            rendered.contains(&format!(
                "\x1b[2K\x1b[48;5;236m\x1b[0m{}{}\x1b[0m{}{}",
                red.start(),
                selected,
                base.start(),
                suffix
            )),
            "{rendered:?}"
        );
        assert!(rendered.contains("\x1b[0m\x1b[48;5;236m"));
    }
}

#[test]
fn only_span_change_repaints_even_in_plain_mode() {
    for color in [false, true] {
        let output = Output::default();
        let renderer = TerminalFrameRenderer::new(output.clone(), color, 10, 12, true);
        let cancel = CancellationToken::new();
        for style in ["\x1b[31m", "\x1b[34m"] {
            let items: Vec<Arc<dyn LiveBufferItem>> = vec![Arc::new(Live(LiveBufferRender {
                lines: vec![TerminalLine::with_spans(
                    "status",
                    TerminalStyle::default(),
                    vec![TerminalCellStyleSpan::new(0, 1, TerminalStyle::new(style))],
                )],
                caret: Some(LiveBufferCaret { row: 0, cells: 0 }),
                retention: LiveBufferRetention::Fixed,
            }))];
            renderer.draw(&items, 8, 100, &cancel).unwrap();
            assert_eq!(output.take().matches("\x1b[2K").count(), 1);
        }
    }
}

#[test]
fn retention_budgets_and_terminal_height_are_independent() {
    let output = Output::default();
    let renderer = TerminalFrameRenderer::new(output.clone(), false, 2, 2, true);
    let cancel = CancellationToken::new();
    let items = vec![
        live(&["oldest", "middle"], LiveBufferRetention::Tail, None),
        live(&["newest"], LiveBufferRetention::Tail, None),
        live(&["fixed"], LiveBufferRetention::Fixed, None),
        live(
            &["one", "two", "three", "four"],
            LiveBufferRetention::Caret,
            Some((3, 20)),
        ),
    ];
    renderer.draw(&items, 8, 100, &cancel).unwrap();
    let all = output.take();
    assert_eq!(all.matches("\x1b[2K").count(), 5);
    assert!(!all.contains("oldest"));
    for text in ["middle", "newest", "fixed", "three", "four"] {
        assert!(all.contains(text));
    }
    assert!(all.ends_with("\x1b[7C\x1b[?7h\x1b[?25h"));
    renderer.draw(&items, 8, 2, &cancel).unwrap();
    let clipped = output.take();
    assert!(clipped.contains("three"));
    assert!(clipped.contains("four"));
    renderer.clear(&cancel).unwrap();
    output.take();
    renderer
        .draw(
            &[live(
                &["prompt", "option1", "option2"],
                LiveBufferRetention::Fixed,
                Some((0, -5)),
            )],
            8,
            2,
            &cancel,
        )
        .unwrap();
    let top = output.take();
    assert!(top.contains("prompt"));
    assert!(top.contains("option1"));
    assert!(!top.contains("option2"));
}

#[test]
fn invalid_carets_and_retention_return_source_messages_and_release_gate() {
    let output = Output::default();
    let renderer = TerminalFrameRenderer::new(output.clone(), false, 1, 1, true);
    let cancel = CancellationToken::new();
    let cases = [
        (vec![], "The live buffer must contain exactly one caret."),
        (
            vec![
                live(&["x"], LiveBufferRetention::Fixed, Some((0, 0))),
                live(&["y"], LiveBufferRetention::Fixed, Some((0, 0))),
            ],
            "The live buffer must contain exactly one caret.",
        ),
        (
            vec![live(&["x"], LiveBufferRetention::Fixed, Some((-1, 0)))],
            "The live buffer caret is outside its item.",
        ),
        (
            vec![live(&["x"], LiveBufferRetention::Fixed, Some((1, 0)))],
            "The live buffer caret is outside its item.",
        ),
        (
            vec![live(&["x", "y"], LiveBufferRetention::Tail, Some((0, 0)))],
            "The live buffer caret was removed by retention.",
        ),
        (
            vec![
                live(&["x", "y"], LiveBufferRetention::Caret, None),
                live(&["p"], LiveBufferRetention::Fixed, Some((0, 0))),
            ],
            "Caret-retained live buffer content requires a caret.",
        ),
    ];
    for (items, message) in cases {
        let error = renderer.draw(&items, 8, 100, &cancel).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
        assert_eq!(error.to_string(), message);
        assert_eq!(output.take(), "");
    }
    renderer.draw(&frame(&["x"]), 0, 0, &cancel).unwrap();
    assert!(output.take().contains("\x1b[2Kx\r"));
}

struct Scrollback {
    text: &'static str,
    sequence: Option<u32>,
    completed: bool,
    layout: ScrollbackLayout,
    starts: bool,
    ends: bool,
    packing: Option<u32>,
    expected_previous: Option<u32>,
}
impl ScrollbackItem for Scrollback {
    fn is_completed(&self) -> bool {
        self.completed
    }
    fn layout(&self) -> ScrollbackLayout {
        self.layout
    }
    fn starts_layout(&self) -> bool {
        self.starts
    }
    fn ends_layout(&self) -> bool {
        self.ends
    }
    fn sequence_identity(&self) -> Option<ItemIdentity> {
        self.sequence.map(|id| Arc::new(id) as ItemIdentity)
    }
    fn packing_identity(&self) -> Option<ItemIdentity> {
        self.packing.map(|id| Arc::new(id) as ItemIdentity)
    }
    fn packs(&self, previous: Option<&ItemIdentity>) -> bool {
        self.packing.is_some()
            && previous.and_then(|id| id.downcast_ref::<u32>()).copied() == self.packing
    }
    fn continues(&self, previous: &dyn ScrollbackItem) -> bool {
        self.sequence.is_some()
            && previous
                .sequence_identity()
                .and_then(|id| id.downcast_ref::<u32>().copied())
                == self.sequence
    }
    fn render(&self, context: ScrollbackRenderContext) -> io::Result<Vec<String>> {
        if let Some(expected) = self.expected_previous {
            assert_eq!(
                context
                    .previous_packing
                    .and_then(|id| id.downcast_ref::<u32>().copied()),
                Some(expected)
            );
        }
        assert!(context.inline_diff);
        Ok(if self.text.is_empty() {
            vec![]
        } else {
            vec![self.text.into()]
        })
    }
}
fn scroll(text: &'static str, sequence: Option<u32>, completed: bool) -> Scrollback {
    Scrollback {
        text,
        sequence,
        completed,
        layout: ScrollbackLayout::Compact,
        starts: true,
        ends: true,
        packing: None,
        expected_previous: None,
    }
}

#[test]
fn pending_sequences_drain_recursively_in_source_order() {
    let output = Output::default();
    let renderer = TerminalFrameRenderer::new(output.clone(), false, 10, 12, true);
    let cancel = CancellationToken::new();
    let items = frame(&["prompt"]);
    for (text, sequence, completed) in [
        ("a-open", Some(1), false),
        ("b-open", Some(2), false),
        ("c-done", Some(3), true),
        ("b-end", Some(2), true),
    ] {
        renderer
            .commit(
                Arc::new(scroll(text, sequence, completed)),
                &items,
                12,
                100,
                &cancel,
            )
            .unwrap();
        let bytes = output.take();
        assert_eq!(bytes.contains(text), text == "a-open");
    }
    renderer
        .commit(
            Arc::new(scroll("a-end", Some(1), true)),
            &items,
            12,
            100,
            &cancel,
        )
        .unwrap();
    let bytes = output.take();
    assert!(
        bytes.contains("a-end\r\nb-open\r\nb-end\r\nc-done\r\n"),
        "{bytes:?}"
    );
}

#[test]
fn layout_gaps_coalesce_and_packing_uses_consumer_value_equality() {
    let output = Output::default();
    let renderer = TerminalFrameRenderer::new(output.clone(), false, 10, 12, true);
    let cancel = CancellationToken::new();
    let items = frame(&["prompt"]);
    let mut first = scroll("first", None, true);
    first.layout = ScrollbackLayout::Block;
    first.ends = false;
    first.packing = Some(7);
    renderer
        .commit(Arc::new(first), &items, 12, 100, &cancel)
        .unwrap();
    output.take();
    let mut second = scroll("second", None, true);
    second.layout = ScrollbackLayout::Block;
    second.ends = false;
    second.packing = Some(7);
    second.expected_previous = Some(7);
    renderer
        .commit(Arc::new(second), &items, 12, 100, &cancel)
        .unwrap();
    let packed = output.take();
    assert!(packed.contains("\x1b[?25hsecond\r\n"), "{packed:?}");
    let mut third = scroll("third", None, true);
    third.layout = ScrollbackLayout::Assistant;
    renderer
        .commit(Arc::new(third), &items, 12, 100, &cancel)
        .unwrap();
    let separated = output.take();
    assert!(separated.contains("\x1b[?25h\r\nthird\r\n\r\n"));
    let mut fourth = scroll("fourth", None, true);
    fourth.layout = ScrollbackLayout::User;
    renderer
        .commit(Arc::new(fourth), &items, 12, 100, &cancel)
        .unwrap();
    let coalesced = output.take();
    assert!(coalesced.contains("\x1b[?25hfourth\r\n\r\n"));
}

#[test]
fn output_failures_discard_pending_bytes_and_release_admission() {
    for flush in [false, true] {
        let output = Output::default();
        let renderer = TerminalFrameRenderer::new(output.clone(), false, 10, 12, true);
        let cancel = CancellationToken::new();
        {
            let mut state = output.0.lock().unwrap();
            state.fail_write = !flush;
            state.fail_flush = flush;
        }
        assert!(renderer.draw(&frame(&["failed"]), 8, 100, &cancel).is_err());
        output.take();
        renderer.draw(&frame(&["retry"]), 8, 100, &cancel).unwrap();
        let retry = output.take();
        assert_eq!(retry.matches("\x1b[2K").count(), 1);
        assert!(!retry.contains("failed"));
        assert!(retry.contains("retry"));
        output.0.lock().unwrap().fail_flush = true;
        assert!(renderer.clear(&cancel).is_err());
        output.take();
        renderer.clear(&cancel).unwrap();
        assert_eq!(output.take(), "");
    }
}

#[test]
fn empty_completion_after_clear_releases_pending_without_losing_sequence_state() {
    let output = Output::default();
    let renderer = TerminalFrameRenderer::new(output.clone(), false, 10, 12, true);
    let cancel = CancellationToken::new();
    let items = frame(&["prompt"]);
    renderer
        .commit(
            Arc::new(scroll("open", Some(1), false)),
            &items,
            12,
            100,
            &cancel,
        )
        .unwrap();
    renderer
        .commit(
            Arc::new(scroll("queued", None, true)),
            &items,
            12,
            100,
            &cancel,
        )
        .unwrap();
    output.take();
    renderer.clear(&cancel).unwrap();
    output.take();
    renderer
        .commit(
            Arc::new(scroll("", Some(1), true)),
            &items,
            12,
            100,
            &cancel,
        )
        .unwrap();
    let bytes = output.take();
    assert!(bytes.starts_with("queued\r\n"), "{bytes:?}");
    assert!(bytes.contains("prompt"));
}

#[test]
fn failed_commit_keeps_advanced_sequence_state_but_does_not_replay_output() {
    let output = Output::default();
    let renderer = TerminalFrameRenderer::new(output.clone(), false, 10, 12, true);
    let cancel = CancellationToken::new();
    let items = frame(&["prompt"]);
    output.0.lock().unwrap().fail_write = true;
    assert!(
        renderer
            .commit(
                Arc::new(scroll("lost-open", Some(1), false)),
                &items,
                12,
                100,
                &cancel
            )
            .is_err()
    );
    assert_eq!(output.take(), "");
    renderer
        .commit(
            Arc::new(scroll("queued", None, true)),
            &items,
            12,
            100,
            &cancel,
        )
        .unwrap();
    let pending = output.take();
    assert!(!pending.contains("queued"));
    assert!(!pending.contains("lost-open"));
    renderer
        .commit(
            Arc::new(scroll("close", Some(1), true)),
            &items,
            12,
            100,
            &cancel,
        )
        .unwrap();
    let closed = output.take();
    assert!(closed.contains("close\r\nqueued\r\n"));
    assert!(!closed.contains("lost-open"));
}

#[test]
fn base_style_changes_repaint_only_changed_row_and_padding_uses_background() {
    let output = Output::default();
    let renderer = TerminalFrameRenderer::new(output.clone(), true, 10, 12, true);
    let cancel = CancellationToken::new();
    for style in ["\x1b[31m", "\x1b[34m"] {
        let items: Vec<Arc<dyn LiveBufferItem>> = vec![Arc::new(Live(LiveBufferRender {
            lines: vec![
                TerminalLine::new("x", TerminalStyle::new(style)),
                TerminalLine::new("p", TerminalStyle::default()),
            ],
            caret: Some(LiveBufferCaret { row: 1, cells: 0 }),
            retention: LiveBufferRetention::Fixed,
        }))];
        renderer.draw(&items, 3, 100, &cancel).unwrap();
        let bytes = output.take();
        assert_eq!(
            bytes.matches("\x1b[2K").count(),
            if style == "\x1b[31m" { 2 } else { 1 }
        );
        assert!(bytes.contains(&format!("{style}x\x1b[0m\x1b[48;5;236m  \x1b[0m")));
    }
}

struct BoundaryBlock {
    started: mpsc::Sender<()>,
    released: Arc<(Mutex<bool>, Condvar)>,
}
impl BoundaryBlock {
    fn wait(self) {
        self.started.send(()).unwrap();
        let mut released = self.released.0.lock().unwrap();
        while !*released {
            released = self.released.1.wait(released).unwrap();
        }
    }
}
struct BoundaryOutput {
    output: Output,
    write_block: Option<BoundaryBlock>,
    flush_block: Option<BoundaryBlock>,
}
impl Write for BoundaryOutput {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        if let Some(block) = self.write_block.take() {
            block.wait();
        }
        self.output.write(bytes)
    }
    fn flush(&mut self) -> io::Result<()> {
        if let Some(block) = self.flush_block.take() {
            block.wait();
        }
        self.output.flush()
    }
}
struct BrokenLive;
impl LiveBufferItem for BrokenLive {
    fn render(&self, _: LiveBufferRenderContext) -> io::Result<LiveBufferRender> {
        Err(io::Error::other("render failed"))
    }
}

#[test]
fn admission_boundaries_match_pinned_source_observations() {
    for line in include_str!("fixtures/boundaries/observations.jsonl").lines() {
        let expected: serde_json::Value = serde_json::from_str(line).unwrap();
        let Some(mode) = expected["name"]
            .as_str()
            .unwrap()
            .strip_prefix("admission-")
        else {
            continue;
        };
        let output = Output::default();
        let (write_tx, write_rx) = mpsc::channel();
        let (flush_tx, flush_rx) = mpsc::channel();
        let write_release = Arc::new((Mutex::new(false), Condvar::new()));
        let flush_release = Arc::new((Mutex::new(false), Condvar::new()));
        let renderer = Arc::new(TerminalFrameRenderer::new(
            BoundaryOutput {
                output: output.clone(),
                write_block: matches!(mode, "waiting" | "admitted").then(|| BoundaryBlock {
                    started: write_tx,
                    released: write_release.clone(),
                }),
                flush_block: (mode == "admitted").then(|| BoundaryBlock {
                    started: flush_tx,
                    released: flush_release.clone(),
                }),
            },
            false,
            10,
            12,
            true,
        ));
        let cancel = CancellationToken::new();
        let mut errors = Vec::new();
        let release = |gate: &Arc<(Mutex<bool>, Condvar)>| {
            *gate.0.lock().unwrap() = true;
            gate.1.notify_all();
        };
        let draw =
            |text: &str, token: &CancellationToken| renderer.draw(&frame(&[text]), 8, 4, token);
        if mode == "before" {
            cancel.cancel();
            let error = draw("first", &cancel).unwrap_err();
            assert_eq!(error.kind(), io::ErrorKind::Interrupted);
            errors.push("cancelled".to_owned());
            draw("after", &CancellationToken::new()).unwrap();
        } else if matches!(mode, "waiting" | "admitted") {
            let first = {
                let renderer = renderer.clone();
                let token = if mode == "admitted" {
                    cancel.clone()
                } else {
                    CancellationToken::new()
                };
                thread::spawn(move || renderer.draw(&frame(&["first"]), 8, 4, &token))
            };
            write_rx.recv_timeout(Duration::from_secs(5)).unwrap();
            if mode == "waiting" {
                let (result_tx, result_rx) = mpsc::channel();
                let waiting = {
                    let renderer = renderer.clone();
                    let token = cancel.clone();
                    thread::spawn(move || {
                        result_tx
                            .send(renderer.draw(&frame(&["waiting"]), 8, 4, &token))
                            .unwrap()
                    })
                };
                cancel.cancel();
                let error = result_rx
                    .recv_timeout(Duration::from_secs(5))
                    .unwrap()
                    .unwrap_err();
                assert_eq!(error.kind(), io::ErrorKind::Interrupted);
                errors.push("cancelled".to_owned());
                waiting.join().unwrap();
                release(&write_release);
                first.join().unwrap().unwrap();
                draw("after", &CancellationToken::new()).unwrap();
            } else {
                cancel.cancel();
                release(&write_release);
                flush_rx.recv_timeout(Duration::from_secs(5)).unwrap();
                assert!(
                    !first.is_finished(),
                    "cancelled admitted operation must still await flush"
                );
                release(&flush_release);
                first.join().unwrap().unwrap();
            }
        } else {
            {
                let mut state = output.0.lock().unwrap();
                state.fail_write = mode == "write-error";
                state.fail_flush = mode == "flush-error";
            }
            let items = if mode == "render-error" {
                vec![Arc::new(BrokenLive) as Arc<dyn LiveBufferItem>]
            } else {
                frame(&["first"])
            };
            errors.push(
                renderer
                    .draw(&items, 8, 4, &CancellationToken::new())
                    .unwrap_err()
                    .to_string(),
            );
            draw("after", &CancellationToken::new()).unwrap();
        }
        let state = output.0.lock().unwrap();
        let mut hex = String::new();
        for byte in &state.bytes {
            write!(hex, "{byte:02x}").unwrap();
        }
        assert_eq!(serde_json::json!(errors), expected["errors"], "{mode}");
        assert_eq!(
            serde_json::json!(state.writes),
            expected["writes"],
            "{mode}"
        );
        assert_eq!(
            serde_json::json!(state.flushes),
            expected["flushes"],
            "{mode}"
        );
        assert_eq!(hex, expected["output"].as_str().unwrap(), "{mode}");
        assert_eq!(
            expected["write_cancelable"],
            serde_json::json!(vec![false; state.writes]),
            "source must use uncancelable writes: {mode}"
        );
        assert_eq!(
            expected["flush_cancelable"],
            serde_json::json!(vec![false; state.flushes]),
            "source must use uncancelable flushes: {mode}"
        );
    }
}

struct BlockingOutput {
    output: Output,
    entered: Option<mpsc::Sender<()>>,
    released: Arc<(Mutex<bool>, Condvar)>,
}
impl Write for BlockingOutput {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        if let Some(entered) = self.entered.take() {
            entered.send(()).unwrap();
            let (lock, changed) = &*self.released;
            let mut released = lock.lock().unwrap();
            while !*released {
                released = changed.wait(released).unwrap();
            }
        }
        self.output.write(bytes)
    }
    fn flush(&mut self) -> io::Result<()> {
        self.output.flush()
    }
}

#[test]
fn concurrent_draw_waiter_cancels_but_admitted_write_and_flush_are_uncancelable() {
    let output = Output::default();
    let (entered_tx, entered_rx) = mpsc::channel();
    let released = Arc::new((Mutex::new(false), Condvar::new()));
    let renderer = Arc::new(TerminalFrameRenderer::new(
        BlockingOutput {
            output: output.clone(),
            entered: Some(entered_tx),
            released: released.clone(),
        },
        false,
        10,
        12,
        true,
    ));
    let admitted_cancel = CancellationToken::new();
    let first = {
        let renderer = renderer.clone();
        let cancel = admitted_cancel.clone();
        thread::spawn(move || renderer.draw(&frame(&["first"]), 8, 100, &cancel))
    };
    entered_rx.recv_timeout(Duration::from_secs(5)).unwrap();
    admitted_cancel.cancel();
    let waiting_cancel = CancellationToken::new();
    let (result_tx, result_rx) = mpsc::channel();
    let waiter = {
        let renderer = renderer.clone();
        let cancel = waiting_cancel.clone();
        thread::spawn(move || {
            result_tx
                .send(renderer.draw(&frame(&["cancelled"]), 8, 100, &cancel))
                .unwrap();
        })
    };
    waiting_cancel.cancel();
    assert!(
        result_rx
            .recv_timeout(Duration::from_secs(5))
            .unwrap()
            .is_err()
    );
    assert_eq!(output.0.lock().unwrap().flushes, 0);
    *released.0.lock().unwrap() = true;
    released.1.notify_all();
    first.join().unwrap().unwrap();
    waiter.join().unwrap();
    assert_eq!(output.0.lock().unwrap().flushes, 1);
    let mut workers = Vec::new();
    for _ in 0..8 {
        let renderer = renderer.clone();
        workers.push(thread::spawn(move || {
            renderer.draw(&frame(&["same"]), 8, 100, &CancellationToken::new())
        }));
    }
    for worker in workers {
        worker.join().unwrap().unwrap();
    }
    let bytes = output.take();
    assert!(!bytes.contains("cancelled"));
    assert_eq!(bytes.matches("\x1b[2K").count(), 2);
}
