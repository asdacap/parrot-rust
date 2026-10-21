//! Shared slash-dialog consumer port and the supplied-stream basic dialog.
//!
//! This is the minimal consumer contract for the command components. T72 remains
//! the primary owner of the source dialog interfaces; final integration must use
//! this one contract rather than introducing another option or dialog schema.

use std::future::Future;
use std::io::{self, BufRead, Write};

use parrot_domain::contracts::async_ops::{BoxFuture, Cancellation};
use tonic::Status;

/// The source option matches its ID exactly (labels are not aliases).
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DialogOption {
    pub id: String,
    pub label: String,
    pub description: String,
}

impl DialogOption {
    pub fn new(
        id: impl Into<String>,
        label: impl Into<String>,
        description: impl Into<String>,
    ) -> Self {
        Self {
            id: id.into(),
            label: label.into(),
            description: description.into(),
        }
    }

    pub fn matches(&self, answer: &str) -> bool {
        self.id == answer
    }
}

/// Object-safe dialog boundary shared by terminal and browser command consumers.
///
/// `load` is type-erased only at this boundary; [`load`] retains a typed result.
/// An implementation must run the supplied work to completion before returning
/// success. Browser adapters must delegate to the actual WebSlashDialog methods.
pub trait SlashDialog: Send {
    fn select<'a>(
        &'a mut self,
        title: &'a str,
        options: &'a [DialogOption],
        cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<Option<DialogOption>, Status>>;

    fn read_text<'a>(
        &'a mut self,
        prompt: &'a str,
        cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<Option<String>, Status>>;

    fn read_secret<'a>(
        &'a mut self,
        prompt: &'a str,
        cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<Option<String>, Status>>;

    fn confirm<'a>(
        &'a mut self,
        lines: &'a [String],
        cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<bool, Status>>;

    fn show<'a>(
        &'a mut self,
        lines: &'a [String],
        cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<(), Status>>;

    fn print<'a>(
        &'a mut self,
        lines: &'a [String],
        cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<(), Status>>;

    fn show_error<'a>(
        &'a mut self,
        message: &'a str,
        cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<(), Status>>;

    fn load<'a>(
        &'a mut self,
        activity: &'a str,
        work: BoxFuture<'a, Result<(), Status>>,
        cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<(), Status>>;
}

/// Runs typed work through the same object-safe loading boundary. As in the
/// source and WebSlashDialog, the work receives cancellation from its caller.
pub async fn load<T: Send>(
    dialog: &mut dyn SlashDialog,
    activity: &str,
    work: impl Future<Output = Result<T, Status>> + Send,
    cancellation: &dyn Cancellation,
) -> Result<T, Status> {
    let mut result = None;
    dialog
        .load(
            activity,
            Box::pin(async {
                result = Some(work.await?);
                Ok(())
            }),
            cancellation,
        )
        .await?;
    result.ok_or_else(|| Status::internal("dialog load did not run supplied work"))
}

/// Key input must provide intercepted keys without echoing their contents.
/// Installing a real console implementation (including echo restoration) is a
/// FINAL-WIRING-DEFERRED platform edge, not provided by supplied streams.
pub trait SecretKeyInput: Send {
    fn read_key(&mut self) -> io::Result<SecretKey>;
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SecretKey {
    Enter,
    Escape,
    ControlC,
    Backspace,
    Character(char),
}

/// Input for the single Basic dialog and interactive loop.
///
/// The blanket supplied-reader adapter remains synchronous: it cannot cancel an
/// already-blocked `BufRead`. Native callers supply a cooperatively polled input.
pub trait DialogInput: Send {
    fn read_byte<'a>(
        &'a mut self,
        cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, io::Result<Option<u8>>>;

