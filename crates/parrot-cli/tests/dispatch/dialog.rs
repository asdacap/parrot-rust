use std::collections::VecDeque;
use std::future::Future;
use std::io::{self, BufRead, Cursor, Read, Write};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::task::{Context, Poll, Wake, Waker};

use parrot_cli::dispatch::dialog::{
    BasicSlashDialog, DialogOption, SecretKey, SecretKeyInput, SlashDialog, load,
};
use parrot_domain::contracts::async_ops::{BoxFuture, Cancellation};
use tonic::Code;

#[derive(Clone, Default)]
struct Token(Arc<AtomicBool>);

impl Token {
    fn cancel(&self) {
        self.0.store(true, Ordering::SeqCst);
    }
}

impl Cancellation for Token {
    fn is_cancelled(&self) -> bool {
        self.0.load(Ordering::SeqCst)
    }

    fn cancelled(&self) -> BoxFuture<'_, ()> {
        if self.is_cancelled() {
            Box::pin(async {})
        } else {
            Box::pin(std::future::pending())
        }
    }
}

struct NoWake;
impl Wake for NoWake {
    fn wake(self: Arc<Self>) {}
}

// Supplied-memory operations finish on the first poll. This deliberately does
// not pretend to be the application's asynchronous runtime or blocking-I/O join.
fn complete<T>(future: impl Future<Output = T>) -> T {
    let waker = Waker::from(Arc::new(NoWake));
    match std::pin::pin!(future)
        .as_mut()
        .poll(&mut Context::from_waker(&waker))
    {
        Poll::Ready(value) => value,
        Poll::Pending => panic!("supplied-stream dialog unexpectedly yielded"),
    }
}

fn options() -> Vec<DialogOption> {
    vec![
        DialogOption::new("build", "Build", "Implement a change"),
        DialogOption::new("review", "Review", "Inspect a change"),
    ]
}

fn text(bytes: Vec<u8>) -> String {
    String::from_utf8(bytes).unwrap()
}

fn newline() -> &'static str {
    if cfg!(windows) { "\r\n" } else { "\n" }
}

#[test]
fn selects_by_number_or_id_retries_invalid_input_and_dismisses_blank_or_end_of_input() {
    for (input, expected, invalid) in [
        ("wrong\nbuild\n", Some("build"), true),
        ("2\n", Some("review"), false),
        ("\n", None, false),
        ("", None, false),
        (" \t\n", None, false),
    ] {
        let token = Token::default();
        let mut dialog = BasicSlashDialog::new(Cursor::new(input), Vec::new(), Vec::new());
        let port: &mut dyn SlashDialog = &mut dialog;
        let selected = complete(port.select("Choose a mode", &options(), &token)).unwrap();
        assert_eq!(selected.as_ref().map(|option| option.id.as_str()), expected);
        let (_, output, error) = dialog.into_parts();
        assert!(text(output).contains("Choose a mode"));
        assert_eq!(
            text(error),
            if invalid {
                format!("Choose a listed number or id.{}", newline())
            } else {
                String::new()
            }
        );
    }
}

#[test]
fn selection_renders_source_menu_and_reprompts_without_reprinting_it() {
    let mut dialog = BasicSlashDialog::new(Cursor::new("wrong\n2\n"), Vec::new(), Vec::new());
    complete(dialog.select("Choose a mode", &options(), &Token::default())).unwrap();
    let (_, output, _) = dialog.into_parts();
    let nl = newline();
    assert_eq!(
        text(output),
        format!(
            "Choose a mode{nl}  1. Build — Implement a change{nl}  2. Review — Inspect a change{nl}> > "
        )
    );
}

