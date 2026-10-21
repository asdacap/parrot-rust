//! Original ShellProcessOwner/ManagedShellProcess predicates, using controlled
//! execution futures to expose the precise claim and shutdown interleavings.

#[path = "domain.rs"]
mod domain_bridge;

use super::*;
use crate::process::ownership::execution::{
    Execution, NeverCancelled, ProcessLifetime, ProcessResult,
};
use crate::process::ownership::managed::{
    CompletionMessage, CompletionReport, CompletionSink, NoProcessDiagnostics, ProcessDiagnostic,
    ProcessDiagnostics,
};
use parrot_domain::contracts::async_ops::{BoxFuture, Cancellation};
use std::path::PathBuf;
use std::sync::Mutex as StdMutex;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::Duration;
use tokio::sync::Notify;

#[derive(Default)]
struct FakeExecution {
    result: StdMutex<Option<Result<ProcessResult, (io::ErrorKind, String)>>>,
    changed: Notify,
    output: StdMutex<Vec<u16>>,
    signals: StdMutex<Vec<i32>>,
    cancel_count: AtomicUsize,
    settle_count: AtomicUsize,
    settled: AtomicBool,
    fail_settle: AtomicBool,
    settle_gate: StdMutex<Option<Arc<ProcessLifetime>>>,
    block_write: AtomicBool,
    pipe: AtomicBool,
}
impl FakeExecution {
    fn append(&self, text: &str) {
        self.output.lock().unwrap().extend(text.encode_utf16());
    }
    fn complete(&self, exit: i32) {
        *self.result.lock().unwrap() = Some(Ok(ProcessResult {
            exit_code: exit,
            elapsed_milliseconds: 5001,
            stdout: String::from_utf16_lossy(&self.output.lock().unwrap()),
            stderr: String::new(),
            blob_path: None,
        }));
        self.changed.notify_waiters();
    }
    fn failure(&self, kind: io::ErrorKind, message: &str) {
        *self.result.lock().unwrap() = Some(Err((kind, message.into())));
        self.changed.notify_waiters();
    }
    fn result(&self) -> Option<io::Result<ProcessResult>> {
        self.result.lock().unwrap().as_ref().map(|result| {
            result
                .clone()
                .map_err(|(kind, text)| io::Error::new(kind, text))
        })
    }
}
impl Execution for FakeExecution {
    fn is_completed(&self) -> bool {
        self.result().is_some()
    }
    fn is_pseudo_terminal(&self) -> bool {
        !self.pipe.load(Ordering::Acquire)
    }
    fn stdout_path(&self) -> Option<PathBuf> {
        self.pipe
            .load(Ordering::Acquire)
            .then(|| PathBuf::from("/fake-stdout"))
    }
    fn stderr_path(&self) -> Option<PathBuf> {
        self.pipe
            .load(Ordering::Acquire)
            .then(|| PathBuf::from("/fake-stderr"))
    }
    fn read_transcript(&self, offset: u64) -> io::Result<(u64, Vec<u16>)> {
        let output = self.output.lock().unwrap();
        let offset = usize::try_from(offset).map_err(io::Error::other)?;
        if offset > output.len() {
            return Err(io::Error::other("invalid cursor"));
        }
        Ok((output.len() as u64, output[offset..].to_vec()))
    }
    fn read_result(&self, offset: u64) -> io::Result<(u64, ProcessResult)> {
        let mut result = self
            .result()
            .ok_or_else(|| io::Error::from(io::ErrorKind::WouldBlock))??;
        let (cursor, output) = self.read_transcript(offset)?;
        if offset > 0 {
            result.stdout = String::from_utf16_lossy(&output);
        }
        Ok((cursor, result))
    }
    fn wait(&self) -> BoxFuture<'_, io::Result<ProcessResult>> {
        Box::pin(async move {
            loop {
                let changed = self.changed.notified();
                tokio::pin!(changed);
                changed.as_mut().enable();
                if let Some(result) = self.result() {
                    return result;
                }
                changed.await;
            }
        })
    }
    fn write_stdin<'a>(&'a self, input: &'a str) -> BoxFuture<'a, io::Result<()>> {
        Box::pin(async move {
            if self.block_write.load(Ordering::Acquire) {
                std::future::pending::<()>().await;
            }
            self.append(input);
            Ok(())
        })
    }
    fn send_signal(&self, signal: i32) -> io::Result<()> {
        self.signals.lock().unwrap().push(signal);
        Ok(())
    }
    fn request_cancel(&self) {
        self.cancel_count.fetch_add(1, Ordering::AcqRel);
        if self.result().is_none() {
            self.failure(io::ErrorKind::Interrupted, "cancelled");
        }
    }
    fn settle(&self) -> BoxFuture<'_, io::Result<()>> {
        Box::pin(async move {
            let gate = self.settle_gate.lock().unwrap().clone();
            if let Some(gate) = gate {
                gate.cancelled().await;
            }
            if !self.settled.swap(true, Ordering::AcqRel) {
                self.settle_count.fetch_add(1, Ordering::AcqRel);
            }
            if self.fail_settle.load(Ordering::Acquire) {
                Err(io::Error::other("settlement failed"))
            } else {
                Ok(())
            }
        })
    }
}

#[derive(Default)]
struct Sink {
    calls: StdMutex<Vec<CompletionMessage>>,
    failures: AtomicUsize,
}
impl CompletionSink for Sink {
    fn send<'a>(
        &'a self,
        message: &'a CompletionMessage,
        cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, io::Result<()>> {
        Box::pin(async move {
            self.calls.lock().unwrap().push(message.clone());
            cancellation
                .check()
                .map_err(|_| io::Error::from(io::ErrorKind::Interrupted))?;
            if self
                .failures
                .fetch_update(Ordering::AcqRel, Ordering::Acquire, |remaining| {
                    remaining.checked_sub(1)
                })
                .is_ok()
            {
                Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    "delivery failed",
                ))
            } else {
                Ok(())
            }
        })
    }
}

#[derive(Default)]
struct Diagnostics(StdMutex<Vec<ProcessDiagnostic>>);
impl ProcessDiagnostics for Diagnostics {
    fn write(&self, event: ProcessDiagnostic) {
        self.0.lock().unwrap().push(event);
    }
}