    /// Keeps the actual native raw/echo lease alive through a secret read.
    /// Supplied readers have no inferred console identity or echo capability.
    fn secret_lease(&mut self) -> io::Result<Option<crate::terminal_input::RawTerminal>> {
        Ok(None)
    }
}

impl<R: BufRead + Send> DialogInput for R {
    fn read_byte<'a>(
        &'a mut self,
        cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, io::Result<Option<u8>>> {
        Box::pin(async move {
            cancellation
                .check()
                .map_err(|error| io::Error::new(io::ErrorKind::Interrupted, error))?;
            let mut byte = [0];
            Ok((self.read(&mut byte)? != 0).then_some(byte[0]))
        })
    }
}

/// Basic source dialog over explicitly supplied streams. Output is synchronous;
/// a blocked supplied writer is not made cancellable by the async input port.
pub struct BasicSlashDialog<R: DialogInput, W: Write, E: Write> {
    input: R,
    output: W,
    error: E,
    secret_keys: Option<Box<dyn SecretKeyInput>>,
    skip_lf: bool,
}

impl<R: DialogInput, W: Write, E: Write> BasicSlashDialog<R, W, E> {
    pub fn new(input: R, output: W, error: E) -> Self {
        Self {
            input,
            output,
            error,
            secret_keys: None,
            skip_lf: false,
        }
    }

    pub fn with_secret_input(mut self, keys: impl SecretKeyInput + 'static) -> Self {
        self.secret_keys = Some(Box::new(keys));
        self
    }

    pub fn into_parts(self) -> (R, W, E) {
        (self.input, self.output, self.error)
    }

    fn prompt(&mut self, prompt: &str, cancellation: &dyn Cancellation) -> Result<(), Status> {
        write_line(&mut self.output, prompt, cancellation)?;
        write(&mut self.output, b"> ", cancellation)?;
        self.output.flush().map_err(io_status)
    }

    async fn read_byte(&mut self, cancellation: &dyn Cancellation) -> Result<Option<u8>, Status> {
        check(cancellation)?;
        let result = self.input.read_byte(cancellation).await;
        check(cancellation)?;
        result.map_err(io_status)
    }

    pub async fn read_line(
        &mut self,
        cancellation: &dyn Cancellation,
    ) -> Result<Option<String>, Status> {
        let mut line = Vec::new();
        loop {
            check(cancellation)?;
            let Some(byte) = self.read_byte(cancellation).await? else {
                return if line.is_empty() {
                    Ok(None)
                } else {
                    String::from_utf8(line)
                        .map(Some)
                        .map_err(|error| Status::internal(error.to_string()))
                };
            };
            if self.skip_lf {
                self.skip_lf = false;
                if byte == b'\n' {
                    continue;
                }
            }
            match byte {
                b'\r' | b'\n' => {
                    self.skip_lf = byte == b'\r';
                    return String::from_utf8(line)
                        .map(Some)
                        .map_err(|error| Status::internal(error.to_string()));
                }
                byte => line.push(byte),
            }
        }
    }

    fn read_secret_keys(
        &mut self,
        cancellation: &dyn Cancellation,
    ) -> Result<Option<String>, Status> {
        let mut typed = String::new();
        loop {
            check(cancellation)?;
            let key = self
                .secret_keys
                .as_mut()
                .expect("secret key input selected")
                .read_key()
                .map_err(io_status)?;
            if let Some(accepted) = update_secret(&mut typed, key) {
                write_line(&mut self.output, "", cancellation)?;
                return Ok(accepted.then_some(typed));
            }
        }
    }

