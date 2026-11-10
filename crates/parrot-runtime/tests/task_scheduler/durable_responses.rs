use super::{fixture::*, harness::Harness, support::*};
use parrot_domain::values::{
    agent_state::{AcceptedTaskCall, AgentStateUpdate, TaskCallReceipt, TaskExecutionSnapshot},
    agent_tasks::{ExecutionStatus::*, TaskAssignee, TaskCallIdentity, TaskPayload},
};
use parrot_runtime::task_scheduler::{TaskConcurrencyBudget, TaskConfiguration};
use parrot_storage::conversation::HistoryForkBoundary;
use std::sync::atomic::Ordering;

fn call(sequence: i64, name: &str) -> TaskCallIdentity {
    TaskCallIdentity {
        assistant_sequence: sequence,
        call_id: name.into(),
    }
}

fn broken_response() -> TaskConfiguration {
    TaskConfiguration::defaults(templates_with_content(
        "prompt_templates:\n  agent-task.set-assigned:\n    template: '{{ 1 / 0 }}'\n",
    ))
}

#[tokio::test]
async fn accepted_response_contains_full_work_and_replays_after_completion_and_template_change() {
    let first = Harness::new();
    let root = first.root("owner").await;
    let held = Gate::new();
    first
        .provider
        .script("parallel", [Reply::held("answer", &held)]);
    let mut dependency = state("dependency", Succeeded);
    dependency.result = Some("complete dependency result with {{ literal braces }}".into());
    let mut own = dependent("own", "dependency");
    own.payload = TaskPayload::Instruction("full unique instruction with {{ unchanged }}".into());
    own.acceptance_criteria = "full unique acceptance criteria".into();
    let scheduler = root.get_service::<Scheduler>().unwrap();
    let key = call(10, "actionable");
    let accepted = scheduler
        .set_tool_tasks(
            vec![dependency, own, task("parallel")],
            root.selection().current().unwrap(),
            HistoryForkBoundary::AfterCompletedHistory,
            key.clone(),
        )
        .unwrap()
        .join()
        .await
        .unwrap();
    assert_eq!(accepted.accepted_count, 3);
    for expected in [
        "full unique instruction with {{ unchanged }}",
        "full unique acceptance criteria",
        "complete dependency result with {{ literal braces }}",
        "/owner/parallel",
    ] {
        assert!(accepted.text.contains(expected), "{}", accepted.text);
    }
    held.entered().await;
    assert!(
        !first
            .factory
            .get("owner")
            .task_session
            .calls
            .lock()
            .unwrap()
            .iter()
            .any(|notice| notice.text.contains("AgentTask assigned to self:"))
    );
    let mut finished = state("own", Succeeded);
    finished.dependencies = vec!["dependency".into()];
    finished.payload =
        TaskPayload::Instruction("full unique instruction with {{ unchanged }}".into());
    finished.acceptance_criteria = "full unique acceptance criteria".into();
    finished.result = Some("owner completed".into());
    set(&root, vec![finished]).await;
    held.open();
    complete(&root).await;
    assert_eq!(
        scheduler.lookup_accepted(&key).await.unwrap(),
        Some(accepted.clone())
    );
    scheduler.suspend().join().await.unwrap();
    first.close().await;
    let second = Harness::with_history(32, first.histories.clone(), first.provider.clone());
    *second.factory.configuration.lock().unwrap() = Some(TaskConfiguration::defaults(
        templates_with_content(
            "prompt_templates:\n  agent-task.set-assigned:\n    template: 'Changed response {{ count }}'\n",
        ),
    ));
    let restored = second.reopen_root(root.identity().clone()).await;
    assert_eq!(
        restored
            .get_service::<Scheduler>()
            .unwrap()
            .lookup_accepted(&key)
            .await
            .unwrap(),
        Some(accepted)
    );
    second.close().await;
}

