use super::{fixture::*, harness::Harness, support::*};
use parrot_domain::values::{
    agent_state::AgentStateUpdate,
    agent_tasks::{AcceptanceVerdict, ExecutionStatus::*, LeafResponse},
};
use parrot_runtime::{children::*, task_scheduler::TaskConfiguration};
use parrot_storage::{conversation::HistoryForkBoundary, events::NeverCancelled};
use std::sync::{Arc, atomic::Ordering};

async fn freeze(h: &Harness, root: &Arc<Scope<Security>>) {
    root.get_service::<Scheduler>()
        .unwrap()
        .suspend()
        .join()
        .await
        .unwrap();
    h.registry.shutdown().join().await.unwrap();
}
async fn freeze_after_io_fault(h: &Harness, root: &Arc<Scope<Security>>) {
    let suspension = root
        .get_service::<Scheduler>()
        .unwrap()
        .suspend()
        .join()
        .await
        .unwrap_err();
    assert_eq!(suspension.code(), "io");
    let shutdown = h.registry.shutdown().join().await.unwrap_err();
    assert_eq!(shutdown, suspension);
}
async fn activate(root: &Arc<Scope<Security>>) {
    bounded(
        root.get_service::<Scheduler>()
            .unwrap()
            .activate_executions()
            .join(),
    )
    .await
    .unwrap();
}
async fn wait_saved(
    root: &Arc<Scope<Security>>,
    predicate: impl Fn(&parrot_domain::values::agent_state::TaskGraphSnapshot) -> bool,
) {
    bounded(async {
        loop {
            let saved = root.agent_state().load().await.unwrap().unwrap();
            if predicate(&saved.tasks) {
                return;
            }
            tokio::task::yield_now().await;
        }
    })
    .await;
}

#[tokio::test]
async fn recovery_prepares_all_owners_before_pending_admission_and_preserves_reduced_limits() {
    use parrot_runtime::task_scheduler::TaskConcurrencyBudget;

    for restored_limit in [1, 0] {
        let first = Harness::new();
        *first.factory.concurrency.lock().unwrap() = Some(TaskConcurrencyBudget::new(1));
        let earlier = first.root("earlier-owner").await;
        let later = first.root("later-owner").await;
        let initial: Vec<_> = (0..3).map(|_| Gate::new()).collect();
        let resumed: Vec<_> = (0..4).map(|_| Gate::new()).collect();
        for (index, name) in ["a", "b", "c"].into_iter().enumerate() {
            first.provider.script(
                name,
                [
                    Reply::held("answer", &initial[index]),
                    Reply::held("answer", &resumed[index]),
                ],
            );
        }
        first
            .provider
            .script("pending", [Reply::held("answer", &resumed[3])]);
        set(&later, vec![dedicated(task("b")), dedicated(task("c"))]).await;
        initial[1].entered().await;
        initial[2].entered().await;
        set(
            &earlier,
            vec![dedicated(task("a")), dedicated(task("pending"))],
        )
        .await;
        initial[0].entered().await;
        assert_eq!(snapshot(&earlier).await[1].state, Pending);
        for root in [&earlier, &later] {
            root.get_service::<Scheduler>()
                .unwrap()
                .suspend()
                .join()
                .await
                .unwrap();
        }
        first.registry.shutdown().join().await.unwrap();
        let earlier_saved = earlier.agent_state().load().await.unwrap().unwrap().tasks;
        let later_saved = later.agent_state().load().await.unwrap().unwrap().tasks;

        let second = Harness::with_history(32, first.histories.clone(), first.provider.clone());
        *second.factory.concurrency.lock().unwrap() =
            Some(TaskConcurrencyBudget::new(restored_limit));
        let earlier_restored = second.reopen_root(earlier.identity().clone()).await;
        let later_restored = second.reopen_root(later.identity().clone()).await;
        for root in [&earlier_restored, &later_restored] {
            let scheduler = root.get_service::<Scheduler>().unwrap();
            scheduler.resolve_workers().await.unwrap();
            scheduler.prepare_recovery().await.unwrap();
            scheduler.prepare_recovery().await.unwrap();
        }
        assert_eq!(
            earlier_restored
                .agent_state()
                .load()
                .await
                .unwrap()
                .unwrap()
                .tasks,
            earlier_saved
        );
        assert_eq!(
            later_restored
                .agent_state()
                .load()
                .await
                .unwrap()
                .unwrap()
                .tasks,
            later_saved
        );
        activate(&earlier_restored).await;
        resumed[0].entered().await;
        assert_eq!(snapshot(&earlier_restored).await[1].state, Pending);
        assert!(requests(&second.provider, "pending").is_empty());
        assert!(earlier_restored.children().resolve("pending").is_err());
        activate(&later_restored).await;
        resumed[1].entered().await;
        resumed[2].entered().await;
        for (root, saved) in [
            (&earlier_restored, &earlier_saved),
            (&later_restored, &later_saved),
        ] {
            let current = root.agent_state().load().await.unwrap().unwrap().tasks;
            for (current, saved) in current.entries.iter().zip(&saved.entries) {
                assert_eq!(
                    current.borrow_worker().worker_session_id,
                    saved.borrow_worker().worker_session_id
                );
                assert_eq!(current.borrow_worker().runner, saved.borrow_worker().runner);
            }
        }
        let free_owner = second.root("free-owner").await;
        let free = Gate::new();
        second
            .provider
            .script("free", [Reply::held("answer", &free)]);
        set(&free_owner, vec![dedicated(task("free"))]).await;
        free.entered().await;

        resumed[1].open();
        wait_tasks(&later_restored, |tasks| tasks[0].state == Succeeded).await;
        if restored_limit == 0 {
            assert_eq!(snapshot(&earlier_restored).await[1].state, Pending);
            assert!(requests(&second.provider, "pending").is_empty());
            resumed[0].open();
        }
        resumed[3].entered().await;
        if restored_limit != 0 {
            resumed[0].open();
        }
        resumed[2].open();
        resumed[3].open();
        free.open();
        complete(&earlier_restored).await;
        complete(&later_restored).await;
        complete(&free_owner).await;
        second.close().await;
    }
}

