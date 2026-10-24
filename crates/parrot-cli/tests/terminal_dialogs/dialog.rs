//! Source-shaped EnhancedSlashDialogTests. Original .NET rows remain fresh NOT_RUN.

use std::{
    collections::VecDeque,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    task::Poll,
};

use parrot_cli::{
    dispatch::dialog::{self, DialogOption, SlashDialog},
    terminal_dialogs::dialog::{CommitScrollback, EnhancedSlashDialog, LiveInputHost},
    terminal_frames::{
        LiveBufferCaret, LiveBufferItem, LiveBufferRenderContext, LiveBufferRetention,
        ScrollbackItem, ScrollbackRenderContext,
    },
    terminal_input::{TerminalKey, TerminalKeyKind},
};
use parrot_domain::contracts::async_ops::{BoxFuture, Cancellation};
use tonic::{Code, Status};

#[derive(Default)]
struct TestCancellation {
    cancelled: AtomicBool,
    changed: tokio::sync::Notify,
}
impl TestCancellation {
    fn cancel(&self) {
        self.cancelled.store(true, Ordering::Release);
        self.changed.notify_waiters();
    }
}
impl Cancellation for TestCancellation {
    fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::Acquire)
    }
    fn cancelled(&self) -> BoxFuture<'_, ()> {
        Box::pin(async move {
            loop {
                let notified = self.changed.notified();
                if self.is_cancelled() {
                    return;
                }
                notified.await;
            }
        })
    }
}

#[derive(Default)]
struct ScriptedHost {
    keys: Mutex<VecDeque<TerminalKey>>,
    frames: Mutex<Vec<Vec<Arc<dyn LiveBufferItem>>>>,
    resets: AtomicUsize,
    cancel_on_read: Option<Arc<TestCancellation>>,
    cancel_after_frame: Option<(usize, Arc<TestCancellation>)>,
    fail_read: bool,
}
impl ScriptedHost {
    fn new(keys: impl IntoIterator<Item = TerminalKey>) -> Arc<Self> {
        Arc::new(Self {
            keys: Mutex::new(keys.into_iter().collect()),
            ..Self::default()
        })
    }
    fn rendered(&self, columns: usize) -> Vec<Vec<Vec<String>>> {
        self.frames
            .lock()
            .unwrap()
            .iter()
            .map(|frame| {
                frame
                    .iter()
                    .map(|item| {
                        item.render(LiveBufferRenderContext {
                            columns,
                            color: false,
                        })
                        .unwrap()
                        .lines
                        .into_iter()
                        .map(|line| line.text().to_owned())
                        .collect()
                    })
                    .collect()
            })
            .collect()
    }
}
impl LiveInputHost for ScriptedHost {
    fn read_key<'a>(
        &'a self,
        cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<TerminalKey, Status>> {
        Box::pin(async move {
            if self.fail_read {
                return Err(Status::unavailable("input failed"));
            }
            let key = self.keys.lock().unwrap().pop_front();
            if let Some(key) = key {
                if let Some(cancellation) = &self.cancel_on_read {
                    cancellation.cancel();
                }
                Ok(key)
            } else {
                cancellation.cancelled().await;
                Err(Status::cancelled("scripted read cancelled"))
            }
        })
    }
    fn replace_input<'a>(
        &'a self,
        items: Vec<Arc<dyn LiveBufferItem>>,
        cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<(), Status>> {
        Box::pin(async move {
            cancellation
                .check()
                .map_err(|_| Status::cancelled("scripted draw cancelled"))?;
            let count = {
                let mut frames = self.frames.lock().unwrap();
                frames.push(items);
                frames.len()
            };
            if let Some((at, cancellation)) = &self.cancel_after_frame {
                if count == *at {
                    cancellation.cancel();
                }
            }
            Ok(())
        })
    }
    fn reset_input(&self) {
        self.resets.fetch_add(1, Ordering::AcqRel);
    }
}
fn key(kind: TerminalKeyKind) -> TerminalKey {
    TerminalKey::new(kind)
}
fn text(value: &str) -> Vec<TerminalKey> {
    value
        .chars()
        .map(|ch| TerminalKey::with_text(TerminalKeyKind::Character, ch.to_string()))
        .collect()
}
fn submit(mut keys: Vec<TerminalKey>) -> Vec<TerminalKey> {
    keys.push(key(TerminalKeyKind::Submit));
    keys
}
fn dialog(host: &Arc<ScriptedHost>) -> EnhancedSlashDialog {
    let commit: CommitScrollback =
        Arc::new(|_, _| Box::pin(async { panic!("unexpected scrollback commit") }));
    EnhancedSlashDialog::new(host.clone(), commit)
}
fn assert_prompt(item: &Arc<dyn LiveBufferItem>, text: &str, column: isize) {
    let rendered = item
        .render(LiveBufferRenderContext {
            columns: 80,
            color: false,
        })
        .unwrap();
    assert_eq!(rendered.lines.len(), 1);
    assert_eq!(rendered.lines[0].text(), text);
    assert_eq!(
        rendered.caret,
        Some(LiveBufferCaret {
            row: 0,
            cells: column
        })
    );
    assert_eq!(rendered.retention, LiveBufferRetention::Caret);
}

