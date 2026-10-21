use super::{fixture::*, harness::Harness, support::*};
use parrot_domain::{
    protocol::messages::parrot::AgentTaskProgressSnapshot, values::agent_tasks::ExecutionStatus::*,
};
use parrot_runtime::task_scheduler::PROGRESS_KIND;
use protobuf::Message;
use std::sync::atomic::Ordering;

#[tokio::test]
async fn success_wakes_owner_before_graph_completes_initial_update_only_records_and_retry_keeps_id_order()
 {
    let h = Harness::new();
    let root = h.root("owner").await;
    let first = Gate::new();
    let second = Gate::new();
    h.provider.script("a", [Reply::held("answer", &first)]);
    h.provider.script("b", [Reply::held("answer", &second)]);
    let owner = h.factory.get("owner");
    owner.task_session.send_failures.store(2, Ordering::SeqCst);
    set(&root, vec![task("a"), dependent("b", "a")]).await;
    first.entered().await;
    owner
        .task_session
        .wait_calls(|calls| {
            calls.iter().any(|call| {
                call.kind == "record"
                    && call
                        .text
                        .contains("AgentTask states changed by your update:")
            })
        })
        .await;
    assert!(requests(&h.provider, "owner").is_empty());
    first.open();
    second.entered().await;
    owner
        .task_session
        .wait_calls(|calls| {
            calls
                .iter()
                .filter(|call| call.kind == "send" && call.text.contains("AgentTask succeeded: a"))
                .count()
                == 3
        })
        .await;
    h.provider.wait_requests("owner", 1).await;
    let awake = requests(&h.provider, "owner");
    assert!(
        conversation(&awake[0])
            .contains("AgentTask succeeded: a\nDescription: Do a\n\nResult:\nanswer")
    );
    assert!(!conversation(&awake[0]).contains("All AgentTasks are complete:"));
    assert_eq!(
        snapshot(&root)
            .await
            .iter()
            .map(|task| task.state)
            .collect::<Vec<_>>(),
        [Succeeded, Running]
    );
    second.open();
    complete(&root).await;
    owner
        .task_session
        .wait_calls(|calls| {
            calls.iter().any(|call| {
                call.kind == "send" && call.text.contains("All AgentTasks are complete:")
            })
        })
        .await;
    let calls = owner.task_session.calls.lock().unwrap().clone();
    let a: Vec<_> = calls
        .iter()
        .filter(|call| call.kind == "send" && call.text.contains("AgentTask succeeded: a"))
        .collect();
    assert_eq!(a.len(), 3);
    assert!(
        a.iter()
            .all(|call| call.id == a[0].id && call.text == a[0].text)
    );
    let sends: Vec<_> = calls.iter().filter(|call| call.kind == "send").collect();
    assert!(sends[3].text.contains("AgentTask succeeded: b"));
    assert!(sends[4].text.contains("All AgentTasks are complete:"));
    h.close().await;
}

#[tokio::test]
async fn settling_drains_pending_notifications_as_records_without_waking() {
    let h = Harness::new();
    let root = h.root("owner").await;
    let gate = Gate::new();
    h.provider.script("a", [Reply::held("answer", &gate)]);
    set(
        &root,
        vec![task("a"), dependent("b", "a"), state("failed", Failed)],
    )
    .await;
    gate.entered().await;
    let owner = h.factory.get("owner");
    bounded(owner.scheduler.settle().join()).await.unwrap();
    assert!(
        snapshot(&root)
            .await
            .iter()
            .all(|task| task.state == Canceled)
    );
    assert!(
        !owner
            .task_session
            .calls
            .lock()
            .unwrap()
            .iter()
            .any(|call| call.kind == "send")
    );
    assert!(requests(&h.provider, "owner").is_empty());
    assert_eq!(
        h.factory
            .get("a")
            .task_session
            .calls
            .lock()
            .unwrap()
            .iter()
            .filter(|call| call.kind == "record"
                && call.text.contains("the agent session is shutting down."))
            .count(),
        1
    );
    h.close().await;
}

