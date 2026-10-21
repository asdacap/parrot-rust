use super::{
    LinuxBubblewrapSandbox, LinuxSandboxPolicy, UnavailableProcessSandbox,
    UnsandboxedProcessSandbox,
};
use crate::process::execution::{
    LaunchSpec, ProcessSandbox, ProcessStartRequest, ShellProcessExecution, TerminalMode,
    start_selected_launch,
};
use parrot_domain::contracts::async_ops::BoxFuture;
use std::io;
use std::path::PathBuf;

impl LinuxBubblewrapSandbox {
    pub(super) fn selected_launch(
        &self,
        request: &ProcessStartRequest,
        policy: &LinuxSandboxPolicy,
    ) -> io::Result<LaunchSpec> {
        let Some(executable) = &self.executable else {
            return self.pipe_launch(
                policy,
                &request.command,
                &request.resources.launch_directory,
                &request.environment,
            );
        };
        let helper = match request.terminal_mode {
            TerminalMode::Pipe => None,
            TerminalMode::PseudoTerminal => Some(pseudo_terminal_helper(request)?),
        };
        request.resources.provision_scratch()?;
        let mut policy = policy.clone();
        if let Some(devices) = &self.device_paths {
            policy.device_paths.clone_from(devices);
        }
        let mut launch = LaunchSpec::new(executable);
        launch.args = policy.arguments(
            &request.command,
            &request.resources.launch_directory,
            &request.environment,
            helper.as_deref(),
        );
        launch.retain_launch_thread = matches!(request.terminal_mode, TerminalMode::Pipe);
        // The bridge is the live parent. Attach runs inside bubblewrap,
        // not outside it; only pipe bwrap needs a retained spawning thread.
        launch.pty_attach_before_program = false;
        Ok(launch)
    }
}

impl ProcessSandbox for LinuxBubblewrapSandbox {
    type Policy = LinuxSandboxPolicy;

    fn sandbox_available(&self) -> bool {
        LinuxBubblewrapSandbox::sandbox_available(self)
    }

    fn start_unbound<'a>(
        &'a self,
        request: &'a ProcessStartRequest,
        policy: &'a Self::Policy,
    ) -> BoxFuture<'a, io::Result<ShellProcessExecution>> {
        Box::pin(async move {
            let launch = self.selected_launch(request, policy)?;
            start_selected_launch(request, &launch, None).await
        })
    }
}

impl ProcessSandbox for UnsandboxedProcessSandbox {
    type Policy = ();

    fn sandbox_available(&self) -> bool {
        UnsandboxedProcessSandbox::sandbox_available(self)
    }

    fn start_unbound<'a>(
        &'a self,
        request: &'a ProcessStartRequest,
        _policy: &'a Self::Policy,
    ) -> BoxFuture<'a, io::Result<ShellProcessExecution>> {
        Box::pin(async move {
            request.resources.provision()?;
            if matches!(request.terminal_mode, TerminalMode::PseudoTerminal) {
                pseudo_terminal_helper(request)?;
            }
            let mut launch = self.pipe_launch(
                &request.command,
                &request.resources.launch_directory,
                &request.environment,
            );
            // The explicit escape hatch is bridge -> /bin/sh, without bwrap
            // or an attach process. T40 still owns the same execution lifetime.
            launch.pty_attach_before_program = false;
            start_selected_launch(request, &launch, None).await
        })
    }
}

impl ProcessSandbox for UnavailableProcessSandbox {
    type Policy = ();

    fn sandbox_available(&self) -> bool {
        false
    }

    fn start_unbound<'a>(
        &'a self,
        _request: &'a ProcessStartRequest,
        _policy: &'a Self::Policy,
    ) -> BoxFuture<'a, io::Result<ShellProcessExecution>> {
        Box::pin(async move {
            // Reject both modes before provisioning resources or output files.
            Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "Sandboxed shell commands require Linux or macOS.",
            ))
        })
    }
}

fn pseudo_terminal_helper(request: &ProcessStartRequest) -> io::Result<PathBuf> {
    if !cfg!(target_os = "linux") {
        return Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "Pseudo-terminal shell processes require Linux.",
        ));
    }
    let helper = request.pty_helper_path()?;
    if !helper.is_file() {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            "The PTY attach helper does not exist.",
        ));
    }
    Ok(helper)
}
