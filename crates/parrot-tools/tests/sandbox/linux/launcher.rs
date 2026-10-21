//! Regression SOURCE ONLY: NOT_RUN in the code-first continuation.
#![cfg(target_os = "linux")]

use super::{
    LinuxAccessRule, LinuxBubblewrapSandbox, LinuxSandboxPolicy, UnavailableProcessSandbox,
    UnsandboxedProcessSandbox,
};
use crate::process::execution::locator::ExecutableLocator;
use crate::process::execution::{
    ProcessResources, ProcessResult, ProcessSandbox, ProcessStartRequest, ShellProcessExecution,
    TerminalMode,
};
use parrot_domain::contracts::async_ops::{BoxFuture, Cancellation};
use parrot_foundation::paths::AgentScratchDirectory;
use std::fs;
use std::io;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Duration;

static NEXT: AtomicU64 = AtomicU64::new(0);
const LIMIT: Duration = Duration::from_secs(15);

struct Fixture(PathBuf);
impl Fixture {
    fn new() -> Self {
        let root = std::env::temp_dir().join(format!(
            "parrot-t38-launcher-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&root).unwrap();
        Self(root)
    }

    fn request(&self, command: &str, terminal_mode: TerminalMode) -> ProcessStartRequest {
        let scratch = AgentScratchDirectory::new(&self.0.join("agent"), &self.0).unwrap();
        // Model resources removed after factory construction, as in source
        // reprovisioning cases, to observe Start's branch-specific effects.
        fs::remove_dir_all(scratch.root()).unwrap();
        ProcessStartRequest {
            command: command.into(),
            environment: vec![("T38_LITERAL".into(), "$(printf injected); literal".into())],
            resources: ProcessResources::new(self.0.clone(), self.0.join("queue"), scratch),
            terminal_mode,
            pty_helper: Some(self.0.join("missing-helper")),
            cancellation: None,
        }
    }

    fn policy(&self) -> LinuxSandboxPolicy {
        LinuxSandboxPolicy {
            rules: vec![LinuxAccessRule {
                path: self.0.clone(),
                read: true,
                write: true,
            }],
            owned_roots: vec![self.0.clone()],
            device_paths: Vec::new(),
        }
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        fs::remove_dir_all(&self.0).unwrap();
    }
}

fn sandbox() -> LinuxBubblewrapSandbox {
    let path = ExecutableLocator::capture()
        .locate("bwrap")
        .expect("real bwrap required");
    LinuxBubblewrapSandbox::new(&path, false).unwrap()
}

fn helper() -> PathBuf {
    PathBuf::from(env!("PARROT_PTY_HELPER"))
}

async fn result(execution: &ShellProcessExecution) -> ProcessResult {
    let result = tokio::time::timeout(LIMIT, execution.wait())
        .await
        .unwrap()
        .unwrap();
    tokio::time::timeout(LIMIT, execution.settle())
        .await
        .unwrap()
        .unwrap();
    result
}

async fn ready(execution: &ShellProcessExecution) {
    let path = execution.stdout_path().expect("real pipe stdout path");
    tokio::time::timeout(LIMIT, async {
        loop {
            if fs::read_to_string(&path).unwrap().contains("READY") {
                break;
            }
            assert!(!execution.is_completed(), "process exited before READY");
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap();
}

#[tokio::test(flavor = "current_thread")]
async fn unavailable_both_modes_have_no_command_or_resource_effects() {
    let root = Fixture::new();
    let unavailable = LinuxBubblewrapSandbox::new(std::path::Path::new(""), false).unwrap();
    for mode in [TerminalMode::Pipe, TerminalMode::PseudoTerminal] {
        let request = root.request("printf forbidden > marker", mode);
        let error = match unavailable.start(&request, &root.policy()).await {
            Ok(_) => panic!("missing bwrap must fail closed"),
            Err(error) => error,
        };
        assert_eq!(error.kind(), io::ErrorKind::Unsupported);
        assert!(
            UnavailableProcessSandbox
                .start(&request, &())
                .await
                .is_err()
        );
        assert!(!root.0.join("marker").exists());
        assert!(!request.resources.blob_directory.exists());
        assert!(!request.resources.queue_directory.exists());
    }
}

#[tokio::test(flavor = "current_thread")]
async fn explicit_unsandboxed_pipe_runs_real_shell_with_literal_env_and_cwd() {
    let root = Fixture::new();
    let request = root.request(
        "printf '%s' \"$T38_LITERAL\"; printf err >&2; printf cwd > marker; exit 7",
        TerminalMode::Pipe,
    );
    let execution = UnsandboxedProcessSandbox
        .start(&request, &())
        .await
        .unwrap();
    assert!(!execution.is_pseudo_terminal());
    let result = result(&execution).await;
    assert_eq!(result.exit_code, 7);
    assert_eq!(result.stdout, "$(printf injected); literal");
    assert_eq!(result.stderr, "err");
    assert_eq!(fs::read_to_string(root.0.join("marker")).unwrap(), "cwd");
    assert!(request.resources.queue_directory.is_dir());
}

#[tokio::test(flavor = "current_thread")]
async fn unsandboxed_missing_pty_helper_fails_after_provisioning_without_command() {
    let root = Fixture::new();
    let request = root.request("printf forbidden > marker", TerminalMode::PseudoTerminal);
    let error = match UnsandboxedProcessSandbox.start(&request, &()).await {
        Ok(_) => panic!("missing PTY helper must fail"),
        Err(error) => error,
    };
    assert_eq!(error.kind(), io::ErrorKind::NotFound);
    assert!(request.resources.blob_directory.is_dir());
    assert!(request.resources.queue_directory.is_dir());
    assert!(!root.0.join("marker").exists());
    assert_eq!(
        fs::read_dir(&request.resources.blob_directory)
            .unwrap()
            .count(),
        0
    );
}

#[tokio::test(flavor = "current_thread")]
async fn sandboxed_missing_pty_helper_fails_before_provisioning() {
    let root = Fixture::new();
    // A real existing executable suffices: helper validation must precede spawn.
    let sandbox = LinuxBubblewrapSandbox::new(std::path::Path::new("/bin/sh"), false).unwrap();
    let request = root.request("printf forbidden > marker", TerminalMode::PseudoTerminal);
    let error = match sandbox.start(&request, &root.policy()).await {
        Ok(_) => panic!("missing helper must fail before launch"),
        Err(error) => error,
    };
    assert_eq!(error.kind(), io::ErrorKind::NotFound);
    assert!(!request.resources.blob_directory.exists());
    assert!(!request.resources.queue_directory.exists());
    assert!(!root.0.join("marker").exists());
}

#[tokio::test(flavor = "current_thread")]
async fn explicit_unsandboxed_pty_uses_real_bridge_and_no_bwrap() {
    let root = Fixture::new();
    let mut request = root.request(
        "test -t 0 && test -t 1 && stty size; printf '%s' \"$T38_LITERAL\"; printf pty > marker",
        TerminalMode::PseudoTerminal,
    );
    request.pty_helper = Some(helper());
    let execution = UnsandboxedProcessSandbox
        .start(&request, &())
        .await
        .unwrap();
    assert!(execution.is_pseudo_terminal());
    let result = result(&execution).await;
    assert_eq!(result.exit_code, 0);
    assert!(result.stdout.contains("24 80"));
    assert!(result.stdout.contains("$(printf injected); literal"));
    assert_eq!(fs::read_to_string(root.0.join("marker")).unwrap(), "pty");
}

#[tokio::test(flavor = "current_thread")]
#[ignore = "requires actual Linux bwrap/user namespaces; mandatory deferred --include-ignored gate"]
async fn real_sandbox_pipe_executes_and_provisions_no_queue() {
    let root = Fixture::new();
    let request = root.request(
        "printf '%s' \"$T38_LITERAL\"; printf confined > marker; exit 7",
        TerminalMode::Pipe,
    );
    let execution = sandbox().start(&request, &root.policy()).await.unwrap();
    let result = result(&execution).await;
    assert_eq!(result.exit_code, 7);
    assert_eq!(result.stdout, "$(printf injected); literal");
    assert!(result.stderr.is_empty());
    assert_eq!(
        fs::read_to_string(root.0.join("marker")).unwrap(),
        "confined"
    );
    assert!(request.resources.blob_directory.is_dir());
    assert!(!request.resources.queue_directory.exists());
}

#[tokio::test(flavor = "current_thread")]
#[ignore = "requires actual Linux bwrap/user namespaces; mandatory deferred --include-ignored gate"]
async fn real_sandbox_pipe_survives_the_calling_os_thread_exiting() {
    let root = Fixture::new();
    let request = root.request(
        "printf READY; while [ ! -f finish ]; do sleep 0.01; done; printf FINISHED; exit 7",
        TerminalMode::Pipe,
    );
    let policy = root.policy();
    let sandbox = sandbox();
    let caller = std::thread::spawn(move || {
        tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .build()
            .unwrap()
            .block_on(sandbox.start(&request, &policy))
            .unwrap()
    });
    // Joining drops the calling runtime and OS thread before releasing the child.
    let execution = caller.join().unwrap();
    ready(&execution).await;
    fs::write(root.0.join("finish"), "").unwrap();
    let result = result(&execution).await;
    assert_eq!(result.exit_code, 7);
    assert_eq!(result.stdout, "READYFINISHED");
    assert!(result.stderr.is_empty());
}

#[tokio::test(flavor = "current_thread")]
#[ignore = "requires actual Linux bwrap/user namespaces; mandatory deferred --include-ignored gate"]
async fn real_sandbox_pty_attaches_inside_bwrap() {
    let root = Fixture::new();
    let mut request = root.request(
        "test -t 0 && test -t 1 && stty size; printf '%s' \"$T38_LITERAL\"; printf confined-pty > marker",
        TerminalMode::PseudoTerminal,
    );
    request.pty_helper = Some(helper());
    let execution = sandbox().start(&request, &root.policy()).await.unwrap();
    assert!(execution.is_pseudo_terminal());
    let result = result(&execution).await;
    assert_eq!(result.exit_code, 0);
    assert!(result.stdout.contains("24 80"));
    assert!(result.stdout.contains("$(printf injected); literal"));
    assert_eq!(
        fs::read_to_string(root.0.join("marker")).unwrap(),
        "confined-pty"
    );
    assert!(!request.resources.queue_directory.exists());
}

#[tokio::test(flavor = "current_thread")]
async fn direct_run_forces_pipe_and_settles_real_execution() {
    let root = Fixture::new();
    let request = root.request("printf run-result; exit 9", TerminalMode::PseudoTerminal);
    let result = tokio::time::timeout(LIMIT, UnsandboxedProcessSandbox.run(&request, &()))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(result.exit_code, 9);
    assert_eq!(result.stdout, "run-result");
    assert_eq!(request.terminal_mode, TerminalMode::PseudoTerminal);
}

#[tokio::test(flavor = "current_thread")]
async fn failed_sandbox_spawn_removes_output_artifacts_without_unconfined_fallback() {
    let root = Fixture::new();
    let executable = root.0.join("not-executable");
    fs::write(&executable, "not executable").unwrap();
    let sandbox = LinuxBubblewrapSandbox::new(&executable, false).unwrap();
    let request = root.request("printf forbidden > marker", TerminalMode::Pipe);
    assert!(sandbox.start(&request, &root.policy()).await.is_err());
    assert!(!root.0.join("marker").exists());
    assert!(!request.resources.queue_directory.exists());
    assert_eq!(
        fs::read_dir(&request.resources.blob_directory)
            .unwrap()
            .count(),
        0
    );
}

#[test]
fn launcher_devices_override_policy_without_mutating_it_and_pty_shape_is_exact() {
    let root = Fixture::new();
    let sandbox = LinuxBubblewrapSandbox::new(std::path::Path::new("/bin/sh"), false).unwrap();
    let mut request = root.request("true", TerminalMode::PseudoTerminal);
    request.pty_helper = Some(helper());
    let mut policy = root.policy();
    policy.device_paths.push("/dev/zero".into());
    let launch = sandbox
        .clone()
        .with_device_paths(Vec::new())
        .selected_launch(&request, &policy)
        .unwrap();
    assert!(!launch.args.contains(&"--dev-bind-try".into()));
    assert!(!launch.retain_launch_thread);
    assert!(!launch.pty_attach_before_program);
    let attach = launch
        .args
        .iter()
        .position(|arg| arg == "--attach")
        .unwrap();
    assert_eq!(
        &launch.args[attach..],
        &["--attach", "--", "/bin/sh", "-c", "true"]
    );
    assert_eq!(policy.device_paths, vec![PathBuf::from("/dev/zero")]);
    let launch = sandbox
        .with_device_paths(vec!["/dev/null".into()])
        .selected_launch(&request, &policy)
        .unwrap();
    let device = launch
        .args
        .iter()
        .position(|arg| arg == "--dev-bind-try")
        .unwrap();
    assert_eq!(
        &launch.args[device + 1..device + 3],
        &["/dev/null", "/dev/null"]
    );
}

struct TestCancellation(AtomicBool);
impl Cancellation for TestCancellation {
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

#[tokio::test(flavor = "current_thread")]
async fn cancellation_after_start_is_owned_without_polling_result() {
    let root = Fixture::new();
    let cancellation = Arc::new(TestCancellation(AtomicBool::new(false)));
    let mut request = root.request(
        "printf READY; while :; do sleep 1; done",
        TerminalMode::Pipe,
    );
    request.cancellation = Some(cancellation.clone());
    let execution = UnsandboxedProcessSandbox
        .start(&request, &())
        .await
        .unwrap();
    ready(&execution).await;
    cancellation.0.store(true, Ordering::Release);
    tokio::time::timeout(LIMIT, async {
        while !execution.is_completed() {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap();
    assert!(execution.wait().await.is_err());
    let _ = tokio::time::timeout(LIMIT, execution.settle())
        .await
        .unwrap();
}
