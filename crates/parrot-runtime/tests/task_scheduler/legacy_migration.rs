use super::{fixture::*, harness::Harness, support::*};
use parrot_domain::values::{
    agent_state::{
        AGENT_STATE_FILE_NAME, ExecutionOutcome, TaskExecutionSnapshot, TaskGraphMigrationSnapshot,
        TaskMigrationDisposition,
    },
    agent_tasks::{ExecutionStatus::*, TaskAssignee},
};
use parrot_runtime::children::Scope;
use std::{
    path::PathBuf,
    sync::{Arc, atomic::Ordering},
};

#[path = "../../../../tests/support/legacy_agent_state.rs"]
mod legacy_state;

fn state_path(harness: &Harness, scope: &Scope<Security>) -> PathBuf {
    let mut path = harness.histories.path.join("root-agents");
    for name in &scope.identity().name_path {
        path.push(name);
    }
    path.join(AGENT_STATE_FILE_NAME)
}

async fn freeze(harness: &Harness, root: &Arc<Scope<Security>>) {
    root.get_service::<Scheduler>()
        .unwrap()
        .suspend()
        .join()
        .await
        .unwrap();
    harness.registry.shutdown().join().await.unwrap();
}

async fn legacy_leaf() -> (Harness, Arc<Scope<Security>>) {
    let harness = Harness::new();
    let root = harness.root("owner").await;
    let held = Gate::new();
    harness
        .provider
        .script("work", [Reply::held("answer", &held)]);
    let mut done = state("done", Succeeded);
    done.result = Some("preserved result".into());
    let mut failed = state("failed", Failed);
    failed.failure = Some("preserved failure".into());
    set(
        &root,
        vec![
            dedicated(task("work")),
            done,
            failed,
            state("canceled", Canceled),
        ],
    )
    .await;
    held.entered().await;
    freeze(&harness, &root).await;
    legacy_state::rewrite_snapshot_as_version_one(&state_path(&harness, &root), &[0]);
    (harness, root)
}

async fn prepare(root: &Arc<Scope<Security>>) -> Arc<Scheduler> {
    let scheduler = root.get_service::<Scheduler>().unwrap();
    scheduler.resolve_workers().await.unwrap();
    let owned = scheduler.capture_owned_workers().await.unwrap();
    scheduler
        .prepare_migration(TaskMigrationDisposition::Reassign, owned)
        .await
        .unwrap();
    scheduler
}

#[tokio::test]
async fn legacy_leaf_stops_before_self_assignment_and_second_reopen_does_not_migrate_again() {
    let (first, root) = legacy_leaf().await;
    let second = Harness::with_history(32, first.histories.clone(), first.provider.clone());
    let restored = second.reopen_root(root.identity().clone()).await;
    let scheduler = prepare(&restored).await;
    let worker = restored.children().resolve("work").unwrap();
    let before = snapshot(&restored).await;
    assert_eq!(before[0].state, Pending);
    assert!(matches!(
        scheduler.capture_migration().await,
        TaskGraphMigrationSnapshot::Retiring { .. }
    ));
    assert_eq!(requests(&second.provider, "work").len(), 1);
    scheduler.retire_legacy().await.unwrap();
    assert_eq!(
        scheduler.capture_migration().await,
        TaskGraphMigrationSnapshot::Current
    );
    let execution = worker
        .agent_state()
        .load()
        .await
        .unwrap()
        .unwrap()
        .execution
        .unwrap();
    assert_eq!(
        execution.terminal.unwrap().outcome,
        ExecutionOutcome::Canceled
    );
    assert_eq!(execution.result_claim, None);
    assert_eq!(execution.parent_notification, None);
    assert!(
        !second
            .factory
            .get("work")
            .adapter
            .session()
            .has_recovery_work()
            .await
            .unwrap()
    );
    assert_eq!(&snapshot(&restored).await[1..], &before[1..]);
    scheduler.activate_executions().join().await.unwrap();
    assert_eq!(
        scheduler.capture_detail("work").await.unwrap().assigned_to,
        Some(TaskAssignee::Owner)
    );
    assert_eq!(requests(&second.provider, "work").len(), 1);
    let assignment = restored
        .agent_state()
        .load()
        .await
        .unwrap()
        .unwrap()
        .tasks
        .entries[0]
        .execution
        .clone();
    assert!(matches!(assignment, TaskExecutionSnapshot::Owner { .. }));
    freeze(&second, &restored).await;
    let third = Harness::with_history(32, first.histories.clone(), first.provider.clone());
    let reopened = third.reopen_root(root.identity().clone()).await;
    let scheduler = reopened.get_service::<Scheduler>().unwrap();
    assert_eq!(
        scheduler.capture_migration().await,
        TaskGraphMigrationSnapshot::Current
    );
    scheduler.activate_executions().join().await.unwrap();
    assert_eq!(
        reopened
            .agent_state()
            .load()
            .await
            .unwrap()
            .unwrap()
            .tasks
            .entries[0]
            .execution,
        assignment
    );
    assert_eq!(requests(&third.provider, "work").len(), 1);
    assert!(
        !third
            .factory
            .get("work")
            .adapter
            .session()
            .has_recovery_work()
            .await
            .unwrap()
    );
    freeze(&third, &reopened).await;
}

