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
async fn replacement_initial_enqueue_holds_state_gate_but_not_whole_provider_turn() {
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
    *worker.task_session.enqueue_hook.lock().unwrap() = Some(Box::new(move || {
        let waker = Waker::from(Arc::new(NoWake));
        let mut context = Context::from_waker(&waker);
        let mut snapshot = Box::pin(scheduler.snapshot());
        let snapshot_pending = matches!(snapshot.as_mut().poll(&mut context), Poll::Pending);
        let mut detail = Box::pin(scheduler.capture_detail("a"));
        let detail_pending = matches!(detail.as_mut().poll(&mut context), Poll::Pending);
        *observed_probes.lock().unwrap() = Some((snapshot_pending, detail_pending));
        // Admit a real contending visibility-only update before delegating enqueue.
        // It cannot mutate until this initial enqueue exits the scheduler state gate.
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
        (true, true),
        "initial enqueue must occur under the scheduler gate, after prior admission completed"
    );
}
