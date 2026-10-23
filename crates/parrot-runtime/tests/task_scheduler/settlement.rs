use super::{fixture::*, harness::Harness, support::*};
use parrot_domain::values::agent_tasks::{ExecutionStatus::*, Task};
use parrot_storage::conversation::HistoryForkBoundary;
use std::{
    future::Future,
    sync::{Arc, atomic::Ordering},
    task::Poll,
};

#[tokio::test]
async fn restarted_generation_waits_for_actual_provider_close_and_stop_record_before_enqueue() {
    let h = Harness::new();
    let root = h.root("owner").await;
    let running = Gate::new();
    let close = Gate::new();
    let stop_record = Gate::new();
    let replacement = Gate::new();
    h.provider.script(
        "a",
        [
            Reply {
                close_gate: Some(close.clone()),
                ..Reply::held("answer", &running)
            },
            Reply::held("answer", &replacement),
        ],
    );
    set(&root, vec![task("a")]).await;
    running.entered().await;
    let built = h.factory.get("a");
    *built.task_session.record_gate.lock().unwrap() = Some(stop_record.clone());
    let mut updated = task("a");
    updated.payload =
        parrot_domain::values::agent_tasks::TaskPayload::Instruction("new instruction".into());
    set(&root, vec![updated]).await;
    close.entered().await;
    assert_eq!(requests(&h.provider, "a").len(), 1);
    assert_eq!(
        built
            .task_session
            .calls
            .lock()
            .unwrap()
            .iter()
            .filter(|call| call.kind == "enqueue")
            .count(),
        1
    );
    close.open();
    stop_record.entered().await;
    assert_eq!(requests(&h.provider, "a").len(), 1);
    assert_eq!(
        built
            .task_session
            .calls
            .lock()
            .unwrap()
            .iter()
            .filter(|call| call.kind == "enqueue")
            .count(),
        1
    );
    stop_record.open();
    replacement.entered().await;
    assert_eq!(requests(&h.provider, "a").len(), 2);
    let calls = built.task_session.calls.lock().unwrap().clone();
    let stop = calls
        .iter()
        .position(|call| {
            call.kind == "record"
                && call
                    .text
                    .contains("the owner reset or changed this task; it restarts in this session.")
        })
        .unwrap();
    assert!(
        stop < calls
            .iter()
            .rposition(|call| call.kind == "enqueue")
            .unwrap()
    );
    let history = built.history.conversation().await.unwrap();
    let stopped = history
        .iter()
        .position(|item| {
            parrot_storage::events::content(&item.message.parts)
                .contains("Your AgentTask turn was stopped:")
        })
        .unwrap();
    let resumed = history
        .iter()
        .rposition(|item| {
            parrot_storage::events::content(&item.message.parts).contains("new instruction")
        })
        .unwrap();
    assert!(stopped < resumed);
    replacement.open();
    complete(&root).await;
    h.close().await;
}

#[tokio::test]
async fn running_terminal_updates_stop_and_record_exact_reason_without_overwriting_owner_state() {
    for (state, reason) in [
        (Canceled, "the owner canceled this task."),
        (Succeeded, "the owner marked this task as succeeded."),
        (Failed, "the owner marked this task as failed."),
    ] {
        let h = Harness::new();
        let root = h.root("owner").await;
        let gate = Gate::new();
        h.provider.script("a", [Reply::held("answer", &gate)]);
        set(&root, vec![task("a")]).await;
        gate.entered().await;
        let update = Task {
            state,
            result: (state == Succeeded).then(|| "manual".into()),
            failure: (state == Failed).then(|| "owner failure".into()),
            ..task("a")
        };
        set(&root, vec![update]).await;
        let built = h.factory.get("a");
        built
            .task_session
            .wait_calls(|calls| {
                calls
                    .iter()
                    .any(|call| call.kind == "record" && call.text.contains(reason))
            })
            .await;
        let tasks = snapshot(&root).await;
        assert_eq!(tasks[0].state, state);
        assert_eq!(
            tasks[0].result.as_deref(),
            if state == Succeeded {
                Some("manual")
            } else {
                None
            }
        );
        assert_eq!(requests(&h.provider, "a").len(), 1);
        h.close().await;
    }
}

