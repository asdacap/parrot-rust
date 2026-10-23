use super::{fixture::*, harness::Harness, support::*};
use parrot_domain::values::agent_tasks::{ExecutionStatus::*, TaskCallIdentity};
use parrot_storage::conversation::HistoryForkBoundary;
use std::sync::atomic::Ordering;

fn call(sequence: i64, name: &str) -> TaskCallIdentity {
    TaskCallIdentity {
        assistant_sequence: sequence,
        call_id: name.into(),
    }
}

#[tokio::test]
async fn accepted_call_replay_preserves_generation_and_original_count_but_new_id_can_reset() {
    let h = Harness::new();
    let root = h.root("owner").await;
    let held = Gate::new();
    h.provider.script("a", [Reply::held("answer", &held)]);
    let scheduler = root.get_service::<Scheduler>().unwrap();
    let key = call(17, "accepted");
    let first = scheduler
        .set_tool_tasks(
            vec![task("a")],
            root.selection().current().unwrap(),
            HistoryForkBoundary::AfterCompletedHistory,
            key.clone(),
        )
        .unwrap();
    let duplicate = scheduler
        .set_tool_tasks(
            vec![task("a"), task("must-not-exist")],
            root.selection().current().unwrap(),
            HistoryForkBoundary::AfterCompletedHistory,
            key.clone(),
        )
        .unwrap();
    assert_eq!(first.join().await.unwrap(), 1);
    assert_eq!(duplicate.join().await.unwrap(), 1);
    held.entered().await;
    let before = root.agent_state().load().await.unwrap().unwrap();
    assert_eq!(before.tasks.entries.len(), 1);
    assert_eq!(before.tasks.accepted_calls.len(), 1);
    let generation = before.tasks.entries[0]
        .runner
        .as_ref()
        .unwrap()
        .generation_id
        .clone();
    held.open();
    complete(&root).await;
    assert_eq!(
        scheduler
            .set_tool_tasks(
                vec![task("a")],
                root.selection().current().unwrap(),
                HistoryForkBoundary::AfterCompletedHistory,
                key
            )
            .unwrap()
            .join()
            .await
            .unwrap(),
        1
    );
    assert_eq!(snapshot(&root).await[0].state, Succeeded);
    assert_eq!(
        root.agent_state()
            .load()
            .await
            .unwrap()
            .unwrap()
            .tasks
            .entries[0]
            .runner
            .as_ref()
            .unwrap()
            .generation_id,
        generation
    );
    scheduler
        .set_tool_tasks(
            vec![task("a")],
            root.selection().current().unwrap(),
            HistoryForkBoundary::AfterCompletedHistory,
            call(17, "explicit-reset"),
        )
        .unwrap()
        .join()
        .await
        .unwrap();
    complete(&root).await;
    let saved = root.agent_state().load().await.unwrap().unwrap();
    assert_ne!(
        saved.tasks.entries[0]
            .runner
            .as_ref()
            .unwrap()
            .generation_id,
        generation
    );
    assert_eq!(saved.tasks.accepted_calls.len(), 2);
    h.close().await;
}

#[tokio::test]
async fn current_batch_receipts_are_bounded_and_stale_batches_cannot_reapply() {
    let h = Harness::new();
    let root = h.root("owner").await;
    let scheduler = root.get_service::<Scheduler>().unwrap();
    for name in ["one", "two"] {
        scheduler
            .set_tool_tasks(
                vec![state(name, Canceled)],
                root.selection().current().unwrap(),
                HistoryForkBoundary::AfterCompletedHistory,
                call(8, name),
            )
            .unwrap()
            .join()
            .await
            .unwrap();
    }
    assert_eq!(
        root.agent_state()
            .load()
            .await
            .unwrap()
            .unwrap()
            .tasks
            .accepted_calls
            .len(),
        2
    );
    assert_eq!(
        scheduler.lookup_accepted(&call(8, "one")).await.unwrap(),
        Some(1)
    );
    scheduler
        .set_tool_tasks(
            vec![state("new", Canceled)],
            root.selection().current().unwrap(),
            HistoryForkBoundary::AfterCompletedHistory,
            call(9, "new"),
        )
        .unwrap()
        .join()
        .await
        .unwrap();
    let saved = root.agent_state().load().await.unwrap().unwrap();
    assert_eq!(saved.tasks.accepted_calls.len(), 1);
    assert!(scheduler.lookup_accepted(&call(8, "one")).await.is_err());
    assert!(
        scheduler
            .set_tool_tasks(
                vec![task("must-not-run")],
                root.selection().current().unwrap(),
                HistoryForkBoundary::AfterCompletedHistory,
                call(8, "late")
            )
            .unwrap()
            .join()
            .await
            .is_err()
    );
    let after = root.agent_state().load().await.unwrap().unwrap();
    assert_eq!(after.tasks.entries, saved.tasks.entries);
    assert_eq!(after.tasks.accepted_calls, saved.tasks.accepted_calls);
    assert_eq!(after.tasks.revision, saved.tasks.revision);
    h.close().await;
}

