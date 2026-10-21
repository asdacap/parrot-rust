//! Real scoped process-owner and production filename wiring checks.

use std::{
    fs, io,
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering},
    },
    time::Duration,
};

use parrot_domain::contracts::{
    async_ops::{BoxFuture, Cancellation},
    paths::{RootOrigin, SelectedSessionRoot},
    status::{ActiveWorkState, ProcessStatusSource},
};
use parrot_host::composition::processes::{ScopedProcessStatus, production_filename_generator};
use parrot_runtime::active_work::blockers::{ActiveWorkBlocker, ProcessBlocker};
use parrot_storage::artifacts::output_blobs::ToolOutputBlobStore;
use parrot_tools::process::ownership::{
    execution::{Execution, NeverCancelled, ProcessResult},
    inventory::{ActiveShellProcessState, ActivityKind},
    managed::{
        ClaimedProcess, CompletionMessage, CompletionReport, CompletionSink, NoProcessDiagnostics,
        ProcessReport,
    },
    owner::{ProcessAgent, ProcessStart, ShellProcessOwner},
};
use tokio::sync::Notify;

#[test]
fn production_filenames_feed_the_domain_injected_storage_consumer() {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let directory = std::env::temp_dir().join(format!(
        "t62-production-filenames-{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ));
    fs::create_dir_all(&directory).unwrap();
    let root = SelectedSessionRoot::new(directory, RootOrigin::Current).unwrap();
    let names = production_filename_generator();
    let store = ToolOutputBlobStore::for_session(&root, "blobs", names).unwrap();
    let content = "Synthetic T62 output.\n".repeat(4096);
    let first = store.persist(&content, &NeverCancelled).unwrap();
    let second = store.persist(&content, &NeverCancelled).unwrap();
    assert_ne!(
        first, second,
        "the storage consumer retains exclusive creation"
    );
    let files: Vec<_> = fs::read_dir(store.directory())
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .collect();
    assert_eq!(files.len(), 2);
    for path in files {
        let filename = path.file_name().unwrap().to_str().unwrap();
        let words: Vec<_> = filename.split('-').collect();
        assert_eq!(words.len(), 3, "no random token is added: {filename}");
        assert!(!words[0].is_empty());
        assert!(!words[1].is_empty());
        assert_eq!(words[2], "arse.dat");
        assert_eq!(fs::read_to_string(&path).unwrap(), content);
        assert!(first.contains(path.to_str().unwrap()) || second.contains(path.to_str().unwrap()));
    }
}

/// Only native execution is controlled. The registry, claims, delivery,
/// retirement, scope lifetime and observer are the real production components.
#[derive(Default)]
struct ControlledExecution {
    complete: AtomicBool,
    settled: AtomicUsize,
    changed: Notify,
}

impl ControlledExecution {
    fn complete(&self) {
        self.complete.store(true, Ordering::Release);
        self.changed.notify_waiters();
    }

    fn result() -> ProcessResult {
        ProcessResult {
            exit_code: 0,
            elapsed_milliseconds: 1,
            stdout: "synthetic output".into(),
            stderr: String::new(),
            blob_path: None,
        }
    }
}

impl Execution for ControlledExecution {
    fn is_completed(&self) -> bool {
        self.complete.load(Ordering::Acquire)
    }
    fn is_pseudo_terminal(&self) -> bool {
        false
    }
    fn stdout_path(&self) -> Option<PathBuf> {
        None
    }
    fn stderr_path(&self) -> Option<PathBuf> {
        None
    }
    fn read_transcript(&self, offset: u64) -> io::Result<(u64, Vec<u16>)> {
        Ok((offset, Vec::new()))
    }
    fn read_result(&self, offset: u64) -> io::Result<(u64, ProcessResult)> {
        if !self.is_completed() {
            return Err(io::ErrorKind::WouldBlock.into());
        }
        Ok((offset, Self::result()))
    }
    fn wait(&self) -> BoxFuture<'_, io::Result<ProcessResult>> {
        Box::pin(async move {
            loop {
                let changed = self.changed.notified();
                tokio::pin!(changed);
                changed.as_mut().enable();
                if self.is_completed() {
                    return Ok(Self::result());
                }
                changed.await;
            }
        })
    }
    fn write_stdin<'a>(&'a self, _: &'a str) -> BoxFuture<'a, io::Result<()>> {
        Box::pin(async { Ok(()) })
    }
    fn send_signal(&self, _: i32) -> io::Result<()> {
        Ok(())
    }
    fn request_cancel(&self) {
        self.complete();
    }
    fn settle(&self) -> BoxFuture<'_, io::Result<()>> {
        Box::pin(async move {
            self.settled.fetch_add(1, Ordering::AcqRel);
            Ok(())
        })
    }
}

