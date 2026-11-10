use super::{fixture::*, harness::Harness, support::*};
use parrot_domain::values::agent_tasks::ExecutionStatus::{Pending, Running, Succeeded};
use parrot_runtime::task_scheduler::TaskConcurrencyBudget;
use std::sync::atomic::Ordering;

async fn wait_capacity_failure(h: &Harness) {
    bounded(async {
        loop {
            if h.factory
                .task_diagnostics
                .0
                .lock()
                .unwrap()
                .iter()
                .any(|event| event.operation == "capacity" && event.error_code == Some("io"))
            {
                return;
            }
            tokio::task::yield_now().await;
        }
    })
    .await;
}

#[tokio::test]
async fn suspension_joins_held_executions_and_releases_capacity_after_observer_save_failure() {
    let h = Harness::new();
    *h.factory.concurrency.lock().unwrap() = Some(TaskConcurrencyBudget::new(2));
    let first = h.root("first-owner").await;
    let failing = h.root("failing-owner").await;
    let gates: Vec<_> = (0..4).map(|_| Gate::new()).collect();
    let closes: Vec<_> = (0..2).map(|_| Gate::new()).collect();
    for (name, gate) in ["a", "b"].into_iter().zip(&gates) {
        h.provider.script(name, [Reply::held("answer", gate)]);
    }
    for ((name, gate), close) in ["c", "d"].into_iter().zip(&gates[2..]).zip(&closes) {
        h.provider.script(
            name,
            [Reply {
                close_gate: Some(close.clone()),
                ..Reply::held("answer", gate)
            }],
        );
    }
    set(&first, vec![dedicated(task("a")), dedicated(task("b"))]).await;
    gates[0].entered().await;
    gates[1].entered().await;
    set(
        &failing,
        vec![
            dedicated(task("c")),
            dedicated(task("d")),
            dedicated(task("pending")),
        ],
    )
    .await;
    gates[2].entered().await;
    gates[3].entered().await;
    assert_eq!(snapshot(&failing).await[2].state, Pending);
    *h.factory.graph_faults.owner.lock().unwrap() = "failing-owner".into();
    h.factory
        .graph_faults
        .fail_before
        .store(true, Ordering::SeqCst);
    gates[1].open();
    wait_capacity_failure(&h).await;

    let suspension = failing.get_service::<Scheduler>().unwrap().suspend();
    closes[0].entered().await;
    assert!(!suspension.is_finished());
    closes[0].open();
    closes[1].entered().await;
    assert!(!suspension.is_finished());
    closes[1].open();
    let failure = bounded(suspension.join()).await.unwrap_err();
    assert_eq!(failure.code(), "io");
    assert!(failure.to_string().contains("graph snapshot write failed"));
    assert!(requests(&h.provider, "pending").is_empty());

    let third = h.root("third-owner").await;
    let available: Vec<_> = (0..3).map(|_| Gate::new()).collect();
    for (name, gate) in ["e", "f", "g"].into_iter().zip(&available) {
        h.provider.script(name, [Reply::held("answer", gate)]);
    }
    set(
        &third,
        vec![
            dedicated(task("e")),
            dedicated(task("f")),
            dedicated(task("g")),
        ],
    )
    .await;
    for gate in &available {
        gate.entered().await;
    }
    for gate in &available {
        gate.open();
    }
    gates[0].open();
    complete(&first).await;
    complete(&third).await;
    assert_eq!(
        bounded(h.registry.shutdown().join()).await.unwrap_err(),
        failure
    );
}