#[tokio::test]
async fn render_failure_blocks_later_batches_and_visibility_without_stranding_settlement() {
    let first = Harness::new();
    *first.factory.configuration.lock().unwrap() = Some(broken_response());
    let root = first.root("owner").await;
    let scheduler = root.get_service::<Scheduler>().unwrap();
    let key = call(11, "preparing");
    assert!(
        scheduler
            .set_tool_tasks(
                vec![task("own")],
                root.selection().current().unwrap(),
                HistoryForkBoundary::AfterCompletedHistory,
                key.clone()
            )
            .unwrap()
            .join()
            .await
            .is_err()
    );
    let saved = root.agent_state().load().await.unwrap().unwrap();
    assert!(matches!(
        saved.tasks.accepted_calls[0].receipt,
        TaskCallReceipt::Preparing { .. }
    ));
    assert!(matches!(
        saved.tasks.entries[0].execution,
        TaskExecutionSnapshot::Owner { .. }
    ));
    assert!(
        scheduler
            .set_tool_tasks(
                vec![task("must-not-exist")],
                root.selection().current().unwrap(),
                HistoryForkBoundary::AfterCompletedHistory,
                call(12, "later")
            )
            .unwrap()
            .join()
            .await
            .is_err()
    );
    let before = scheduler.snapshot().await;
    let mut hidden = before.clone();
    hidden[0].hidden = true;
    assert!(
        scheduler
            .apply_visibility_changes(before, hidden)
            .join()
            .await
            .is_err()
    );
    let after = root.agent_state().load().await.unwrap().unwrap();
    assert_eq!(after.tasks, saved.tasks);
    first.close().await;
    let second = Harness::with_history(32, first.histories.clone(), first.provider.clone());
    let restored = second.reopen_root(root.identity().clone()).await;
    let scheduler = restored.get_service::<Scheduler>().unwrap();
    scheduler.prepare_recovery().await.unwrap();
    scheduler.finalize_preparing().await.unwrap();
    let response = scheduler.lookup_accepted(&key).await.unwrap().unwrap();
    assert!(response.text.contains("Work on own"));
    assert_eq!(scheduler.snapshot().await.len(), 1);
    assert!(first.provider.requests.lock().unwrap().is_empty());
    second.close().await;
}

#[tokio::test]
async fn failed_seal_recovers_registered_identity_once_without_duplicate_capacity_or_prompt() {
    let first = Harness::new();
    let held = Gate::new();
    first
        .provider
        .script("parallel", [Reply::held("answer", &held)]);
    *first.factory.concurrency.lock().unwrap() = Some(TaskConcurrencyBudget::new(0));
    let root = first.root("owner").await;
    *first.factory.graph_faults.owner.lock().unwrap() = "owner".into();
    first
        .factory
        .graph_faults
        .fail_seal
        .store(true, Ordering::SeqCst);
    let scheduler = root.get_service::<Scheduler>().unwrap();
    let key = call(20, "seal-crash");
    assert!(
        scheduler
            .set_tool_tasks(
                vec![task("own"), task("parallel")],
                root.selection().current().unwrap(),
                HistoryForkBoundary::AfterCompletedHistory,
                key.clone()
            )
            .unwrap()
            .join()
            .await
            .is_err()
    );
    let saved = root.agent_state().load().await.unwrap().unwrap();
    assert!(matches!(
        saved.tasks.accepted_calls[0].receipt,
        TaskCallReceipt::Preparing { .. }
    ));
    let worker = saved.tasks.entries[1].borrow_worker().clone();
    assert!(worker.worker_session_id.is_some());
    let prompt_id = worker.runner.as_ref().unwrap().prompt_message_id.clone();
    assert!(first.provider.requests.lock().unwrap().is_empty());
    scheduler.suspend().join().await.unwrap();
    first.close().await;
    let second = Harness::with_history(32, first.histories.clone(), first.provider.clone());
    let budget = TaskConcurrencyBudget::new(0);
    *second.factory.concurrency.lock().unwrap() = Some(budget.clone());
    let restored = second.reopen_root(root.identity().clone()).await;
    let scheduler = restored.get_service::<Scheduler>().unwrap();
    scheduler.resolve_workers().await.unwrap();
    scheduler.prepare_recovery().await.unwrap();
    scheduler.finalize_preparing().await.unwrap();
    let response = scheduler.lookup_accepted(&key).await.unwrap().unwrap();
    assert!(response.text.contains("/owner/parallel"));
    assert_eq!(
        restored
            .agent_state()
            .load()
            .await
            .unwrap()
            .unwrap()
            .tasks
            .entries[1]
            .borrow_worker()
            .worker_session_id,
        worker.worker_session_id
    );
    scheduler.activate_executions().join().await.unwrap();
    held.entered().await;
    budget.set_limit(1);
    set(&restored, vec![task("additional")]).await;
    wait_tasks(&restored, |tasks| tasks[2].state == Succeeded).await;
    assert_eq!(requests(&second.provider, "additional").len(), 1);
    held.open();
    wait_tasks(&restored, |tasks| tasks[1].state == Succeeded).await;
    assert_eq!(requests(&second.provider, "parallel").len(), 1);
    let events = second.histories.repository.events().replay().await.unwrap();
    assert_eq!(
        events
            .iter()
            .filter(|event| event.has_input_admitted()
                && event.input_admitted().message_id == prompt_id)
            .count(),
        1
    );
    let mut finished = state("own", Succeeded);
    finished.result = Some("owner completed".into());
    set(&restored, vec![finished]).await;
    second.close().await;
}