#[tokio::test]
async fn recovery_normalizes_pending_stops_under_zero_budget_before_waiting_owners_activate() {
    use parrot_runtime::task_scheduler::TaskConcurrencyBudget;

    for stopped in [false, true] {
        let first = Harness::new();
        *first.factory.concurrency.lock().unwrap() = Some(TaskConcurrencyBudget::new(1));
        let root = first.root("owner").await;
        let waiting = first.root("waiting-owner").await;
        let initial: Vec<_> = (0..3).map(|_| Gate::new()).collect();
        let resumed: Vec<_> = (0..4).map(|_| Gate::new()).collect();
        for (index, name) in ["a", "b", "c"].into_iter().enumerate() {
            first.provider.script(
                name,
                [
                    Reply::held("answer", &initial[index]),
                    Reply::held("answer", &resumed[index]),
                ],
            );
        }
        first
            .provider
            .script("d", [Reply::held("answer", &resumed[3])]);
        set(&root, vec![dedicated(task("a")), dedicated(task("b"))]).await;
        initial[0].entered().await;
        initial[1].entered().await;
        set(&waiting, vec![dedicated(task("c")), dedicated(task("d"))]).await;
        initial[2].entered().await;
        assert_eq!(snapshot(&waiting).await[1].state, Pending);
        waiting
            .get_service::<Scheduler>()
            .unwrap()
            .freeze_for_suspension()
            .join()
            .await
            .unwrap();
        let record = Gate::new();
        let worker = first.factory.get("a");
        *worker.task_session.record_gate.lock().unwrap() = Some(record.clone());
        let mut replacement = dedicated(task("a"));
        replacement.acceptance_criteria = "replacement".into();
        set(&root, vec![replacement]).await;
        record.entered().await;
        wait_saved(&root, |graph| {
            graph.entries[0].task.state == Running
                && graph.entries[0].borrow_worker().pending_stop.is_some()
        })
        .await;
        root.get_service::<Scheduler>()
            .unwrap()
            .freeze_for_suspension()
            .join()
            .await
            .unwrap();
        worker.adapter.session().suspend_handle();
        let suspension = root.get_service::<Scheduler>().unwrap().suspend();
        record.open();
        bounded(suspension.join()).await.unwrap();
        freeze(&first, &waiting).await;
        let mut saved = root.agent_state().load().await.unwrap().unwrap().tasks;
        assert_eq!(saved.entries[0].task.state, Running);
        saved.entries[0]
            .borrow_worker_mut()
            .pending_stop
            .as_mut()
            .unwrap()
            .stopped = stopped;
        root.agent_state()
            .update(AgentStateUpdate::Tasks(saved.clone()))
            .await
            .unwrap();

        let second = Harness::with_history(32, first.histories.clone(), first.provider.clone());
        *second.factory.concurrency.lock().unwrap() = Some(TaskConcurrencyBudget::new(0));
        let restored = second.reopen_root(root.identity().clone()).await;
        let waiting_restored = second.reopen_root(waiting.identity().clone()).await;
        for owner in [&restored, &waiting_restored] {
            let scheduler = owner.get_service::<Scheduler>().unwrap();
            scheduler.resolve_workers().await.unwrap();
            scheduler.prepare_recovery().await.unwrap();
            scheduler.prepare_recovery().await.unwrap();
        }
        let normalized = restored.agent_state().load().await.unwrap().unwrap().tasks;
        let mut expected = saved;
        expected.entries[0].task.state = Pending;
        assert_eq!(normalized, expected);
        let stop_record = Gate::new();
        *second
            .factory
            .get("a")
            .task_session
            .record_gate
            .lock()
            .unwrap() = Some(stop_record.clone());
        activate(&waiting_restored).await;
        resumed[2].entered().await;
        activate(&restored).await;
        resumed[1].entered().await;
        stop_record.entered().await;
        assert_eq!(snapshot(&restored).await[0].state, Pending);
        assert_eq!(snapshot(&waiting_restored).await[1].state, Pending);
        assert_eq!(requests(&second.provider, "a").len(), 1);
        assert!(requests(&second.provider, "d").is_empty());

        resumed[1].open();
        let tasks = wait_tasks(&restored, |tasks| tasks[1].state == Succeeded).await;
        assert_eq!(tasks[0].state, if stopped { Running } else { Pending });
        stop_record.open();
        resumed[0].entered().await;
        assert_eq!(snapshot(&waiting_restored).await[1].state, Pending);
        resumed[2].open();
        resumed[3].entered().await;
        resumed[0].open();
        resumed[3].open();
        complete(&restored).await;
        complete(&waiting_restored).await;
        second.close().await;
    }
}

#[tokio::test]
async fn suspension_releases_prepared_recovery_admissions_without_changing_checkpoints() {
    use parrot_runtime::task_scheduler::TaskConcurrencyBudget;

    let first = Harness::new();
    let root = first.root("owner").await;
    let held: Vec<_> = (0..3).map(|_| Gate::new()).collect();
    for (name, gate) in ["a", "b", "c"].into_iter().zip(&held) {
        first.provider.script(name, [Reply::held("answer", gate)]);
    }
    set(
        &root,
        vec![
            dedicated(task("a")),
            dedicated(task("b")),
            dedicated(task("c")),
        ],
    )
    .await;
    for gate in &held {
        gate.entered().await;
    }
    freeze(&first, &root).await;
    let saved = root.agent_state().load().await.unwrap().unwrap().tasks;
    let second = Harness::with_history(32, first.histories.clone(), first.provider.clone());
    *second.factory.concurrency.lock().unwrap() = Some(TaskConcurrencyBudget::new(1));
    let restored = second.reopen_root(root.identity().clone()).await;
    let scheduler = restored.get_service::<Scheduler>().unwrap();
    scheduler.prepare_recovery().await.unwrap();
    scheduler.prepare_recovery().await.unwrap();
    scheduler.suspend().join().await.unwrap();
    assert_eq!(
        restored.agent_state().load().await.unwrap().unwrap().tasks,
        saved
    );
    let fresh = second.root("fresh-owner").await;
    let available: Vec<_> = (0..2).map(|_| Gate::new()).collect();
    for (name, gate) in ["new-a", "new-b"].into_iter().zip(&available) {
        second.provider.script(name, [Reply::held("answer", gate)]);
    }
    set(
        &fresh,
        vec![dedicated(task("new-a")), dedicated(task("new-b"))],
    )
    .await;
    for gate in &available {
        gate.entered().await;
    }
    for gate in &available {
        gate.open();
    }
    complete(&fresh).await;
    second.close().await;
}

