//! Linux bubblewrap planning and execution over an already-materialized policy.
//!
//! This adapter does not authorize commands. The caller supplies resource paths
//! and ordered effective rules; T36 composition remains a separate integration
//! obligation. Actual launch, cancellation, output and settlement belong to T40.

mod launcher;

#[cfg(test)]
#[path = "../../../tests/sandbox/linux/launcher.rs"]
mod launcher_tests;

use std::ffi::{CString, OsString};
use std::io;
use std::path::{Path, PathBuf};

use crate::process::execution::LaunchSpec;

/// An ordered effective mount rule, not a request to change security policy.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LinuxAccessRule {
    pub path: PathBuf,
    pub read: bool,
    pub write: bool,
}

/// Inputs already owned/provisioned by the session-resource layer.
#[derive(Clone, Debug, Default)]
pub struct LinuxSandboxPolicy {
    pub rules: Vec<LinuxAccessRule>,
    pub owned_roots: Vec<PathBuf>,
    pub device_paths: Vec<PathBuf>,
}

impl LinuxSandboxPolicy {
    /// Preserve effective rule order, including later child exceptions.
    pub fn arguments(
        &self,
        command: &str,
        working_directory: &Path,
        environment: &[(String, String)],
        pseudo_terminal_helper: Option<&Path>,
    ) -> Vec<OsString> {
        let mut args: Vec<OsString> = [
            "--die-with-parent",
            "--new-session",
            "--unshare-user",
            "--unshare-pid",
            "--unshare-ipc",
            "--unshare-uts",
            "--cap-drop",
            "ALL",
            "--ro-bind",
            "/",
            "/",
        ]
        .into_iter()
        .map(OsString::from)
        .collect();
        for (key, value) in environment {
            args.extend(["--setenv".into(), key.into(), value.into()]);
        }
        for rule in &self.rules {
            let mut parents = Vec::new();
            let mut parent = rule.path.parent();
            while let Some(directory) = parent {
                if !self
                    .owned_roots
                    .iter()
                    .any(|root| directory.starts_with(root))
                {
                    break;
                }
                parents.push(directory);
                parent = directory.parent();
            }
            for directory in parents.into_iter().rev() {
                args.extend(["--dir".into(), directory.as_os_str().to_owned()]);
            }
            if rule.read {
                args.extend([
                    if rule.write { "--bind" } else { "--ro-bind" }.into(),
                    rule.path.as_os_str().to_owned(),
                    rule.path.as_os_str().to_owned(),
                ]);
            } else if rule.path.is_dir() {
                args.extend(["--tmpfs".into(), rule.path.as_os_str().to_owned()]);
            } else {
                args.extend([
                    "--ro-bind".into(),
                    "/dev/null".into(),
                    rule.path.as_os_str().to_owned(),
                ]);
            }
        }
        if let Some(helper) = pseudo_terminal_helper {
            args.extend([
                "--ro-bind".into(),
                helper.as_os_str().to_owned(),
                helper.as_os_str().to_owned(),
            ]);
        }
        args.extend([
            "--dev".into(),
            "/dev".into(),
            "--proc".into(),
            "/proc".into(),
        ]);
        for device in &self.device_paths {
            args.extend([
                "--dev-bind-try".into(),
                device.as_os_str().to_owned(),
                device.as_os_str().to_owned(),
            ]);
        }
        args.extend([
            "--chdir".into(),
            working_directory.as_os_str().to_owned(),
            "--".into(),
        ]);
        if let Some(helper) = pseudo_terminal_helper {
            args.extend([
                helper.as_os_str().to_owned(),
                "--attach".into(),
                "--".into(),
            ]);
        }
        args.extend(["/bin/sh".into(), "-c".into(), command.into()]);
        args
    }
}

/// A validated executable; absence is not permission to launch unconfined.
#[derive(Clone, Debug)]
pub struct LinuxBubblewrapSandbox {
    executable: Option<PathBuf>,
    device_paths: Option<Vec<PathBuf>>,
}

