use super::*;

use parrot_domain::protocol::messages::parrot::{Delivery, InputSource};
use parrot_domain::values::identifiers::Identifier;

use crate::process::execution::{LaunchSpec, ShellProcessExecution};
use crate::process::ownership::managed::{CompletionMessage, CompletionSink, NoProcessDiagnostics};
use crate::process::ownership::monitor::{
    MonitorDelivery, MonitorEvent, MonitorInputSource, MonitorReason,
};
use crate::process::ownership::owner::{ProcessAgent, ProcessStart, ShellProcessOwner};

#[derive(Default)]
struct RecordingSink {
    messages: Mutex<Vec<String>>,
    events: Mutex<Vec<MonitorEvent>>,
    completed: Mutex<Vec<CompletionMessage>>,
    failures: Mutex<Vec<String>>,
}

impl MonitorSink for RecordingSink {
    fn render_event(&self, identity: &MonitorIdentity, lines: &str) -> io::Result<String> {
        Ok(format!(
            "Monitor {} ({}):\n{lines}",
            identity.name, identity.description
        ))
    }

    fn persist_event<'a>(
        &'a self,
        _: String,
        _: &'a dyn Cancellation,
    ) -> BoxFuture<'a, io::Result<String>> {
        Box::pin(async { Err(io::Error::other("unexpected oversized event")) })
    }

    fn send_event<'a>(
        &'a self,
        event: MonitorEvent,
        lifetime: &'a dyn Cancellation,
    ) -> BoxFuture<'a, io::Result<()>> {
        Box::pin(async move {
            lifetime
                .check()
                .map_err(|error| io::Error::new(io::ErrorKind::Interrupted, error))?;
            self.messages.lock().unwrap().push(event.text.clone());
            self.events.lock().unwrap().push(event);
            Ok(())
        })
    }

    fn observe_failed(&self, _: &MonitorIdentity, failure: &io::Error) {
        self.failures.lock().unwrap().push(failure.to_string());
    }

    fn render_completion(
        &self,
        identity: &MonitorIdentity,
        report: &MonitorReport,
    ) -> io::Result<String> {
        assert_eq!(report.reason, MonitorReason::Exited);
        Ok(format!(
            "Monitor {} ({}) exited with code {}. Events delivered: {}.",
            identity.name, identity.description, report.exit_code, report.events
        ))
    }
}

impl CompletionSink for RecordingSink {
    fn send<'a>(
        &'a self,
        message: &'a CompletionMessage,
        cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, io::Result<()>> {
        Box::pin(async move {
            cancellation
                .check()
                .map_err(|error| io::Error::new(io::ErrorKind::Interrupted, error))?;
            self.messages.lock().unwrap().push(message.text.clone());
            self.completed.lock().unwrap().push(message.clone());
            Ok(())
        })
    }
}

#[tokio::test]
async fn actual_owner_monitor_blocks_turn_until_terminal_delivery() {
    let directory = std::env::temp_dir().join(format!(
        "parrot-owner-monitor-{}",
        Identifier::event_id().unwrap()
    ));
    let sink = Arc::new(RecordingSink::default());
    let owner = ShellProcessOwner::new(
        "agent-id",
        None,
        sink.clone(),
        Arc::new(NoProcessDiagnostics),
    )
    .unwrap();
    let input = ProcessStart {
        requested_name: Some("watch".into()),
        command: "printf 'a\\nb\\n'; sleep 1; echo c; echo hidden >&2; exit 3".into(),
        description: "ticks".into(),
        origin_tool_call_id: "monitor-call".into(),
        agent: ProcessAgent {
            session_id: "agent-id".into(),
            name: "main".into(),
            parent_session_id: String::new(),
            parent_name: String::new(),
            depth: 0,
        },
    };
    let mut spec = LaunchSpec::new("/bin/sh");
    spec.args = vec!["-c".into(), input.command.clone().into()];
    let blob_directory = directory.clone();
    let claimed = owner
        .start(
            input,
            Arc::new(MonitorProcessReport::new(
                Duration::from_millis(300_000),
                sink.clone(),
            )),
            move |_| {
                Box::pin(async move {
                    let execution = ShellProcessExecution::start_pipe(&spec, blob_directory)?;
                    Ok(Arc::new(execution) as Arc<dyn Execution>)
                })
            },
        )
        .await
        .unwrap();
    let process = claimed.process().clone();
    let yielded = claimed.yield_now();
    assert_eq!(yielded.activity_kind, ActivityKind::Monitor);
    assert!(yielded.stdout_path.as_ref().unwrap().is_absolute());
    assert!(yielded.stderr_path.as_ref().unwrap().is_absolute());
    let active = owner.active().await;
    assert_eq!(active.len(), 1);
    assert_eq!(active[0].id, "agent-id/watch");
    assert_eq!(active[0].name, "watch");
    assert_eq!(owner.capture_inventory().processes.len(), 1);

    let completed = tokio::time::timeout(Duration::from_secs(10), async {
        while !process.retired() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await;
    let active_after_completion = owner.active().await;
    // Always join the actual child, report, delivery and inventory lifetimes.
    let disposed = owner.dispose().await;
    completed.unwrap();
    disposed.unwrap();
    assert!(active_after_completion.is_empty());

    assert_eq!(
        *sink.messages.lock().unwrap(),
        [
            "Monitor watch (ticks):\na\nb",
            "Monitor watch (ticks):\nc",
            "Monitor watch (ticks) exited with code 3. Events delivered: 3."
        ]
    );
    let events = sink.events.lock().unwrap();
    assert_eq!(events.len(), 2);
    assert!(events.iter().all(|event| {
        event.source == MonitorInputSource::Monitor
            && event.delivery == MonitorDelivery::Steer
            && event.activity_name == "watch"
            && event.activity_description == "monitor watch event"
    }));
    let completed = sink.completed.lock().unwrap();
    assert_eq!(completed.len(), 1);
    assert_eq!(completed[0].delivery, Delivery::DELIVERY_STEER);
    assert_eq!(
        completed[0].input_source,
        InputSource::INPUT_SOURCE_UNSPECIFIED
    );
    assert!(sink.failures.lock().unwrap().is_empty());
    assert!(owner.capture_inventory().removed);
    assert!(owner.capture_inventory().processes.is_empty());
    std::fs::remove_dir_all(directory).unwrap();
}