#[tokio::test]
async fn print_commits_all_lines_without_opening_a_dialog() {
    let cancellation = TestCancellation::default();
    let host = ScriptedHost::new([]);
    let committed = Arc::new(Mutex::new(Vec::<Arc<dyn ScrollbackItem>>::new()));
    let capture = committed.clone();
    let commit: CommitScrollback = Arc::new(move |item, c| {
        let capture = capture.clone();
        Box::pin(async move {
            c.check()
                .map_err(|_| Status::cancelled("commit cancelled"))?;
            capture.lock().unwrap().push(item);
            Ok(())
        })
    });
    let mut dialog = EnhancedSlashDialog::new(host.clone(), commit);
    let lines: Vec<_> = (0..20).map(|index| format!("Status {index}")).collect();
    dialog.print(&lines, &cancellation).await.unwrap();
    assert!(host.frames.lock().unwrap().is_empty());
    let committed = committed.lock().unwrap();
    assert_eq!(committed.len(), 1);
    assert!(committed[0].is_completed());
    assert_eq!(
        committed[0]
            .render(ScrollbackRenderContext {
                columns: 80,
                color: false,
                inline_diff: false,
                previous_packing: None
            })
            .unwrap(),
        lines
    );
}

#[tokio::test]
async fn picker_filters_navigates_and_selects_through_live_items() {
    let mut keys = text("al");
    keys.extend([key(TerminalKeyKind::Down), key(TerminalKeyKind::Submit)]);
    let host = ScriptedHost::new(keys);
    let options = [
        DialogOption::new("alpha", "Alpha", "first"),
        DialogOption::new("alpine", "Alpine", "mountain"),
        DialogOption::new("beta", "Beta", "second"),
    ];
    let selected = dialog(&host)
        .select("Filter: ", &options, &TestCancellation::default())
        .await
        .unwrap();
    assert_eq!(selected, Some(options[1].clone()));
    let frames = host.rendered(80);
    assert_eq!(
        frames[2],
        vec![
            vec!["Filter: "],
            vec!["> al"],
            vec!["> Alpha — first"],
            vec!["  Alpine — mountain"]
        ]
    );
    assert_eq!(
        frames.last().unwrap(),
        &vec![vec!["Filter: "], vec!["> Alpine"]]
    );
    let items = host.frames.lock().unwrap();
    assert_prompt(&items[2][1], "> al", 4);
    assert_prompt(&items.last().unwrap()[1], "> Alpine", 8);
    assert_eq!(host.resets.load(Ordering::Acquire), 1);
}