#[tokio::test]
async fn failed_recovery_activation_releases_prepared_usage_during_suspension() {
    use parrot_runtime::task_scheduler::TaskConcurrencyBudget;

    let first = Harness::new();
    let root = first.root("owner").await;
    let held: Vec<_> = (0..3).map(|_| Gate::new()).collect();
    for (name, gate) in ["a", "b", "c"].into_iter().zip(&held) {
        first.provider.script(
            name,
            [Reply::held("answer", gate), Reply::held("answer", gate)],
        );
    }
    set(
        &root,
        vec![
            dedicated(task("a")),
            dedicated(task("b")),
            dedicated(task("c")),
        ],
    )
    .await;
    for gate in &held {
        gate.entered().await;
    }
    freeze(&first, &root).await;
    let budget = TaskConcurrencyBudget::new(1);
    let second = Harness::with_history(32, first.histories.clone(), first.provider.clone());
    *second.factory.concurrency.lock().unwrap() = Some(budget.clone());
    let restored = second.reopen_root(root.identity().clone()).await;
    let scheduler = restored.get_service::<Scheduler>().unwrap();
    scheduler.prepare_recovery().await.unwrap();
    *second.factory.graph_faults.owner.lock().unwrap() = "owner".into();
    second
        .factory
        .graph_faults
        .fail_before
        .store(true, Ordering::SeqCst);
    let failure = bounded(scheduler.activate_executions().join())
        .await
        .unwrap_err();
    assert_eq!(failure.code(), "io");
    freeze(&second, &restored).await;

    let third = Harness::new();
    *third.factory.concurrency.lock().unwrap() = Some(budget);
    let fresh = third.root("fresh-owner").await;
    let available: Vec<_> = (0..2).map(|_| Gate::new()).collect();
    for (name, gate) in ["new-a", "new-b"].into_iter().zip(&available) {
        third.provider.script(name, [Reply::held("answer", gate)]);
    }
    set(
        &fresh,
        vec![dedicated(task("new-a")), dedicated(task("new-b"))],
    )
    .await;
    for gate in &available {
        gate.entered().await;
    }
    for gate in &available {
        gate.open();
    }
    complete(&fresh).await;
    third.close().await;
}

#[tokio::test]
async fn suspended_stop_record_preserves_notice_without_poisoning_close_and_replays_on_reopen() {
    let first = Harness::new();
    let root = first.root("owner").await;
    let provider = Gate::new();
    first
        .provider
        .script("a", [Reply::held("answer", &provider)]);
    set(&root, vec![dedicated(task("a"))]).await;
    provider.entered().await;
    let worker = first.factory.get("a");
    let record = Gate::new();
    *worker.task_session.record_gate.lock().unwrap() = Some(record.clone());
    set(&root, vec![dedicated(state("a", Canceled))]).await;
    record.entered().await;
    let scheduler = root.get_service::<Scheduler>().unwrap();
    first.registry.freeze_admissions().join().await.unwrap();
    scheduler.freeze_for_suspension().join().await.unwrap();
    first
        .factory
        .get("owner")
        .adapter
        .session()
        .suspend_handle();
    worker.adapter.session().suspend_handle();
    let suspension = scheduler.suspend();
    assert!(!suspension.is_finished());
    record.open();
    bounded(suspension.join()).await.unwrap();
    first.registry.shutdown().join().await.unwrap();
    let saved = root.agent_state().load().await.unwrap().unwrap();
    assert_eq!(saved.tasks.entries[0].task.state, Canceled);
    let stop = saved.tasks.entries[0]
        .borrow_worker()
        .pending_stop
        .as_ref()
        .unwrap();
    assert!(stop.stopped);
    assert!(
        saved
            .tasks
            .notifications
            .iter()
            .any(|notice| notice.message_id == stop.notice_message_id)
    );
    let second = Harness::with_history(32, first.histories.clone(), first.provider.clone());
    let restored = second.reopen_root(root.identity().clone()).await;
    activate(&restored).await;
    wait_saved(&restored, |graph| {
        graph.entries[0].borrow_worker().pending_stop.is_none()
    })
    .await;
    assert_eq!(snapshot(&restored).await[0].state, Canceled);
    assert_eq!(requests(&second.provider, "a").len(), 1);
    freeze(&second, &restored).await;
}

#[tokio::test]
async fn registry_freeze_during_task_worker_construction_preserves_pending_attempt_for_reopen() {
    let first = Harness::new();
    let root = first.root("owner").await;
    let construction = Gate::new();
    *first.factory.construction.lock().unwrap() = Some(construction.clone());
    let scheduler = root.get_service::<Scheduler>().unwrap();
    let update = scheduler
        .set_tasks(
            vec![dedicated(task("held-construction"))],
            root.selection().current().unwrap(),
            HistoryForkBoundary::AfterCompletedHistory,
        )
        .unwrap();
    construction.entered().await;
    let frozen = first.registry.freeze_admissions();
    assert!(!frozen.is_finished());
    construction.open();
    bounded(frozen.join()).await.unwrap();
    bounded(update.join()).await.unwrap();
    assert!(root.children().snapshot_children().is_empty());
    let before = root.agent_state().load().await.unwrap().unwrap();
    assert_eq!(before.tasks.entries[0].task.state, Pending);
    assert!(before.tasks.entries[0].task.failure.is_none());
    let attempt = before.tasks.entries[0]
        .borrow_worker()
        .runner
        .as_ref()
        .unwrap();
    assert_eq!(attempt.attempt, 1);
    let generation = attempt.generation_id.clone();
    let identity = root.identity().clone();
    freeze(&first, &root).await;
    assert!(first.provider.requests.lock().unwrap().is_empty());
    let second = Harness::with_history(32, first.histories.clone(), first.provider.clone());
    let restored = second.reopen_root(identity).await;
    assert_eq!(
        restored
            .agent_state()
            .load()
            .await
            .unwrap()
            .unwrap()
            .tasks
            .entries[0]
            .borrow_worker()
            .runner
            .as_ref()
            .unwrap()
            .generation_id,
        generation
    );
    activate(&restored).await;
    assert_eq!(
        complete(&restored).await[0].result.as_deref(),
        Some("answer")
    );
    freeze(&second, &restored).await;
}