#[tokio::test]
async fn completed_worker_outcome_waits_for_preparing_response_and_recovers_without_provider_repeat()
 {
    let first = Harness::new();
    *first.factory.configuration.lock().unwrap() = Some(broken_response());
    let root = first.root("owner").await;
    let held = Gate::new();
    first
        .provider
        .script("work", [Reply::held("answer", &held)]);
    set(
        &root,
        vec![dedicated(task("work")), dependent("next", "work")],
    )
    .await;
    held.entered().await;
    let scheduler = root.get_service::<Scheduler>().unwrap();
    let key = call(30, "preparing-with-live-child");
    assert!(
        scheduler
            .set_tool_tasks(
                vec![state("bookkeeping", Canceled)],
                root.selection().current().unwrap(),
                HistoryForkBoundary::AfterCompletedHistory,
                key.clone()
            )
            .unwrap()
            .join()
            .await
            .is_err()
    );
    held.open();
    bounded(async {
        loop {
            let saved = root.agent_state().load().await.unwrap().unwrap();
            if saved.tasks.entries[0]
                .borrow_worker()
                .runner
                .as_ref()
                .unwrap()
                .outcome
                .is_some()
            {
                return;
            }
            tokio::task::yield_now().await;
        }
    })
    .await;
    assert!(scheduler.lookup_accepted(&key).await.is_err());
    let saved = root.agent_state().load().await.unwrap().unwrap();
    assert_eq!(saved.tasks.entries[0].task.state, Running);
    assert_eq!(saved.tasks.entries[1].task.state, Pending);
    assert!(matches!(
        saved.tasks.accepted_calls[0].receipt,
        TaskCallReceipt::Preparing { .. }
    ));
    let checkpoint = saved.tasks.entries[0]
        .borrow_worker()
        .runner
        .clone()
        .unwrap();
    let _ = bounded(scheduler.suspend().join()).await;
    let _ = bounded(first.registry.shutdown().join()).await;
    let second = Harness::with_history(32, first.histories.clone(), first.provider.clone());
    let restored = second.reopen_root(root.identity().clone()).await;
    let scheduler = restored.get_service::<Scheduler>().unwrap();
    scheduler.resolve_workers().await.unwrap();
    scheduler.prepare_recovery().await.unwrap();
    scheduler.finalize_preparing().await.unwrap();
    let response = scheduler.lookup_accepted(&key).await.unwrap().unwrap();
    assert!(
        response
            .text
            .contains("No task is currently assigned to self.")
    );
    assert!(response.text.contains("/owner/work"));
    assert_eq!(scheduler.snapshot().await[0].state, Running);
    scheduler.activate_executions().join().await.unwrap();
    wait_tasks(&restored, |tasks| tasks[0].state == Succeeded).await;
    assert_eq!(
        scheduler.capture_detail("next").await.unwrap().assigned_to,
        Some(TaskAssignee::Owner)
    );
    assert_eq!(requests(&second.provider, "work").len(), 1);
    let saved = restored.agent_state().load().await.unwrap().unwrap();
    assert_eq!(
        saved.tasks.entries[0]
            .borrow_worker()
            .runner
            .as_ref()
            .unwrap()
            .prompt_message_id,
        checkpoint.prompt_message_id
    );
    assert_eq!(
        scheduler.lookup_accepted(&key).await.unwrap(),
        Some(response)
    );
    let mut finished = state("next", Succeeded);
    finished.dependencies = vec!["work".into()];
    finished.result = Some("next completed by owner".into());
    set(&restored, vec![finished]).await;
    second.close().await;
}