fn input(owner: &str, name: Option<&str>) -> ProcessStart {
    ProcessStart {
        requested_name: name.map(str::to_owned),
        command: "secret-command".into(),
        description: "secret-description".into(),
        origin_tool_call_id: "secret-call".into(),
        agent: ProcessAgent {
            session_id: owner.into(),
            name: "main".into(),
            parent_session_id: String::new(),
            parent_name: String::new(),
            depth: 0,
        },
    }
}
fn owner(id: &str, sink: Arc<Sink>) -> Arc<ShellProcessOwner> {
    Arc::new(ShellProcessOwner::new(id, None, sink, Arc::new(NoProcessDiagnostics)).unwrap())
}
async fn start(
    owner: &ShellProcessOwner,
    name: Option<&str>,
    execution: Arc<FakeExecution>,
) -> ClaimedProcess {
    owner
        .start(
            input(owner.session_id(), name),
            Arc::new(CompletionReport),
            move |_| Box::pin(async move { Ok(execution as Arc<dyn Execution>) }),
        )
        .await
        .unwrap()
}
async fn until(mut ready: impl FnMut() -> bool) {
    tokio::time::timeout(Duration::from_secs(5), async {
        while !ready() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
}

struct Directory(PathBuf);
impl Directory {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!(
            "managed-owner-{}",
            parrot_domain::values::identifiers::Identifier::event_id().unwrap()
        ));
        std::fs::create_dir(&path).unwrap();
        Self(path)
    }
}
impl Drop for Directory {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

struct SynchronousObservationReport {
    blocking: AtomicBool,
    kind: crate::process::ownership::inventory::ActivityKind,
}
impl ProcessReport for SynchronousObservationReport {
    fn blocks_turn(&self) -> bool {
        self.blocking.load(Ordering::Acquire)
    }
    fn activity_kind(&self) -> crate::process::ownership::inventory::ActivityKind {
        self.kind
    }
    fn observe<'a>(
        &'a self,
        _: &'a ActiveShellProcessState,
        _: Arc<dyn Execution>,
        _: &'a dyn Cancellation,
    ) -> BoxFuture<'a, io::Result<()>> {
        Box::pin(async { Ok(()) })
    }
    fn complete(&self, state: &ActiveShellProcessState, output: &str) -> io::Result<String> {
        CompletionReport.complete(state, output)
    }
}

#[tokio::test]
async fn synchronous_observation_reads_live_report_flags_not_activity_kind() {
    use crate::process::ownership::inventory::ActivityKind;
    let owner = owner("agent", Arc::new(Sink::default()));
    let blocking_monitor = Arc::new(SynchronousObservationReport {
        blocking: AtomicBool::new(true),
        kind: ActivityKind::Monitor,
    });
    let nonblocking_execution = Arc::new(SynchronousObservationReport {
        blocking: AtomicBool::new(false),
        kind: ActivityKind::Execution,
    });
    let mut executions = Vec::new();
    for (name, report) in [
        ("monitor", blocking_monitor.clone()),
        ("execution", nonblocking_execution.clone()),
    ] {
        let execution = Arc::new(FakeExecution::default());
        executions.push(execution.clone());
        owner
            .start(input("agent", Some(name)), report, move |_| {
                Box::pin(async move { Ok(execution as Arc<dyn Execution>) })
            })
            .await
            .unwrap()
            .yield_now();
    }
    assert_eq!(
        owner.observe_active(),
        [ActiveProcessObservation {
            id: "agent/monitor".into(),
            name: "monitor".into(),
            state: ProcessStatusState::Running,
        }]
    );
    let snapshot = owner.observe_snapshot();
    assert_eq!(snapshot.len(), 2);
    blocking_monitor.blocking.store(false, Ordering::Release);
    nonblocking_execution
        .blocking
        .store(true, Ordering::Release);
    assert_eq!(
        owner.observe_active(),
        [ActiveProcessObservation {
            id: "agent/execution".into(),
            name: "execution".into(),
            state: ProcessStatusState::Running,
        }]
    );
    assert_eq!(owner.observe_snapshot(), snapshot);
    assert_eq!(owner.active().await, owner.observe_active());
    assert_eq!(owner.snapshot().await, snapshot);
    owner.dispose().await.unwrap();
    for execution in executions {
        assert!(execution.cancel_count.load(Ordering::Acquire) > 0);
        assert_eq!(execution.settle_count.load(Ordering::Acquire), 1);
    }
}

#[tokio::test]
async fn synchronous_observation_keeps_claimed_completion_until_retirement_and_replacement() {
    let sink = Arc::new(Sink::default());
    let owner = owner("agent", sink.clone());
    let first = Arc::new(FakeExecution::default());
    let claimed = start(&owner, Some("same"), first.clone()).await;
    let old = Arc::clone(claimed.process());
    let before = owner.observe_snapshot();
    assert_eq!(before[0].process_id, old.state.process_id);
    first.complete(0);
    until(|| old.completed()).await;
    assert!(!old.retired());
    assert!(sink.calls.lock().unwrap().is_empty());
    assert_eq!(owner.observe_snapshot(), before);
    assert_eq!(owner.observe_active()[0].state, ProcessStatusState::Running);
    assert!(!claimed.wait(None, &NeverCancelled).await.unwrap().running);
    assert!(old.retired());
    assert!(owner.observe_active().is_empty());
    assert!(owner.observe_snapshot().is_empty());
    let second = Arc::new(FakeExecution::default());
    let successor = start(&owner, Some("same"), second.clone()).await;
    let after = owner.observe_snapshot();
    assert_eq!(after.len(), 1);
    assert_eq!(after[0].process_id, successor.process().state.process_id);
    assert_ne!(after[0].process_id, before[0].process_id);
    assert_eq!(owner.observe_active()[0].id, "agent/same");
    assert!(old.claim().is_err());
    successor.yield_now();
    owner.dispose().await.unwrap();
    assert_eq!(first.settle_count.load(Ordering::Acquire), 1);
    assert_eq!(second.settle_count.load(Ordering::Acquire), 1);
}