#[tokio::test]
async fn graph_load_round_trips_definitions_outcomes_visibility_without_activation() {
    let first = Harness::new();
    let root = first.root("owner").await;
    let mut done = state("done", Succeeded);
    done.result = Some("saved result".into());
    done.hidden = true;
    let mut failed = composite("composite", vec![task("nested")]);
    failed.state = Failed;
    failed.failure = Some("saved failure".into());
    set(&root, vec![done, failed, state("canceled", Canceled)]).await;
    let expected = snapshot(&root).await;
    let identity = root.identity().clone();
    freeze(&first, &root).await;
    let second = Harness::with_history(32, first.histories.clone(), first.provider.clone());
    let restored = second.reopen_root(identity).await;
    assert_eq!(snapshot(&restored).await, expected);
    assert!(
        second
            .factory
            .get("owner")
            .task_session
            .calls
            .lock()
            .unwrap()
            .is_empty()
    );
    assert_eq!(second.registry.snapshot_scopes().len(), 1);
    freeze(&second, &restored).await;
}

#[tokio::test]
async fn suspended_retry_and_repair_continue_same_prompt_and_original_budgets() {
    let first = Harness::new();
    let mut config = TaskConfiguration::defaults(templates());
    config.maximum_attempts = 2;
    config.maximum_response_repairs = 1;
    *first.factory.configuration.lock().unwrap() = Some(config);
    first.factory.parser.answer(
        "retry",
        LeafResponse {
            result: "replacement".into(),
            verdict: AcceptanceVerdict::RejectAndRetry {
                feedback: "fix it".into(),
                payload: Some("Again".into()),
                replacement_result: None,
            },
        },
    );
    let held = Gate::new();
    first.provider.script(
        "a",
        [
            Reply::text("retry"),
            Reply::held("invalid", &held),
            Reply::text("invalid"),
            Reply::text("invalid-two"),
        ],
    );
    let root = first.root("owner").await;
    set(&root, vec![dedicated(task("a"))]).await;
    held.entered().await;
    let worker_id = first.factory.get("a").history.agent_session_id().to_owned();
    let before = root.agent_state().load().await.unwrap().unwrap();
    let prompt_id = before.tasks.entries[0]
        .borrow_worker()
        .runner
        .as_ref()
        .unwrap()
        .prompt_message_id
        .clone();
    assert_eq!(
        before.tasks.entries[0]
            .borrow_worker()
            .runner
            .as_ref()
            .unwrap()
            .attempt,
        2
    );
    let identity = root.identity().clone();
    freeze(&first, &root).await;
    let second = Harness::with_history(32, first.histories.clone(), first.provider.clone());
    let mut config = TaskConfiguration::defaults(templates());
    config.maximum_attempts = 9;
    config.maximum_response_repairs = 9;
    *second.factory.configuration.lock().unwrap() = Some(config);
    second
        .factory
        .parser
        .error("invalid", "first invalid envelope");
    second
        .factory
        .parser
        .error("invalid-two", "second invalid envelope");
    let restored = second.reopen_root(identity).await;
    assert_eq!(
        second.factory.get("a").history.agent_session_id(),
        worker_id
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
            .borrow_worker()
            .runner
            .as_ref()
            .unwrap()
            .prompt_message_id,
        prompt_id
    );
    activate(&restored).await;
    let done = wait_tasks(&restored, |tasks| tasks[0].state == Failed).await;
    assert_eq!(done[0].result.as_deref(), Some("replacement"));
    assert_eq!(
        done[0].failure.as_deref(),
        Some("leaf response invalid: second invalid envelope")
    );
    let saved = restored.agent_state().load().await.unwrap().unwrap();
    let runner = saved.tasks.entries[0]
        .borrow_worker()
        .runner
        .as_ref()
        .unwrap();
    assert_eq!(
        (
            runner.attempt,
            runner.repair_attempt,
            runner.maximum_attempts,
            runner.maximum_response_repairs
        ),
        (2, 1, 2, 1)
    );
    let events = second.histories.repository.events().replay().await.unwrap();
    assert_eq!(
        events
            .iter()
            .filter(|event| event.agent_session_id == worker_id && event.has_input_admitted())
            .count(),
        3
    );
    let history = second
        .factory
        .get("a")
        .history
        .conversation()
        .await
        .unwrap();
    assert!(!history.iter().any(|item| {
        parrot_storage::events::content(&item.message.parts)
            .contains("Your AgentTask turn was stopped:")
    }));
    freeze(&second, &restored).await;
}