#[tokio::test]
async fn unicode_picker_matches_case_in_labels_and_descriptions() {
    for (haystack, needle, expected) in [
        ("prefix ALPHA suffix", "alpha", true),
        ("prefix CAFÉ suffix", "café", true),
        ("ΑΘΉΝΑ", "αθήνα", true),
        ("ΟΣ", "ος", true),
        ("ΟΣ", "οσ", true),
        ("МОСКВА", "москва", true),
        ("anything", "", true),
        ("", "", true),
        ("ᾀ", "ᾈ", true),
        ("ᾈ", "ᾀ", true),
        ("𐐀", "𐐨", true),
        ("𐐨", "𐐀", true),
        ("Straße", "STRASSE", false),
        ("STRASSE", "Straße", false),
        ("ß", "ẞ", false),
        ("ẞ", "ß", false),
        ("ﬃ", "ffi", false),
        ("ffi", "ﬃ", false),
        ("I", "ı", false),
        ("ı", "I", false),
        ("İ", "i", false),
        ("i", "İ", false),
        ("S", "ſ", false),
        ("ſ", "S", false),
        ("K", "K", false),
        ("K", "K", false),
        ("é", "é", false),
        ("é", "é", false),
        ("x𐐀y", "𐐨", true),
        ("x𐐀Y", "𐐨y", true),
        ("x𐐀y", "x𐐨", true),
        ("𐐀𐐁", "𐐨𐐩", true),
        ("x𐐀y", "𐐨z", false),
        ("𐐀￿", "𐐨￿", true),
        ("xᾀ𐐀y", "ᾈ𐐨", true),
        ("𐀀", "𐐨", false),
        ("𐐀", "Ā", false),
    ] {
        for description_only in [false, true] {
            let option = if description_only {
                DialogOption::new("choice", "☃", haystack)
            } else {
                DialogOption::new("choice", haystack, "")
            };
            let mut keys = text(needle);
            keys.extend([key(TerminalKeyKind::Submit), key(TerminalKeyKind::Escape)]);
            let host = ScriptedHost::new(keys);
            let selected = dialog(&host)
                .select(
                    "Unicode: ",
                    std::slice::from_ref(&option),
                    &TestCancellation::default(),
                )
                .await
                .unwrap();
            assert_eq!(selected.is_some(), expected, "{haystack:?} / {needle:?}");
            if expected {
                assert_eq!(selected, Some(option));
            }
            assert_eq!(host.resets.load(Ordering::Acquire), 1);
        }
    }
}

#[tokio::test]
async fn unicode_picker_preserves_filter_order_and_selection() {
    let options = [
        DialogOption::new("0", "first CAFÉ", "none"),
        DialogOption::new("1", "second unrelated", "description-only CAFÉ"),
        DialogOption::new("2", "third café", "none"),
    ];
    for query in ["", "café"] {
        for position in 0..options.len() {
            let mut keys = text(query);
            keys.extend((0..position).map(|_| key(TerminalKeyKind::Down)));
            keys.extend([key(TerminalKeyKind::Submit), key(TerminalKeyKind::Escape)]);
            let host = ScriptedHost::new(keys);
            let selected = dialog(&host)
                .select("Order: ", &options, &TestCancellation::default())
                .await
                .unwrap();
            assert_eq!(selected, Some(options[position].clone()));
            assert_eq!(host.resets.load(Ordering::Acquire), 1);
        }
    }
}

#[tokio::test]
async fn picker_places_prompt_before_no_matches() {
    let mut keys = text("z");
    keys.push(key(TerminalKeyKind::Escape));
    let host = ScriptedHost::new(keys);
    let selected = dialog(&host)
        .select(
            "Question: ",
            &[DialogOption::new("one", "One", "")],
            &TestCancellation::default(),
        )
        .await
        .unwrap();
    assert!(selected.is_none());
    assert_eq!(
        host.rendered(80)[1],
        vec![vec!["Question: "], vec!["> z"], vec!["  No matches"]]
    );
    assert_prompt(&host.frames.lock().unwrap()[1][1], "> z", 3);
}

#[tokio::test]
async fn picker_cancels_on_escape_interrupt_or_end_of_file() {
    for kind in [
        TerminalKeyKind::Escape,
        TerminalKeyKind::Interrupt,
        TerminalKeyKind::EndOfFile,
    ] {
        let host = ScriptedHost::new([key(kind)]);
        assert!(
            dialog(&host)
                .select(
                    "Pick: ",
                    &[DialogOption::new("one", "One", "")],
                    &TestCancellation::default()
                )
                .await
                .unwrap()
                .is_none()
        );
        assert_eq!(host.resets.load(Ordering::Acquire), 1);
    }
}

#[tokio::test]
async fn picker_does_not_interpret_a_key_returned_during_cancellation() {
    let cancellation = Arc::new(TestCancellation::default());
    let host = Arc::new(ScriptedHost {
        keys: Mutex::new([key(TerminalKeyKind::Submit)].into()),
        cancel_on_read: Some(cancellation.clone()),
        ..ScriptedHost::default()
    });
    let result = dialog(&host)
        .select(
            "Pick: ",
            &[DialogOption::new("one", "One", "")],
            cancellation.as_ref(),
        )
        .await;
    assert_eq!(result.unwrap_err().code(), Code::Cancelled);
    assert_eq!(host.frames.lock().unwrap().len(), 1);
    assert_eq!(host.resets.load(Ordering::Acquire), 1);
}