    async fn read_native_secret(
        &mut self,
        cancellation: &dyn Cancellation,
    ) -> Result<Option<String>, Status> {
        let mut typed = String::new();
        loop {
            let Some(first) = self.read_byte(cancellation).await? else {
                return Ok(None);
            };
            let key = match first {
                b'\r' | b'\n' => SecretKey::Enter,
                27 => SecretKey::Escape,
                3 => SecretKey::ControlC,
                8 | 127 => SecretKey::Backspace,
                first => {
                    let mut bytes = vec![first];
                    let character = loop {
                        match std::str::from_utf8(&bytes) {
                            Ok(text) => break text.chars().next().expect("secret byte"),
                            Err(error) if error.error_len().is_none() && bytes.len() < 4 => {
                                let Some(next) = self.read_byte(cancellation).await? else {
                                    return Err(Status::internal("incomplete UTF-8 secret input"));
                                };
                                bytes.push(next);
                            }
                            Err(error) => return Err(Status::internal(error.to_string())),
                        }
                    };
                    SecretKey::Character(character)
                }
            };
            if let Some(accepted) = update_secret(&mut typed, key) {
                write_line(&mut self.output, "", cancellation)?;
                return Ok(accepted.then_some(typed));
            }
        }
    }
}

fn update_secret(typed: &mut String, key: SecretKey) -> Option<bool> {
    match key {
        SecretKey::Enter => return Some(true),
        SecretKey::Escape | SecretKey::ControlC => return Some(false),
        SecretKey::Backspace => {
            let _ = typed.pop();
        }
        SecretKey::Character(character) if !character.is_control() => typed.push(character),
        SecretKey::Character(_) => {}
    }
    None
}

impl<R: DialogInput, W: Write + Send, E: Write + Send> SlashDialog for BasicSlashDialog<R, W, E> {
    fn select<'a>(
        &'a mut self,
        title: &'a str,
        options: &'a [DialogOption],
        cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<Option<DialogOption>, Status>> {
        Box::pin(async move {
            write_line(&mut self.output, title, cancellation)?;
            for (index, option) in options.iter().enumerate() {
                write_line(
                    &mut self.output,
                    &format!("  {}. {} — {}", index + 1, option.label, option.description),
                    cancellation,
                )?;
            }
            loop {
                write(&mut self.output, b"> ", cancellation)?;
                self.output.flush().map_err(io_status)?;
                let Some(answer) = self.read_line(cancellation).await? else {
                    return Ok(None);
                };
                let answer = answer.trim();
                if answer.is_empty() {
                    return Ok(None);
                }
                // NumberStyles.None accepts ASCII digits only, without a sign.
                let numbered = answer
                    .bytes()
                    .all(|byte| byte.is_ascii_digit())
                    .then(|| answer.parse::<i32>().ok())
                    .flatten()
                    .and_then(|number| usize::try_from(number).ok())
                    .and_then(|number| number.checked_sub(1))
                    .and_then(|index| options.get(index));
                if let Some(selected) =
                    numbered.or_else(|| options.iter().find(|o| o.matches(answer)))
                {
                    return Ok(Some(selected.clone()));
                }
                write_line(
                    &mut self.error,
                    "Choose a listed number or id.",
                    cancellation,
                )?;
            }
        })
    }

    fn read_text<'a>(
        &'a mut self,
        prompt: &'a str,
        cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<Option<String>, Status>> {
        Box::pin(async move {
            self.prompt(prompt, cancellation)?;
            self.read_line(cancellation).await
        })
    }

    fn read_secret<'a>(
        &'a mut self,
        prompt: &'a str,
        cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<Option<String>, Status>> {
        Box::pin(async move {
            check(cancellation)?;
            self.prompt(prompt, cancellation)?;
            if self.secret_keys.is_some() {
                self.read_secret_keys(cancellation)
            } else if let Some(_lease) = self.input.secret_lease().map_err(io_status)? {
                self.read_native_secret(cancellation).await
            } else {
                self.read_line(cancellation).await
            }
        })
    }

    fn confirm<'a>(
        &'a mut self,
        lines: &'a [String],
        cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<bool, Status>> {
        Box::pin(async move {
            self.show(lines, cancellation).await?;
            self.prompt("Continue? [y/N]", cancellation)?;
            Ok(self.read_line(cancellation).await?.is_some_and(|answer| {
                let answer = answer.trim();
                answer.eq_ignore_ascii_case("y") || answer.eq_ignore_ascii_case("yes")
            }))
        })
    }