#[tokio::test]
async fn missing_graph_worker_binding_adopts_retained_scope_without_construction_or_prompt_repeat()
{
    let first = Harness::new();
    let held = Gate::new();
    let resumed = Gate::new();
    first.provider.script(
        "a",
        [
            Reply::held("answer", &held),
            Reply::held("answer", &resumed),
        ],
    );
    let root = first.root("owner").await;
    set(&root, vec![dedicated(task("a"))]).await;
    held.entered().await;
    let worker_id = first.factory.get("a").history.agent_session_id().to_owned();
    let identity = root.identity().clone();
    freeze(&first, &root).await;
    let mut saved = root.agent_state().load().await.unwrap().unwrap();
    saved.tasks.entries[0].borrow_worker_mut().worker_session_id = None;
    saved.tasks.entries[0].task.state = Pending;
    root.agent_state()
        .update(AgentStateUpdate::Tasks(saved.tasks))
        .await
        .unwrap();
    let second = Harness::with_history(32, first.histories.clone(), first.provider.clone());
    let restored = second.reopen_root(identity).await;
    let scheduler = restored.get_service::<Scheduler>().unwrap();
    scheduler.resolve_workers().await.unwrap();
    assert_eq!(scheduler.claimed_worker_ids().await, [worker_id.clone()]);
    assert_eq!(second.registry.snapshot_scopes().len(), 2);
    assert_eq!(requests(&second.provider, "a").len(), 1);
    bounded(scheduler.activate_executions().join())
        .await
        .unwrap();
    resumed.entered().await;
    assert_eq!(
        second.factory.get("a").history.agent_session_id(),
        worker_id
    );
    let prompt_id = restored
        .agent_state()
        .load()
        .await
        .unwrap()
        .unwrap()
        .tasks
        .entries[0]
        .borrow_worker()
        .runner
        .as_ref()
        .unwrap()
        .prompt_message_id
        .clone();
    let events = second.histories.repository.events().replay().await.unwrap();
    assert_eq!(
        events
            .iter()
            .filter(|event| event.agent_session_id == worker_id
                && event.has_input_admitted()
                && event.input_admitted().message_id == prompt_id)
            .count(),
        1
    );
    resumed.open();
    complete(&restored).await;
    freeze(&second, &restored).await;
}

#[tokio::test]
async fn failed_graph_write_faults_closed_and_cannot_launch_queued_updates() {
    let h = Harness::new();
    let root = h.root("owner").await;
    *h.factory.graph_faults.owner.lock().unwrap() = "owner".into();
    h.factory
        .graph_faults
        .fail_before
        .store(true, Ordering::SeqCst);
    let scheduler = root.get_service::<Scheduler>().unwrap();
    let update = || {
        scheduler
            .set_tasks(
                vec![dedicated(task("a"))],
                root.selection().current().unwrap(),
                HistoryForkBoundary::AfterCompletedHistory,
            )
            .unwrap()
    };
    assert!(bounded(update().join()).await.is_err());
    assert!(bounded(update().join()).await.is_err());
    assert!(
        root.agent_state()
            .load()
            .await
            .unwrap()
            .unwrap()
            .tasks
            .entries
            .is_empty()
    );
    assert_eq!(h.registry.snapshot_scopes().len(), 1);
    assert!(requests(&h.provider, "a").is_empty());
    freeze(&h, &root).await;
}

#[tokio::test]
async fn saved_cancel_before_worker_stop_never_restarts_the_old_provider() {
    let first = Harness::new();
    let held = Gate::new();
    first.provider.script("a", [Reply::held("answer", &held)]);
    let root = first.root("owner").await;
    set(&root, vec![dedicated(task("a"))]).await;
    held.entered().await;
    *first.factory.graph_faults.owner.lock().unwrap() = "owner".into();
    first
        .factory
        .graph_faults
        .fail_after
        .store(true, Ordering::SeqCst);
    let scheduler = root.get_service::<Scheduler>().unwrap();
    assert!(
        scheduler
            .set_tasks(
                vec![dedicated(state("a", Canceled))],
                root.selection().current().unwrap(),
                HistoryForkBoundary::AfterCompletedHistory
            )
            .unwrap()
            .join()
            .await
            .is_err()
    );
    let saved = root.agent_state().load().await.unwrap().unwrap();
    assert!(
        saved.tasks.entries[0]
            .borrow_worker()
            .pending_stop
            .is_some()
    );
    let identity = root.identity().clone();
    freeze(&first, &root).await;
    let second = Harness::with_history(32, first.histories.clone(), first.provider.clone());
    let restored = second.reopen_root(identity).await;
    activate(&restored).await;
    wait_saved(&restored, |graph| {
        graph.entries[0].borrow_worker().pending_stop.is_none()
    })
    .await;
    assert_eq!(snapshot(&restored).await[0].state, Canceled);
    assert_eq!(requests(&second.provider, "a").len(), 1);
    freeze(&second, &restored).await;
}

