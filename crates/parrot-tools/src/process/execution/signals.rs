//! Stable direct-process signaling. Group cleanup is separately owned by NativeProcess.

use std::io;
#[cfg(target_os = "linux")]
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};

#[derive(Debug)]
pub struct ProcessSignalTarget {
    #[cfg(target_os = "linux")]
    descriptor: Option<OwnedFd>,
    #[cfg(target_os = "linux")]
    open_error: Option<i32>,
    #[cfg(not(target_os = "linux"))]
    pid: libc::pid_t,
}

impl ProcessSignalTarget {
    pub fn open(pid: u32) -> Self {
        #[cfg(target_os = "linux")]
        {
            // Linux's pidfd binds the target independently of subsequent PID reuse.
            let descriptor = unsafe { libc::syscall(libc::SYS_pidfd_open, pid, 0_u32) };
            if descriptor < 0 {
                Self {
                    descriptor: None,
                    open_error: io::Error::last_os_error().raw_os_error(),
                }
            } else {
                Self {
                    descriptor: Some(unsafe { OwnedFd::from_raw_fd(descriptor as i32) }),
                    open_error: None,
                }
            }
        }
        #[cfg(not(target_os = "linux"))]
        Self {
            pid: pid as libc::pid_t,
        }
    }

    pub fn send(&self, signal: i32) -> io::Result<()> {
        #[cfg(target_os = "linux")]
        {
            let descriptor = self.descriptor.as_ref().ok_or_else(|| {
                io::Error::from_raw_os_error(self.open_error.unwrap_or(libc::ENOSYS))
            })?;
            let result = unsafe {
                libc::syscall(
                    libc::SYS_pidfd_send_signal,
                    descriptor.as_raw_fd(),
                    signal,
                    std::ptr::null::<libc::siginfo_t>(),
                    0_u32,
                )
            };
            if result == 0 {
                Ok(())
            } else {
                Err(io::Error::last_os_error())
            }
        }
        #[cfg(not(target_os = "linux"))]
        {
            if unsafe { libc::kill(self.pid, signal) } == 0 {
                Ok(())
            } else {
                Err(io::Error::last_os_error())
            }
        }
    }
}