#[tokio::test]
async fn text_does_not_interpret_a_key_returned_during_cancellation() {
    let cancellation = Arc::new(TestCancellation::default());
    let host = Arc::new(ScriptedHost {
        keys: Mutex::new(text("x").into()),
        cancel_on_read: Some(cancellation.clone()),
        ..ScriptedHost::default()
    });
    assert_eq!(
        dialog(&host)
            .read_text("Name: ", cancellation.as_ref())
            .await
            .unwrap_err()
            .code(),
        Code::Cancelled
    );
    assert_eq!(host.frames.lock().unwrap().len(), 1);
    assert_eq!(host.resets.load(Ordering::Acquire), 1);
}

#[tokio::test]
async fn confirmation_reports_escape_as_cancellation() {
    let host = ScriptedHost::new([key(TerminalKeyKind::Escape)]);
    assert!(
        !dialog(&host)
            .confirm(&["Continue".into()], &TestCancellation::default())
            .await
            .unwrap()
    );
}

#[tokio::test]
async fn text_secret_and_information_are_all_live_input_items() {
    for secret in ["sëcret", "sëcret🙂"] {
        let text_host = ScriptedHost::new(submit(text("hello")));
        let entered = dialog(&text_host)
            .read_text("Name: ", &TestCancellation::default())
            .await
            .unwrap();
        assert_eq!(entered.as_deref(), Some("hello"));
        assert_eq!(
            text_host.rendered(80).last().unwrap(),
            &vec![vec!["Name: "], vec!["> hello"]]
        );
        assert_prompt(
            &text_host.frames.lock().unwrap().last().unwrap()[1],
            "> hello",
            7,
        );
        let mut keys = text(secret);
        keys.extend([
            key(TerminalKeyKind::Left),
            key(TerminalKeyKind::Submit),
            key(TerminalKeyKind::Submit),
            key(TerminalKeyKind::Submit),
        ]);
        let host = ScriptedHost::new(keys);
        let mut dialog = dialog(&host);
        assert_eq!(
            dialog
                .read_secret("Key: ", &TestCancellation::default())
                .await
                .unwrap()
                .as_deref(),
            Some(secret)
        );
        dialog
            .show(
                &["first".into(), "second".into()],
                &TestCancellation::default(),
            )
            .await
            .unwrap();
        dialog
            .show_error("failed", &TestCancellation::default())
            .await
            .unwrap();
        let rendered = host.rendered(80);
        assert!(
            rendered
                .iter()
                .flatten()
                .flatten()
                .all(|line| !line.contains(secret))
        );
        let frames = host.frames.lock().unwrap();
        let count = secret.chars().count() as isize;
        assert_prompt(
            &frames[frames.len() - 4][1],
            &(String::from("> ") + &"*".repeat(count as usize)),
            count + 1,
        );
        assert_prompt(
            &frames[frames.len() - 3][1],
            &(String::from("> ") + &"*".repeat(count as usize)),
            count + 2,
        );
        assert_eq!(rendered[0][0], vec!["Key: "]);
        assert_prompt(&frames[0][1], "> ", 2);
        let information = frames[frames.len() - 2][0]
            .render(LiveBufferRenderContext {
                columns: 80,
                color: true,
            })
            .unwrap();
        assert_eq!(
            information.lines[0].style().start(),
            "\x1b[48;5;236m\x1b[38;5;252m"
        );
        assert_eq!(rendered[rendered.len() - 2], vec![vec!["first", "second"]]);
        assert_eq!(rendered.last().unwrap(), &vec![vec!["failed"]]);
        let failure = frames.last().unwrap()[0]
            .render(LiveBufferRenderContext {
                columns: 80,
                color: true,
            })
            .unwrap();
        assert_eq!(failure.lines[0].style().start(), "\x1b[31m");
    }
}