#[tokio::test]
async fn legacy_count_receipt_uses_unchanged_legacy_template_without_reapplying_tasks() {
    let first = Harness::new();
    let root = first.root("owner").await;
    let key = call(40, "legacy");
    root.agent_state()
        .update(AgentStateUpdate::Tasks(
            parrot_domain::values::agent_state::TaskGraphSnapshot {
                accepted_calls: vec![AcceptedTaskCall {
                    identity: key.clone(),
                    receipt: TaskCallReceipt::LegacyCount { accepted_count: 7 },
                }],
                ..Default::default()
            },
        ))
        .await
        .unwrap();
    first.close().await;
    let second = Harness::with_history(32, first.histories.clone(), first.provider.clone());
    let restored = second.reopen_root(root.identity().clone()).await;
    let response = restored
        .get_service::<Scheduler>()
        .unwrap()
        .lookup_accepted(&key)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(response.accepted_count, 7);
    assert_eq!(
        response.text,
        "AgentTask update accepted (7 tasks). State changes are delivered as notifications."
    );
    assert!(second.provider.requests.lock().unwrap().is_empty());
    second.close().await;
}

#[tokio::test]
async fn capacity_render_failure_keeps_preparing_and_does_not_poison_owned_suspension() {
    let h = Harness::new();
    *h.factory.configuration.lock().unwrap() = Some(broken_response());
    let budget = TaskConcurrencyBudget::new(0);
    *h.factory.concurrency.lock().unwrap() = Some(budget.clone());
    let root = h.root("owner").await;
    let held = Gate::new();
    h.provider.script("held", [Reply::held("answer", &held)]);
    set(
        &root,
        vec![dedicated(task("held")), dedicated(task("waiting"))],
    )
    .await;
    held.entered().await;
    let scheduler = root.get_service::<Scheduler>().unwrap();
    assert!(
        scheduler
            .set_tool_tasks(
                vec![task("own")],
                root.selection().current().unwrap(),
                HistoryForkBoundary::AfterCompletedHistory,
                call(50, "capacity-preparing")
            )
            .unwrap()
            .join()
            .await
            .is_err()
    );
    budget.set_limit(1);
    bounded(async {
        loop {
            if h.factory
                .task_diagnostics
                .0
                .lock()
                .unwrap()
                .iter()
                .any(|event| {
                    event.operation == "capacity" && event.error_code == Some("unexpected")
                })
            {
                return;
            }
            tokio::task::yield_now().await;
        }
    })
    .await;
    let saved = root.agent_state().load().await.unwrap().unwrap();
    assert!(matches!(
        saved.tasks.accepted_calls[0].receipt,
        TaskCallReceipt::Preparing { .. }
    ));
    assert_eq!(requests(&h.provider, "waiting").len(), 0);
    bounded(scheduler.suspend().join()).await.unwrap();
    h.close().await;
}

#[tokio::test]
async fn rich_response_suppresses_captured_retry_for_the_same_self_generation() {
    let h = Harness::new();
    let root = h.root("owner").await;
    let owner = h.factory.get("owner");
    owner.task_session.send_failures.store(1, Ordering::SeqCst);
    set(&root, vec![task("own")]).await;
    owner
        .task_session
        .wait_calls(|calls| {
            calls.iter().any(|call| {
                call.kind == "send" && call.text.contains("AgentTask assigned to self: own")
            })
        })
        .await;
    let before = root
        .agent_state()
        .load()
        .await
        .unwrap()
        .unwrap()
        .tasks
        .entries[0]
        .execution
        .clone();
    let scheduler = root.get_service::<Scheduler>().unwrap();
    let response = scheduler
        .set_tool_tasks(
            vec![state("bookkeeping", Canceled)],
            root.selection().current().unwrap(),
            HistoryForkBoundary::AfterCompletedHistory,
            call(60, "seal-current-self"),
        )
        .unwrap()
        .join()
        .await
        .unwrap();
    assert!(response.text.contains("AgentTask assigned to self: own"));
    assert_eq!(
        root.agent_state()
            .load()
            .await
            .unwrap()
            .unwrap()
            .tasks
            .entries[0]
            .execution,
        before
    );
    owner
        .task_session
        .wait_calls(|calls| {
            calls
                .iter()
                .any(|call| call.kind == "record" && call.text.contains("bookkeeping: canceled"))
        })
        .await;
    assert_eq!(
        owner
            .task_session
            .calls
            .lock()
            .unwrap()
            .iter()
            .filter(
                |call| call.kind == "send" && call.text.contains("AgentTask assigned to self: own")
            )
            .count(),
        1
    );
    assert!(requests(&h.provider, "owner").is_empty());
    h.close().await;
}

