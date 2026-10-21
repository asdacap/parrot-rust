//! Native raw input, ported from UnixRawTerminal at 5f27a8a92625ee31d8bec094e5c3e09550193781.
//! The descriptor is borrowed: its owner must keep it open until restoration. Signal ownership
//! belongs to the caller, which must unwind/close normally; SIGKILL cannot run `Drop`.

use std::io;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReadResult {
    /// VMIN=0/VTIME=1 timed out. This is never an EOF indication.
    PollTick,
    Bytes(usize),
}

fn check_cancel(cancel: &AtomicBool) -> io::Result<()> {
    if cancel.load(Ordering::Acquire) {
        Err(io::Error::new(
            io::ErrorKind::Interrupted,
            "terminal input cancelled",
        ))
    } else {
        Ok(())
    }
}

/// Retry EINTR immediately and EAGAIN at 10, 20, 40, 80 and 160 milliseconds.
/// The injected delay must honor cancellation; production waits check it every millisecond.
/// Cancellation is also checked before every attempt and after every delay/read.
pub fn read_input_with_retry(
    mut read: impl FnMut() -> io::Result<usize>,
    mut delay: impl FnMut(Duration) -> io::Result<()>,
    resource_temporarily_unavailable: i32,
    cancel: &AtomicBool,
) -> io::Result<ReadResult> {
    let mut retries = 0;
    loop {
        check_cancel(cancel)?;
        let result = read();
        check_cancel(cancel)?;
        match result {
            Ok(0) => return Ok(ReadResult::PollTick),
            Ok(count) => return Ok(ReadResult::Bytes(count)),
            Err(error) if error.raw_os_error() == Some(4) => continue,
            Err(error)
                if error.raw_os_error() == Some(resource_temporarily_unavailable)
                    && retries < 5 =>
            {
                delay(Duration::from_millis(10 << retries))?;
                retries += 1;
            }
            Err(error) => return Err(error),
        }
    }
}

#[cfg(all(
    any(target_os = "linux", target_os = "macos"),
    any(target_arch = "x86_64", target_arch = "aarch64")
))]
mod platform {
    use super::*;
    use rustix::fd::BorrowedFd;
    use rustix::termios::{
        self, ControlModes, InputModes, LocalModes, OptionalActions, SpecialCodeIndex, Termios,
    };
    use std::time::Instant;

    const RAW_LOCAL_MODES: LocalModes = LocalModes::ISIG
        .union(LocalModes::ICANON)
        .union(LocalModes::ECHO)
        .union(LocalModes::IEXTEN);

    fn make_raw(mut original: Termios) -> Termios {
        // Deliberately not cfmakeraw: retain output processing and unrelated flags.
        original.input_modes.remove(
            InputModes::BRKINT
                | InputModes::INPCK
                | InputModes::ISTRIP
                | InputModes::ICRNL
                | InputModes::IXON,
        );
        original.control_modes.remove(ControlModes::CSIZE);
        original.control_modes.insert(ControlModes::CS8);
        original.local_modes.remove(RAW_LOCAL_MODES);
        original.special_codes[SpecialCodeIndex::VMIN] = 0;
        original.special_codes[SpecialCodeIndex::VTIME] = 1;
        original
    }

    fn get_attributes(fd: BorrowedFd<'_>) -> io::Result<Termios> {
        termios::tcgetattr(fd).map_err(Into::into)
    }

    fn set_attributes(fd: BorrowedFd<'_>, attributes: &Termios) -> io::Result<()> {
        termios::tcsetattr(fd, OptionalActions::Now, attributes).map_err(Into::into)
    }

    /// Owns the raw-mode lease, but does not own or close its file descriptor.
    pub struct RawTerminal {
        descriptor: i32,
        original: Termios,
        raw: Termios,
        restored: bool,
    }

    impl RawTerminal {
        pub fn open_stdin() -> io::Result<Option<Self>> {
            if std::env::var_os("TERM").is_some_and(|term| term == "dumb") {
                return Ok(None);
            }
            Self::open(0)
        }

        pub fn open(descriptor: i32) -> io::Result<Option<Self>> {
            if descriptor < 0 {
                return Ok(None);
            }
            // SAFETY: the caller keeps this borrowed descriptor open through restoration.
            let fd = unsafe { BorrowedFd::borrow_raw(descriptor) };
            // tcgetattr also rejects non-terminals, matching libc isatty's probe.
            let original = match get_attributes(fd) {
                Ok(original) => original,
                Err(_) => return Ok(None),
            };
            let raw = make_raw(original.clone());
            let mut terminal = Self {
                descriptor,
                original,
                raw,
                restored: false,
            };
            if terminal.ensure_raw().is_err() {
                terminal.restore()?;
                return Ok(None);
            }
            Ok(Some(terminal))
        }