#[tokio::test]
async fn legacy_retirement_joins_claim_acknowledgement_instead_of_early_readiness() {
    let (first, root) = legacy_leaf().await;
    let second = Harness::with_history(32, first.histories.clone(), first.provider.clone());
    let restored = second.reopen_root(root.identity().clone()).await;
    let scheduler = prepare(&restored).await;
    let gate = Gate::new();
    *second
        .factory
        .get("work")
        .task_session
        .retired_ack_gate
        .lock()
        .unwrap() = Some(gate.clone());
    let retiring = scheduler.clone();
    let operation =
        parrot_runtime::children::Operation::start(async move { retiring.retire_legacy().await });
    gate.entered().await;
    assert!(!operation.is_finished());
    assert!(matches!(
        scheduler.capture_migration().await,
        TaskGraphMigrationSnapshot::Retiring { .. }
    ));
    assert_eq!(
        scheduler.capture_detail("work").await.unwrap().task.state,
        Pending
    );
    assert!(scheduler.activate_executions().join().await.is_err());
    let worker = restored.children().resolve("work").unwrap();
    assert!(
        scheduler
            .claimed_worker_ids()
            .await
            .contains(&worker.identity().session.session_id)
    );
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
    gate.open();
    operation.join().await.unwrap();
    scheduler.activate_executions().join().await.unwrap();
    assert_eq!(
        scheduler.capture_detail("work").await.unwrap().assigned_to,
        Some(TaskAssignee::Owner)
    );
    freeze(&second, &restored).await;
}

#[tokio::test]
async fn legacy_retirement_resumes_after_cancellation_before_claim_acknowledgement() {
    let (first, root) = legacy_leaf().await;
    let second = Harness::with_history(32, first.histories.clone(), first.provider.clone());
    let restored = second.reopen_root(root.identity().clone()).await;
    let scheduler = prepare(&restored).await;
    second
        .factory
        .get("work")
        .task_session
        .ack_failures
        .store(1, Ordering::SeqCst);
    assert_eq!(scheduler.retire_legacy().await.unwrap_err().code(), "io");
    let saved = restored.agent_state().load().await.unwrap().unwrap();
    assert!(matches!(
        saved.tasks.migration,
        TaskGraphMigrationSnapshot::Retiring { .. }
    ));
    assert!(matches!(
        saved.tasks.entries[0].execution,
        TaskExecutionSnapshot::RetiringLegacy { .. }
    ));
    let worker = restored.children().resolve("work").unwrap();
    let stopped = worker
        .agent_state()
        .load()
        .await
        .unwrap()
        .unwrap()
        .execution
        .unwrap();
    assert_eq!(
        stopped.terminal.unwrap().outcome,
        ExecutionOutcome::Canceled
    );
    assert!(stopped.result_claim.is_some());
    assert!(
        scheduler
            .claimed_worker_ids()
            .await
            .contains(&worker.identity().session.session_id)
    );
    assert_eq!(scheduler.suspend().join().await.unwrap_err().code(), "io");
    assert_eq!(
        second.registry.shutdown().join().await.unwrap_err().code(),
        "io"
    );
    let third = Harness::with_history(32, first.histories.clone(), first.provider.clone());
    let reopened = third.reopen_root(root.identity().clone()).await;
    let scheduler = prepare(&reopened).await;
    scheduler.retire_legacy().await.unwrap();
    scheduler.activate_executions().join().await.unwrap();
    assert_eq!(
        scheduler.capture_detail("work").await.unwrap().assigned_to,
        Some(TaskAssignee::Owner)
    );
    assert_eq!(requests(&third.provider, "work").len(), 1);
    assert_eq!(
        reopened
            .children()
            .resolve("work")
            .unwrap()
            .agent_state()
            .load()
            .await
            .unwrap()
            .unwrap()
            .execution
            .unwrap()
            .result_claim,
        None
    );
    freeze(&third, &reopened).await;
}

#[tokio::test]
async fn failed_retirement_intent_does_not_cancel_or_launch_old_work() {
    let (first, root) = legacy_leaf().await;
    let second = Harness::with_history(32, first.histories.clone(), first.provider.clone());
    let restored = second.reopen_root(root.identity().clone()).await;
    let scheduler = restored.get_service::<Scheduler>().unwrap();
    scheduler.resolve_workers().await.unwrap();
    let worker = restored.children().resolve("work").unwrap();
    let execution = worker
        .agent_state()
        .load()
        .await
        .unwrap()
        .unwrap()
        .execution;
    *second.factory.graph_faults.owner.lock().unwrap() = "owner".into();
    second
        .factory
        .graph_faults
        .fail_before
        .store(true, Ordering::SeqCst);
    let owned = scheduler.capture_owned_workers().await.unwrap();
    assert_eq!(
        scheduler
            .prepare_migration(TaskMigrationDisposition::Reassign, owned)
            .await
            .unwrap_err()
            .code(),
        "io"
    );
    assert_eq!(
        worker
            .agent_state()
            .load()
            .await
            .unwrap()
            .unwrap()
            .execution,
        execution
    );
    assert_eq!(
        restored
            .agent_state()
            .load()
            .await
            .unwrap()
            .unwrap()
            .tasks
            .migration,
        TaskGraphMigrationSnapshot::Pending
    );
    assert_eq!(requests(&second.provider, "work").len(), 1);
    freeze(&second, &restored).await;
}