#[tokio::test]
async fn dropping_set_observer_keeps_admitted_spawn_and_all_observers_share_spawn_gate() {
    let h = Harness::new();
    let root = h.root("owner").await;
    let construction = Gate::new();
    let provider = Gate::new();
    *h.factory.construction.lock().unwrap() = Some(construction.clone());
    h.provider.script("a", [Reply::held("answer", &provider)]);
    let scheduler = root.get_service::<Scheduler>().unwrap();
    let observer = scheduler
        .set_tasks(
            vec![task("a")],
            root.selection().current().unwrap(),
            HistoryForkBoundary::AfterCompletedHistory,
        )
        .unwrap();
    drop(observer);
    construction.entered().await;
    let mut snapshot_future = Box::pin(scheduler.snapshot());
    let mut detail_future = Box::pin(scheduler.capture_detail("a"));
    assert!(
        std::future::poll_fn(|cx| Poll::Ready(matches!(
            snapshot_future.as_mut().poll(cx),
            Poll::Pending
        )))
        .await
    );
    assert!(
        std::future::poll_fn(|cx| Poll::Ready(matches!(
            detail_future.as_mut().poll(cx),
            Poll::Pending
        )))
        .await
    );
    construction.open();
    provider.entered().await;
    assert_eq!(bounded(snapshot_future).await[0].state, Running);
    assert_eq!(
        bounded(detail_future).await.unwrap().agent_name.as_deref(),
        Some("a")
    );
    provider.open();
    complete(&root).await;
    h.close().await;
}

#[tokio::test]
async fn scope_settlement_cancels_nested_graphs_before_session_resources_and_is_idempotent() {
    let h = Harness::new();
    let root = h.root("owner").await;
    h.factory.plans.lock().unwrap().insert(
        "second-owner".into(),
        ScopePlan {
            final_resources: vec![Arc::new(FinalResource(
                h.factory.trace.clone(),
                "resource:second-owner",
            ))],
            ..Default::default()
        },
    );
    let first_owner = h.spawn(&root, "first-owner").await;
    let second_owner = h.spawn(&root, "second-owner").await;
    let first_gate = Gate::new();
    let second_gate = Gate::new();
    h.provider.script(
        "worker",
        [
            Reply::held("answer", &first_gate),
            Reply::held("answer", &second_gate),
        ],
    );
    set(&first_owner, vec![task("worker")]).await;
    first_gate.entered().await;
    set(&second_owner, vec![task("worker")]).await;
    second_gate.entered().await;
    assert!(!Arc::ptr_eq(
        &first_owner.get_service::<Scheduler>().unwrap(),
        &second_owner.get_service::<Scheduler>().unwrap()
    ));
    bounded(first_owner.close().join()).await.unwrap();
    assert_eq!(snapshot(&first_owner).await[0].state, Canceled);
    assert_eq!(snapshot(&second_owner).await[0].state, Running);
    let rejected = first_owner
        .get_service::<Scheduler>()
        .unwrap()
        .set_tasks(
            vec![task("x")],
            first_owner.selection().current().unwrap(),
            HistoryForkBoundary::AfterCompletedHistory,
        )
        .err()
        .expect("settled scheduler rejects new work");
    assert_eq!(rejected.to_string(), "The agent session is shutting down.");
    let nested = h
        .registry
        .snapshot_scopes()
        .into_iter()
        .find(|scope| {
            scope.identity().session.name == "worker"
                && scope.identity().session.parent_session_id
                    == second_owner.identity().session.session_id
        })
        .unwrap();
    let nested_gate = Gate::new();
    h.provider
        .script("nested", [Reply::held("answer", &nested_gate)]);
    set(&nested, vec![task("nested")]).await;
    nested_gate.entered().await;
    let nested_output = h.factory.get("nested").output.clone();
    assert_eq!(nested_output.closes.load(Ordering::SeqCst), 0);
    h.close().await;
    h.close().await;
    assert_eq!(snapshot(&second_owner).await[0].state, Canceled);
    assert_eq!(snapshot(&nested).await[0].state, Canceled);
    let saved = second_owner.agent_state().load().await.unwrap().unwrap();
    let stopped = saved.tasks.entries[0].pending_stop.as_ref().unwrap();
    assert!(stopped.stopped);
    assert!(
        saved
            .tasks
            .notifications
            .iter()
            .any(|notice| notice.message_id == stopped.notice_message_id)
    );
    assert_eq!(nested_output.closes.load(Ordering::SeqCst), 1);
    let trace = h.factory.trace.values();
    let nested_end = trace
        .iter()
        .position(|event| event == "session:nested")
        .unwrap();
    let owner_end = trace
        .iter()
        .position(|event| event == "resource:second-owner")
        .unwrap();
    assert!(nested_end < owner_end);
}