#[test]
fn selection_matches_only_ordinal_ids_or_unsigned_in_range_i32_numbers() {
    for answer in ["Build", "BUILD", "+1", "-1", "0", "3", "2147483648", "١"] {
        let mut dialog = BasicSlashDialog::new(
            Cursor::new(format!("{answer}\nbuild\n")),
            Vec::new(),
            Vec::new(),
        );
        assert_eq!(
            complete(dialog.select("Mode", &options(), &Token::default()))
                .unwrap()
                .unwrap()
                .id,
            "build"
        );
        assert_eq!(
            text(dialog.into_parts().2),
            format!("Choose a listed number or id.{}", newline())
        );
    }
    for answer in ["01", " 1 ", "build"] {
        let mut dialog =
            BasicSlashDialog::new(Cursor::new(format!("{answer}\n")), Vec::new(), Vec::new());
        assert_eq!(
            complete(dialog.select("Mode", &options(), &Token::default()))
                .unwrap()
                .unwrap()
                .id,
            "build"
        );
        assert!(dialog.into_parts().2.is_empty());
    }
}

#[test]
fn selection_number_precedes_numeric_id_and_out_of_range_number_can_match_id() {
    let choices = [
        DialogOption::new("2", "First", ""),
        DialogOption::new("3", "Second", ""),
    ];
    for (input, expected) in [("2\n", "3"), ("3\n", "3")] {
        let mut dialog = BasicSlashDialog::new(Cursor::new(input), Vec::new(), Vec::new());
        assert_eq!(
            complete(dialog.select("Mode", &choices, &Token::default()))
                .unwrap()
                .unwrap()
                .id,
            expected
        );
    }
}

#[test]
fn confirmation_shows_message_and_accepts_only_yes() {
    for (input, expected) in [
        ("y\n", true),
        (" YES \n", true),
        ("n\n", false),
        ("\n", false),
        ("", false),
        ("true\n", false),
        ("yeah\n", false),
    ] {
        let mut dialog = BasicSlashDialog::new(Cursor::new(input), Vec::new(), Vec::new());
        assert_eq!(
            complete(dialog.confirm(&["Continue".into()], &Token::default())).unwrap(),
            expected
        );
        let (_, output, error) = dialog.into_parts();
        let nl = newline();
        assert_eq!(text(output), format!("Continue{nl}Continue? [y/N]{nl}> "));
        assert!(error.is_empty());
    }
}

#[test]
fn text_secret_show_and_error_use_the_supplied_streams() {
    let token = Token::default();
    let mut dialog = BasicSlashDialog::new(Cursor::new("answer\nsecret\n"), Vec::new(), Vec::new());
    assert_eq!(
        complete(dialog.read_text("Question", &token))
            .unwrap()
            .as_deref(),
        Some("answer")
    );
    assert_eq!(
        complete(dialog.read_secret("Key", &token))
            .unwrap()
            .as_deref(),
        Some("secret")
    );
    complete(dialog.show(&["first".into(), "second".into()], &token)).unwrap();
    complete(dialog.show_error("failure", &token)).unwrap();
    let (_, output, error) = dialog.into_parts();
    let nl = newline();
    assert_eq!(
        text(output),
        format!("Question{nl}> Key{nl}> first{nl}second{nl}")
    );
    assert_eq!(text(error), format!("failure{nl}"));
}

#[test]
fn text_preserves_whitespace_and_handles_cr_lf_crlf_blank_and_eof() {
    let mut dialog = BasicSlashDialog::new(Cursor::new(" a \rb\r\n\nc"), Vec::new(), Vec::new());
    let token = Token::default();
    for expected in [Some(" a "), Some("b"), Some(""), Some("c"), None] {
        assert_eq!(
            complete(dialog.read_text("?", &token)).unwrap().as_deref(),
            expected
        );
    }
}

#[derive(Default)]
struct Writer {
    bytes: Vec<u8>,
    flushes: usize,
    fail_write: bool,
    fail_flush: bool,
    cancel_on_write: Option<Token>,
}

impl Write for Writer {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        if self.fail_write {
            return Err(io::Error::other("write failed"));
        }
        self.bytes.extend_from_slice(bytes);
        if let Some(token) = &self.cancel_on_write {
            token.cancel();
        }
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        self.flushes += 1;
        if self.fail_flush {
            Err(io::Error::other("flush failed"))
        } else {
            Ok(())
        }
    }
}