#[tokio::test]
async fn observer_save_failure_rolls_back_admission_before_shutdown() {
    let h = Harness::new();
    *h.factory.concurrency.lock().unwrap() = Some(TaskConcurrencyBudget::new(1));
    let first = h.root("first-owner").await;
    let failing = h.root("failing-owner").await;
    let gates: Vec<_> = (0..3).map(|_| Gate::new()).collect();
    for (name, gate) in ["a", "b", "c"].into_iter().zip(&gates) {
        h.provider.script(name, [Reply::held("answer", gate)]);
    }
    set(&first, vec![dedicated(task("a")), dedicated(task("b"))]).await;
    gates[0].entered().await;
    gates[1].entered().await;
    set(
        &failing,
        vec![dedicated(task("c")), dedicated(task("pending"))],
    )
    .await;
    gates[2].entered().await;
    assert_eq!(snapshot(&failing).await[1].state, Pending);
    *h.factory.graph_faults.owner.lock().unwrap() = "failing-owner".into();
    h.factory
        .graph_faults
        .fail_before
        .store(true, Ordering::SeqCst);
    gates[1].open();
    wait_capacity_failure(&h).await;

    let third = h.root("third-owner").await;
    let available: Vec<_> = (0..2).map(|_| Gate::new()).collect();
    for (name, gate) in ["d", "e"].into_iter().zip(&available) {
        h.provider.script(name, [Reply::held("answer", gate)]);
    }
    set(&third, vec![dedicated(task("d")), dedicated(task("e"))]).await;
    for gate in &available {
        gate.entered().await;
    }
    assert!(requests(&h.provider, "pending").is_empty());
    for gate in &available {
        gate.open();
    }
    let failure = bounded(failing.get_service::<Scheduler>().unwrap().suspend().join())
        .await
        .unwrap_err();
    assert_eq!(failure.code(), "io");
    gates[0].open();
    complete(&first).await;
    complete(&third).await;
    assert_eq!(
        bounded(h.registry.shutdown().join()).await.unwrap_err(),
        failure
    );
}

#[tokio::test]
async fn default_budget_leaves_fourth_pending_and_free_task_exit_returns_extra_slot() {
    let h = Harness::new();
    let root = h.root("owner").await;
    let gates: Vec<_> = (0..4).map(|_| Gate::new()).collect();
    for (name, gate) in ["a", "b", "c", "d"].into_iter().zip(&gates) {
        h.provider.script(name, [Reply::held("answer", gate)]);
    }
    set(
        &root,
        vec![
            dedicated(task("a")),
            dedicated(task("b")),
            dedicated(task("c")),
            dedicated(task("d")),
        ],
    )
    .await;
    for gate in &gates[..3] {
        gate.entered().await;
    }
    assert_eq!(
        snapshot(&root)
            .await
            .iter()
            .map(|task| task.state)
            .collect::<Vec<_>>(),
        [Running, Running, Running, Pending]
    );
    assert!(
        h.registry
            .snapshot_scopes()
            .iter()
            .all(|scope| scope.identity().session.name != "d")
    );
    gates[0].open();
    gates[3].entered().await;
    for gate in &gates[1..] {
        gate.open();
    }
    complete(&root).await;
    h.close().await;
}