#[tokio::test]
async fn load_shows_spinner_frames_returns_result_and_clears_input() {
    let host = ScriptedHost::new([]);
    let loaded = dialog::load(
        &mut dialog(&host),
        "Loading models…",
        async { Ok(DialogOption::new("id", "Label", "description")) },
        &TestCancellation::default(),
    )
    .await
    .unwrap();
    assert_eq!(loaded.id, "id");
    let frames = host.rendered(80);
    assert_eq!(frames.len(), 2);
    assert_eq!(frames[0], vec![vec!["> "], vec!["⠋ Loading models…"]]);
    assert_eq!(frames.last().unwrap(), &vec![vec!["> "]]);
}

#[tokio::test]
async fn load_propagates_failure_and_still_clears_input() {
    for cancel in [true, false] {
        let cancellation = TestCancellation::default();
        let host = ScriptedHost::new([]);
        let result = dialog(&host)
            .load(
                "Loading models…",
                Box::pin(async {
                    if cancel {
                        cancellation.cancel();
                    }
                    if cancellation.is_cancelled() {
                        Err(Status::cancelled("work cancelled"))
                    } else {
                        Err(Status::invalid_argument("load failed"))
                    }
                }),
                &cancellation,
            )
            .await;
        assert_eq!(
            result.unwrap_err().code(),
            if cancel {
                Code::Cancelled
            } else {
                Code::InvalidArgument
            }
        );
        assert_eq!(host.rendered(80).last().unwrap(), &vec![vec!["> "]]);
    }
}

#[tokio::test]
async fn typed_dialog_load_records_activity_and_joins_successful_work() {
    let host = ScriptedHost::new([]);
    let completed = AtomicBool::new(false);
    let result = dialog::load(
        &mut dialog(&host),
        "Loading models…",
        async {
            tokio::task::yield_now().await;
            completed.store(true, Ordering::Release);
            Ok("result")
        },
        &TestCancellation::default(),
    )
    .await
    .unwrap();
    assert_eq!(result, "result");
    assert!(completed.load(Ordering::Acquire));
    assert_eq!(host.rendered(80)[0][1], vec!["⠋ Loading models…"]);
}

#[tokio::test]
async fn picker_and_text_keep_multiline_context_in_every_editing_frame() {
    for (context, columns, expected) in [
        ("Header\nQuestion", 80, vec!["Header", "Question"]),
        ("Header\nQuestion", 6, vec!["Header", "Questi", "on"]),
        ("Question", 80, vec!["Question"]),
    ] {
        let host = ScriptedHost::new(submit(text("a")));
        dialog(&host)
            .select(
                context,
                &[DialogOption::new("answer", "Answer", "")],
                &TestCancellation::default(),
            )
            .await
            .unwrap();
        let text_host = ScriptedHost::new(submit(text("ab")));
        dialog(&text_host)
            .read_text(context, &TestCancellation::default())
            .await
            .unwrap();
        for frame in host
            .rendered(columns)
            .into_iter()
            .chain(text_host.rendered(columns))
        {
            assert_eq!(frame[0], expected);
        }
    }
}

#[tokio::test]
async fn picker_window_wraps_and_ignores_multiline_paste() {
    let options: Vec<_> = (0..12)
        .map(|i| DialogOption::new(i.to_string(), format!("Option {i}"), ""))
        .collect();
    let host = ScriptedHost::new([
        key(TerminalKeyKind::Up),
        TerminalKey::with_text(TerminalKeyKind::Paste, "not\na filter"),
        key(TerminalKeyKind::Submit),
    ]);
    assert_eq!(
        dialog(&host)
            .select("Pick", &options, &TestCancellation::default())
            .await
            .unwrap(),
        Some(options[11].clone())
    );
    let frames = host.rendered(80);
    assert_eq!(frames[1].len(), 10);
    assert_eq!(frames[1][2], vec!["  Option 4"]);
    assert_eq!(frames[1][9], vec!["> Option 11"]);
    assert_eq!(frames[1], frames[2]);
    let host = ScriptedHost::new([
        key(TerminalKeyKind::Up),
        key(TerminalKeyKind::Down),
        key(TerminalKeyKind::Submit),
    ]);
    assert_eq!(
        dialog(&host)
            .select("Pick", &options, &TestCancellation::default())
            .await
            .unwrap(),
        Some(options[0].clone())
    );
}