#[test]
fn print_flushes_but_show_does_not() {
    let token = Token::default();
    let mut dialog = BasicSlashDialog::new(Cursor::new(""), Writer::default(), Vec::new());
    complete(dialog.show(&["shown".into()], &token)).unwrap();
    complete(dialog.print(&["printed".into()], &token)).unwrap();
    let (_, output, _) = dialog.into_parts();
    assert_eq!(output.flushes, 1);
    let nl = newline();
    assert_eq!(text(output.bytes), format!("shown{nl}printed{nl}"));
}

#[test]
fn load_passes_through_typed_result_failure_and_cancellation_without_ui() {
    let token = Token::default();
    let mut dialog = BasicSlashDialog::new(Cursor::new(""), Writer::default(), Vec::new());
    assert_eq!(
        complete(load(&mut dialog, "Loading", async { Ok(42) }, &token)).unwrap(),
        42
    );
    let failure = tonic::Status::not_found("load failed");
    let returned = complete(load::<()>(
        &mut dialog,
        "Loading",
        async { Err(failure) },
        &token,
    ))
    .unwrap_err();
    assert_eq!(returned.code(), Code::NotFound);
    assert_eq!(returned.message(), "load failed");
    let cancelled = complete(load::<()>(
        &mut dialog,
        "Loading",
        async { Err(tonic::Status::cancelled("work cancelled")) },
        &token,
    ))
    .unwrap_err();
    assert_eq!(cancelled.code(), Code::Cancelled);
    assert_eq!(cancelled.message(), "work cancelled");
    token.cancel();
    // Basic Load delegates even if already cancelled: the supplied work owns
    // cancellation and must receive/run with the caller's actual token.
    let result = complete(load(
        &mut dialog,
        "Loading",
        async { Ok(token.is_cancelled()) },
        &token,
    ))
    .unwrap();
    assert!(result);
    let (_, output, error) = dialog.into_parts();
    assert!(output.bytes.is_empty());
    assert_eq!(output.flushes, 0);
    assert!(error.is_empty());
}

#[test]
fn cancelled_operations_do_not_touch_input_output_or_error() {
    let token = Token::default();
    token.cancel();
    let mut dialog = BasicSlashDialog::new(
        FailingReader,
        Writer {
            fail_write: true,
            ..Writer::default()
        },
        Writer {
            fail_write: true,
            ..Writer::default()
        },
    );
    assert_eq!(
        complete(dialog.select("Mode", &options(), &token))
            .unwrap_err()
            .code(),
        Code::Cancelled
    );
    assert_eq!(
        complete(dialog.read_text("?", &token)).unwrap_err().code(),
        Code::Cancelled
    );
    assert_eq!(
        complete(dialog.read_secret("?", &token))
            .unwrap_err()
            .code(),
        Code::Cancelled
    );
    assert_eq!(
        complete(dialog.confirm(&["?".into()], &token))
            .unwrap_err()
            .code(),
        Code::Cancelled
    );
    assert_eq!(
        complete(dialog.show(&["?".into()], &token))
            .unwrap_err()
            .code(),
        Code::Cancelled
    );
    assert_eq!(
        complete(dialog.print(&[], &token)).unwrap_err().code(),
        Code::Cancelled
    );
    assert_eq!(
        complete(dialog.show_error("?", &token)).unwrap_err().code(),
        Code::Cancelled
    );
    let (_, output, error) = dialog.into_parts();
    assert!(output.bytes.is_empty());
    assert!(error.bytes.is_empty());
    assert_eq!(output.flushes, 0);
}

#[test]
fn cancellation_is_rechecked_between_output_boundaries() {
    let token = Token::default();
    let mut dialog = BasicSlashDialog::new(
        FailingReader,
        Writer {
            cancel_on_write: Some(token.clone()),
            ..Writer::default()
        },
        Vec::new(),
    );
    assert_eq!(
        complete(dialog.read_text("Question", &token))
            .unwrap_err()
            .code(),
        Code::Cancelled
    );
    assert_eq!(text(dialog.into_parts().1.bytes), "Question");
}