#[tokio::test]
async fn synchronous_observation_preserves_ordinal_identity_and_scope_isolation() {
    let first = owner("scope-1", Arc::new(Sink::default()));
    let second = owner("scope-2", Arc::new(Sink::default()));
    let mut executions = Vec::new();
    let mut expected = Vec::new();
    for name in ["\u{e000}", "\u{10000}", "a/z", "a"] {
        let execution = Arc::new(FakeExecution::default());
        executions.push(execution.clone());
        let claimed = start(&first, Some(name), execution).await;
        expected.push(ProcessStatus {
            owner_session_id: "scope-1".into(),
            process_id: claimed.process().state.process_id.clone(),
            name: name.into(),
            state: ProcessStatusState::Running,
        });
        claimed.yield_now();
    }
    expected.sort_by(|left, right| {
        left.process_id
            .encode_utf16()
            .cmp(right.process_id.encode_utf16())
    });
    assert_eq!(first.observe_snapshot(), expected);
    assert!(expected.iter().all(|status| {
        status.process_id.starts_with("shell-process-")
            && status.process_id != format!("scope-1/{}", status.name)
    }));
    assert!(
        expected
            .windows(2)
            .all(|pair| pair[0].process_id != pair[1].process_id)
    );
    assert_eq!(
        first
            .observe_active()
            .iter()
            .map(|status| status.id.as_str())
            .collect::<Vec<_>>(),
        [
            "scope-1/a",
            "scope-1/a/z",
            "scope-1/\u{10000}",
            "scope-1/\u{e000}"
        ]
    );
    assert!(second.observe_snapshot().is_empty());
    let sibling_execution = Arc::new(FakeExecution::default());
    let sibling = start(&second, Some("a"), sibling_execution.clone()).await;
    assert_eq!(second.observe_active()[0].id, "scope-2/a");
    assert_eq!(second.observe_snapshot()[0].owner_session_id, "scope-2");
    assert_eq!(
        second.observe_snapshot()[0].process_id,
        sibling.process().state.process_id
    );
    assert!(
        !expected
            .iter()
            .any(|status| status.process_id == sibling.process().state.process_id)
    );
    sibling.yield_now();
    first.dispose().await.unwrap();
    for execution in executions {
        assert_eq!(execution.settle_count.load(Ordering::Acquire), 1);
    }
    assert_eq!(sibling_execution.cancel_count.load(Ordering::Acquire), 0);
    assert_eq!(sibling_execution.settle_count.load(Ordering::Acquire), 0);
    assert_eq!(second.observe_snapshot().len(), 1);
    second.dispose().await.unwrap();
    assert_eq!(sibling_execution.settle_count.load(Ordering::Acquire), 1);
}

#[tokio::test]
async fn synchronous_observation_does_not_wait_for_startup_but_async_readers_do() {
    let owner = owner("agent", Arc::new(Sink::default()));
    let existing = Arc::new(FakeExecution::default());
    start(&owner, Some("existing"), existing.clone())
        .await
        .yield_now();
    let before_active = owner.observe_active();
    let before_snapshot = owner.observe_snapshot();
    let execution = Arc::new(FakeExecution::default());
    let pending_execution = execution.clone();
    let entered = Arc::new(Notify::new());
    let release = Arc::new(ProcessLifetime::default());
    let factory_entered = entered.clone();
    let factory_release = release.clone();
    let startup = owner.start(
        input("agent", Some("pending")),
        Arc::new(CompletionReport),
        move |_| {
            Box::pin(async move {
                factory_entered.notify_one();
                factory_release.cancelled().await;
                Ok(pending_execution as Arc<dyn Execution>)
            })
        },
    );
    tokio::pin!(startup);
    tokio::select! {
        result = &mut startup => panic!("startup must be pending: {:?}", result.err()),
        () = entered.notified() => {}
    }
    let active = owner.active();
    let snapshot = owner.snapshot();
    tokio::pin!(active, snapshot);
    tokio::select! {
        biased;
        _ = &mut active => panic!("async active must await startup"),
        _ = &mut snapshot => panic!("async snapshot must await startup"),
        () = std::future::ready(()) => {}
    }
    assert_eq!(owner.observe_active(), before_active);
    assert_eq!(owner.observe_snapshot(), before_snapshot);
    release.cancel();
    let (claimed, active, snapshot) = tokio::time::timeout(Duration::from_secs(5), async {
        tokio::join!(&mut startup, &mut active, &mut snapshot)
    })
    .await
    .unwrap();
    let claimed = claimed.unwrap();
    assert_eq!(active, owner.observe_active());
    assert_eq!(snapshot, owner.observe_snapshot());
    assert_eq!(snapshot.len(), 2);
    assert!(
        snapshot
            .iter()
            .any(|status| status.process_id == claimed.process().state.process_id)
    );
    claimed.yield_now();
    owner.dispose().await.unwrap();
    assert_eq!(existing.settle_count.load(Ordering::Acquire), 1);
    assert_eq!(execution.settle_count.load(Ordering::Acquire), 1);
}

#[tokio::test]
async fn synchronous_observation_omits_failed_and_cancelled_pending_startup() {
    let owner = owner("agent", Arc::new(Sink::default()));
    assert!(
        owner
            .start(
                input("agent", Some("failed")),
                Arc::new(CompletionReport),
                |_| { Box::pin(async { Err(io::Error::other("startup failed")) }) }
            )
            .await
            .is_err()
    );
    assert!(owner.observe_active().is_empty());
    assert!(owner.observe_snapshot().is_empty());
    let execution = Arc::new(FakeExecution::default());
    start(&owner, Some("existing"), execution.clone())
        .await
        .yield_now();
    let before = owner.observe_snapshot();
    let entered = Notify::new();
    let startup = owner.start(
        input("agent", Some("pending")),
        Arc::new(CompletionReport),
        |_| {
            entered.notify_one();
            Box::pin(std::future::pending())
        },
    );
    tokio::pin!(startup);
    tokio::select! {
        result = &mut startup => panic!("startup must be pending: {:?}", result.err()),
        () = entered.notified() => {}
    }
    assert_eq!(owner.observe_snapshot(), before);
    assert_eq!(owner.observe_active()[0].id, "agent/existing");
    let (result, disposal) = tokio::time::timeout(Duration::from_secs(5), async {
        tokio::join!(&mut startup, owner.dispose())
    })
    .await
    .unwrap();
    assert_eq!(result.err().unwrap().kind(), io::ErrorKind::Interrupted);
    disposal.unwrap();
    assert!(
        !owner
            .observe_snapshot()
            .iter()
            .any(|status| status.name == "failed" || status.name == "pending")
    );
    assert!(execution.cancel_count.load(Ordering::Acquire) > 0);
    assert_eq!(execution.settle_count.load(Ordering::Acquire), 1);
}

