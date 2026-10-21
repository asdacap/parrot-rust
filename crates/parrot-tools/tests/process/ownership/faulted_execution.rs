//! The original owner fault fixture uses output I/O failure, not permission approval.

use std::io;
use std::sync::Arc;
use std::time::Duration;

use parrot_domain::contracts::async_ops::{BoxFuture, Cancellation};
use parrot_domain::values::identifiers::Identifier;

use crate::process::execution::{LaunchSpec, ShellProcessExecution};
use crate::process::ownership::execution::{Execution, NeverCancelled};
use crate::process::ownership::managed::{
    CompletionMessage, CompletionReport, CompletionSink, NoProcessDiagnostics,
};
use crate::process::ownership::owner::{ProcessAgent, ProcessStart, ShellProcessOwner};

struct Sink;
impl CompletionSink for Sink {
    fn send<'a>(
        &'a self,
        _: &'a CompletionMessage,
        _: &'a dyn Cancellation,
    ) -> BoxFuture<'a, io::Result<()>> {
        Box::pin(async { Ok(()) })
    }
}

#[tokio::test]
async fn original_blob_directory_fault_keeps_real_error_context_and_elapsedless_tombstone() {
    let directory = std::env::temp_dir().join(format!(
        "parrot-owner-fault-{}",
        Identifier::event_id().unwrap()
    ));
    std::fs::create_dir(&directory).unwrap();
    let blobs = directory.join("blobs");
    let marker = directory.join("fault");
    let command = format!(
        "while [ ! -f '{}' ]; do sleep 0.01; done; awk 'BEGIN {{ for (i = 0; i < 1000000; i++) printf \"x\" }}'",
        marker.display()
    );
    let owner = ShellProcessOwner::new(
        "agent-id",
        None,
        Arc::new(Sink),
        Arc::new(NoProcessDiagnostics),
    )
    .unwrap();
    let input = ProcessStart {
        requested_name: Some("faulted".into()),
        command: command.clone(),
        description: String::new(),
        origin_tool_call_id: "call-id".into(),
        agent: ProcessAgent {
            session_id: "agent-id".into(),
            name: "main".into(),
            parent_session_id: String::new(),
            parent_name: String::new(),
            depth: 0,
        },
    };
    let mut spec = LaunchSpec::new("/bin/sh");
    spec.args = vec!["-c".into(), command.into()];
    let output_directory = blobs.clone();
    let claimed = owner
        .start(input, Arc::new(CompletionReport), move |_| {
            Box::pin(async move {
                Ok(
                    Arc::new(ShellProcessExecution::start_pipe(&spec, output_directory)?)
                        as Arc<dyn Execution>,
                )
            })
        })
        .await
        .unwrap();
    let process_id = claimed.process().state.process_id.clone();
    let mut subscription = owner.subscribe_inventory();
    assert_eq!(subscription.recv().await.unwrap().processes.len(), 1);
    std::fs::rename(&blobs, directory.join("blobs-moved")).unwrap();
    std::fs::write(&blobs, []).unwrap();
    std::fs::write(marker, []).unwrap();

    let waited =
        tokio::time::timeout(Duration::from_secs(10), claimed.wait(None, &NeverCancelled)).await;
    let completed = subscription.recv().await.unwrap();
    let resumed = owner.capture_inventory();
    let mut reconnected = owner.subscribe_inventory();
    let replayed = reconnected.recv().await.unwrap();
    let settled = owner.dispose().await;
    let failure = waited.unwrap().unwrap_err();
    let cleanup = std::fs::remove_dir_all(directory);
    eprintln!("fault result: {failure}; settlement: {settled:?}");
    assert!(
        failure.to_string().contains("blob"),
        "original predicate requires blob context; actual failure: {failure}"
    );
    assert!(completed.processes.is_empty());
    assert_eq!(completed.completed_processes.len(), 1);
    assert_eq!(completed.completed_processes[0].process_id, process_id);
    assert_eq!(completed.completed_processes[0].elapsed_milliseconds, None);
    assert_eq!(resumed.completed_processes, completed.completed_processes);
    assert!(replayed.processes.is_empty());
    assert_eq!(replayed.completed_processes, completed.completed_processes);
    // Source AwaitCompletionForDisposal ignores an already-observable result
    // failure after cleanup; the original owner test then awaits Settle normally.
    settled.unwrap();
    cleanup.unwrap();
}