#[tokio::test]
async fn oversized_owner_notification_persists_actual_bytes_once_before_same_id_retry() {
    let h = Harness::new();
    let root = h.root("owner").await;
    let owner = h.factory.get("owner");
    owner.task_session.send_failures.store(2, Ordering::SeqCst);
    let tasks: Vec<_> = (0..900)
        .map(|index| state(&format!("task-{index:04}-{}", "x".repeat(70)), Canceled))
        .collect();
    use std::fmt::Write as _;
    let mut items = String::new();
    for task in &tasks {
        write!(items, "\n- {}: canceled", task.name).unwrap();
    }
    let expected_update = format!("AgentTask states changed by your update:{items}");
    let expected_complete = format!("All AgentTasks are complete:{items}");
    assert!(expected_update.len() > parrot_storage::artifacts::output_blobs::MAXIMUM_INLINE_BYTES);
    set(&root, tasks).await;
    owner
        .task_session
        .wait_calls(|calls| calls.iter().filter(|call| call.kind == "send").count() == 3)
        .await;
    let calls = owner.task_session.calls.lock().unwrap().clone();
    let sends: Vec<_> = calls.iter().filter(|call| call.kind == "send").collect();
    assert_eq!(sends.len(), 3);
    assert!(
        sends
            .iter()
            .all(|call| call.id == sends[0].id && call.text == sends[0].text)
    );
    assert!(
        sends[0]
            .text
            .starts_with("Tool output exceeded 64 KiB and was saved to ")
    );
    let record = calls.iter().find(|call| call.kind == "record").unwrap();
    let path = |text: &str| {
        std::path::PathBuf::from(
            text.strip_prefix("Tool output exceeded 64 KiB and was saved to ")
                .unwrap()
                .strip_suffix('.')
                .unwrap(),
        )
    };
    assert_eq!(
        std::fs::read_to_string(path(&record.text)).unwrap(),
        expected_update
    );
    assert_eq!(
        std::fs::read_to_string(path(&sends[0].text)).unwrap(),
        expected_complete
    );
    assert_eq!(
        std::fs::read_dir(h.histories.path.join("scratch/blobs"))
            .unwrap()
            .count(),
        2
    );
    h.close().await;
}

#[tokio::test]
async fn progress_revisions_are_durable_replayable_and_diagnostics_contain_only_bounded_fields() {
    let h = Harness::new();
    let root = h.root("owner").await;
    let gate = Gate::new();
    h.provider
        .script("private-task", [Reply::held("answer", &gate)]);
    set(&root, vec![task("private-task")]).await;
    gate.entered().await;
    gate.open();
    complete(&root).await;
    let events = h.histories.repository.events().replay().await.unwrap();
    let progress: Vec<_> = events
        .iter()
        .filter(|event| event.has_tool_snapshot())
        .map(|event| event.tool_snapshot())
        .filter(|snapshot| {
            snapshot.kind == PROGRESS_KIND
                && snapshot.owner_agent_session_id == root.identity().session.session_id
        })
        .collect();
    assert!(progress.len() >= 2);
    for (index, snapshot) in progress.iter().enumerate() {
        assert_eq!(snapshot.revision, index as u64 + 1);
        assert!(snapshot.final_chunk);
        let decoded = AgentTaskProgressSnapshot::parse_from_bytes(&snapshot.data).unwrap();
        assert_eq!(decoded.revision, snapshot.revision);
        assert_eq!(decoded.root_nodes.len(), 1);
        assert_eq!(decoded.root_nodes[0].name, "private-task");
        assert_eq!(
            decoded.root_nodes[0].agent_session_id,
            h.factory.get("private-task").history.agent_session_id()
        );
    }
    let last = AgentTaskProgressSnapshot::parse_from_bytes(&progress.last().unwrap().data).unwrap();
    assert!(last.root_nodes[0].hidden);
    let raw = h.histories.repository.events().replay_raw().await.unwrap();
    assert!(raw.len() >= events.len());
    for (event, stored) in events.iter().zip(&raw) {
        assert_eq!(event, &stored.event);
        assert_eq!(
            event,
            &parrot_domain::protocol::messages::parrot::Event::parse_from_bytes(&stored.raw)
                .unwrap()
        );
    }
    {
        let diagnostics = h.factory.task_diagnostics.0.lock().unwrap();
        assert_eq!(
            diagnostics
                .iter()
                .filter(|entry| entry.operation == "running")
                .count(),
            1
        );
        assert_eq!(
            diagnostics
                .iter()
                .filter(|entry| entry.operation == "terminal")
                .count(),
            1
        );
        for entry in diagnostics.iter() {
            assert_eq!(entry.session_id, root.identity().session.session_id);
            assert!(entry.correlation_id.starts_with("task-"));
            assert!(!entry.correlation_id.contains("PRIVATE"));
            assert!(matches!(entry.outcome, "running" | "succeeded"));
            assert!(entry.duration_milliseconds.is_some());
        }
    }
    h.close().await;
}