#[tokio::test]
async fn actual_t40_pipe_yield_keeps_live_files_and_terminal_wait_uses_same_instance() {
    use crate::process::execution::{LaunchSpec, ShellProcessExecution};
    let directory = Directory::new();
    let marker = directory.0.join("finish");
    let mut spec = LaunchSpec::new("/bin/sh");
    spec.args = vec![
        "-c".into(),
        format!(
            "printf early; while [ ! -f '{}' ]; do sleep 0.01; done; printf late; exit 7",
            marker.display()
        )
        .into(),
    ];
    let raw = Arc::new(ShellProcessExecution::start_pipe(&spec, &directory.0).unwrap());
    let execution = raw.clone();
    let sink = Arc::new(Sink::default());
    let owner = owner("agent", sink.clone());
    let claimed = owner
        .start(
            input("agent", Some("work")),
            Arc::new(CompletionReport),
            move |_| Box::pin(async move { Ok(execution as Arc<dyn Execution>) }),
        )
        .await
        .unwrap();
    let process = Arc::clone(claimed.process());
    let yielded = claimed
        .wait(Some(Duration::ZERO), &NeverCancelled)
        .await
        .unwrap();
    assert!(yielded.running);
    let paths = yielded.yielded_process.unwrap();
    let stdout = paths.stdout_path.unwrap();
    let stderr = paths.stderr_path.unwrap();
    until(|| std::fs::read(&stdout).is_ok_and(|bytes| bytes == b"early")).await;
    assert!(std::fs::read(&stderr).unwrap().is_empty());
    let claimed = owner.claim("work").await.unwrap();
    assert_eq!(claimed.process().state.process_id, process.state.process_id);
    std::fs::write(marker, "go").unwrap();
    let completed =
        tokio::time::timeout(Duration::from_secs(5), claimed.wait(None, &NeverCancelled))
            .await
            .unwrap()
            .unwrap();
    let result = completed.result.unwrap();
    assert_eq!(result.exit_code, 7);
    assert_eq!(result.stdout, "earlylate");
    assert_eq!(std::fs::read(&stdout).unwrap(), b"earlylate");
    assert!(process.retired());
    assert!(sink.calls.lock().unwrap().is_empty());
    owner.dispose().await.unwrap();
    assert!(raw.is_completed());
}

#[tokio::test]
async fn actual_t40_shutdown_kills_and_joins_owned_process_not_its_sibling() {
    use crate::process::execution::{LaunchSpec, ShellProcessExecution};
    let first_directory = Directory::new();
    let second_directory = Directory::new();
    let mut spec = LaunchSpec::new("/bin/sh");
    spec.args = vec!["-c".into(), "printf ready; sleep 30 & wait".into()];
    let first_execution =
        Arc::new(ShellProcessExecution::start_pipe(&spec, &first_directory.0).unwrap());
    let second_execution =
        Arc::new(ShellProcessExecution::start_pipe(&spec, &second_directory.0).unwrap());
    let first = owner("first", Arc::new(Sink::default()));
    let second = owner("second", Arc::new(Sink::default()));
    let execution = first_execution.clone();
    first
        .start(
            input("first", Some("same")),
            Arc::new(CompletionReport),
            move |_| Box::pin(async move { Ok(execution as Arc<dyn Execution>) }),
        )
        .await
        .unwrap()
        .yield_now();
    let execution = second_execution.clone();
    second
        .start(
            input("second", Some("same")),
            Arc::new(CompletionReport),
            move |_| Box::pin(async move { Ok(execution as Arc<dyn Execution>) }),
        )
        .await
        .unwrap()
        .yield_now();
    let pid = first_execution.id();
    tokio::time::timeout(Duration::from_secs(5), first.dispose())
        .await
        .unwrap()
        .unwrap();
    assert!(first_execution.is_completed());
    assert!(!second_execution.is_completed());
    assert!(first.capture_inventory().removed);
    assert_eq!(
        unsafe { libc::waitpid(pid as i32, std::ptr::null_mut(), libc::WNOHANG) },
        -1
    );
    assert_eq!(
        io::Error::last_os_error().raw_os_error(),
        Some(libc::ECHILD)
    );
    tokio::time::timeout(Duration::from_secs(5), second.dispose())
        .await
        .unwrap()
        .unwrap();
}

#[cfg(target_os = "linux")]
#[tokio::test]
async fn actual_t40_pty_claims_consume_ready_and_input_suffix_once() {
    use crate::process::execution::{LaunchSpec, ShellProcessExecution};
    let directory = Directory::new();
    let mut spec = LaunchSpec::new("/bin/sh");
    spec.args = vec![
        "-c".into(),
        "stty -echo; printf ready; read value; printf '<%s>' \"$value\"".into(),
    ];
    let raw = Arc::new(
        tokio::time::timeout(
            Duration::from_secs(5),
            ShellProcessExecution::start_pty(
                &spec,
                std::path::Path::new(env!("PARROT_PTY_HELPER")),
                &directory.0,
                24,
                80,
            ),
        )
        .await
        .unwrap()
        .unwrap(),
    );
    until(|| {
        raw.read_transcript(0)
            .is_ok_and(|(_, units)| units == "ready".encode_utf16().collect::<Vec<_>>())
    })
    .await;
    let owner = owner("agent", Arc::new(Sink::default()));
    let execution = raw.clone();
    let claimed = owner
        .start(
            input("agent", Some("pty")),
            Arc::new(CompletionReport),
            move |_| Box::pin(async move { Ok(execution as Arc<dyn Execution>) }),
        )
        .await
        .unwrap();
    let first = claimed
        .wait(Some(Duration::ZERO), &NeverCancelled)
        .await
        .unwrap();
    assert_eq!(first.output, "ready".encode_utf16().collect::<Vec<_>>());
    let completed = owner
        .claim("pty")
        .await
        .unwrap()
        .write_stdin("hello 界\n", Duration::from_secs(5), &NeverCancelled)
        .await
        .unwrap();
    assert!(!completed.running);
    assert_eq!(completed.result.unwrap().stdout, "<hello 界>");
    assert!(owner.claim("pty").await.is_err());
    owner.dispose().await.unwrap();
}

