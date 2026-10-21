//! Raw native execution, without an authorization or sandbox policy.
//!
//! Callers take and concurrently drain the raw output handles before waiting,
//! and join their finite drain threads before publishing a result. No output is
//! accumulated in this layer. Cancellation kills the owned process group and
//! reaps the direct child; this is not universal descendant containment.

use super::LaunchSpec;
use super::signals::ProcessSignalTarget;
use std::fs::File;
use std::io::{self, Read};
use std::os::fd::AsRawFd;
#[cfg(target_os = "linux")]
use std::os::fd::FromRawFd;
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::{Child, ChildStderr, ChildStdout, Command, ExitStatus, Stdio};
use std::sync::mpsc::{self, Sender};
use std::thread::JoinHandle;
use std::time::Duration;

struct LaunchThread {
    release: Option<Sender<()>>,
    thread: Option<JoinHandle<()>>,
}

impl LaunchThread {
    fn join(&mut self) {
        self.release.take();
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

impl Drop for LaunchThread {
    fn drop(&mut self) {
        self.join();
    }
}

pub struct NativeProcess {
    child: Child,
    signals: ProcessSignalTarget,
    status: Option<ExitStatus>,
    launcher: Option<LaunchThread>,
    master: Option<File>,
    reader: Option<PtyReader>,
}

impl NativeProcess {
    /// Starts the already planned executable. This does not select or bypass a sandbox.
    pub fn start_pipe(spec: &LaunchSpec) -> io::Result<Self> {
        let mut command = command(spec);
        command
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        command.process_group(0);
        Self::spawn(command, spec.retain_launch_thread)
    }

    /// Starts the retained Linux bridge and attach helper on a real PTY.
    pub async fn start_pty(
        spec: &LaunchSpec,
        helper: &Path,
        rows: u16,
        columns: u16,
    ) -> io::Result<Self> {
        #[cfg(not(target_os = "linux"))]
        {
            let _ = (spec, helper, rows, columns);
            Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "the retained PTY bridge requires Linux",
            ))
        }
        #[cfg(target_os = "linux")]
        {
            let (master, slave) = open_pty(rows, columns)?;
            let mut bridge = Command::new(helper);
            apply_environment(&mut bridge, spec);
            bridge
                .arg("--bridge")
                .arg(format!(
                    "/proc/{}/fd/{}",
                    std::process::id(),
                    slave.as_raw_fd()
                ))
                .arg("--");
            if spec.pty_attach_before_program {
                bridge.arg(helper).arg("--attach").arg("--");
            }
            bridge
                .arg(&spec.program)
                .args(&spec.args)
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::piped());
            bridge.process_group(0);
            let mut process = Self::spawn(bridge, spec.retain_launch_thread)?;
            process.reader = Some(PtyReader(master.try_clone()?));
            process.master = Some(master);
            // Read exactly the readiness line; do not buffer away later bridge errors.
            let mut ready = Vec::new();
            let stderr = process.child.stderr.as_mut().expect("piped bridge stderr");
            let descriptor = stderr.as_raw_fd();
            let flags = unsafe { libc::fcntl(descriptor, libc::F_GETFL) };
            if flags < 0
                || unsafe { libc::fcntl(descriptor, libc::F_SETFL, flags | libc::O_NONBLOCK) } < 0
            {
                return Err(io::Error::last_os_error());
            }
            while ready.len() < 4096 {
                let mut byte = [0];
                match stderr.read(&mut byte) {
                    Ok(0) => break,
                    Ok(_) => {
                        ready.push(byte[0]);
                        if byte[0] == b'\n' {
                            break;
                        }
                    }
                    Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                    Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                        // The future owns process: cancellation/drop kills and
                        // reaps the started bridge, without a detached reader.
                        tokio::time::sleep(Duration::from_millis(5)).await;
                    }
                    Err(error) => return Err(error),
                }
            }
            if ready != b"READY\n" {
                return Err(io::Error::other(format!(
                    "PTY bridge did not become ready: {}",
                    String::from_utf8_lossy(&ready).trim_end()
                )));
            }
            if unsafe { libc::fcntl(descriptor, libc::F_SETFL, flags) } < 0 {
                return Err(io::Error::last_os_error());
            }
            // The bridge now owns its slave descriptor. Dropping our copy is
            // necessary for the master drain to observe EOF after child exit.
            drop(slave);
            Ok(process)
        }
    }

    fn spawn(mut command: Command, retain_thread: bool) -> io::Result<Self> {
        let (child, launcher) = if retain_thread {
            // Linux PDEATHSIG follows the OS thread which spawned the child.
            let (send, receive) = mpsc::sync_channel(1);
            let (release, hold) = mpsc::channel();
            let thread = std::thread::Builder::new()
                .name("parrot-process-launch".into())
                .spawn(move || {
                    let spawned = command.spawn();
                    let started = spawned.is_ok();
                    if let Err(undelivered) = send.send(spawned) {
                        if let Ok(mut child) = undelivered.0 {
                            let _ = child.kill();
                            let _ = child.wait();
                        }
                        return;
                    }
                    if started {
                        let _ = hold.recv();
                    }
                })?;
            let mut launcher = LaunchThread {
                release: Some(release),
                thread: Some(thread),
            };
            match receive.recv() {
                Ok(Ok(child)) => (child, Some(launcher)),
                Ok(Err(error)) => {
                    launcher.join();
                    return Err(error);
                }
                Err(_) => {
                    launcher.join();
                    return Err(io::Error::other(
                        "native launch thread stopped before returning a child",
                    ));
                }
            }
        } else {
            (command.spawn()?, None)
        };
        let signals = ProcessSignalTarget::open(child.id());
        Ok(Self {
            child,
            signals,
            status: None,
            launcher,
            master: None,
            reader: None,
        })
    }

    pub fn id(&self) -> u32 {
        self.child.id()
    }
    pub fn is_pseudo_terminal(&self) -> bool {
        self.master.is_some()
    }
    pub fn take_stdout(&mut self) -> Option<ChildStdout> {
        self.child.stdout.take()
    }
    pub fn take_stderr(&mut self) -> Option<ChildStderr> {
        self.child.stderr.take()
    }
    pub fn take_pty_reader(&mut self) -> Option<PtyReader> {
        self.reader.take()
    }

    /// Matches the source: interactive stdin is available only for PTY processes.
    pub async fn write_stdin(&mut self, bytes: &[u8]) -> io::Result<()> {
        let master = self.master.as_ref().ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::Unsupported,
                "standard input is available only for pseudo-terminal processes",
            )
        })?;
        if self.status.is_some() {
            return Err(io::Error::new(
                io::ErrorKind::BrokenPipe,
                "the pseudo-terminal process has completed",
            ));
        }
        let mut remaining = bytes;
        while !remaining.is_empty() {
            let count = unsafe {
                libc::write(
                    master.as_raw_fd(),
                    remaining.as_ptr().cast(),
                    remaining.len(),
                )
            };
            match count.cmp(&0) {
                std::cmp::Ordering::Greater => remaining = &remaining[count as usize..],
                std::cmp::Ordering::Equal => tokio::time::sleep(Duration::from_millis(5)).await,
                std::cmp::Ordering::Less => {
                    let error = io::Error::last_os_error();
                    if error.kind() == io::ErrorKind::Interrupted {
                        continue;
                    }
                    if error.kind() != io::ErrorKind::WouldBlock {
                        return Err(error);
                    }
                    tokio::time::sleep(Duration::from_millis(5)).await;
                }
            }
        }
        Ok(())
    }

    /// Makes one nonblocking terminal write for a composing execution owner.
    pub fn try_write_stdin(&mut self, bytes: &[u8]) -> io::Result<usize> {
        let master = self.master.as_ref().ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::Unsupported,
                "standard input is available only for pseudo-terminal processes",
            )
        })?;
        if self.status.is_some() {
            return Err(io::Error::new(
                io::ErrorKind::BrokenPipe,
                "the pseudo-terminal process has completed",
            ));
        }
        let count = unsafe { libc::write(master.as_raw_fd(), bytes.as_ptr().cast(), bytes.len()) };
        if count >= 0 {
            Ok(count as usize)
        } else {
            Err(io::Error::last_os_error())
        }
    }

    /// Sends the terminal's EOF character; the reader stays open to drain output.
    pub async fn close_stdin(&mut self) -> io::Result<()> {
        self.write_stdin(&[4]).await
    }

    pub fn resize(&self, rows: u16, columns: u16) -> io::Result<()> {
        let master = self.master.as_ref().ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::Unsupported,
                "pipe processes have no terminal window",
            )
        })?;
        resize(master.as_raw_fd(), rows, columns)
    }

    pub fn send_signal(&self, signal: i32) -> io::Result<()> {
        if self.status.is_some() {
            return Err(io::Error::from_raw_os_error(libc::ESRCH));
        }
        self.signals.send(signal)
    }

    /// Polling does not own a detached task; dropping this future leaves the
    /// process owned by this object. Drop/cancel still synchronously reap it.
    pub async fn wait(&mut self) -> io::Result<ExitStatus> {
        loop {
            if let Some(status) = self.try_wait()? {
                return Ok(status);
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    }

    /// Polls exit without holding a composing owner's lock across an await.
    pub fn try_wait(&mut self) -> io::Result<Option<ExitStatus>> {
        if self.status.is_none() {
            self.status = self.child.try_wait()?;
            if self.status.is_some() {
                if let Some(mut launcher) = self.launcher.take() {
                    launcher.join();
                }
            }
        }
        Ok(self.status)
    }

    /// Requests cancellation while retaining ownership until reap/drop.
    pub fn request_cancel(&mut self) {
        self.kill_owned();
    }

    pub async fn cancel_and_wait(&mut self) -> io::Result<ExitStatus> {
        self.kill_owned();
        self.wait().await
    }

    fn kill_owned(&mut self) {
        // Do not signal a numeric group after reaping its leader: it could have
        // been reused. Stable pidfd signaling is retained for explicit signals.
        if self.status.is_some() {
            return;
        }
        #[cfg(target_os = "linux")]
        if let Some(master) = &self.master {
            let mut foreground: libc::pid_t = 0;
            if unsafe { libc::ioctl(master.as_raw_fd(), libc::TIOCGPGRP, &mut foreground) } == 0
                && foreground > 0
            {
                let _ = unsafe { libc::kill(-foreground, libc::SIGKILL) };
            }
        }
        let _ = unsafe { libc::kill(-(self.child.id() as libc::pid_t), libc::SIGKILL) };
        let _ = self.signals.send(libc::SIGKILL);
        // pidfd may be unavailable on an older kernel; Child::kill addresses the
        // still-owned unreaped child, not an arbitrary caller-provided PID.
        let _ = self.child.kill();
    }
}

impl Drop for NativeProcess {
    fn drop(&mut self) {
        if self.status.is_none() {
            self.kill_owned();
            self.status = self.child.wait().ok();
        }
        if let Some(mut launcher) = self.launcher.take() {
            launcher.join();
        }
    }
}

fn command(spec: &LaunchSpec) -> Command {
    let mut command = Command::new(&spec.program);
    command.args(&spec.args);
    apply_environment(&mut command, spec);
    command
}

fn apply_environment(command: &mut Command, spec: &LaunchSpec) {
    if let Some(directory) = &spec.current_dir {
        command.current_dir(directory);
    }
    if spec.clear_environment {
        command.env_clear();
    }
    for (key, value) in &spec.environment {
        if let Some(value) = value {
            command.env(key, value);
        } else {
            command.env_remove(key);
        }
    }
}

pub struct PtyReader(File);

impl AsRawFd for PtyReader {
    fn as_raw_fd(&self) -> std::os::fd::RawFd {
        self.0.as_raw_fd()
    }
}

impl Read for PtyReader {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        if buffer.is_empty() {
            return Ok(0);
        }
        loop {
            match self.0.read(buffer) {
                Ok(count) => return Ok(count),
                Err(error) if error.raw_os_error() == Some(libc::EIO) => return Ok(0),
                Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                    let mut descriptor = libc::pollfd {
                        fd: self.0.as_raw_fd(),
                        events: libc::POLLIN,
                        revents: 0,
                    };
                    if unsafe { libc::poll(&mut descriptor, 1, -1) } < 0 {
                        let error = io::Error::last_os_error();
                        if error.kind() != io::ErrorKind::Interrupted {
                            return Err(error);
                        }
                    }
                }
                Err(error) => return Err(error),
            }
        }
    }
}