#[tokio::test]
async fn increasing_limit_wakes_pending_tasks_without_another_tool_update() {
    let h = Harness::new();
    let budget = TaskConcurrencyBudget::new(0);
    *h.factory.concurrency.lock().unwrap() = Some(budget.clone());
    let root = h.root("owner").await;
    let gates: Vec<_> = (0..3).map(|_| Gate::new()).collect();
    for (name, gate) in ["a", "b", "c"].into_iter().zip(&gates) {
        h.provider.script(name, [Reply::held("answer", gate)]);
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
    gates[0].entered().await;
    assert_eq!(snapshot(&root).await[1].state, Pending);
    budget.set_limit(2);
    assert_eq!(budget.get_limit(), 2);
    gates[1].entered().await;
    gates[2].entered().await;
    assert!(
        snapshot(&root)
            .await
            .iter()
            .all(|task| task.state == Running)
    );
    for gate in &gates {
        gate.open();
    }
    complete(&root).await;
    h.close().await;
}

#[tokio::test]
async fn decreasing_limit_to_zero_keeps_held_runs_and_each_owners_free_allowance() {
    let h = Harness::new();
    let budget = TaskConcurrencyBudget::new(2);
    *h.factory.concurrency.lock().unwrap() = Some(budget.clone());
    let root = h.root("owner").await;
    let other = h.root("other-owner").await;
    let gates: Vec<_> = (0..6).map(|_| Gate::new()).collect();
    for (name, gate) in ["a", "b", "c", "d", "e", "f"].into_iter().zip(&gates) {
        h.provider.script(name, [Reply::held("answer", gate)]);
    }
    set(
        &root,
        vec![
            dedicated(task("a")),
            dedicated(task("b")),
            dedicated(task("c")),
            dedicated(task("d")),
        ],
    )
    .await;
    for gate in &gates[..3] {
        gate.entered().await;
    }
    budget.set_limit(0);
    set(&other, vec![dedicated(task("e")), dedicated(task("f"))]).await;
    gates[4].entered().await;
    assert_eq!(snapshot(&other).await[1].state, Pending);
    assert_eq!(
        snapshot(&root)
            .await
            .iter()
            .map(|task| task.state)
            .collect::<Vec<_>>(),
        [Running, Running, Running, Pending]
    );
    for index in [0, 1] {
        gates[index].open();
        let tasks = wait_tasks(&root, |tasks| tasks[index].state == Succeeded).await;
        assert_eq!(tasks[2].state, Running);
        assert_eq!(tasks[3].state, Pending);
        assert!(requests(&h.provider, "d").is_empty());
    }
    gates[2].open();
    gates[3].entered().await;
    gates[4].open();
    gates[5].entered().await;
    gates[3].open();
    gates[5].open();
    complete(&root).await;
    complete(&other).await;
    h.close().await;
}

#[tokio::test]
async fn another_owner_wakes_without_a_tool_update_when_shared_capacity_is_released() {
    let h = Harness::new();
    *h.factory.concurrency.lock().unwrap() = Some(TaskConcurrencyBudget::new(1));
    let first = h.root("first-owner").await;
    let second = h.root("second-owner").await;
    let gates: Vec<_> = (0..4).map(|_| Gate::new()).collect();
    for (name, gate) in ["a", "b", "c", "d"].into_iter().zip(&gates) {
        h.provider.script(name, [Reply::held("answer", gate)]);
    }
    set(&first, vec![dedicated(task("a")), dedicated(task("b"))]).await;
    gates[0].entered().await;
    gates[1].entered().await;
    set(&second, vec![dedicated(task("c")), dedicated(task("d"))]).await;
    gates[2].entered().await;
    assert_eq!(snapshot(&second).await[1].state, Pending);
    gates[0].open();
    gates[3].entered().await;
    for gate in &gates[1..] {
        gate.open();
    }
    complete(&first).await;
    complete(&second).await;
    h.close().await;
}

#[tokio::test]
async fn zero_budget_allows_nested_owner_and_wakes_its_next_task_on_free_exit() {
    let h = Harness::new();
    *h.factory.concurrency.lock().unwrap() = Some(TaskConcurrencyBudget::new(0));
    let root = h.root("owner").await;
    let parent_gate = Gate::new();
    let child_gate = Gate::new();
    h.provider
        .script("parent", [Reply::held("answer", &parent_gate)]);
    h.provider.script("next", [Reply::text("answer")]);
    h.provider
        .script("nested", [Reply::held("answer", &child_gate)]);
    h.provider.script("nested-next", [Reply::text("answer")]);
    set(
        &root,
        vec![dedicated(task("parent")), dedicated(task("next"))],
    )
    .await;
    parent_gate.entered().await;
    let parent = h
        .registry
        .snapshot_scopes()
        .into_iter()
        .find(|scope| scope.identity().session.name == "parent")
        .unwrap();
    set(
        &parent,
        vec![dedicated(task("nested")), dedicated(task("nested-next"))],
    )
    .await;
    child_gate.entered().await;
    assert_eq!(snapshot(&root).await[1].state, Pending);
    assert_eq!(snapshot(&parent).await[1].state, Pending);
    child_gate.open();
    let tasks = complete(&parent).await;
    assert!(tasks.iter().all(|task| task.state == Succeeded));
    parent_gate.open();
    complete(&root).await;
    h.close().await;
}

#[tokio::test]
async fn cancellation_returns_capacity_once_and_pending_work_still_runs() {
    let h = Harness::new();
    *h.factory.concurrency.lock().unwrap() = Some(TaskConcurrencyBudget::new(0));
    let root = h.root("owner").await;
    let gate = Gate::new();
    h.provider.script("a", [Reply::held("answer", &gate)]);
    h.provider.script("b", [Reply::text("answer")]);
    set(&root, vec![dedicated(task("a")), dedicated(task("b"))]).await;
    gate.entered().await;
    set(
        &root,
        vec![dedicated(state(
            "a",
            parrot_domain::values::agent_tasks::ExecutionStatus::Canceled,
        ))],
    )
    .await;
    wait_tasks(&root, |tasks| tasks[1].state == Succeeded).await;
    h.close().await;
}

#[tokio::test]
async fn retry_keeps_free_admission_until_the_task_finishes() {
    use parrot_domain::values::agent_tasks::{AcceptanceVerdict, LeafResponse};
    let h = Harness::new();
    *h.factory.concurrency.lock().unwrap() = Some(TaskConcurrencyBudget::new(0));
    let root = h.root("owner").await;
    let retry_gate = Gate::new();
    h.factory.parser.answer(
        "retry",
        LeafResponse {
            result: "partial".into(),
            verdict: AcceptanceVerdict::RejectAndRetry {
                feedback: "again".into(),
                payload: None,
                replacement_result: None,
            },
        },
    );
    h.provider.script(
        "a",
        [Reply::text("retry"), Reply::held("answer", &retry_gate)],
    );
    h.provider.script("b", [Reply::text("answer")]);
    set(&root, vec![dedicated(task("a")), dedicated(task("b"))]).await;
    retry_gate.entered().await;
    assert_eq!(snapshot(&root).await[1].state, Pending);
    assert!(requests(&h.provider, "b").is_empty());
    retry_gate.open();
    complete(&root).await;
    h.close().await;
}

#[tokio::test]
async fn failed_worker_construction_rolls_back_the_free_admission() {
    let h = Harness::new();
    *h.factory.concurrency.lock().unwrap() = Some(TaskConcurrencyBudget::new(0));
    let root = h.root("owner").await;
    h.factory.plans.lock().unwrap().insert(
        "a".into(),
        ScopePlan {
            failure: Some(parrot_runtime::children::ChildError::failure(
                "unexpected",
                "scripted construction failure",
            )),
            ..ScopePlan::default()
        },
    );
    h.provider.script("b", [Reply::text("answer")]);
    set(&root, vec![dedicated(task("a")), dedicated(task("b"))]).await;
    wait_tasks(&root, |tasks| tasks[1].state == Succeeded).await;
    assert_eq!(
        snapshot(&root).await[0].state,
        parrot_domain::values::agent_tasks::ExecutionStatus::Failed
    );
    h.close().await;
}

#[tokio::test]
async fn zero_budget_replacement_reserves_after_old_run_exits_before_stop_record_finishes() {
    let h = Harness::new();
    *h.factory.concurrency.lock().unwrap() = Some(TaskConcurrencyBudget::new(0));
    let root = h.root("owner").await;
    let old = Gate::new();
    let record = Gate::new();
    let replacement = Gate::new();
    h.provider.script(
        "a",
        [
            Reply::held("answer", &old),
            Reply::held("answer", &replacement),
        ],
    );
    set(&root, vec![dedicated(task("a"))]).await;
    old.entered().await;
    *h.factory.get("a").task_session.record_gate.lock().unwrap() = Some(record.clone());
    let mut update = dedicated(task("a"));
    update.acceptance_criteria = "changed".into();
    set(&root, vec![update]).await;
    record.entered().await;
    wait_tasks(&root, |tasks| tasks[0].state == Running).await;
    assert_eq!(requests(&h.provider, "a").len(), 1);
    record.open();
    replacement.entered().await;
    replacement.open();
    complete(&root).await;
    h.close().await;
}

#[tokio::test]
async fn canceling_reserved_replacement_returns_capacity_to_another_owner() {
    use parrot_domain::values::agent_tasks::ExecutionStatus::Canceled;
    let h = Harness::new();
    *h.factory.concurrency.lock().unwrap() = Some(TaskConcurrencyBudget::new(1));
    let root = h.root("owner").await;
    let other = h.root("other-owner").await;
    let gates: Vec<_> = (0..4).map(|_| Gate::new()).collect();
    for (name, gate) in ["a", "b", "c", "d"].into_iter().zip(&gates) {
        h.provider.script(name, [Reply::held("answer", gate)]);
    }
    set(&root, vec![dedicated(task("a")), dedicated(task("b"))]).await;
    gates[0].entered().await;
    gates[1].entered().await;
    let record = Gate::new();
    *h.factory.get("a").task_session.record_gate.lock().unwrap() = Some(record.clone());
    let mut update = dedicated(task("a"));
    update.acceptance_criteria = "changed".into();
    set(&root, vec![update]).await;
    record.entered().await;
    wait_tasks(&root, |tasks| tasks[0].state == Running).await;
    set(&other, vec![dedicated(task("c")), dedicated(task("d"))]).await;
    gates[2].entered().await;
    assert_eq!(snapshot(&other).await[1].state, Pending);
    set(&root, vec![dedicated(state("a", Canceled))]).await;
    gates[3].entered().await;
    record.open();
    for gate in &gates[1..] {
        gate.open();
    }
    complete(&other).await;
    h.close().await;
}