#[tokio::test]
async fn saved_response_survives_ack_failure_and_newer_worker_execution() {
    for successor in [false, true] {
        let first = Harness::new();
        let held = Gate::new();
        first.provider.script(
            "a",
            [
                Reply::held("answer", &held),
                Reply::text("successor output"),
            ],
        );
        let root = first.root("owner").await;
        set(&root, vec![dedicated(task("a"))]).await;
        held.entered().await;
        let worker = first.factory.get("a");
        worker.task_session.ack_failures.store(1, Ordering::SeqCst);
        held.open();
        wait_saved(&root, |graph| {
            graph.entries[0]
                .borrow_worker()
                .runner
                .as_ref()
                .is_some_and(|runner| runner.response_reference.is_some())
        })
        .await;
        bounded(async {
            loop {
                if worker.scheduler.snapshot().await.is_empty()
                    && worker.task_session.ack_failures.load(Ordering::SeqCst) == 0
                {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await;
        let saved = root.agent_state().load().await.unwrap().unwrap();
        let runner = saved.tasks.entries[0]
            .borrow_worker()
            .runner
            .as_ref()
            .unwrap();
        let reference = runner.response_reference.clone().unwrap();
        let claim = parrot_domain::values::agent_state::TaskResultClaim {
            owner_session_id: root.identity().session.session_id.clone(),
            claim_id: runner.prompt_message_id.clone(),
        };
        if successor {
            worker
                .adapter
                .session()
                .acknowledge_claimed_result(&reference, &claim)
                .await
                .unwrap();
            let sent = worker
                .adapter
                .session()
                .send_text_message("unrelated successor".into(), Arc::new(NeverCancelled))
                .await
                .unwrap();
            sent.execution.join().await.unwrap();
        }
        let identity = root.identity().clone();
        freeze_after_io_fault(&first, &root).await;
        let second = Harness::with_history(32, first.histories.clone(), first.provider.clone());
        let restored = second.reopen_root(identity).await;
        let checkpoint = restored
            .agent_state()
            .load()
            .await
            .unwrap()
            .unwrap()
            .tasks
            .entries[0]
            .borrow_worker()
            .runner
            .clone();
        let mut visible = dedicated(state("a", Running));
        visible.hidden = true;
        set(&restored, vec![visible]).await;
        assert_eq!(
            restored
                .agent_state()
                .load()
                .await
                .unwrap()
                .unwrap()
                .tasks
                .entries[0]
                .borrow_worker()
                .runner,
            checkpoint
        );
        activate(&restored).await;
        let done = complete(&restored).await;
        assert_eq!(done[0].result.as_deref(), Some("answer"));
        assert_eq!(
            requests(&second.provider, "a").len(),
            if successor { 2 } else { 1 }
        );
        assert!(
            second
                .factory
                .get("a")
                .task_session
                .calls
                .lock()
                .unwrap()
                .iter()
                .all(|call| call.kind != "enqueue")
        );
        assert!(
            restored
                .agent_state()
                .load()
                .await
                .unwrap()
                .unwrap()
                .tasks
                .entries[0]
                .borrow_worker()
                .runner
                .as_ref()
                .unwrap()
                .outcome
                .is_some()
        );
        freeze(&second, &restored).await;
    }
}

#[tokio::test]
async fn canceling_waiter_does_not_cancel_or_message_unrelated_predecessor() {
    let h = Harness::new();
    let root = h.root("owner").await;
    let child = h.spawn(&root, "a").await;
    let held = Gate::new();
    h.provider.script("a", [Reply::held("answer", &held)]);
    let worker = h.factory.get("a");
    let predecessor = worker
        .adapter
        .session()
        .send_text_message("unrelated work".into(), Arc::new(NeverCancelled))
        .await
        .unwrap();
    held.entered().await;
    set(&root, vec![dedicated(task("a"))]).await;
    set(&root, vec![dedicated(state("a", Canceled))]).await;
    wait_saved(&root, |graph| {
        graph.entries[0].borrow_worker().pending_stop.is_none()
    })
    .await;
    assert!(!predecessor.execution.is_finished());
    assert!(
        worker
            .task_session
            .calls
            .lock()
            .unwrap()
            .iter()
            .all(|call| call.kind != "enqueue"
                && !call.text.contains("Your AgentTask turn was stopped:"))
    );
    assert_eq!(requests(&h.provider, "a").len(), 1);
    assert_eq!(
        child.identity().session.session_id,
        worker.history.agent_session_id()
    );
    held.open();
    predecessor.execution.join().await.unwrap();
    h.close().await;
}

#[tokio::test]
async fn shared_worker_activation_waits_for_rightful_reservation_not_provider_completion() {
    let first = Harness::new();
    let held = Gate::new();
    let resumed = Gate::new();
    let second_prompt = Gate::new();
    first.provider.script(
        "a",
        [
            Reply::held("answer", &held),
            Reply::held("answer", &resumed),
            Reply::held("answer", &second_prompt),
        ],
    );
    let root = first.root("owner").await;
    set(&root, vec![dedicated(task("A!")), dedicated(task("a"))]).await;
    held.entered().await;
    let identity = root.identity().clone();
    freeze(&first, &root).await;
    let mut saved = root.agent_state().load().await.unwrap().unwrap();
    saved.tasks.entries.reverse();
    root.agent_state()
        .update(AgentStateUpdate::Tasks(saved.tasks))
        .await
        .unwrap();
    let second = Harness::with_history(32, first.histories.clone(), first.provider.clone());
    let restored = second.reopen_root(identity).await;
    activate(&restored).await;
    resumed.entered().await;
    assert_eq!(requests(&second.provider, "a").len(), 2);
    assert!(
        snapshot(&restored)
            .await
            .iter()
            .all(|task| task.state == Running)
    );
    resumed.open();
    second_prompt.entered().await;
    second_prompt.open();
    complete(&restored).await;
    assert_eq!(second.registry.snapshot_scopes().len(), 2);
    freeze(&second, &restored).await;
}

#[tokio::test]
async fn oversized_outbox_replays_exact_id_and_body_after_ack_save_failure() {
    let first = Harness::new();
    let root = first.root("owner").await;
    *first.factory.graph_faults.owner.lock().unwrap() = "owner".into();
    first
        .factory
        .graph_faults
        .fail_ack
        .store(true, Ordering::SeqCst);
    let tasks = (0..900)
        .map(|index| state(&format!("task-{index:04}-{}", "x".repeat(70)), Canceled))
        .collect();
    set(&root, tasks).await;
    let owner = first.factory.get("owner");
    owner
        .task_session
        .wait_calls(|calls| calls.iter().any(|call| call.kind == "record"))
        .await;
    bounded(async {
        while first.factory.graph_faults.fail_ack.load(Ordering::SeqCst) {
            tokio::task::yield_now().await;
        }
    })
    .await;
    let original = owner
        .task_session
        .calls
        .lock()
        .unwrap()
        .iter()
        .find(|call| call.kind == "record")
        .unwrap()
        .clone();
    let identity = root.identity().clone();
    freeze_after_io_fault(&first, &root).await;
    let blob_count = std::fs::read_dir(first.histories.path.join("scratch/blobs"))
        .unwrap()
        .count();
    assert_eq!(blob_count, 2);
    let second = Harness::with_history(32, first.histories.clone(), first.provider.clone());
    let restored = second.reopen_root(identity).await;
    activate(&restored).await;
    let replay_owner = second.factory.get("owner");
    assert!(replay_owner.task_session.calls.lock().unwrap().is_empty());
    restored
        .get_service::<Scheduler>()
        .unwrap()
        .activate_notifications()
        .join()
        .await
        .unwrap();
    replay_owner
        .task_session
        .wait_calls(|calls| calls.iter().any(|call| call.kind == "record"))
        .await;
    wait_saved(&restored, |graph| graph.notifications.is_empty()).await;
    let repeated = replay_owner
        .task_session
        .calls
        .lock()
        .unwrap()
        .iter()
        .find(|call| call.kind == "record")
        .unwrap()
        .clone();
    assert_eq!(
        (repeated.id, repeated.text),
        (original.id.clone(), original.text)
    );
    assert_eq!(
        std::fs::read_dir(second.histories.path.join("scratch/blobs"))
            .unwrap()
            .count(),
        blob_count
    );
    let events = second.histories.repository.events().replay().await.unwrap();
    assert_eq!(
        events
            .iter()
            .filter(|event| event.has_input_admitted()
                && event.input_admitted().message_id == original.id)
            .count(),
        1
    );
    freeze(&second, &restored).await;
}

#[tokio::test]
async fn nested_graph_outbox_stays_gated_until_parent_worker_reservation_is_ready() {
    let first = Harness::new();
    let held = Gate::new();
    let resumed = Gate::new();
    first.provider.script(
        "a",
        [
            Reply::held("answer", &held),
            Reply::held("answer", &resumed),
        ],
    );
    let root = first.root("owner").await;
    set(&root, vec![dedicated(task("a"))]).await;
    held.entered().await;
    let identity = root.identity().clone();
    freeze(&first, &root).await;
    let root_snapshot = root.agent_state().load().await.unwrap().unwrap();
    let worker = root.children().snapshot_children();
    assert!(worker.is_empty());
    let old_worker = first.factory.get("a");
    let worker_id = old_worker.history.agent_session_id().to_owned();
    let mut child_names = identity.name_path.clone();
    child_names.push("a".into());
    let files = AgentStateFiles::new(
        first
            .histories
            .repository
            .events()
            .database()
            .selected_root()
            .clone(),
    );
    let child_store = files.open(&child_names, &worker_id).await.unwrap();
    let mut graph = child_store.load().await.unwrap().unwrap().tasks;
    let mut entry = root_snapshot.tasks.entries[0].clone();
    entry.task = state("inner", Succeeded);
    entry.borrow_worker_mut().worker_session_id = None;
    entry.borrow_worker_mut().runner = None;
    entry.borrow_worker_mut().pending_stop = None;
    graph.entries = vec![entry];
    graph.complete = true;
    graph.all_resolved = true;
    graph.notifications = vec![
        parrot_domain::values::agent_state::TaskNotificationSnapshot {
            message_id: "nested-notice".into(),
            target_session_id: worker_id,
            text: "nested saved notification".into(),
            wake: true,
            ready: true,
            owner_assignment: None,
        },
    ];
    child_store
        .update(AgentStateUpdate::Tasks(graph))
        .await
        .unwrap();
    let second = Harness::with_history(32, first.histories.clone(), first.provider.clone());
    let restored = second.reopen_root(identity).await;
    let child = restored.children().resolve("a").unwrap();
    let child_scheduler = child.get_service::<Scheduler>().unwrap();
    child_scheduler.resolve_workers().await.unwrap();
    child_scheduler.activate_executions().join().await.unwrap();
    assert!(
        second
            .factory
            .get("a")
            .task_session
            .calls
            .lock()
            .unwrap()
            .is_empty()
    );
    activate(&restored).await;
    resumed.entered().await;
    child_scheduler
        .activate_notifications()
        .join()
        .await
        .unwrap();
    second
        .factory
        .get("a")
        .task_session
        .wait_calls(|calls| {
            calls
                .iter()
                .any(|call| call.kind == "send" && call.id == "nested-notice")
        })
        .await;
    let prompt_id = root_snapshot.tasks.entries[0]
        .borrow_worker()
        .runner
        .as_ref()
        .unwrap()
        .prompt_message_id
        .clone();
    let events = second.histories.repository.events().replay().await.unwrap();
    assert_eq!(
        events
            .iter()
            .filter(|event| event.has_input_admitted()
                && event.input_admitted().message_id == prompt_id)
            .count(),
        1
    );
    resumed.open();
    complete(&restored).await;
    freeze(&second, &restored).await;
}

#[tokio::test]
async fn failed_parent_response_save_keeps_claim_until_recovery_consumes_original_terminal() {
    let first = Harness::new();
    let held = Gate::new();
    first.provider.script("a", [Reply::held("answer", &held)]);
    let root = first.root("owner").await;
    set(&root, vec![dedicated(task("a"))]).await;
    held.entered().await;
    *first.factory.graph_faults.owner.lock().unwrap() = "owner".into();
    first
        .factory
        .graph_faults
        .fail_response
        .store(true, Ordering::SeqCst);
    held.open();
    bounded(async {
        while first
            .factory
            .graph_faults
            .fail_response
            .load(Ordering::SeqCst)
        {
            tokio::task::yield_now().await;
        }
    })
    .await;
    let saved = root.agent_state().load().await.unwrap().unwrap();
    assert!(
        saved.tasks.entries[0]
            .borrow_worker()
            .runner
            .as_ref()
            .unwrap()
            .response_reference
            .is_none()
    );
    let identity = root.identity().clone();
    let worker = root.children().resolve("a").unwrap();
    assert!(
        worker
            .agent_state()
            .load()
            .await
            .unwrap()
            .unwrap()
            .execution
            .unwrap()
            .result_claim
            .is_some()
    );
    freeze_after_io_fault(&first, &root).await;
    let second = Harness::with_history(32, first.histories.clone(), first.provider.clone());
    let restored = second.reopen_root(identity).await;
    activate(&restored).await;
    assert_eq!(
        complete(&restored).await[0].result.as_deref(),
        Some("answer")
    );
    assert_eq!(requests(&second.provider, "a").len(), 1);
    freeze(&second, &restored).await;
}

#[tokio::test]
async fn persisted_stopped_flag_still_releases_matching_canceled_result_claim() {
    let first = Harness::new();
    let held = Gate::new();
    first.provider.script("a", [Reply::held("answer", &held)]);
    let root = first.root("owner").await;
    set(&root, vec![dedicated(task("a"))]).await;
    held.entered().await;
    *first.factory.graph_faults.owner.lock().unwrap() = "owner".into();
    first
        .factory
        .graph_faults
        .fail_stopped
        .store(true, Ordering::SeqCst);
    set(&root, vec![dedicated(state("a", Canceled))]).await;
    wait_saved(&root, |graph| {
        graph.entries[0]
            .borrow_worker()
            .pending_stop
            .as_ref()
            .is_some_and(|stop| stop.stopped)
    })
    .await;
    bounded(async {
        while first
            .factory
            .graph_faults
            .fail_stopped
            .load(Ordering::SeqCst)
        {
            tokio::task::yield_now().await;
        }
    })
    .await;
    let worker = root.children().resolve("a").unwrap();
    assert!(
        worker
            .agent_state()
            .load()
            .await
            .unwrap()
            .unwrap()
            .execution
            .unwrap()
            .result_claim
            .is_some()
    );
    let identity = root.identity().clone();
    freeze_after_io_fault(&first, &root).await;
    let second = Harness::with_history(32, first.histories.clone(), first.provider.clone());
    let restored = second.reopen_root(identity).await;
    activate(&restored).await;
    wait_saved(&restored, |graph| {
        graph.entries[0].borrow_worker().pending_stop.is_none()
    })
    .await;
    assert!(
        restored
            .children()
            .resolve("a")
            .unwrap()
            .agent_state()
            .load()
            .await
            .unwrap()
            .unwrap()
            .execution
            .unwrap()
            .result_claim
            .is_none()
    );
    assert_eq!(requests(&second.provider, "a").len(), 1);
    freeze(&second, &restored).await;
}

#[tokio::test]
async fn restored_dependency_cancellation_stops_saved_runner_before_later_reset() {
    let first = Harness::new();
    let held = Gate::new();
    first
        .provider
        .script("child", [Reply::held("answer", &held)]);
    let root = first.root("owner").await;
    set(
        &root,
        vec![
            dedicated(state("dependency", Succeeded)),
            dedicated(dependent("child", "dependency")),
        ],
    )
    .await;
    held.entered().await;
    let original = root.agent_state().load().await.unwrap().unwrap();
    let old_prompt = original.tasks.entries[1]
        .borrow_worker()
        .runner
        .as_ref()
        .unwrap()
        .prompt_message_id
        .clone();
    let child_id = first
        .factory
        .get("child")
        .history
        .agent_session_id()
        .to_owned();
    let identity = root.identity().clone();
    freeze(&first, &root).await;
    let mut saved = root.agent_state().load().await.unwrap().unwrap();
    saved.tasks.entries[0].task.state = Canceled;
    assert!(
        saved.tasks.entries[1]
            .borrow_worker()
            .pending_stop
            .is_none()
    );
    root.agent_state()
        .update(AgentStateUpdate::Tasks(saved.tasks))
        .await
        .unwrap();
    let second = Harness::with_history(32, first.histories.clone(), first.provider.clone());
    let restored = second.reopen_root(identity).await;
    activate(&restored).await;
    wait_saved(&restored, |graph| {
        graph.entries[1].borrow_worker().pending_stop.is_none()
            && graph.entries[1].borrow_worker().runner.is_none()
    })
    .await;
    assert_eq!(snapshot(&restored).await[1].state, Canceled);
    assert_eq!(requests(&second.provider, "child").len(), 1);
    set(&restored, vec![dedicated(task("dependency"))]).await;
    complete(&restored).await;
    let saved = restored.agent_state().load().await.unwrap().unwrap();
    assert_ne!(
        saved.tasks.entries[1]
            .borrow_worker()
            .runner
            .as_ref()
            .unwrap()
            .prompt_message_id,
        old_prompt
    );
    assert_eq!(
        second.factory.get("child").history.agent_session_id(),
        child_id
    );
    assert_eq!(requests(&second.provider, "child").len(), 2);
    freeze(&second, &restored).await;
}

#[tokio::test]
async fn saved_outcome_finalizes_ownership_and_reserves_new_dependents_before_activation_returns() {
    let first = Harness::new();
    let root = first.root("owner").await;
    set(&root, vec![dedicated(task("a"))]).await;
    complete(&root).await;
    let worker = first.factory.get("a");
    worker
        .adapter
        .session()
        .record(
            vec![parrot_storage::events::ConversationPart::text(
                "unrelated pending input",
            )],
            "unrelated-pending".into(),
            parrot_domain::protocol::messages::parrot::Delivery::DELIVERY_QUEUE,
            Arc::new(NeverCancelled),
        )
        .await
        .unwrap();
    let worker_id = worker.history.agent_session_id().to_owned();
    let identity = root.identity().clone();
    freeze(&first, &root).await;
    let mut saved = root.agent_state().load().await.unwrap().unwrap();
    let mut dependent_entry = saved.tasks.entries[0].clone();
    dependent_entry.task = dedicated(dependent("b", "a"));
    dependent_entry.diagnostic_id = "dependent-diagnostic".into();
    dependent_entry.borrow_worker_mut().worker_session_id = None;
    dependent_entry.borrow_worker_mut().runner = None;
    dependent_entry.borrow_worker_mut().terminal_notification = None;
    saved.tasks.entries[0].task.state = Running;
    saved.tasks.entries.push(dependent_entry);
    saved.tasks.complete = false;
    saved.tasks.all_resolved = false;
    saved.tasks.notifications.clear();
    root.agent_state()
        .update(AgentStateUpdate::Tasks(saved.tasks))
        .await
        .unwrap();
    let second = Harness::with_history(32, first.histories.clone(), first.provider.clone());
    let held = Gate::new();
    second.provider.script("b", [Reply::held("answer", &held)]);
    let restored = second.reopen_root(identity).await;
    let scheduler = restored.get_service::<Scheduler>().unwrap();
    scheduler.resolve_workers().await.unwrap();
    assert!(scheduler.claimed_worker_ids().await.contains(&worker_id));
    bounded(scheduler.activate_executions().join())
        .await
        .unwrap();
    assert_eq!(
        snapshot(&restored)
            .await
            .iter()
            .map(|task| task.state)
            .collect::<Vec<_>>(),
        [Succeeded, Running]
    );
    assert!(!scheduler.claimed_worker_ids().await.contains(&worker_id));
    let dependent_id = second
        .factory
        .get("b")
        .history
        .agent_session_id()
        .to_owned();
    assert!(scheduler.claimed_worker_ids().await.contains(&dependent_id));
    assert!(
        restored
            .children()
            .resolve("b")
            .unwrap()
            .agent_state()
            .load()
            .await
            .unwrap()
            .unwrap()
            .execution
            .is_some()
    );
    assert_eq!(requests(&second.provider, "a").len(), 1);
    held.entered().await;
    held.open();
    complete(&restored).await;
    freeze(&second, &restored).await;
}