#[tokio::test]
async fn owners_isolate_names_inventory_and_settlement() {
    let first = owner("agent-1", Arc::new(Sink::default()));
    let second = owner("agent-2", Arc::new(Sink::default()));
    let a = Arc::new(FakeExecution::default());
    let b = Arc::new(FakeExecution::default());
    start(&first, Some("shared"), a.clone()).await.yield_now();
    start(&second, Some("shared"), b.clone()).await.yield_now();
    start(
        &first,
        Some("first-only"),
        Arc::new(FakeExecution::default()),
    )
    .await
    .yield_now();
    assert!(second.claim("first-only").await.is_err());
    assert_eq!(
        first
            .active()
            .await
            .iter()
            .map(|item| item.id.as_str())
            .collect::<Vec<_>>(),
        ["agent-1/first-only", "agent-1/shared"]
    );
    assert_eq!(first.capture_inventory().processes.len(), 2);
    assert_eq!(second.capture_inventory().processes.len(), 1);
    first.dispose().await.unwrap();
    first.dispose().await.unwrap();
    assert!(first.capture_inventory().removed);
    assert_eq!(b.cancel_count.load(Ordering::Acquire), 0);
    assert!(!b.settled.load(Ordering::Acquire));
    assert!(
        first
            .start(
                input("agent-1", Some("late")),
                Arc::new(CompletionReport),
                |_| Box::pin(async { panic!("factory must not run") })
            )
            .await
            .is_err()
    );
    second.dispose().await.unwrap();
}

#[tokio::test]
async fn reservation_waits_for_retirement_and_old_instance_never_targets_successor() {
    let owner = owner("agent", Arc::new(Sink::default()));
    let first = Arc::new(FakeExecution::default());
    let claimed = start(&owner, Some("shared"), first.clone()).await;
    let old = Arc::clone(claimed.process());
    first.complete(0);
    until(|| old.completed()).await;
    assert!(
        owner
            .start(
                input("agent", Some("shared")),
                Arc::new(CompletionReport),
                |_| Box::pin(async { panic!("reserved name must not launch") })
            )
            .await
            .is_err()
    );
    assert!(!claimed.wait(None, &NeverCancelled).await.unwrap().running);
    let successor = Arc::new(FakeExecution::default());
    let new = start(&owner, Some("shared"), successor.clone()).await;
    assert_ne!(old.state.process_id, new.process().state.process_id);
    assert!(old.claim().is_err());
    old.request_cancel();
    assert_eq!(successor.cancel_count.load(Ordering::Acquire), 0);
    assert!(successor.signals.lock().unwrap().is_empty());
    new.yield_now();
    owner.dispose().await.unwrap();
}

#[tokio::test]
async fn generated_names_skip_retired_history_and_failed_start_consumes_counter() {
    let owner = owner("agent", Arc::new(Sink::default()));
    let first = Arc::new(FakeExecution::default());
    let claimed = start(&owner, Some("shell-1"), first.clone()).await;
    first.complete(0);
    claimed.wait(None, &NeverCancelled).await.unwrap();
    let generated = start(&owner, None, Arc::new(FakeExecution::default())).await;
    assert_eq!(generated.process().name(), "shell-2");
    generated.yield_now();
    assert!(
        owner
            .start(input("agent", None), Arc::new(CompletionReport), |_| {
                Box::pin(async { Err(io::Error::other("startup failure")) })
            })
            .await
            .is_err()
    );
    let next = start(&owner, None, Arc::new(FakeExecution::default())).await;
    assert_eq!(next.process().name(), "shell-4");
    next.yield_now();
    start(&owner, Some(""), Arc::new(FakeExecution::default()))
        .await
        .yield_now();
    start(
        &owner,
        Some(" parent/name "),
        Arc::new(FakeExecution::default()),
    )
    .await
    .yield_now();
    assert!(owner.claim("shell-3").await.is_err());
    owner.dispose().await.unwrap();
}

#[tokio::test]
async fn cancelled_or_dropped_observation_releases_claim_without_cancelling_process() {
    let owner = owner("agent", Arc::new(Sink::default()));
    let execution = Arc::new(FakeExecution::default());
    let claim = start(&owner, Some("work"), execution.clone()).await;
    assert!(owner.claim("work").await.is_err());
    let cancelled = ProcessLifetime::default();
    cancelled.cancel();
    assert_eq!(
        claim.wait(None, &cancelled).await.unwrap_err().kind(),
        io::ErrorKind::Interrupted
    );
    let claim = owner.claim("work").await.unwrap();
    assert!(
        tokio::time::timeout(Duration::from_millis(10), claim.wait(None, &NeverCancelled))
            .await
            .is_err()
    );
    drop(owner.claim("work").await.unwrap());
    assert_eq!(execution.cancel_count.load(Ordering::Acquire), 0);
    owner.dispose().await.unwrap();
}

#[tokio::test]
async fn cancelled_stdin_observer_releases_claim_without_cancelling_process() {
    let owner = owner("agent", Arc::new(Sink::default()));
    let execution = Arc::new(FakeExecution::default());
    execution.block_write.store(true, Ordering::Release);
    let claim = start(&owner, Some("work"), execution.clone()).await;
    assert!(
        tokio::time::timeout(
            Duration::from_millis(10),
            claim.write_stdin("input", Duration::ZERO, &NeverCancelled)
        )
        .await
        .is_err()
    );
    drop(owner.claim("work").await.unwrap());
    assert_eq!(execution.cancel_count.load(Ordering::Acquire), 0);
    owner.dispose().await.unwrap();
}

#[tokio::test]
async fn cursor_segments_are_committed_once_and_terminal_wait_retires() {
    let owner = owner("agent", Arc::new(Sink::default()));
    let execution = Arc::new(FakeExecution::default());
    execution.append("first");
    let claim = start(&owner, Some("work"), execution.clone()).await;
    let first = claim
        .wait(Some(Duration::ZERO), &NeverCancelled)
        .await
        .unwrap();
    assert_eq!(first.output, "first".encode_utf16().collect::<Vec<_>>());
    assert_eq!(first.format(), "first\nwork");
    execution.append("second");
    let second = owner
        .claim("work")
        .await
        .unwrap()
        .wait(Some(Duration::ZERO), &NeverCancelled)
        .await
        .unwrap();
    assert_eq!(second.output, "second".encode_utf16().collect::<Vec<_>>());
    let claim = owner.claim("work").await.unwrap();
    execution.append("final");
    execution.complete(7);
    let completed = claim.wait(None, &NeverCancelled).await.unwrap();
    assert_eq!(completed.result.unwrap().stdout, "final");
    assert!(owner.active().await.is_empty());
    assert!(owner.claim("work").await.is_err());
    owner.dispose().await.unwrap();
}