fn resize(descriptor: i32, rows: u16, columns: u16) -> io::Result<()> {
    let size = libc::winsize {
        ws_row: rows,
        ws_col: columns,
        ws_xpixel: 0,
        ws_ypixel: 0,
    };
    if unsafe { libc::ioctl(descriptor, libc::TIOCSWINSZ, &size) } == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

#[cfg(target_os = "linux")]
fn open_pty(rows: u16, columns: u16) -> io::Result<(File, File)> {
    // Both descriptors must be close-on-exec at allocation, not in a later
    // fcntl window during which another thread could spawn an inheriting child.
    let descriptor = unsafe {
        libc::posix_openpt(libc::O_RDWR | libc::O_NOCTTY | libc::O_CLOEXEC | libc::O_NONBLOCK)
    };
    if descriptor < 0 {
        return Err(io::Error::last_os_error());
    }
    let master = unsafe { File::from_raw_fd(descriptor) };
    if unsafe { libc::grantpt(descriptor) } != 0 || unsafe { libc::unlockpt(descriptor) } != 0 {
        return Err(io::Error::last_os_error());
    }
    let mut path = [0_u8; 4096];
    let result = unsafe { libc::ptsname_r(descriptor, path.as_mut_ptr().cast(), path.len()) };
    if result != 0 {
        return Err(io::Error::from_raw_os_error(result));
    }
    let path = std::ffi::CStr::from_bytes_until_nul(&path)
        .map_err(|_| io::Error::other("PTY slave path is not NUL-terminated"))?;
    let descriptor = unsafe {
        libc::open(
            path.as_ptr(),
            libc::O_RDWR | libc::O_NOCTTY | libc::O_CLOEXEC,
        )
    };
    if descriptor < 0 {
        return Err(io::Error::last_os_error());
    }
    let slave = unsafe { File::from_raw_fd(descriptor) };
    resize(master.as_raw_fd(), rows, columns)?;
    Ok((master, slave))
}

#[cfg(test)]
#[path = "../../../tests/process/execution/native.rs"]
mod tests;
