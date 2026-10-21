//! Native raw input, ported from UnixRawTerminal at 5f27a8a92625ee31d8bec094e5c3e09550193781.
//! The descriptor is borrowed: its owner must keep it open until restoration. Signal ownership
//! belongs to the caller, which must unwind/close normally; SIGKILL cannot run `Drop`.

use std::io;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

/// libc termios on Linux x86_64/aarch64 (not the shorter kernel ioctl termios).
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct LinuxTermios {
    pub input_flags: u32,
    pub output_flags: u32,
    pub control_flags: u32,
    pub local_flags: u32,
    pub line: u8,
    pub control_characters: [u8; 32],
    pub input_speed: u32,
    pub output_speed: u32,
}

/// Darwin's 64-bit libc termios, on both Intel and Apple Silicon.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct DarwinTermios {
    pub input_flags: u64,
    pub output_flags: u64,
    pub control_flags: u64,
    pub local_flags: u64,
    pub control_characters: [u8; 20],
    pub input_speed: u64,
    pub output_speed: u64,
}

pub fn make_linux_raw(mut original: LinuxTermios) -> LinuxTermios {
    original.input_flags &= !0x532;
    original.control_flags = (original.control_flags & !0x30) | 0x30;
    original.local_flags &= !0x800b;
    original.control_characters[6] = 0;
    original.control_characters[5] = 1;
    original
}

pub fn make_darwin_raw(mut original: DarwinTermios) -> DarwinTermios {
    original.input_flags &= !0x332;
    original.control_flags = (original.control_flags & !0x300) | 0x300;
    original.local_flags &= !0x588;
    original.control_characters[16] = 0;
    original.control_characters[17] = 1;
    original
}

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
    use std::ffi::c_void;
    use std::time::Instant;

    #[cfg(target_os = "linux")]
    type Termios = LinuxTermios;
    #[cfg(target_os = "macos")]
    type Termios = DarwinTermios;
    #[cfg(target_os = "linux")]
    const RAW_LOCAL_FLAGS: u32 = 0x800b;
    #[cfg(target_os = "macos")]
    const RAW_LOCAL_FLAGS: u64 = 0x588;
    #[cfg(target_os = "linux")]
    const WOULD_BLOCK: i32 = 11;
    #[cfg(target_os = "macos")]
    const WOULD_BLOCK: i32 = 35;

    unsafe extern "C" {
        fn isatty(fd: i32) -> i32;
        fn tcgetattr(fd: i32, attributes: *mut Termios) -> i32;
        fn tcsetattr(fd: i32, actions: i32, attributes: *const Termios) -> i32;
        #[link_name = "read"]
        fn read_descriptor(fd: i32, buffer: *mut c_void, count: usize) -> isize;
    }

    fn get_attributes(fd: i32) -> io::Result<Termios> {
        let mut attributes = Termios::default();
        // SAFETY: the selected type exactly matches libc termios on the supported targets.
        if unsafe { tcgetattr(fd, &mut attributes) } == 0 {
            Ok(attributes)
        } else {
            Err(io::Error::last_os_error())
        }
    }

    fn set_attributes(fd: i32, attributes: &Termios) -> io::Result<()> {
        // SAFETY: attributes is a valid libc termios; TCSANOW is zero on both platforms.
        if unsafe { tcsetattr(fd, 0, attributes) } == 0 {
            Ok(())
        } else {
            Err(io::Error::last_os_error())
        }
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
            // SAFETY: isatty accepts any descriptor and rejects invalid/non-TTY inputs.
            if unsafe { isatty(descriptor) } != 1 {
                return Ok(None);
            }
            let original = match get_attributes(descriptor) {
                Ok(original) => original,
                Err(_) => return Ok(None),
            };
            #[cfg(target_os = "linux")]
            let raw = make_linux_raw(original);
            #[cfg(target_os = "macos")]
            let raw = make_darwin_raw(original);
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

        fn ensure_raw(&self) -> io::Result<()> {
            set_attributes(self.descriptor, &self.raw)?;
            let applied = get_attributes(self.descriptor)?;
            if applied.local_flags & RAW_LOCAL_FLAGS != 0 {
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
                || {
                    // SAFETY: buffer is writable for its length; fd remains caller-owned.
                    let count = unsafe {
                        read_descriptor(self.descriptor, buffer.as_mut_ptr().cast(), buffer.len())
                    };
                    if count < 0 {
                        Err(io::Error::last_os_error())
                    } else {
                        Ok(count as usize)
                    }
                },
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
                WOULD_BLOCK,
                cancel,
            )
        }

        /// Restore exactly once on success. A failed explicit restore can be retried by Drop.
        pub fn restore(&mut self) -> io::Result<()> {
            if !self.restored {
                set_attributes(self.descriptor, &self.original)?;
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
