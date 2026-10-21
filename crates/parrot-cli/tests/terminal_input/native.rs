//! Source cases: UnixRawTerminalTests.cs at 5f27a8a92625ee31d8bec094e5c3e09550193781.
//! Run through the workspace terminal-input integration target, not a synthetic PTY shim.

#[path = "../support/native_termios.rs"]
mod native_termios;
use native_termios::{DarwinTermios, LinuxTermios, make_darwin_raw, make_linux_raw};
use parrot_cli::terminal_input::native::{RawTerminal, ReadResult, read_input_with_retry};
use std::io;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

#[test]
fn linux_raw_masks_preserve_unrelated_fields_and_set_poll_timeout() {
    assert_eq!(std::mem::size_of::<LinuxTermios>(), 60);
    assert_eq!(std::mem::offset_of!(LinuxTermios, control_characters), 17);
    assert_eq!(std::mem::offset_of!(LinuxTermios, input_speed), 52);
    let original = LinuxTermios {
        input_flags: u32::MAX,
        output_flags: 123,
        control_flags: 0,
        local_flags: u32::MAX,
        line: 7,
        control_characters: [9; 32],
        input_speed: 42,
        output_speed: 43,
    };
    let raw = make_linux_raw(original);
    assert_eq!(raw.input_flags, !0x532);
    assert_eq!(raw.local_flags, !0x800b);
    assert_eq!(raw.control_flags, 0x30);
    assert_eq!(raw.control_characters[6], 0);
    assert_eq!(raw.control_characters[5], 1);
    assert_eq!(raw.control_characters[4], 9);
    assert_eq!(raw.output_flags, original.output_flags);
    assert_eq!(raw.input_speed, original.input_speed);
    assert_eq!(raw.output_speed, original.output_speed);
    assert_eq!(raw.line, original.line);
}

#[test]
fn darwin_raw_masks_preserve_unrelated_fields_and_set_poll_timeout() {
    assert_eq!(std::mem::size_of::<DarwinTermios>(), 72);
    assert_eq!(std::mem::offset_of!(DarwinTermios, control_characters), 32);
    assert_eq!(std::mem::offset_of!(DarwinTermios, input_speed), 56);
    let original = DarwinTermios {
        input_flags: u64::MAX,
        output_flags: 123,
        control_flags: 0,
        local_flags: u64::MAX,
        control_characters: [9; 20],
        input_speed: 42,
        output_speed: 43,
    };
    let raw = make_darwin_raw(original);
    assert_eq!(raw.input_flags, !0x332);
    assert_eq!(raw.local_flags, !0x588);
    assert_eq!(raw.control_flags, 0x300);
    assert_eq!(raw.control_characters[16], 0);
    assert_eq!(raw.control_characters[17], 1);
    assert_eq!(raw.control_characters[15], 9);
    assert_eq!(raw.output_flags, original.output_flags);
    assert_eq!(raw.input_speed, original.input_speed);
    assert_eq!(raw.output_speed, original.output_speed);
}

#[test]
fn read_retries_eintr_then_temporary_unavailability_with_source_backoff() {
    for would_block in [11, 35] {
        let mut results = [
            Err(4),
            Err(would_block),
            Err(would_block),
            Err(would_block),
            Ok(2),
        ]
        .into_iter();
        let mut delays = Vec::new();
        let result = read_input_with_retry(
            || {
                results
                    .next()
                    .unwrap()
                    .map_err(io::Error::from_raw_os_error)
            },
            |delay| {
                delays.push(delay.as_millis());
                Ok(())
            },
            would_block,
            &AtomicBool::new(false),
        )
        .unwrap();
        assert_eq!(result, ReadResult::Bytes(2));
        assert_eq!(delays, [10, 20, 40]);
        assert!(results.next().is_none());
    }
}