impl LinuxBubblewrapSandbox {
    pub fn new(path: &Path, require_trusted_path: bool) -> io::Result<Self> {
        if path.as_os_str().is_empty() {
            return Ok(Self {
                executable: None,
                device_paths: None,
            });
        }
        if !path.is_absolute() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "the bubblewrap path must be absolute",
            ));
        }
        let canonical = path.canonicalize()?;
        if !canonical.is_file() {
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                "the bubblewrap executable does not exist",
            ));
        }
        if require_trusted_path && !trusted_parent_chain(&canonical)? {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "bubblewrap must be installed below a non-writable system path",
            ));
        }
        Ok(Self {
            executable: Some(canonical),
            device_paths: None,
        })
    }

    /// Launcher configuration overrides policy devices, including an empty list.
    pub fn with_device_paths(mut self, device_paths: Vec<PathBuf>) -> Self {
        self.device_paths = Some(device_paths);
        self
    }

    pub fn sandbox_available(&self) -> bool {
        self.executable.is_some()
    }

    /// Pipe launch; the executor must retain its spawning OS thread until exit.
    pub fn pipe_launch(
        &self,
        policy: &LinuxSandboxPolicy,
        command: &str,
        working_directory: &Path,
        environment: &[(String, String)],
    ) -> io::Result<LaunchSpec> {
        let mut launch = LaunchSpec::new(self.executable.as_ref().ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::Unsupported,
                "bubblewrap is required; install bwrap and enable unprivileged user namespaces",
            )
        })?);
        launch.args = policy.arguments(command, working_directory, environment, None);
        launch.retain_launch_thread = true;
        Ok(launch)
    }

    /// The PTY service owns the bridge and descriptors; this supplies its inner argv.
    pub fn pseudo_terminal_arguments(
        &self,
        policy: &LinuxSandboxPolicy,
        helper: &Path,
        command: &str,
        working_directory: &Path,
        environment: &[(String, String)],
    ) -> io::Result<Vec<OsString>> {
        let launch = self.pipe_launch(policy, command, working_directory, environment)?;
        if !helper.is_file() {
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                "the PTY attach helper does not exist",
            ));
        }
        let mut args = vec![launch.program.into_os_string()];
        args.extend(policy.arguments(command, working_directory, environment, Some(helper)));
        Ok(args)
    }
}

fn trusted_parent_chain(path: &Path) -> io::Result<bool> {
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt;
        for parent in path.ancestors().skip(1) {
            let value = CString::new(parent.as_os_str().as_bytes())
                .map_err(|error| io::Error::new(io::ErrorKind::InvalidInput, error))?;
            // SAFETY: `value` is a live NUL-terminated path; access retains no pointer.
            if unsafe { libc::access(value.as_ptr(), libc::W_OK) } == 0 {
                return Ok(false);
            }
        }
        Ok(true)
    }
    #[cfg(not(unix))]
    {
        let _ = path;
        Ok(false)
    }
}

/// Unsupported platforms must fail, not silently select the explicit escape hatch.
pub struct UnavailableProcessSandbox;

impl UnavailableProcessSandbox {
    pub fn sandbox_available(&self) -> bool {
        false
    }

    pub fn pipe_launch(&self) -> io::Result<LaunchSpec> {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "Sandboxed shell commands require Linux or macOS.",
        ))
    }
}

/// Only an explicit authorization gate may choose this unconfined adapter.
pub struct UnsandboxedProcessSandbox;

impl UnsandboxedProcessSandbox {
    pub fn sandbox_available(&self) -> bool {
        true
    }

    pub fn pipe_launch(
        &self,
        command: &str,
        working_directory: &Path,
        environment: &[(String, String)],
    ) -> LaunchSpec {
        let mut launch = LaunchSpec::new("/bin/sh");
        launch.args = vec!["-c".into(), command.into()];
        launch.current_dir = Some(working_directory.to_owned());
        launch.environment.extend(
            environment
                .iter()
                .map(|(key, value)| (key.into(), Some(value.into()))),
        );
        launch
    }
}

/// Source-compatible prefix classification, deliberately not shell parsing.
#[derive(Clone, Debug)]
pub struct ReadOnlyExecCommandClassifier {
    prefixes: Vec<String>,
}

impl ReadOnlyExecCommandClassifier {
    pub fn new(prefixes: Vec<String>) -> Self {
        Self { prefixes }
    }

    pub fn is_match(&self, command: &str) -> bool {
        let command = command.trim_start_matches([' ', '\t', '\r', '\n']);
        self.prefixes.iter().any(|prefix| {
            command
                .strip_prefix(prefix)
                .is_some_and(|rest| rest.is_empty() || rest.starts_with([' ', '\t', '\r', '\n']))
        })
    }
}