struct FailingReader;
impl Read for FailingReader {
    fn read(&mut self, _: &mut [u8]) -> io::Result<usize> {
        Err(io::Error::other("read failed"))
    }
}
impl BufRead for FailingReader {
    fn fill_buf(&mut self) -> io::Result<&[u8]> {
        Err(io::Error::other("read failed"))
    }
    fn consume(&mut self, _: usize) {}
}

#[test]
fn input_output_error_and_flush_failures_are_not_swallowed() {
    let token = Token::default();
    let mut dialog = BasicSlashDialog::new(FailingReader, Vec::new(), Vec::new());
    let error = complete(dialog.read_text("?", &token)).unwrap_err();
    assert_eq!(error.code(), Code::Internal);
    assert_eq!(error.message(), "read failed");
    let mut dialog = BasicSlashDialog::new(
        Cursor::new(""),
        Writer {
            fail_write: true,
            ..Writer::default()
        },
        Writer {
            fail_write: true,
            ..Writer::default()
        },
    );
    assert_eq!(
        complete(dialog.show(&["?".into()], &token))
            .unwrap_err()
            .message(),
        "write failed"
    );
    assert_eq!(
        complete(dialog.show_error("?", &token))
            .unwrap_err()
            .message(),
        "write failed"
    );
    let mut dialog = BasicSlashDialog::new(
        Cursor::new(""),
        Writer {
            fail_flush: true,
            ..Writer::default()
        },
        Vec::new(),
    );
    assert_eq!(
        complete(dialog.print(&[], &token)).unwrap_err().message(),
        "flush failed"
    );
    let mut dialog = BasicSlashDialog::new(
        Cursor::new("wrong\n"),
        Vec::new(),
        Writer {
            fail_write: true,
            ..Writer::default()
        },
    );
    assert_eq!(
        complete(dialog.select("Mode", &options(), &token))
            .unwrap_err()
            .message(),
        "write failed"
    );
}

struct Keys(VecDeque<SecretKey>);
impl SecretKeyInput for Keys {
    fn read_key(&mut self) -> io::Result<SecretKey> {
        self.0
            .pop_front()
            .ok_or_else(|| io::Error::other("keys exhausted"))
    }
}

#[test]
fn secret_key_input_edits_filters_controls_and_never_echoes_contents() {
    let keys = Keys(VecDeque::from([
        SecretKey::Backspace,
        SecretKey::Character('a'),
        SecretKey::Character('b'),
        SecretKey::Backspace,
        SecretKey::Character('\t'),
        SecretKey::Character('c'),
        SecretKey::Enter,
    ]));
    let mut dialog =
        BasicSlashDialog::new(FailingReader, Vec::new(), Vec::new()).with_secret_input(keys);
    assert_eq!(
        complete(dialog.read_secret("Key", &Token::default()))
            .unwrap()
            .as_deref(),
        Some("ac")
    );
    let nl = newline();
    assert_eq!(text(dialog.into_parts().1), format!("Key{nl}> {nl}"));
}

#[test]
fn secret_key_escape_and_control_c_dismiss_and_key_errors_propagate() {
    for end in [SecretKey::Escape, SecretKey::ControlC] {
        let keys = Keys(VecDeque::from([SecretKey::Character('s'), end]));
        let mut dialog =
            BasicSlashDialog::new(FailingReader, Vec::new(), Vec::new()).with_secret_input(keys);
        assert_eq!(
            complete(dialog.read_secret("Key", &Token::default())).unwrap(),
            None
        );
        let nl = newline();
        assert_eq!(text(dialog.into_parts().1), format!("Key{nl}> {nl}"));
    }
    let mut dialog = BasicSlashDialog::new(FailingReader, Vec::new(), Vec::new())
        .with_secret_input(Keys(VecDeque::new()));
    assert_eq!(
        complete(dialog.read_secret("Key", &Token::default()))
            .unwrap_err()
            .message(),
        "keys exhausted"
    );
}