#[test]
fn read_stops_after_five_temporary_unavailability_retries() {
    let mut reads = 0;
    let mut delays = Vec::new();
    let error = read_input_with_retry(
        || {
            reads += 1;
            Err(io::Error::from_raw_os_error(11))
        },
        |delay| {
            delays.push(delay.as_millis());
            Ok(())
        },
        11,
        &AtomicBool::new(false),
    )
    .unwrap_err();
    assert_eq!(error.raw_os_error(), Some(11));
    assert_eq!(reads, 6);
    assert_eq!(delays, [10, 20, 40, 80, 160]);
}

#[test]
fn zero_is_a_poll_tick_not_latched_eof_and_other_errors_are_not_retried() {
    let cancel = AtomicBool::new(false);
    assert_eq!(
        read_input_with_retry(|| Ok(0), |_| panic!("unexpected wait"), 11, &cancel).unwrap(),
        ReadResult::PollTick
    );
    assert_eq!(
        read_input_with_retry(|| Ok(1), |_| panic!("unexpected wait"), 11, &cancel).unwrap(),
        ReadResult::Bytes(1)
    );
    assert_eq!(
        read_input_with_retry(
            || Err(io::Error::from_raw_os_error(9)),
            |_| panic!("unexpected wait"),
            11,
            &cancel
        )
        .unwrap_err()
        .raw_os_error(),
        Some(9)
    );
}

#[test]
fn cancellation_stops_before_read_and_after_retry_wait() {
    let cancel = AtomicBool::new(true);
    assert_eq!(
        read_input_with_retry(
            || panic!("cancelled read"),
            |_| panic!("cancelled wait"),
            11,
            &cancel
        )
        .unwrap_err()
        .kind(),
        io::ErrorKind::Interrupted
    );
    cancel.store(false, Ordering::Release);
    let mut reads = 0;
    let error = read_input_with_retry(
        || {
            reads += 1;
            Err(io::Error::from_raw_os_error(11))
        },
        |_| {
            cancel.store(true, Ordering::Release);
            Ok(())
        },
        11,
        &cancel,
    )
    .unwrap_err();
    assert_eq!(error.kind(), io::ErrorKind::Interrupted);
    assert_eq!(reads, 1);
}

#[test]
fn non_terminal_open_is_unavailable() {
    assert!(RawTerminal::open(-1).unwrap().is_none());
    let file = std::fs::File::open("/dev/null").unwrap();
    #[cfg(unix)]
    {
        use std::os::fd::AsRawFd;
        assert!(RawTerminal::open(file.as_raw_fd()).unwrap().is_none());
    }
}

#[cfg(all(
    any(target_os = "linux", target_os = "macos"),
    any(target_arch = "x86_64", target_arch = "aarch64")
))]
mod pty {
    use super::*;
    use std::ffi::{c_char, c_int, c_ulong, c_void};
    use std::io::{BufRead, BufReader, Write};
    use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
    use std::process::{Command, Stdio};
    use std::time::Instant;

    #[cfg(target_os = "linux")]
    type Attributes = LinuxTermios;
    #[cfg(target_os = "macos")]
    type Attributes = DarwinTermios;
    #[cfg(target_os = "linux")]
    const RESIZE: c_ulong = 0x5414;
    #[cfg(target_os = "macos")]
    const RESIZE: c_ulong = 0x80087467;
    #[repr(C)]
    struct WindowSize {
        rows: u16,
        columns: u16,
        x: u16,
        y: u16,
    }
    #[repr(C)]
    struct PollFd {
        fd: c_int,
        events: i16,
        revents: i16,
    }