    fn show<'a>(
        &'a mut self,
        lines: &'a [String],
        cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<(), Status>> {
        Box::pin(async move {
            for line in lines {
                write_line(&mut self.output, line, cancellation)?;
            }
            Ok(())
        })
    }

    fn print<'a>(
        &'a mut self,
        lines: &'a [String],
        cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<(), Status>> {
        Box::pin(async move {
            self.show(lines, cancellation).await?;
            check(cancellation)?;
            self.output.flush().map_err(io_status)
        })
    }

    fn show_error<'a>(
        &'a mut self,
        message: &'a str,
        cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<(), Status>> {
        Box::pin(async move { write_line(&mut self.error, message, cancellation) })
    }

    fn load<'a>(
        &'a mut self,
        _activity: &'a str,
        work: BoxFuture<'a, Result<(), Status>>,
        _cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<(), Status>> {
        work
    }
}

fn check(cancellation: &dyn Cancellation) -> Result<(), Status> {
    cancellation
        .check()
        .map_err(|error| Status::cancelled(error.to_string()))
}

fn io_status(error: io::Error) -> Status {
    Status::internal(error.to_string())
}

fn write(
    output: &mut impl Write,
    bytes: &[u8],
    cancellation: &dyn Cancellation,
) -> Result<(), Status> {
    check(cancellation)?;
    output.write_all(bytes).map_err(io_status)
}

fn write_line(
    output: &mut impl Write,
    line: &str,
    cancellation: &dyn Cancellation,
) -> Result<(), Status> {
    write(output, line.as_bytes(), cancellation)?;
    write(
        output,
        if cfg!(windows) { b"\r\n" } else { b"\n" },
        cancellation,
    )
}

#[cfg(test)]
mod async_input_tests {
    use super::*;
    use parrot_runtime::agent_turn::lifecycle::Signal;
    use std::io::Cursor;

    #[tokio::test]
    async fn supplied_reader_keeps_crlf_and_eof_behavior_through_async_port() {
        let c = Signal::new();
        let mut dialog =
            BasicSlashDialog::new(Cursor::new(b"one\r\ntwo\rthree"), Vec::new(), Vec::new());
        assert_eq!(dialog.read_line(&c).await.unwrap().as_deref(), Some("one"));
        assert_eq!(dialog.read_line(&c).await.unwrap().as_deref(), Some("two"));
        assert_eq!(
            dialog.read_line(&c).await.unwrap().as_deref(),
            Some("three")
        );
        assert_eq!(dialog.read_line(&c).await.unwrap(), None);
    }

    #[tokio::test]
    async fn native_secret_key_rules_keep_unicode_backspace_and_no_echo() {
        let c = Signal::new();
        let mut dialog = BasicSlashDialog::new(
            Cursor::new("aé\u{7f}文\r".as_bytes()),
            Vec::new(),
            Vec::new(),
        );
        assert_eq!(
            dialog.read_native_secret(&c).await.unwrap().as_deref(),
            Some("a文")
        );
        let (_, output, _) = dialog.into_parts();
        assert_eq!(output, b"\n");
    }

    struct Waiting;
    impl DialogInput for Waiting {
        fn read_byte<'a>(
            &'a mut self,
            c: &'a dyn Cancellation,
        ) -> BoxFuture<'a, io::Result<Option<u8>>> {
            Box::pin(async move {
                c.cancelled().await;
                Err(io::Error::new(io::ErrorKind::Interrupted, "stopped"))
            })
        }
    }

    #[tokio::test]
    async fn cancelled_native_style_read_is_joined_and_preserves_cancelled_status() {
        let c = Signal::new();
        let mut dialog = BasicSlashDialog::new(Waiting, Vec::new(), Vec::new());
        let reading = dialog.read_line(&c);
        let cancelling = async {
            tokio::task::yield_now().await;
            c.cancel();
        };
        let (result, ()) = tokio::join!(reading, cancelling);
        assert_eq!(result.unwrap_err().code(), tonic::Code::Cancelled);
    }
}