        fn descriptor(&self) -> BorrowedFd<'_> {
            // SAFETY: the descriptor remains caller-owned and open for this lease.
            unsafe { BorrowedFd::borrow_raw(self.descriptor) }
        }

        fn ensure_raw(&self) -> io::Result<()> {
            set_attributes(self.descriptor(), &self.raw)?;
            let applied = get_attributes(self.descriptor())?;
            if applied.local_modes.intersects(RAW_LOCAL_MODES) {
                return Err(io::Error::other("failed to verify raw terminal mode"));
            }
            Ok(())
        }

        /// Synchronous read; silence bounds cancellation latency to a VTIME tick (~100ms).
        /// This never interprets a zero-byte read as end-of-input.
        pub fn read(&mut self, buffer: &mut [u8], cancel: &AtomicBool) -> io::Result<ReadResult> {
            check_cancel(cancel)?;
            if self.restored {
                return Err(io::Error::new(
                    io::ErrorKind::BrokenPipe,
                    "raw terminal is restored",
                ));
            }
            self.ensure_raw()?;
            read_input_with_retry(
                || rustix::io::read(self.descriptor(), &mut *buffer).map_err(Into::into),
                |duration| {
                    let deadline = Instant::now() + duration;
                    loop {
                        check_cancel(cancel)?;
                        let remaining = deadline.saturating_duration_since(Instant::now());
                        if remaining.is_zero() {
                            return Ok(());
                        }
                        std::thread::sleep(remaining.min(Duration::from_millis(1)));
                    }
                },
                rustix::io::Errno::AGAIN.raw_os_error(),
                cancel,
            )
        }

        /// Restore exactly once on success. A failed explicit restore can be retried by Drop.
        pub fn restore(&mut self) -> io::Result<()> {
            if !self.restored {
                set_attributes(self.descriptor(), &self.original)?;
                self.restored = true;
            }
            Ok(())
        }
    }

    impl Drop for RawTerminal {
        fn drop(&mut self) {
            // Drop cannot report errors; callers needing diagnostics should explicitly restore.
            let _ = self.restore();
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        use rustix::termios::OutputModes;

        #[test]
        fn typed_raw_masks_preserve_unrelated_fields_and_poll_timeout() {
            // SAFETY: Termios contains only integer fields and integer bitflags;
            // zero is valid for every field on the supported Unix targets.
            let mut original: Termios = unsafe { std::mem::zeroed() };
            original.input_modes = InputModes::from_bits_retain(!0);
            original.local_modes = LocalModes::from_bits_retain(!0);
            original.output_modes = OutputModes::from_bits_retain(123);
            original.special_codes[SpecialCodeIndex::VINTR] = 9;
            original.set_speed(9600).unwrap();
            original
                .control_modes
                .insert(ControlModes::PARENB | ControlModes::CSTOPB);
            #[cfg(target_os = "linux")]
            {
                original.line_discipline = 7;
            }
            let raw = make_raw(original.clone());
            #[cfg(target_os = "linux")]
            let (input_mask, local_mask, character_size) = (0x532, 0x800b, 0x30);
            #[cfg(target_os = "macos")]
            let (input_mask, local_mask, character_size) = (0x332, 0x588, 0x300);
            assert_eq!(raw.input_modes.bits(), !input_mask);
            assert_eq!(raw.local_modes.bits(), !local_mask);
            assert_eq!(
                raw.control_modes.bits(),
                (original.control_modes.bits() & !character_size) | character_size
            );
            assert_eq!(raw.special_codes[SpecialCodeIndex::VMIN], 0);
            assert_eq!(raw.special_codes[SpecialCodeIndex::VTIME], 1);
            assert_eq!(raw.special_codes[SpecialCodeIndex::VINTR], 9);
            assert_eq!(raw.output_modes, original.output_modes);
            assert_eq!(raw.input_speed(), original.input_speed());
            assert_eq!(raw.output_speed(), original.output_speed());
            #[cfg(target_os = "linux")]
            assert_eq!(raw.line_discipline, original.line_discipline);
        }
    }
}

#[cfg(not(all(
    any(target_os = "linux", target_os = "macos"),
    any(target_arch = "x86_64", target_arch = "aarch64")
)))]
mod platform {
    use super::*;

    pub struct RawTerminal;
    impl RawTerminal {
        pub fn open_stdin() -> io::Result<Option<Self>> {
            Ok(None)
        }
        pub fn open(_descriptor: i32) -> io::Result<Option<Self>> {
            Ok(None)
        }
        pub fn read(&mut self, _buffer: &mut [u8], _cancel: &AtomicBool) -> io::Result<ReadResult> {
            Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "unsupported terminal ABI",
            ))
        }
        pub fn restore(&mut self) -> io::Result<()> {
            Ok(())
        }
    }
}

pub use platform::RawTerminal;