#[tokio::test]
async fn uncommitted_receipt_is_not_visible_after_snapshot_failure() {
    let h = Harness::new();
    let root = h.root("owner").await;
    *h.factory.graph_faults.owner.lock().unwrap() = "owner".into();
    h.factory
        .graph_faults
        .fail_before
        .store(true, Ordering::SeqCst);
    let scheduler = root.get_service::<Scheduler>().unwrap();
    let key = call(3, "failed");
    assert!(
        scheduler
            .set_tool_tasks(
                vec![task("a")],
                root.selection().current().unwrap(),
                HistoryForkBoundary::AfterCompletedHistory,
                key.clone()
            )
            .unwrap()
            .join()
            .await
            .is_err()
    );
    assert!(scheduler.lookup_accepted(&key).await.is_err());
    assert!(
        root.agent_state()
            .load()
            .await
            .unwrap()
            .unwrap()
            .tasks
            .accepted_calls
            .is_empty()
    );
    scheduler.suspend().join().await.unwrap();
    h.close().await;
}

#[tokio::test]
async fn crash_after_graph_acceptance_replays_without_resetting_resumed_attempt() {
    let first = Harness::new();
    let root = first.root("owner").await;
    let held = Gate::new();
    first.provider.script("a", [Reply::held("answer", &held)]);
    let key = call(42, "unfinished-tool");
    let scheduler = root.get_service::<Scheduler>().unwrap();
    scheduler
        .set_tool_tasks(
            vec![task("a")],
            root.selection().current().unwrap(),
            HistoryForkBoundary::AfterCompletedHistory,
            key.clone(),
        )
        .unwrap()
        .join()
        .await
        .unwrap();
    held.entered().await;
    let saved = root.agent_state().load().await.unwrap().unwrap();
    let old = saved.tasks.entries[0].runner.clone().unwrap();
    let identity = root.identity().clone();
    scheduler.suspend().join().await.unwrap();
    first.close().await;
    let second = Harness::with_history(32, first.histories.clone(), first.provider.clone());
    let restored = second.reopen_root(identity).await;
    let scheduler = restored.get_service::<Scheduler>().unwrap();
    assert_eq!(scheduler.lookup_accepted(&key).await.unwrap(), Some(1));
    assert_eq!(
        scheduler
            .set_tool_tasks(
                vec![task("a")],
                restored.selection().current().unwrap(),
                HistoryForkBoundary::AfterCompletedHistory,
                key
            )
            .unwrap()
            .join()
            .await
            .unwrap(),
        1
    );
    assert_eq!(
        restored
            .agent_state()
            .load()
            .await
            .unwrap()
            .unwrap()
            .tasks
            .entries[0]
            .runner
            .as_ref()
            .unwrap(),
        &old
    );
    scheduler.activate_executions().join().await.unwrap();
    complete(&restored).await;
    let events = second.histories.repository.events().replay().await.unwrap();
    assert_eq!(
        events
            .iter()
            .filter(|event| event.has_input_admitted()
                && event.input_admitted().message_id == old.prompt_message_id)
            .count(),
        1
    );
    scheduler.suspend().join().await.unwrap();
    second.close().await;
}