#[derive(Default)]
struct Sink(AtomicUsize);
impl CompletionSink for Sink {
    fn send<'a>(
        &'a self,
        _: &'a CompletionMessage,
        _: &'a dyn Cancellation,
    ) -> BoxFuture<'a, io::Result<()>> {
        Box::pin(async move {
            self.0.fetch_add(1, Ordering::AcqRel);
            Ok(())
        })
    }
}

struct NonBlockingReport;
impl ProcessReport for NonBlockingReport {
    fn blocks_turn(&self) -> bool {
        false
    }
    // Deliberately the same activity as a blocking report: never infer grants
    // or turn blocking from an inventory activity kind.
    fn activity_kind(&self) -> ActivityKind {
        ActivityKind::Execution
    }
    fn observe<'a>(
        &'a self,
        _: &'a ActiveShellProcessState,
        _: Arc<dyn Execution>,
        _: &'a dyn Cancellation,
    ) -> BoxFuture<'a, io::Result<()>> {
        Box::pin(async { Ok(()) })
    }
    fn complete(&self, _: &ActiveShellProcessState, output: &str) -> io::Result<String> {
        Ok(output.into())
    }
}

fn owner(id: &str) -> (Arc<ShellProcessOwner>, Arc<Sink>) {
    let sink = Arc::new(Sink::default());
    let owner = Arc::new(
        ShellProcessOwner::new(id, None, sink.clone(), Arc::new(NoProcessDiagnostics)).unwrap(),
    );
    (owner, sink)
}

async fn start(
    owner: &ShellProcessOwner,
    name: &str,
    report: Arc<dyn ProcessReport>,
) -> (ClaimedProcess, Arc<ControlledExecution>) {
    let execution = Arc::new(ControlledExecution::default());
    let launched = execution.clone();
    let claim = owner
        .start(
            ProcessStart {
                requested_name: Some(name.into()),
                command: "synthetic command".into(),
                description: "synthetic description".into(),
                origin_tool_call_id: "synthetic call".into(),
                agent: ProcessAgent {
                    session_id: owner.session_id().into(),
                    name: "agent".into(),
                    parent_session_id: String::new(),
                    parent_name: String::new(),
                    depth: 0,
                },
            },
            report,
            move |_| Box::pin(async move { Ok(launched as Arc<dyn Execution>) }),
        )
        .await
        .unwrap();
    (claim, execution)
}

