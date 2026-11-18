use super::{LaunchSpec, locator::ExecutableLocator, native::NativeProcess};
use parrot_domain::contracts::async_ops::Cancellation;
use std::{
    io::{self, Read},
    os::fd::AsRawFd,
    path::Path,
    time::Duration,
};

const READ_DIRECTORY: &str = "LC_ALL=C ls -A . >/dev/null";
const MAX_ERROR_BYTES: usize = 4096;

/// Read the launch directory in the native sandbox without session policy or resources.
/// Cancellation aborts the check. Other errors describe unavailable sandbox execution.
pub async fn probe_sandbox_read(
    directory: &Path,
    cancellation: &dyn Cancellation,
) -> io::Result<()> {
    check_cancellation(cancellation)?;
    let launch = plan_read(&ExecutableLocator::capture(), directory)?;
    execute_probe(&launch, cancellation, Duration::from_secs(5)).await
}

fn plan_read(locator: &ExecutableLocator, directory: &Path) -> io::Result<LaunchSpec> {
    #[cfg(target_os = "linux")]
    {
        use crate::sandbox::linux::{LinuxBubblewrapSandbox, LinuxSandboxPolicy};
        let executable = locator.locate("bwrap").unwrap_or_default();
        LinuxBubblewrapSandbox::new(&executable, false)?.pipe_launch(
            &LinuxSandboxPolicy::default(),
            READ_DIRECTORY,
            directory,
            &[],
        )
    }
    #[cfg(target_os = "macos")]
    {
        let _ = locator;
        crate::sandbox::macos::MacSeatbeltSandbox::new(Path::new("/usr/bin/sandbox-exec"), true)?
            .read_launch(directory, READ_DIRECTORY)
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        let _ = (locator, directory, READ_DIRECTORY);
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "No native process sandbox is available",
        ))
    }
}

async fn execute_probe(
    launch: &LaunchSpec,
    cancellation: &dyn Cancellation,
    timeout: Duration,
) -> io::Result<()> {
    check_cancellation(cancellation)?;
    let mut child = NativeProcess::start_pipe(launch)?;
    let mut stdout = child.take_stdout().expect("pipe stdout");
    let mut stderr = child.take_stderr().expect("pipe stderr");
    let mut error = Vec::new();
    let deadline = tokio::time::sleep(timeout);
    tokio::pin!(deadline);
    loop {
        check_cancellation(cancellation)?;
        read_available(&mut stdout, &mut Vec::new(), 0)?;
        read_available(&mut stderr, &mut error, MAX_ERROR_BYTES)?;
        if let Some(status) = child.try_wait()? {
            read_available(&mut stderr, &mut error, MAX_ERROR_BYTES)?;
            return if status.success() {
                Ok(())
            } else {
                Err(io::Error::other(format!(
                    "Sandbox directory read exited with {status}: {}",
                    String::from_utf8_lossy(&error).trim()
                )))
            };
        }
        tokio::select! {
            biased;
            () = cancellation.cancelled() => return Err(interrupted()),
            () = &mut deadline => return Err(io::Error::new(io::ErrorKind::TimedOut, "Sandbox directory read timed out")),
            () = tokio::time::sleep(Duration::from_millis(5)) => {}
        }
    }
}

fn read_available(
    stream: &mut (impl Read + AsRawFd),
    output: &mut Vec<u8>,
    limit: usize,
) -> io::Result<()> {
    let mut descriptor = libc::pollfd {
        fd: stream.as_raw_fd(),
        events: libc::POLLIN,
        revents: 0,
    };
    let ready = unsafe { libc::poll(&mut descriptor, 1, 0) };
    if ready < 0 {
        let error = io::Error::last_os_error();
        return if error.kind() == io::ErrorKind::Interrupted {
            Ok(())
        } else {
            Err(error)
        };
    }
    if ready > 0 {
        let mut buffer = [0; 4096];
        match stream.read(&mut buffer) {
            Ok(count) => {
                output.extend_from_slice(&buffer[..count.min(limit.saturating_sub(output.len()))])
            }
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
            Err(error) => return Err(error),
        }
    }
    Ok(())
}

fn check_cancellation(cancellation: &dyn Cancellation) -> io::Result<()> {
    if cancellation.is_cancelled() {
        Err(interrupted())
    } else {
        Ok(())
    }
}

fn interrupted() -> io::Error {
    io::Error::new(
        io::ErrorKind::Interrupted,
        "Sandbox directory read canceled",
    )
}

#[cfg(test)]
#[path = "../../../tests/process/execution/probe.rs"]
mod tests;
