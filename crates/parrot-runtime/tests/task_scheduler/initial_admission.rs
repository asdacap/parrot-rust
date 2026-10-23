use super::{fixture::*, harness::Harness, support::*};
use parrot_domain::values::agent_tasks::{ExecutionStatus::Running, Task};
use parrot_runtime::children::Operation;
use parrot_storage::conversation::HistoryForkBoundary;
use std::{
    future::Future,
    sync::{Arc, Mutex},
    task::{Context, Poll, Wake, Waker},
};

struct NoWake;
impl Wake for NoWake {
    fn wake(self: Arc<Self>) {}
}

#[tokio::test]
async fn replacement_initial_enqueue_follows_durable_generation_without_holding_turn_gate() {
    let h = Harness::new();
    let owner = h.root("owner").await;
    let old_provider = Gate::new();
    let old_stop_record = Gate::new();
    let new_provider = Gate::new();
    h.provider.script(
        "a",
        [
            Reply::held("answer", &old_provider),
            Reply::held("answer", &new_provider),
        ],
    );
    set(&owner, vec![task("a")]).await;
    old_provider.entered().await;
    let worker = h.factory.get("a");
    *worker.task_session.record_gate.lock().unwrap() = Some(old_stop_record.clone());
    let mut replacement = task("a");
    replacement.acceptance_criteria = "replacement acceptance".into();
    set(&owner, vec![replacement.clone()]).await;
    old_stop_record.entered().await;
    // The replacement update's FIFO admission tail is complete. Its actual execution
    // is deterministically parked behind the old generation's stop record instead.
    assert_eq!(snapshot(&owner).await[0].state, Running);
    let probes = Arc::new(Mutex::new(None));
    let contending: Arc<Mutex<Option<Operation<()>>>> = Arc::new(Mutex::new(None));
    let scheduler = owner.get_service::<Scheduler>().unwrap();
    let selection = owner.selection().current().unwrap();
    let (observed_probes, observed_update) = (probes.clone(), contending.clone());
    let snapshot_path = h.histories.path.join("root-agents/owner/agent-state.json");
    let expected = owner
        .agent_state()
        .load()
        .await
        .unwrap()
        .unwrap()
        .tasks
        .entries[0]
        .runner
        .clone()
        .unwrap();
    *worker.task_session.enqueue_hook.lock().unwrap() = Some(Box::new(move || {
        let saved: parrot_domain::values::agent_state::AgentStateSnapshot =
            serde_json::from_slice(&std::fs::read(&snapshot_path).unwrap()).unwrap();
        let checkpoint = saved.tasks.entries[0].runner.as_ref().unwrap();
        assert_eq!(checkpoint.generation_id, expected.generation_id);
        assert_eq!(checkpoint.prompt_message_id, expected.prompt_message_id);
        assert_eq!(
            checkpoint.current_task.acceptance_criteria,
            "replacement acceptance"
        );
        let waker = Waker::from(Arc::new(NoWake));
        let mut context = Context::from_waker(&waker);
        let mut snapshot = Box::pin(scheduler.snapshot());
        let snapshot_pending = matches!(snapshot.as_mut().poll(&mut context), Poll::Pending);
        let mut detail = Box::pin(scheduler.capture_detail("a"));
        let detail_pending = matches!(detail.as_mut().poll(&mut context), Poll::Pending);
        *observed_probes.lock().unwrap() = Some((snapshot_pending, detail_pending));
        // Admit a real contending visibility-only update before delegating enqueue.
        // Its matching generation keeps the admitted prompt while visibility changes.
        *observed_update.lock().unwrap() = Some(
            scheduler
                .set_tasks(
                    vec![Task {
                        state: Running,
                        hidden: true,
                        ..replacement
                    }],
                    selection,
                    HistoryForkBoundary::AfterCompletedHistory,
                )
                .unwrap(),
        );
    }));
    old_stop_record.open();
    new_provider.entered().await;
    let observed = probes
        .lock()
        .unwrap()
        .expect("replacement enqueue probe ran");
    let update = contending
        .lock()
        .unwrap()
        .take()
        .expect("contending update admitted");
    bounded(update.join()).await.unwrap();
    let tasks = snapshot(&owner).await;
    let calls = worker.task_session.calls.lock().unwrap().clone();
    assert_eq!(
        calls.iter().filter(|call| call.kind == "enqueue").count(),
        2
    );
    assert_eq!(requests(&h.provider, "a").len(), 2);
    assert_eq!(tasks[0].state, Running);
    assert!(tasks[0].hidden);
    // Mutation completed with the provider still held: the gate is not held for a turn.
    new_provider.open();
    complete(&owner).await;
    h.close().await;
    assert_eq!(
        observed,
        (false, false),
        "checkpoint-aware enqueue must not hold the scheduler gate across callbacks"
    );
}