    #[cfg_attr(target_os = "linux", link(name = "util"))]
    unsafe extern "C" {
        fn openpty(
            master: *mut c_int,
            slave: *mut c_int,
            name: *mut c_char,
            attributes: *const Attributes,
            size: *const WindowSize,
        ) -> c_int;
    }
    #[cfg(target_os = "linux")]
    type PollCount = c_ulong;
    #[cfg(target_os = "macos")]
    type PollCount = u32;
    unsafe extern "C" {
        fn tcgetattr(fd: c_int, attributes: *mut Attributes) -> c_int;
        fn tcsetattr(fd: c_int, actions: c_int, attributes: *const Attributes) -> c_int;
        fn write(fd: c_int, bytes: *const c_void, length: usize) -> isize;
        fn poll(descriptors: *mut PollFd, count: PollCount, timeout: c_int) -> c_int;
        fn ioctl(fd: c_int, request: c_ulong, ...) -> c_int;
        fn kill(pid: c_int, signal: c_int) -> c_int;
        fn signal(number: c_int, handler: usize) -> usize;
        #[cfg(target_os = "linux")]
        fn fcntl(fd: c_int, command: c_int, ...) -> c_int;
    }

    struct Pty {
        master: OwnedFd,
        slave: OwnedFd,
    }
    impl Pty {
        fn open() -> Self {
            let (mut master, mut slave) = (-1, -1);
            // SAFETY: openpty writes two descriptors; null optional arguments request defaults.
            assert_eq!(
                unsafe {
                    openpty(
                        &mut master,
                        &mut slave,
                        std::ptr::null_mut(),
                        std::ptr::null(),
                        std::ptr::null(),
                    )
                },
                0,
                "openpty: {}",
                io::Error::last_os_error()
            );
            // SAFETY: successful openpty transferred these new descriptors to us.
            unsafe {
                Self {
                    master: OwnedFd::from_raw_fd(master),
                    slave: OwnedFd::from_raw_fd(slave),
                }
            }
        }
        fn attributes(&self) -> Attributes {
            attributes(self.slave.as_raw_fd())
        }
        fn send(&self, byte: u8) {
            // SAFETY: byte points to one readable byte and master remains open.
            assert_eq!(
                unsafe { write(self.master.as_raw_fd(), (&byte as *const u8).cast(), 1) },
                1
            );
        }
        fn has_output(&self) -> bool {
            let mut descriptor = PollFd {
                fd: self.master.as_raw_fd(),
                events: 1,
                revents: 0,
            };
            // SAFETY: a valid one-element pollfd array. POLLIN is one on both OSes.
            let count = unsafe { poll(&mut descriptor, 1, 200) };
            assert!(count >= 0);
            count > 0
        }
        fn resize(&self) {
            let size = WindowSize {
                rows: 51,
                columns: 132,
                x: 0,
                y: 0,
            };
            // SAFETY: TIOCSWINSZ accepts a pointer to this platform's four-u16 winsize.
            assert_eq!(unsafe { ioctl(self.slave.as_raw_fd(), RESIZE, &size) }, 0);
        }
    }
    fn attributes(fd: c_int) -> Attributes {
        let mut attributes = Attributes::default();
        // SAFETY: matches the supported platform's libc termios layout.
        assert_eq!(unsafe { tcgetattr(fd, &mut attributes) }, 0);
        attributes
    }
    fn read_byte(raw: &mut RawTerminal, buffer: &mut [u8]) -> usize {
        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            assert!(Instant::now() < deadline, "PTY input timeout");
            if let ReadResult::Bytes(count) = raw.read(buffer, &AtomicBool::new(false)).unwrap() {
                return count;
            }
        }
    }

    #[test]
    fn real_pty_unbuffered_no_echo_poll_tick_reapply_resize_and_explicit_restore() {
        let pty = Pty::open();
        let original = pty.attributes();
        let mut raw = RawTerminal::open(pty.slave.as_raw_fd()).unwrap().unwrap();
        #[cfg(target_os = "linux")]
        assert_eq!(pty.attributes(), make_linux_raw(original));
        #[cfg(target_os = "macos")]
        assert_eq!(pty.attributes(), make_darwin_raw(original));
        let mut buffer = [0; 8];
        assert_eq!(
            raw.read(&mut buffer, &AtomicBool::new(false)).unwrap(),
            ReadResult::PollTick
        );
        pty.send(b'q');
        assert_eq!(read_byte(&mut raw, &mut buffer), 1);
        assert_eq!(buffer[0], b'q');
        assert!(!pty.has_output(), "kernel echo or alternate-screen output");
        // A higher-level console library may restore canonical mode between reads.
        // SAFETY: original was retrieved from this live slave descriptor.
        assert_eq!(unsafe { tcsetattr(pty.slave.as_raw_fd(), 0, &original) }, 0);
        assert_eq!(
            raw.read(&mut buffer, &AtomicBool::new(false)).unwrap(),
            ReadResult::PollTick
        );
        #[cfg(target_os = "linux")]
        assert_eq!(pty.attributes().local_flags & 0x800b, 0);
        #[cfg(target_os = "macos")]
        assert_eq!(pty.attributes().local_flags & 0x588, 0);
        pty.resize();
        pty.send(b'r');
        assert_eq!(read_byte(&mut raw, &mut buffer), 1);
        assert_eq!(buffer[0], b'r');
        raw.restore().unwrap();
        raw.restore().unwrap();
        assert_eq!(pty.attributes(), original);
        assert_eq!(
            raw.read(&mut buffer, &AtomicBool::new(false))
                .unwrap_err()
                .kind(),
            io::ErrorKind::BrokenPipe
        );
    }

    #[test]
    fn real_pty_drop_restores_on_unwind_and_cancellation() {
        let pty = Pty::open();
        let original = pty.attributes();
        let failure = std::panic::catch_unwind(|| {
            let _raw = RawTerminal::open(pty.slave.as_raw_fd()).unwrap().unwrap();
            panic!("exercise raw terminal unwind");
        });
        assert!(failure.is_err());
        assert_eq!(pty.attributes(), original);
        {
            let mut raw = RawTerminal::open(pty.slave.as_raw_fd()).unwrap().unwrap();
            let cancel = AtomicBool::new(false);
            std::thread::scope(|scope| {
                scope.spawn(|| {
                    std::thread::sleep(Duration::from_millis(20));
                    cancel.store(true, Ordering::Release);
                });
                let started = Instant::now();
                assert_eq!(
                    raw.read(&mut [0; 1], &cancel).unwrap_err().kind(),
                    io::ErrorKind::Interrupted
                );
                assert!(started.elapsed() < Duration::from_secs(1));
            });
        }
        assert_eq!(pty.attributes(), original);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn real_linux_nonblocking_pty_retry_wait_is_cancellable_and_restores() {
        let pty = Pty::open();
        let original = pty.attributes();
        // SAFETY: F_GETFL=3, F_SETFL=4, O_NONBLOCK=0x800 for supported Linux ABIs.
        let flags = unsafe { fcntl(pty.slave.as_raw_fd(), 3) };
        assert!(flags >= 0);
        assert_eq!(unsafe { fcntl(pty.slave.as_raw_fd(), 4, flags | 0x800) }, 0);
        {
            let mut raw = RawTerminal::open(pty.slave.as_raw_fd()).unwrap().unwrap();
            let cancel = AtomicBool::new(false);
            std::thread::scope(|scope| {
                scope.spawn(|| {
                    std::thread::sleep(Duration::from_millis(25));
                    cancel.store(true, Ordering::Release);
                });
                let started = Instant::now();
                assert_eq!(
                    raw.read(&mut [0; 1], &cancel).unwrap_err().kind(),
                    io::ErrorKind::Interrupted
                );
                assert!(started.elapsed() < Duration::from_secs(1));
            });
        }
        assert_eq!(pty.attributes(), original);
    }

    static SIGNAL_CANCEL: AtomicBool = AtomicBool::new(false);
    extern "C" fn cancel_signal(_number: c_int) {
        SIGNAL_CANCEL.store(true, Ordering::Release);
    }

    #[test]
    fn real_pty_signal_owner_restores_in_child() {
        const CHILD: &str = "PARROT_NATIVE_SIGNAL_CHILD_FD";
        if let Ok(fd) = std::env::var(CHILD) {
            let fd = fd.parse().unwrap();
            // Only this subprocess installs a handler; production code never takes signal ownership.
            // SAFETY: the handler only stores to a lock-free AtomicBool; SIGTERM is 15 on both OSes.
            assert_ne!(unsafe { signal(15, cancel_signal as usize) }, usize::MAX);
            {
                let mut raw = RawTerminal::open(fd).unwrap().unwrap();
                println!("NATIVE_RAW_READY");
                io::stdout().flush().unwrap();
                loop {
                    match raw.read(&mut [0; 1], &SIGNAL_CANCEL) {
                        Err(error) if error.kind() == io::ErrorKind::Interrupted => break,
                        Ok(_) => (),
                        Err(error) => panic!("{error}"),
                    }
                }
            }
            return;
        }
        let pty = Pty::open();
        let original = pty.attributes();
        let mut child = Command::new(std::env::current_exe().unwrap())
            .args([
                "real_pty_signal_owner_restores_in_child",
                "--nocapture",
                "--test-threads=1",
            ])
            .env(CHILD, pty.slave.as_raw_fd().to_string())
            .stdout(Stdio::piped())
            .spawn()
            .unwrap();
        let mut stdout = BufReader::new(child.stdout.take().unwrap());
        let (ready_send, ready_receive) = std::sync::mpsc::channel();
        let readiness = std::thread::spawn(move || {
            let mut line = String::new();
            while stdout.read_line(&mut line).unwrap() != 0 {
                if line.contains("NATIVE_RAW_READY") {
                    let _ = ready_send.send(stdout);
                    break;
                }
                line.clear();
            }
        });
        let _stdout = match ready_receive.recv_timeout(Duration::from_secs(5)) {
            Ok(stdout) => stdout,
            Err(error) => {
                let _ = child.kill();
                let _ = child.wait();
                let _ = readiness.join();
                panic!("child did not become ready: {error}");
            }
        };
        readiness.join().unwrap();
        pty.resize();
        // SAFETY: kill targets our live child. Resize is handled by higher layers, not raw input.
        assert_eq!(unsafe { kill(child.id() as c_int, 28) }, 0);
        // SAFETY: this child's SIGTERM handler requests cooperative cancellation and RAII close.
        assert_eq!(unsafe { kill(child.id() as c_int, 15) }, 0);
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            if let Some(status) = child.try_wait().unwrap() {
                assert!(status.success());
                break;
            }
            if Instant::now() >= deadline {
                child.kill().unwrap();
                let _ = child.wait();
                panic!("signal child did not restore/exit within five seconds");
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        assert_eq!(
            pty.attributes(),
            original,
            "child must restore the real slave attributes"
        );
    }

    #[test]
    fn term_dumb_stdin_open_does_not_change_terminal() {
        const CHILD: &str = "PARROT_NATIVE_DUMB_CHILD";
        if std::env::var_os(CHILD).is_some() {
            let original = attributes(0);
            assert!(RawTerminal::open_stdin().unwrap().is_none());
            assert_eq!(attributes(0), original);
            return;
        }
        let pty = Pty::open();
        let slave = std::fs::File::from(pty.slave);
        let mut child = Command::new(std::env::current_exe().unwrap())
            .args([
                "term_dumb_stdin_open_does_not_change_terminal",
                "--test-threads=1",
            ])
            .env(CHILD, "1")
            .env("TERM", "dumb")
            .stdin(slave)
            .stdout(Stdio::null())
            .spawn()
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            if let Some(status) = child.try_wait().unwrap() {
                assert!(status.success());
                break;
            }
            if Instant::now() >= deadline {
                child.kill().unwrap();
                let _ = child.wait();
                panic!("TERM=dumb child did not exit within five seconds");
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    }
}