#[tokio::test]
async fn text_is_bounded_untrimmed_and_cleanup_survives_error_or_observer_drop() {
    let input = "🙂".repeat(4097);
    let host = ScriptedHost::new([
        TerminalKey::with_text(TerminalKeyKind::Paste, &input),
        key(TerminalKeyKind::Submit),
    ]);
    let returned = dialog(&host)
        .read_text("Name", &TestCancellation::default())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(returned, "🙂".repeat(4096));
    let host = ScriptedHost::new(submit(text("  value  ")));
    assert_eq!(
        dialog(&host)
            .read_text("Name", &TestCancellation::default())
            .await
            .unwrap()
            .as_deref(),
        Some("  value  ")
    );
    let failed = Arc::new(ScriptedHost {
        fail_read: true,
        ..ScriptedHost::default()
    });
    assert_eq!(
        dialog(&failed)
            .read_secret("Secret", &TestCancellation::default())
            .await
            .unwrap_err()
            .code(),
        Code::Unavailable
    );
    assert_eq!(failed.resets.load(Ordering::Acquire), 1);
    let host = ScriptedHost::new([]);
    let mut dialog = dialog(&host);
    let cancellation = TestCancellation::default();
    let mut reading = dialog.read_secret("Secret", &cancellation);
    std::future::poll_fn(|cx| {
        assert!(reading.as_mut().poll(cx).is_pending());
        Poll::Ready(())
    })
    .await;
    drop(reading);
    assert_eq!(host.resets.load(Ordering::Acquire), 1);
}

#[tokio::test]
async fn message_scrolls_nine_lines_and_clamps_both_ends() {
    let host = ScriptedHost::new([
        key(TerminalKeyKind::Up),
        key(TerminalKeyKind::Down),
        key(TerminalKeyKind::Down),
        key(TerminalKeyKind::Down),
        key(TerminalKeyKind::Submit),
    ]);
    let lines: Vec<_> = (0..11).map(|i| format!("Line {i}")).collect();
    assert!(
        dialog(&host)
            .confirm(&lines, &TestCancellation::default())
            .await
            .unwrap()
    );
    let frames = host.rendered(80);
    assert_eq!(frames[0][0], lines[0..9]);
    assert_eq!(frames[1], frames[0]);
    assert_eq!(frames[2][0], lines[1..10]);
    assert_eq!(frames[3][0], lines[2..11]);
    assert_eq!(frames[4], frames[3]);
}

#[tokio::test]
async fn selection_rechecks_cancellation_after_collapsed_frame_and_literal_unicode_is_preserved() {
    let cancellation = Arc::new(TestCancellation::default());
    let host = Arc::new(ScriptedHost {
        keys: Mutex::new([key(TerminalKeyKind::Submit)].into()),
        cancel_after_frame: Some((2, cancellation.clone())),
        ..ScriptedHost::default()
    });
    assert_eq!(
        dialog(&host)
            .select(
                "Pick",
                &[DialogOption::new("one", "One", "")],
                cancellation.as_ref()
            )
            .await
            .unwrap_err()
            .code(),
        Code::Cancelled
    );
    assert_eq!(host.resets.load(Ordering::Acquire), 1);
    let host = ScriptedHost::new(submit(text("ß")));
    let options = [
        DialogOption::new("literal", "Straße", ""),
        DialogOption::new("expanded", "STRASSE", ""),
    ];
    assert_eq!(
        dialog(&host)
            .select("Pick", &options, &TestCancellation::default())
            .await
            .unwrap(),
        Some(options[0].clone())
    );
    // This bounded literal/ASCII behavior is not arbitrary-Unicode OrdinalIgnoreCase proof.
}

#[tokio::test]
async fn load_observer_cancellation_drops_borrowed_work_without_detaching_it() {
    struct Dropped(Arc<AtomicBool>);
    impl Drop for Dropped {
        fn drop(&mut self) {
            self.0.store(true, Ordering::Release);
        }
    }
    let cancellation = TestCancellation::default();
    let host = ScriptedHost::new([]);
    let dropped = Arc::new(AtomicBool::new(false));
    let marker = Dropped(dropped.clone());
    let result = dialog(&host)
        .load(
            "Loading",
            Box::pin(async {
                let _marker = marker;
                cancellation.cancel();
                std::future::pending::<Result<(), Status>>().await
            }),
            &cancellation,
        )
        .await;
    assert_eq!(result.unwrap_err().code(), Code::Cancelled);
    assert!(dropped.load(Ordering::Acquire));
    assert_eq!(host.rendered(80).last().unwrap(), &vec![vec!["> "]]);
}