#[tokio::test]
async fn real_owner_observer_preserves_blocking_retirement_order_and_scope() {
    let (owner, sink) = owner("scope/owner");
    let observer = Arc::new(ScopedProcessStatus::new(owner.clone()));
    assert!(Arc::ptr_eq(observer.owner(), &owner));
    let mut claims = Vec::new();
    for name in ["\u{e000}", "😀", "a"] {
        claims.push(start(&owner, name, Arc::new(CompletionReport)).await);
    }
    let nonblocking = start(&owner, "nonblocking", Arc::new(NonBlockingReport)).await;
    let (foreign_owner, _) = self::owner("foreign");
    let foreign = start(
        &foreign_owner,
        "foreign-process",
        Arc::new(CompletionReport),
    )
    .await;

    let active = observer.active();
    assert_eq!(
        active
            .iter()
            .map(|item| item.name.as_str())
            .collect::<Vec<_>>(),
        ["a", "😀", "\u{e000}"]
    );
    assert!(
        active
            .iter()
            .all(|item| item.id == format!("scope/owner/{}", item.name)
                && item.state == ActiveWorkState::Running)
    );
    let snapshot = observer.snapshot();
    assert_eq!(
        snapshot.len(),
        4,
        "nonblocking work is still in the snapshot"
    );
    assert!(
        snapshot
            .iter()
            .all(|item| item.owner_session_id == "scope/owner"
                && item.process_id.starts_with("shell-process-")
                && item.state == ActiveWorkState::Running
                && item.id() == format!("scope/owner/{}", item.name))
    );
    assert!(snapshot.windows(2).all(|pair| {
        pair[0]
            .process_id
            .encode_utf16()
            .cmp(pair[1].process_id.encode_utf16())
            .is_le()
    }));
    assert_eq!(observer.active().len(), 3);
    assert_eq!(
        observer.snapshot(),
        snapshot,
        "observation does not claim or settle work"
    );
    let blocker = ProcessBlocker {
        processes: observer.clone(),
    };
    let section = blocker
        .observe()
        .await
        .unwrap()
        .unwrap()
        .work_section
        .unwrap();
    assert!(section.contains("scope/owner/a"));
    assert!(!section.contains("nonblocking"));
    assert!(!section.contains("foreign-process"));
    assert_eq!(sink.0.load(Ordering::Acquire), 0);

    for (claim, execution) in claims {
        execution.complete();
        claim.wait(None, &NeverCancelled).await.unwrap();
    }
    assert!(
        observer.active().is_empty(),
        "retired blocking work is excluded"
    );
    assert_eq!(observer.snapshot().len(), 1);
    assert!(blocker.observe().await.unwrap().is_none());
    nonblocking.1.complete();
    nonblocking.0.wait(None, &NeverCancelled).await.unwrap();
    assert!(observer.snapshot().is_empty());
    foreign.1.complete();
    foreign.0.wait(None, &NeverCancelled).await.unwrap();
    owner.dispose().await.unwrap();
    foreign_owner.dispose().await.unwrap();
}

#[tokio::test]
async fn completed_undelivered_work_remains_running_until_actual_delivery_and_name_reuse() {
    let (owner, sink) = owner("owner");
    let observer = ScopedProcessStatus::new(owner.clone());
    let (claim, execution) = start(&owner, "work", Arc::new(CompletionReport)).await;
    let before = observer.snapshot();
    execution.complete();
    tokio::time::timeout(Duration::from_secs(2), async {
        while !claim.process().completed() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("actual managed observer records completion");
    assert_eq!(observer.snapshot(), before);
    assert_eq!(observer.active().len(), 1);
    assert_eq!(
        sink.0.load(Ordering::Acquire),
        0,
        "initial claim retains delivery ownership"
    );
    claim.wait(None, &NeverCancelled).await.unwrap();
    assert!(observer.snapshot().is_empty());
    assert!(observer.active().is_empty());
    let (replacement, replacement_execution) =
        start(&owner, "work", Arc::new(CompletionReport)).await;
    let after = observer.snapshot();
    assert_eq!(after.len(), 1);
    assert_ne!(after[0].process_id, before[0].process_id);
    assert_eq!(after[0].id(), "owner/work");
    replacement_execution.complete();
    replacement.wait(None, &NeverCancelled).await.unwrap();
    owner.dispose().await.unwrap();
    assert!(execution.settled.load(Ordering::Acquire) > 0);
    assert!(replacement_execution.settled.load(Ordering::Acquire) > 0);
}
