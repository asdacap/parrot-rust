//! Source-shaped sandbox selection and owned execution startup.
//!
//! Policies and resource paths are supplied by their owning session. Selecting a
//! sandbox is not filesystem authorization, and an unavailable or failed sandbox
//! is never retried through the explicit unsandboxed branch.

use super::locator::ExecutableLocator;
use super::{LaunchSpec, ProcessResult, ShellProcessExecution};
use crate::sandbox::linux::UnsandboxedProcessSandbox;
use parrot_domain::contracts::async_ops::{BoxFuture, Cancellation};
use parrot_foundation::paths::AgentScratchDirectory;
use std::fs;
use std::io;
#[cfg(target_os = "macos")]
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TerminalMode {
    Pipe,
    PseudoTerminal,
}

/// Already selected session resources, not a request to authorize arbitrary paths.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProcessResources {
    pub launch_directory: PathBuf,
    pub queue_directory: PathBuf,
    pub blob_directory: PathBuf,
    scratch: AgentScratchDirectory,
}

impl ProcessResources {
    /// Consume the actual selected scratch resource. This does not select or
    /// resolve another session root, authorize paths, or provision anything.
    pub fn new(
        launch_directory: impl Into<PathBuf>,
        queue_directory: impl Into<PathBuf>,
        scratch: AgentScratchDirectory,
    ) -> Self {
        Self {
            launch_directory: launch_directory.into(),
            queue_directory: queue_directory.into(),
            blob_directory: scratch.blob_directory(),
            scratch,
        }
    }

    pub fn scratch(&self) -> &AgentScratchDirectory {
        &self.scratch
    }

    /// Delegate Root/scratch/blobs provisioning to the existing resource owner.
    /// Linux sandbox argument construction does not provision the queue.
    pub fn provision_scratch(&self) -> io::Result<()> {
        self.scratch.provision()
    }

    /// Explicit unsandboxed startup also provisions the queue. Seatbelt applies
    /// its additional source-required queue permissions in its own adapter.
    pub fn provision(&self) -> io::Result<()> {
        self.provision_scratch()?;
        fs::create_dir_all(&self.queue_directory)
    }
}

#[derive(Clone)]
pub struct ProcessStartRequest {
    pub command: String,
    /// Overrides only. Platform adapters preserve their source environment rules.
    pub environment: Vec<(String, String)>,
    pub resources: ProcessResources,
    pub terminal_mode: TerminalMode,
    /// Defaults to the retained helper alongside the current executable.
    pub pty_helper: Option<PathBuf>,
    /// Retained by the execution until settlement, not just observed by `start`.
    pub cancellation: Option<Arc<dyn Cancellation>>,
}

impl ProcessStartRequest {
    pub fn new(command: impl Into<String>, resources: ProcessResources) -> Self {
        Self {
            command: command.into(),
            environment: Vec::new(),
            resources,
            terminal_mode: TerminalMode::Pipe,
            pty_helper: None,
            cancellation: None,
        }
    }

    pub fn pty_helper_path(&self) -> io::Result<PathBuf> {
        if let Some(path) = &self.pty_helper {
            return Ok(path.clone());
        }
        let executable = std::env::current_exe()?;
        let directory = executable.parent().ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::NotFound,
                "The executable directory is unavailable.",
            )
        })?;
        Ok(directory.join("parrot-pty-attach"))
    }
}

/// The source process-wide switch is sampled exactly once for each start.
#[derive(Debug)]
pub struct SandboxGate {
    enabled: AtomicBool,
}

impl SandboxGate {
    pub fn new(enabled: bool) -> Self {
        Self {
            enabled: AtomicBool::new(enabled),
        }
    }

    pub fn enabled(&self) -> bool {
        self.enabled.load(Ordering::Acquire)
    }

    pub fn set_enabled(&self, enabled: bool) {
        self.enabled.store(enabled, Ordering::Release);
    }
}

/// Concrete platform adapters own policy compilation and launch planning.
/// The default start links cancellation to the execution's owned lifetime.
pub trait ProcessSandbox: Send + Sync {
    type Policy: Send + Sync;

    fn sandbox_available(&self) -> bool;

    fn start_unbound<'a>(
        &'a self,
        request: &'a ProcessStartRequest,
        policy: &'a Self::Policy,
    ) -> BoxFuture<'a, io::Result<ShellProcessExecution>>;

    fn start<'a>(
        &'a self,
        request: &'a ProcessStartRequest,
        policy: &'a Self::Policy,
    ) -> BoxFuture<'a, io::Result<ShellProcessExecution>> {
        Box::pin(async move {
            let start = self.start_unbound(request, policy);
            let mut execution = match &request.cancellation {
                Some(cancellation) => {
                    tokio::select! {
                        biased;
                        execution = start => execution?,
                        () = cancellation.cancelled() => {
                            return Err(io::Error::new(
                                io::ErrorKind::Interrupted,
                                "The process start was canceled.",
                            ));
                        }
                    }
                }
                None => start.await?,
            };
            if let Some(cancellation) = &request.cancellation {
                execution.bind_cancellation(cancellation.clone())?;
            }
            Ok(execution)
        })
    }

    fn run<'a>(
        &'a self,
        request: &'a ProcessStartRequest,
        policy: &'a Self::Policy,
    ) -> BoxFuture<'a, io::Result<ProcessResult>> {
        Box::pin(async move {
            let mut pipe_request = request.clone();
            pipe_request.terminal_mode = TerminalMode::Pipe;
            let execution = self.start(&pipe_request, policy).await?;
            let result = execution.wait().await;
            execution.settle().await?;
            result
        })
    }
}

