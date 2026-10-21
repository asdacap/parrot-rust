//! Code-first regression sources. These additions have NOT been executed.

use super::*;
use crate::sandbox::linux::UnavailableProcessSandbox;
use parrot_domain::contracts::async_ops::{BoxFuture, Cancellation};
use parrot_foundation::paths::AgentScratchDirectory;
use std::fs;
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Duration;

static NEXT: AtomicU64 = AtomicU64::new(0);

struct Directory(PathBuf);
impl Directory {
    fn new() -> Self {
        let directory = std::env::temp_dir().join(format!(
            "parrot-runner-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&directory).unwrap();
        Self(directory)
    }

    fn request(&self, command: &str) -> ProcessStartRequest {
        let scratch = AgentScratchDirectory::new(&self.0.join("agent"), &self.0).unwrap();
        let resources = ProcessResources::new(&self.0, self.0.join("queue"), scratch);
        ProcessStartRequest::new(command, resources)
    }
}
impl Drop for Directory {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

#[derive(Default)]
struct Cancel(AtomicBool);
impl Cancellation for Cancel {
    fn is_cancelled(&self) -> bool {
        self.0.load(Ordering::Acquire)
    }
    fn cancelled(&self) -> BoxFuture<'_, ()> {
        Box::pin(async move {
            while !self.is_cancelled() {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
    }
}

fn shell(command: &str) -> LaunchSpec {
    let mut spec = LaunchSpec::new("/bin/sh");
    spec.args = vec!["-c".into(), command.into()];
    spec
}

#[tokio::test]
async fn enabled_unavailable_runner_does_not_start_or_provision_queue() {
    let directory = Directory::new();
    let request = directory.request("printf should-not-run");
    let runner = ProcessRunner::new(UnavailableProcessSandbox, Arc::new(SandboxGate::new(true)));
    assert!(!runner.sandbox_available());
    let error = runner
        .start(&request, &())
        .await
        .err()
        .expect("unavailable sandbox");
    assert_eq!(error.kind(), io::ErrorKind::Unsupported);
    assert!(!request.resources.queue_directory.exists());
}

#[tokio::test]
async fn explicit_gate_off_run_forces_pipe_and_forwards_environment_and_cwd() {
    let directory = Directory::new();
    let mut request = directory.request("printf '%s|%s' \"$T40_RUNNER_VALUE\" \"$PWD\"");
    request
        .environment
        .push(("T40_RUNNER_VALUE".into(), "π-value".into()));
    request.terminal_mode = TerminalMode::PseudoTerminal;
    request.pty_helper = Some(directory.0.join("missing-helper"));
    let gate = Arc::new(SandboxGate::new(false));
    let runner = ProcessRunner::new(UnavailableProcessSandbox, gate.clone());
    let result = runner.run(&request, &()).await.unwrap();
    assert_eq!(result.exit_code, 0);
    assert_eq!(result.stdout, format!("π-value|{}", directory.0.display()));
    assert!(result.stderr.is_empty());
    assert!(request.resources.queue_directory.is_dir());
    // Availability remains about the configured sandbox, not the escape hatch.
    assert!(!runner.sandbox_available());
    gate.set_enabled(true);
    assert_eq!(
        runner.start(&request, &()).await.err().unwrap().kind(),
        io::ErrorKind::Unsupported
    );
}

#[tokio::test]
async fn request_cancellation_remains_linked_after_start_returns() {
    let directory = Directory::new();
    let mut request = directory.request("exec sleep 60");
    let cancellation = Arc::new(Cancel::default());
    request.cancellation = Some(cancellation.clone());
    let runner = ProcessRunner::new(UnavailableProcessSandbox, Arc::new(SandboxGate::new(false)));
    let execution = runner.start(&request, &()).await.unwrap();
    let pid = execution.id();
    cancellation.0.store(true, Ordering::Release);
    let _ = tokio::time::timeout(Duration::from_secs(5), execution.wait())
        .await
        .unwrap();
    execution.settle().await.unwrap();
    assert!(execution.is_completed());
    assert_eq!(
        unsafe { libc::waitpid(pid as i32, std::ptr::null_mut(), libc::WNOHANG) },
        -1
    );
    assert_eq!(
        io::Error::last_os_error().raw_os_error(),
        Some(libc::ECHILD)
    );
}

#[tokio::test]
async fn profile_cleanup_is_owned_until_settlement_not_result_completion() {
    let directory = Directory::new();
    let profile = directory.0.join("profile.sb");
    fs::write(&profile, "synthetic-profile").unwrap();
    let execution = ShellProcessExecution::start_pipe_with_cleanup_path(
        &shell("printf complete"),
        &directory.0,
        profile.clone(),
    )
    .unwrap();
    assert_eq!(execution.wait().await.unwrap().stdout, "complete");
    assert!(profile.is_file());
    execution.settle().await.unwrap();
    assert!(!profile.exists());
    execution.settle().await.unwrap();
}

#[test]
fn profile_cleanup_survives_failed_process_start() {
    let directory = Directory::new();
    let profile = directory.0.join("profile.sb");
    fs::write(&profile, "synthetic-profile").unwrap();
    let spec = LaunchSpec::new(directory.0.join("not-an-executable"));
    assert!(
        ShellProcessExecution::start_pipe_with_cleanup_path(&spec, &directory.0, profile.clone(),)
            .is_err()
    );
    assert!(!profile.exists());
}

#[tokio::test]
async fn dropping_profile_execution_joins_child_and_removes_profile() {
    let directory = Directory::new();
    let profile = directory.0.join("profile.sb");
    fs::write(&profile, "synthetic-profile").unwrap();
    let execution = ShellProcessExecution::start_pipe_with_cleanup_path(
        &shell("exec sleep 60"),
        &directory.0,
        profile.clone(),
    )
    .unwrap();
    let pid = execution.id();
    drop(execution);
    assert!(!profile.exists());
    assert_eq!(
        unsafe { libc::waitpid(pid as i32, std::ptr::null_mut(), libc::WNOHANG) },
        -1
    );
    assert_eq!(
        io::Error::last_os_error().raw_os_error(),
        Some(libc::ECHILD)
    );
}

#[test]
fn resources_reuse_actual_scratch_provision_without_creating_queue() {
    let directory = Directory::new();
    let request = directory.request("unused");
    fs::remove_dir_all(request.resources.scratch().root()).unwrap();
    request.resources.provision_scratch().unwrap();
    assert!(request.resources.scratch().root().is_dir());
    assert!(request.resources.scratch().scratch_path().is_dir());
    assert!(request.resources.blob_directory.is_dir());
    assert!(!request.resources.queue_directory.exists());
}

#[test]
fn pty_attach_placement_is_explicit_without_changing_raw_defaults() {
    let spec = LaunchSpec::new(Path::new("/bin/sh"));
    assert!(spec.pty_attach_before_program);
    let mut planned = spec.clone();
    planned.pty_attach_before_program = false;
    assert!(!planned.pty_attach_before_program);
    assert!(spec.pty_attach_before_program);
}