#[tokio::test]
async fn yielded_completion_is_delivered_once_with_original_prefix() {
    let sink = Arc::new(Sink::default());
    let owner = owner("agent", sink.clone());
    let execution = Arc::new(FakeExecution::default());
    execution.append("payload");
    let claim = start(&owner, Some("work"), execution.clone()).await;
    let process = Arc::clone(claim.process());
    claim.yield_now();
    execution.complete(0);
    until(|| process.retired()).await;
    let calls = sink.calls.lock().unwrap().clone();
    assert_eq!(calls.len(), 1);
    assert_eq!(
        calls[0].text,
        "Shell process 'work' completed.\nProcess exited with code 0 after 5.00s\n[stdout]\npayload"
    );
    assert_eq!(calls[0].activity_description, "process work completion");
    assert_eq!(calls[0].owner_agent_session_id, "agent");
    assert_eq!(
        calls[0].delivery,
        parrot_domain::protocol::messages::parrot::Delivery::DELIVERY_STEER
    );
    assert_eq!(
        calls[0].input_source,
        parrot_domain::protocol::messages::parrot::InputSource::INPUT_SOURCE_UNSPECIFIED
    );
    drop(calls);
    owner.dispose().await.unwrap();
    assert_eq!(sink.calls.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn failed_terminal_delivery_retries_same_message_once_then_retires_and_settles() {
    let sink = Arc::new(Sink::default());
    sink.failures.store(2, Ordering::Release);
    let owner = owner("agent", sink.clone());
    let execution = Arc::new(FakeExecution::default());
    let claim = start(&owner, Some("work"), execution.clone()).await;
    let process = Arc::clone(claim.process());
    claim.yield_now();
    execution.complete(0);
    until(|| process.retired()).await;
    assert!(owner.active().await.is_empty());
    let calls = sink.calls.lock().unwrap().clone();
    assert_eq!(calls.len(), 2);
    assert_eq!(calls[0].message_id, calls[1].message_id);
    drop(calls);
    owner.dispose().await.unwrap();
}

#[tokio::test]
async fn shutdown_starts_all_cleanup_retains_error_after_dropped_observer_and_ends_inventory() {
    let owner = owner("agent", Arc::new(Sink::default()));
    let quick = Arc::new(FakeExecution::default());
    start(&owner, Some("quick"), quick.clone())
        .await
        .yield_now();
    let blocked = Arc::new(FakeExecution::default());
    let gate = Arc::new(ProcessLifetime::default());
    *blocked.settle_gate.lock().unwrap() = Some(gate.clone());
    start(&owner, Some("blocked"), blocked.clone())
        .await
        .yield_now();
    let failed = Arc::new(FakeExecution::default());
    failed.fail_settle.store(true, Ordering::Release);
    start(&owner, Some("failed"), failed.clone())
        .await
        .yield_now();
    assert!(
        tokio::time::timeout(Duration::from_millis(20), owner.settle())
            .await
            .is_err()
    );
    until(|| quick.settled.load(Ordering::Acquire) && failed.settled.load(Ordering::Acquire)).await;
    assert!(blocked.cancel_count.load(Ordering::Acquire) > 0);
    let mut subscription = owner.subscribe_inventory();
    let disposal = owner.dispose();
    tokio::pin!(disposal);
    assert!(
        tokio::time::timeout(Duration::from_millis(10), &mut disposal)
            .await
            .is_err()
    );
    assert!(subscription.recv().await.unwrap().removed);
    assert!(subscription.recv().await.is_none());
    gate.cancel();
    assert_eq!(disposal.await.unwrap_err().to_string(), "settlement failed");
    assert!(owner.settle().await.is_err());
    for execution in [quick, blocked, failed] {
        assert_eq!(execution.settle_count.load(Ordering::Acquire), 1);
    }
}

#[tokio::test]
async fn publication_failure_cleans_started_execution_without_registering_name() {
    let owner = owner("agent", Arc::new(Sink::default()));
    let execution = Arc::new(FakeExecution::default());
    let retained = execution.clone();
    assert!(
        owner
            .start(
                input("wrong-owner", Some("work")),
                Arc::new(CompletionReport),
                move |_| Box::pin(async move { Ok(execution as Arc<dyn Execution>) })
            )
            .await
            .is_err()
    );
    assert!(owner.claim("work").await.is_err());
    assert!(retained.cancel_count.load(Ordering::Acquire) > 0);
    assert_eq!(retained.settle_count.load(Ordering::Acquire), 1);
    owner.dispose().await.unwrap();
}

#[tokio::test]
async fn explicit_scope_failure_has_no_local_fallback_and_bare_names_do_not_resolve() {
    let local = owner("local", Arc::new(Sink::default()));
    let remote = owner("remote", Arc::new(Sink::default()));
    start(&local, Some("shared"), Arc::new(FakeExecution::default()))
        .await
        .yield_now();
    start(
        &local,
        Some("child/shared"),
        Arc::new(FakeExecution::default()),
    )
    .await
    .yield_now();
    start(&remote, Some("shared"), Arc::new(FakeExecution::default()))
        .await
        .yield_now();
    drop(
        claim_address(&local, "shared", |_| {
            panic!("bare lookup must not resolve scope")
        })
        .await
        .unwrap(),
    );
    assert!(
        claim_address(&local, "child/shared", |_| Box::pin(async {
            Err(io::Error::other("explicit scope denied"))
        }))
        .await
        .is_err()
    );
    let target = remote.clone();
    let claimed = claim_address(&local, "child/shared", move |_| {
        Box::pin(async move { Ok((target, "shared".into())) })
    })
    .await
    .unwrap();
    assert_eq!(claimed.process().state.owner_agent_session_id, "remote");
    drop(claimed);
    local.dispose().await.unwrap();
    remote.dispose().await.unwrap();
}

#[tokio::test]
async fn owner_shutdown_does_not_cancel_parent_or_sibling_lifetime() {
    let parent = Arc::new(ProcessLifetime::default());
    let first = ShellProcessOwner::new(
        "first",
        Some(parent.clone()),
        Arc::new(Sink::default()),
        Arc::new(NoProcessDiagnostics),
    )
    .unwrap();
    let second = ShellProcessOwner::new(
        "second",
        Some(parent.clone()),
        Arc::new(Sink::default()),
        Arc::new(NoProcessDiagnostics),
    )
    .unwrap();
    let a = Arc::new(FakeExecution::default());
    let b = Arc::new(FakeExecution::default());
    start(&first, Some("work"), a).await.yield_now();
    start(&second, Some("work"), b.clone()).await.yield_now();
    first.dispose().await.unwrap();
    assert!(!parent.is_cancelled());
    assert_eq!(b.cancel_count.load(Ordering::Acquire), 0);
    parent.cancel();
    until(|| b.cancel_count.load(Ordering::Acquire) > 0).await;
    second.dispose().await.unwrap();
}

#[tokio::test]
async fn diagnostics_follow_completion_without_process_payloads() {
    let diagnostics = Arc::new(Diagnostics::default());
    let owner = ShellProcessOwner::new(
        "agent",
        None,
        Arc::new(Sink::default()),
        diagnostics.clone(),
    )
    .unwrap();
    let execution = Arc::new(FakeExecution::default());
    execution.append("secret-output");
    let claim = start(&owner, Some("secret-name"), execution.clone()).await;
    claim
        .wait(Some(Duration::ZERO), &NeverCancelled)
        .await
        .unwrap();
    owner
        .claim("secret-name")
        .await
        .unwrap()
        .send_signal(15, &NeverCancelled)
        .unwrap();
    execution.complete(7);
    until(|| {
        diagnostics
            .0
            .lock()
            .unwrap()
            .iter()
            .any(|event| event.operation == "completed")
    })
    .await;
    owner.dispose().await.unwrap();
    let events = diagnostics.0.lock().unwrap();
    for operation in ["start", "yield", "signal", "completed"] {
        assert!(events.iter().any(|event| event.operation == operation));
    }
    assert!(
        events
            .iter()
            .any(|event| event.operation == "completed" && event.outcome == "failed")
    );
    assert!(!format!("{events:?}").contains("secret-"));
}

#[tokio::test]
async fn faulted_completion_retains_tombstone_without_elapsed_time() {
    let owner = owner("agent", Arc::new(Sink::default()));
    let execution = Arc::new(FakeExecution::default());
    let claim = start(&owner, Some("fault"), execution.clone()).await;
    let process = Arc::clone(claim.process());
    execution.failure(io::ErrorKind::Other, "secret-error-payload");
    until(|| process.completed()).await;
    let snapshot = owner.capture_inventory();
    assert!(snapshot.processes.is_empty());
    assert_eq!(snapshot.completed_processes.len(), 1);
    assert_eq!(
        snapshot.completed_processes[0].process_id,
        process.state.process_id
    );
    assert_eq!(snapshot.completed_processes[0].elapsed_milliseconds, None);
    let mut resumed = owner.subscribe_inventory();
    assert_eq!(
        resumed.recv().await.unwrap().completed_processes,
        snapshot.completed_processes
    );
    assert_eq!(
        claim
            .wait(None, &NeverCancelled)
            .await
            .unwrap_err()
            .to_string(),
        "secret-error-payload"
    );
    owner.dispose().await.unwrap();
}

#[tokio::test]
async fn cancelled_signal_records_cancelled_without_sending_or_losing_claim() {
    let diagnostics = Arc::new(Diagnostics::default());
    let owner = ShellProcessOwner::new(
        "agent",
        None,
        Arc::new(Sink::default()),
        diagnostics.clone(),
    )
    .unwrap();
    let execution = Arc::new(FakeExecution::default());
    let claim = start(&owner, Some("secret-name"), execution.clone()).await;
    let cancellation = ProcessLifetime::default();
    cancellation.cancel();
    assert_eq!(
        claim.send_signal(15, &cancellation).unwrap_err().kind(),
        io::ErrorKind::Interrupted
    );
    assert!(execution.signals.lock().unwrap().is_empty());
    drop(owner.claim("secret-name").await.unwrap());
    assert!(
        diagnostics
            .0
            .lock()
            .unwrap()
            .iter()
            .any(|event| event.operation == "signal" && event.outcome == "cancelled")
    );
    assert!(!format!("{:?}", diagnostics.0.lock().unwrap()).contains("secret-"));
    owner.dispose().await.unwrap();
}

#[tokio::test]
async fn actual_t40_parent_cancellation_records_cancelled_without_command_payloads() {
    use crate::process::execution::{LaunchSpec, ShellProcessExecution};
    let directory = Directory::new();
    let mut spec = LaunchSpec::new("/bin/sh");
    spec.args = vec!["-c".into(), "exec sleep 30 # secret-command".into()];
    let raw = Arc::new(ShellProcessExecution::start_pipe(&spec, &directory.0).unwrap());
    let parent = Arc::new(ProcessLifetime::default());
    let diagnostics = Arc::new(Diagnostics::default());
    let owner = ShellProcessOwner::new(
        "agent",
        Some(parent.clone()),
        Arc::new(Sink::default()),
        diagnostics.clone(),
    )
    .unwrap();
    let execution = raw.clone();
    let claim = owner
        .start(
            input("agent", Some("secret-name")),
            Arc::new(CompletionReport),
            move |_| Box::pin(async move { Ok(execution as Arc<dyn Execution>) }),
        )
        .await
        .unwrap();
    let process = Arc::clone(claim.process());
    claim
        .wait(Some(Duration::ZERO), &NeverCancelled)
        .await
        .unwrap();
    parent.cancel();
    until(|| process.completed()).await;
    tokio::time::timeout(Duration::from_secs(5), owner.dispose())
        .await
        .unwrap()
        .unwrap();
    let events = diagnostics.0.lock().unwrap();
    assert!(
        events
            .iter()
            .any(|event| event.operation == "completed" && event.outcome == "cancelled")
    );
    assert!(!format!("{events:?}").contains("secret-"));
    assert!(raw.is_completed());
}

#[tokio::test]
async fn shutdown_cancels_pending_factory_and_closes_inventory_before_startup_gate() {
    let owner = owner("agent", Arc::new(Sink::default()));
    let old = Arc::new(FakeExecution::default());
    start(&owner, Some("old"), old.clone()).await.yield_now();
    let entered = Arc::new(AtomicBool::new(false));
    let observed = entered.clone();
    let startup = owner.start(
        input("agent", Some("pending")),
        Arc::new(CompletionReport),
        move |lifetime| {
            Box::pin(async move {
                observed.store(true, Ordering::Release);
                lifetime.cancelled().await;
                Err(io::Error::from(io::ErrorKind::Interrupted))
            })
        },
    );
    tokio::pin!(startup);
    tokio::select! {
        result = &mut startup => panic!("startup must be pending: {:?}", result.err()),
        () = until(|| entered.load(Ordering::Acquire)) => {},
    }
    let mut subscription = owner.subscribe_inventory();
    let disposal = owner.dispose();
    tokio::pin!(disposal);
    // Keep startup unpolled: it still owns the registry gate. Disposal must
    // nevertheless end publication and cancel the already-owned process.
    assert!(
        tokio::time::timeout(Duration::from_millis(10), &mut disposal)
            .await
            .is_err()
    );
    assert!(subscription.recv().await.unwrap().removed);
    assert!(subscription.recv().await.is_none());
    until(|| old.cancel_count.load(Ordering::Acquire) > 0).await;
    let (started, disposed) = tokio::time::timeout(Duration::from_secs(5), async {
        tokio::join!(&mut startup, &mut disposal)
    })
    .await
    .unwrap();
    assert_eq!(started.err().unwrap().kind(), io::ErrorKind::Interrupted);
    disposed.unwrap();
    assert!(old.settled.load(Ordering::Acquire));
    assert!(owner.capture_inventory().processes.is_empty());
}

#[cfg(target_os = "linux")]
#[tokio::test]
async fn shutdown_drops_real_pending_pty_readiness_and_reaps_started_helper() {
    use crate::process::execution::{LaunchSpec, ShellProcessExecution};
    use std::os::unix::fs::PermissionsExt;
    let directory = Directory::new();
    let helper = directory.0.join("stalled-helper");
    let pid_path = directory.0.join("helper-pid");
    std::fs::write(
        &helper,
        format!(
            "#!/bin/sh\nprintf '%s' \"$$\" > '{}'\nexec sleep 30\n",
            pid_path.display()
        ),
    )
    .unwrap();
    std::fs::set_permissions(&helper, std::fs::Permissions::from_mode(0o700)).unwrap();
    let owner = owner("agent", Arc::new(Sink::default()));
    let old = Arc::new(FakeExecution::default());
    start(&owner, Some("old"), old.clone()).await.yield_now();
    let blob_directory = directory.0.clone();
    let startup = owner.start(
        input("agent", Some("pending-pty")),
        Arc::new(CompletionReport),
        move |_| {
            Box::pin(async move {
                let mut spec = LaunchSpec::new("/bin/sh");
                spec.args = vec!["-c".into(), "exit 0".into()];
                let execution =
                    ShellProcessExecution::start_pty(&spec, &helper, &blob_directory, 24, 80)
                        .await?;
                Ok(Arc::new(execution) as Arc<dyn Execution>)
            })
        },
    );
    tokio::pin!(startup);
    tokio::select! {
        result = &mut startup => panic!("readiness must be pending: {:?}", result.err()),
        () = until(|| std::fs::read_to_string(&pid_path).is_ok_and(|text| text.parse::<i32>().is_ok())) => {},
    }
    let pid = std::fs::read_to_string(pid_path)
        .unwrap()
        .parse::<i32>()
        .unwrap();
    let (started, disposed) = tokio::time::timeout(Duration::from_secs(5), async {
        tokio::join!(&mut startup, owner.dispose())
    })
    .await
    .unwrap();
    assert_eq!(started.err().unwrap().kind(), io::ErrorKind::Interrupted);
    disposed.unwrap();
    assert!(owner.capture_inventory().removed);
    assert!(old.cancel_count.load(Ordering::Acquire) > 0);
    assert!(old.settled.load(Ordering::Acquire));
    assert_eq!(
        unsafe { libc::waitpid(pid, std::ptr::null_mut(), libc::WNOHANG) },
        -1
    );
    assert_eq!(
        io::Error::last_os_error().raw_os_error(),
        Some(libc::ECHILD)
    );
    assert_eq!(unsafe { libc::kill(pid, 0) }, -1);
    assert_eq!(io::Error::last_os_error().raw_os_error(), Some(libc::ESRCH));
}

#[tokio::test]
async fn retired_near_limit_pipes_release_execution_despite_retained_name_history() {
    let owner = owner("agent", Arc::new(Sink::default()));
    let mut history = Vec::new();
    for _ in 0..16 {
        let execution = Arc::new(FakeExecution::default());
        execution.pipe.store(true, Ordering::Release);
        execution.append(&"x".repeat(64_000));
        let weak = Arc::downgrade(&execution);
        let claim = start(&owner, Some("reused"), execution.clone()).await;
        history.push(Arc::clone(claim.process()));
        execution.complete(0);
        drop(execution);
        let result = claim.wait(None, &NeverCancelled).await.unwrap();
        assert_eq!(result.result.as_ref().unwrap().stdout.len(), 64_000);
        drop(result);
        until(|| weak.upgrade().is_none()).await;
    }
    assert_eq!(owner.registry.lock().await.owned.len(), 16);
    assert!(
        history
            .iter()
            .all(|process| process.completed() && process.retired())
    );
    assert!(owner.active().await.is_empty());
    owner.dispose().await.unwrap();
}

#[tokio::test]
async fn actual_retired_pipe_releases_cached_output_while_owner_retains_metadata() {
    use crate::process::execution::{LaunchSpec, ShellProcessExecution};
    let directory = Directory::new();
    let mut spec = LaunchSpec::new("/bin/sh");
    spec.args = vec!["-c".into(), "printf '%064000d' 0".into()];
    let execution = Arc::new(ShellProcessExecution::start_pipe(&spec, &directory.0).unwrap());
    let weak = Arc::downgrade(&execution);
    let owner = owner("agent", Arc::new(Sink::default()));
    let claim = owner
        .start(
            input("agent", Some("large-pipe")),
            Arc::new(CompletionReport),
            move |_| Box::pin(async move { Ok(execution as Arc<dyn Execution>) }),
        )
        .await
        .unwrap();
    let process = Arc::clone(claim.process());
    let result = tokio::time::timeout(Duration::from_secs(5), claim.wait(None, &NeverCancelled))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(result.result.as_ref().unwrap().stdout.len(), 64_000);
    assert!(result.result.as_ref().unwrap().blob_path.is_none());
    drop(result);
    until(|| weak.upgrade().is_none()).await;
    assert!(process.retired() && process.completed());
    assert_eq!(owner.registry.lock().await.owned.len(), 1);
    owner.dispose().await.unwrap();
}
