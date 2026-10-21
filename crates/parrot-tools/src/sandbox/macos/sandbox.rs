use std::ffi::CString;
use std::fs;
use std::io;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::PermissionsExt;
use std::path::{Component, Path, PathBuf};

use parrot_domain::contracts::async_ops::BoxFuture;

use crate::process::execution::{
    ProcessSandbox, ProcessStartRequest, ShellProcessExecution, TerminalMode,
};

use super::profile::PrivateSeatbeltProfile;
use super::{SeatbeltLaunchPlan, SeatbeltPolicySnapshot};

/// A validated Seatbelt executable, never an authorization to run unconfined.
#[derive(Clone, Debug)]
pub struct MacSeatbeltSandbox {
    executable: Option<PathBuf>,
}

impl MacSeatbeltSandbox {
    pub fn new(path: &Path, require_trusted_path: bool) -> io::Result<Self> {
        if path.as_os_str().is_empty() {
            return Ok(Self { executable: None });
        }
        if !path.is_absolute() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "The Seatbelt path must be absolute.",
            ));
        }
        let canonical = match canonicalize_executable(path) {
            Ok(path) => path,
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                return Ok(Self { executable: None });
            }
            Err(error) => return Err(error),
        };
        let metadata = fs::metadata(&canonical)?;
        if metadata.is_dir() || metadata.permissions().mode() & 0o111 == 0 {
            return Ok(Self { executable: None });
        }
        if require_trusted_path && !trusted_parent_chain(&canonical)? {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "sandbox-exec must be installed below a non-writable system path",
            ));
        }
        Ok(Self {
            executable: Some(canonical),
        })
    }

    pub fn sandbox_available(&self) -> bool {
        self.executable.is_some()
    }
}

impl ProcessSandbox for MacSeatbeltSandbox {
    type Policy = SeatbeltPolicySnapshot;

    fn sandbox_available(&self) -> bool {
        MacSeatbeltSandbox::sandbox_available(self)
    }

    fn start_unbound<'a>(
        &'a self,
        request: &'a ProcessStartRequest,
        profile: &'a Self::Policy,
    ) -> BoxFuture<'a, io::Result<ShellProcessExecution>> {
        Box::pin(async move {
            let executable = self.executable.as_ref().ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::Unsupported,
                    "macOS Seatbelt is required but /usr/bin/sandbox-exec is unavailable",
                )
            })?;
            if request.terminal_mode == TerminalMode::PseudoTerminal {
                return Err(io::Error::new(
                    io::ErrorKind::Unsupported,
                    "Pseudo-terminal shell processes are not yet supported on macOS.",
                ));
            }

            request.resources.provision()?;
            let profile =
                PrivateSeatbeltProfile::create(&request.resources.queue_directory, profile)?;
            let inherited_environment = std::env::vars_os().filter_map(|(key, value)| {
                Some((key.into_string().ok()?, value.into_string().ok()?))
            });
            let launch = SeatbeltLaunchPlan::create(
                executable,
                profile.path(),
                &request.command,
                &request.resources.launch_directory,
                inherited_environment,
                request.environment.iter().cloned(),
            );
            ShellProcessExecution::start_pipe_with_cleanup_path(
                &launch,
                &request.resources.blob_directory,
                profile.into_cleanup_path(),
            )
        })
    }
}

fn canonicalize_executable(path: &Path) -> io::Result<PathBuf> {
    let mut full = PathBuf::new();
    for component in path.components() {
        if component == Component::ParentDir {
            full.pop();
        } else {
            full.push(component.as_os_str());
        }
    }
    if fs::symlink_metadata(&full)?.file_type().is_symlink() {
        fs::canonicalize(full)
    } else {
        Ok(full)
    }
}

fn trusted_parent_chain(path: &Path) -> io::Result<bool> {
    for parent in path.ancestors().skip(1) {
        let value = CString::new(parent.as_os_str().as_bytes())
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidInput, error))?;
        // SAFETY: value is a live NUL-terminated path; access retains no pointer.
        if unsafe { libc::access(value.as_ptr(), libc::W_OK) } == 0 {
            return Ok(false);
        }
    }
    Ok(true)
}