#[tokio::test]
async fn preparing_replay_keeps_sealing_owned_after_its_lookup_observer_is_canceled() {
    let first = Harness::new();
    *first.factory.configuration.lock().unwrap() = Some(broken_response());
    let root = first.root("owner").await;
    let key = call(70, "loaded-preparing");
    assert!(
        root.get_service::<Scheduler>()
            .unwrap()
            .set_tool_tasks(
                vec![task("own")],
                root.selection().current().unwrap(),
                HistoryForkBoundary::AfterCompletedHistory,
                key.clone()
            )
            .unwrap()
            .join()
            .await
            .is_err()
    );
    first.close().await;
    let second = Harness::with_history(32, first.histories.clone(), first.provider.clone());
    let restored = second.reopen_root(root.identity().clone()).await;
    let scheduler = restored.get_service::<Scheduler>().unwrap();
    scheduler.prepare_recovery().await.unwrap();
    *second.factory.graph_faults.owner.lock().unwrap() = "owner".into();
    let gate = Gate::new();
    *second.factory.graph_faults.seal_gate.lock().unwrap() = Some(gate.clone());
    let replay = {
        let scheduler = scheduler.clone();
        let key = key.clone();
        tokio::spawn(async move { scheduler.lookup_accepted(&key).await })
    };
    gate.entered().await;
    assert!(matches!(
        restored
            .agent_state()
            .load()
            .await
            .unwrap()
            .unwrap()
            .tasks
            .accepted_calls[0]
            .receipt,
        TaskCallReceipt::Preparing { .. }
    ));
    replay.abort();
    assert!(replay.await.unwrap_err().is_cancelled());
    gate.open();
    bounded(async {
        loop {
            let saved = restored.agent_state().load().await.unwrap().unwrap();
            if matches!(
                saved.tasks.accepted_calls[0].receipt,
                TaskCallReceipt::Accepted { .. }
            ) {
                return;
            }
            tokio::task::yield_now().await;
        }
    })
    .await;
    let accepted = bounded(scheduler.lookup_accepted(&key))
        .await
        .unwrap()
        .unwrap();
    assert!(accepted.text.contains("Work on own"));
    let later = scheduler
        .set_tool_tasks(
            vec![state("later", Canceled)],
            restored.selection().current().unwrap(),
            HistoryForkBoundary::AfterCompletedHistory,
            call(70, "after-replay"),
        )
        .unwrap();
    assert_eq!(bounded(later.join()).await.unwrap().accepted_count, 1);
    assert_eq!(scheduler.snapshot().await.len(), 2);
    second.close().await;
}

#[tokio::test]
async fn completing_run_does_not_reacquire_capacity_while_sealing_preparing_response() {
    let h = Harness::new();
    let configuration = TaskConfiguration::defaults(templates_with_content(
        "prompt_templates:\n  agent-task.set-assigned:\n    template: '{% if dedicated|length < 2 %}{{ 1 / 0 }}{% else %}sealed {{ dedicated[0].agent_path }} {{ dedicated[1].agent_path }}{% endif %}'\n",
    ));
    *h.factory.configuration.lock().unwrap() = Some(configuration);
    *h.factory.concurrency.lock().unwrap() = Some(TaskConcurrencyBudget::new(0));
    let root = h.root("owner").await;
    let first = Gate::new();
    let second = Gate::new();
    h.provider.script("first", [Reply::held("answer", &first)]);
    h.provider
        .script("second", [Reply::held("answer", &second)]);
    set(
        &root,
        vec![dedicated(task("first")), dedicated(task("second"))],
    )
    .await;
    first.entered().await;
    let scheduler = root.get_service::<Scheduler>().unwrap();
    let key = call(80, "capacity-sensitive-response");
    assert!(
        scheduler
            .set_tool_tasks(
                vec![task("own")],
                root.selection().current().unwrap(),
                HistoryForkBoundary::AfterCompletedHistory,
                key.clone()
            )
            .unwrap()
            .join()
            .await
            .is_err()
    );
    first.open();
    second.entered().await;
    let response = scheduler.lookup_accepted(&key).await.unwrap().unwrap();
    assert_eq!(response.text, "sealed /owner/first /owner/second");
    assert_eq!(scheduler.snapshot().await[0].state, Succeeded);
    second.open();
    wait_tasks(&root, |tasks| tasks[1].state == Succeeded).await;
    set(&root, vec![dedicated(task("after-completion"))]).await;
    wait_tasks(&root, |tasks| tasks[3].state == Succeeded).await;
    assert_eq!(requests(&h.provider, "after-completion").len(), 1);
    let mut finished = state("own", Succeeded);
    finished.result = Some("owner completed".into());
    set(&root, vec![finished]).await;
    h.close().await;
}
