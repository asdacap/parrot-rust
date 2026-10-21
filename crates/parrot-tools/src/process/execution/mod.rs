//! Native process launching and execution lifetimes.
//!
//! A launch specification is already planned by a platform adapter. It is not
//! authorization, a materialized security policy, or permission to downgrade a
//! sandbox launch.

#[path = "execution.rs"]
mod lifetime;
pub use lifetime::{ProcessResult, ShellProcessExecution};

mod descendants;
pub mod filenames;
pub mod locator;
pub mod native;
pub mod output;
mod pipe_decoder;
pub mod runner;
pub mod signals;
pub mod transcript;
pub use runner::{
    ConfiguredProcessRunner, ProcessResources, ProcessRunner, ProcessSandbox, ProcessStartRequest,
    SandboxGate, TerminalMode, locate_configured, start_selected_launch,
};

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::path::PathBuf;

/// The platform-neutral start information consumed by the native executor.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LaunchSpec {
    pub program: PathBuf,
    pub args: Vec<OsString>,
    pub current_dir: Option<PathBuf>,
    /// `Some` sets a value; `None` removes a variable from the child environment.
    pub environment: BTreeMap<OsString, Option<OsString>>,
    /// Seatbelt starts with a clean outer environment; Linux inherits it.
    pub clear_environment: bool,
    /// Keep the spawning OS thread alive until exit for Linux PDEATHSIG users.
    pub retain_launch_thread: bool,
    /// Raw PTY launches attach before the target by default. Sandboxed plans
    /// which attach inside their namespace set this to false.
    pub pty_attach_before_program: bool,
}

impl LaunchSpec {
    pub fn new(program: impl Into<PathBuf>) -> Self {
        Self {
            program: program.into(),
            args: Vec::new(),
            current_dir: None,
            environment: BTreeMap::new(),
            clear_environment: false,
            retain_launch_thread: false,
            pty_attach_before_program: true,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::LaunchSpec;
    use std::path::PathBuf;

    #[test]
    fn launch_defaults_preserve_inherited_environment() {
        let spec = LaunchSpec::new("/bin/sh");
        assert_eq!(spec.program, PathBuf::from("/bin/sh"));
        assert!(spec.args.is_empty());
        assert!(spec.environment.is_empty());
        assert_eq!(spec.current_dir, None);
        assert!(!spec.clear_environment);
        assert!(!spec.retain_launch_thread);
    }
}