pub struct ProcessRunner<S> {
    sandbox: S,
    gate: Arc<SandboxGate>,
}

impl<S: ProcessSandbox> ProcessRunner<S> {
    pub fn new(sandbox: S, gate: Arc<SandboxGate>) -> Self {
        Self { sandbox, gate }
    }

    pub fn sandbox_gate(&self) -> &Arc<SandboxGate> {
        &self.gate
    }

    pub fn sandbox_available(&self) -> bool {
        self.sandbox.sandbox_available()
    }

    pub async fn start(
        &self,
        request: &ProcessStartRequest,
        policy: &S::Policy,
    ) -> io::Result<ShellProcessExecution> {
        if self.gate.enabled() {
            self.sandbox.start(request, policy).await
        } else {
            UnsandboxedProcessSandbox.start(request, &()).await
        }
    }

    pub async fn run(
        &self,
        request: &ProcessStartRequest,
        policy: &S::Policy,
    ) -> io::Result<ProcessResult> {
        let mut pipe_request = request.clone();
        pipe_request.terminal_mode = TerminalMode::Pipe;
        let execution = self.start(&pipe_request, policy).await?;
        let result = execution.wait().await;
        // Source await-using disposes even a faulted Result; disposal errors are
        // separate and can supersede the already observable work failure.
        execution.settle().await?;
        result
    }
}

/// Start one already-selected plan; no sandbox selection or fallback occurs here.
pub async fn start_selected_launch(
    request: &ProcessStartRequest,
    spec: &LaunchSpec,
    cleanup_path: Option<PathBuf>,
) -> io::Result<ShellProcessExecution> {
    match request.terminal_mode {
        TerminalMode::Pipe => match cleanup_path {
            Some(path) => ShellProcessExecution::start_pipe_with_cleanup_path(
                spec,
                &request.resources.blob_directory,
                path,
            ),
            None => ShellProcessExecution::start_pipe(spec, &request.resources.blob_directory),
        },
        TerminalMode::PseudoTerminal => {
            if let Some(path) = cleanup_path {
                // Platform profile ownership is only used by source pipe mode.
                match fs::remove_file(path) {
                    Ok(()) => (),
                    Err(error) if error.kind() == io::ErrorKind::NotFound => (),
                    Err(error) => return Err(error),
                }
                return Err(io::Error::new(
                    io::ErrorKind::Unsupported,
                    "Pseudo-terminal profile execution is not supported.",
                ));
            }
            let helper = request.pty_helper_path()?;
            if !helper.is_file() {
                return Err(io::Error::new(
                    io::ErrorKind::NotFound,
                    "The PTY attach helper does not exist.",
                ));
            }
            ShellProcessExecution::start_pty(
                spec,
                &helper,
                &request.resources.blob_directory,
                24,
                80,
            )
            .await
        }
    }
}

#[cfg(target_os = "linux")]
pub type ConfiguredProcessRunner = ProcessRunner<crate::sandbox::linux::LinuxBubblewrapSandbox>;
#[cfg(target_os = "macos")]
pub type ConfiguredProcessRunner = ProcessRunner<crate::sandbox::macos::MacSeatbeltSandbox>;
#[cfg(not(any(target_os = "linux", target_os = "macos")))]
pub type ConfiguredProcessRunner = ProcessRunner<crate::sandbox::linux::UnavailableProcessSandbox>;

/// Locate the actual configured launcher independently of utility-report lists.
pub fn locate_configured(
    locator: &ExecutableLocator,
    gate: Arc<SandboxGate>,
    device_paths: Vec<PathBuf>,
) -> io::Result<ConfiguredProcessRunner> {
    #[cfg(target_os = "linux")]
    {
        let executable = locator.locate("bwrap").unwrap_or_default();
        let sandbox = crate::sandbox::linux::LinuxBubblewrapSandbox::new(&executable, false)?
            .with_device_paths(device_paths);
        Ok(ProcessRunner::new(sandbox, gate))
    }
    #[cfg(target_os = "macos")]
    {
        let _ = (locator, device_paths);
        let sandbox = crate::sandbox::macos::MacSeatbeltSandbox::new(
            Path::new("/usr/bin/sandbox-exec"),
            true,
        )?;
        Ok(ProcessRunner::new(sandbox, gate))
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        let _ = (locator, device_paths);
        Ok(ProcessRunner::new(
            crate::sandbox::linux::UnavailableProcessSandbox,
            gate,
        ))
    }
}

#[cfg(test)]
#[path = "../../../tests/process/execution/runner.rs"]
mod tests;
