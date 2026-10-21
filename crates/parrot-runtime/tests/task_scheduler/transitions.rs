use super::{fixture::*, harness::Harness, support::*};
use parrot_domain::values::agent_tasks::{ExecutionStatus::*, Task};
use parrot_storage::conversation::HistoryForkBoundary;
use std::sync::Arc;

#[tokio::test]
async fn dependency_order_failure_blocking_transitive_reset_and_retained_workers() {
    let h = Harness::new();
    let root = h.root("owner").await;
    let gates: Vec<_> = (0..6).map(|_| Gate::new()).collect();
    for (name, first, second) in [("a", 0, 3), ("b", 1, 4), ("c", 2, 5)] {
        h.provider.script(
            name,
            [
                Reply::held("answer", &gates[first]),
                Reply::held("answer", &gates[second]),
            ],
        );
    }
    set(
        &root,
        vec![task("a"), dependent("b", "a"), dependent("c", "b")],
    )
    .await;
    gates[0].entered().await;
    assert_eq!(
        snapshot(&root)
            .await
            .iter()
            .map(|task| task.state)
            .collect::<Vec<_>>(),
        [Running, Pending, Pending]
    );
    assert!(requests(&h.provider, "b").is_empty());
    gates[0].open();
    gates[1].entered().await;
    assert_eq!(
        snapshot(&root)
            .await
            .iter()
            .map(|task| task.state)
            .collect::<Vec<_>>(),
        [Succeeded, Running, Pending]
    );
    assert!(prompt(&requests(&h.provider, "b")[0]).contains("[a] Do a\nResult: answer"));
    assert!(prompt(&requests(&h.provider, "b")[0]).contains("[c] Do c"));
    gates[1].open();
    gates[2].entered().await;
    gates[2].open();
    assert!(complete(&root).await.iter().all(|task| task.hidden));
    let retained = h.registry.snapshot_scopes();
    set(&root, vec![task("a")]).await;
    gates[3].entered().await;
    assert_eq!(
        snapshot(&root)
            .await
            .iter()
            .map(|task| task.state)
            .collect::<Vec<_>>(),
        [Running, Pending, Pending]
    );
    assert!(
        snapshot(&root).await[1..]
            .iter()
            .all(|task| task.result.is_none() && task.failure.is_none())
    );
    gates[3].open();
    gates[4].entered().await;
    gates[4].open();
    gates[5].entered().await;
    gates[5].open();
    complete(&root).await;
    let after = h.registry.snapshot_scopes();
    assert_eq!(retained.len(), after.len());
    for scope in retained {
        assert!(after.iter().any(|other| Arc::ptr_eq(other, &scope)));
    }
    for name in ["a", "b", "c"] {
        assert_eq!(requests(&h.provider, name).len(), 2);
    }
    assert_eq!(
        h.factory.parser.leaf_inputs.lock().unwrap().as_slice(),
        ["answer"; 6]
    );
    h.close().await;
}

#[tokio::test]
async fn rejected_failure_holds_dependents_until_retry_acceptance_or_cancel_then_reset() {
    for resolution in [Pending, Succeeded, Canceled] {
        let h = Harness::new();
        let root = h.root("owner").await;
        h.factory.parser.answer(
            "halt",
            parrot_domain::values::agent_tasks::LeafResponse {
                result: "partial".into(),
                verdict: parrot_domain::values::agent_tasks::AcceptanceVerdict::RejectAndHalt {
                    feedback: "broken".into(),
                },
            },
        );
        h.provider
            .script("a", [Reply::text("halt"), Reply::text("answer")]);
        set(
            &root,
            vec![task("a"), dependent("b", "a"), dependent("c", "b")],
        )
        .await;
        let failed = wait_tasks(&root, |tasks| tasks[0].state == Failed).await;
        assert_eq!(
            failed.iter().map(|task| task.state).collect::<Vec<_>>(),
            [Failed, Pending, Pending]
        );
        assert_eq!(failed[0].failure.as_deref(), Some("broken"));
        assert!(!failed.iter().any(|task| task.hidden));
        assert!(requests(&h.provider, "b").is_empty());
        let mut update = state("a", resolution);
        update.result = (resolution == Succeeded).then(|| "manual".into());
        set(&root, vec![update]).await;
        if resolution == Canceled {
            assert!(
                snapshot(&root)
                    .await
                    .iter()
                    .all(|task| task.state == Canceled && task.hidden)
            );
            assert!(requests(&h.provider, "b").is_empty());
            set(&root, vec![task("a")]).await;
        }
        complete(&root).await;
        assert_eq!(
            requests(&h.provider, "a").len(),
            if resolution == Succeeded { 1 } else { 2 }
        );
        assert!(
            prompt(&requests(&h.provider, "b")[0]).contains(if resolution == Succeeded {
                "Result: manual"
            } else {
                "Result: answer"
            })
        );
        h.close().await;
    }
}

#[tokio::test]
async fn merged_graph_validation_is_forwarded_and_rejection_is_atomic() {
    let h = Harness::new();
    let root = h.root("owner").await;
    set(&root, vec![state("held", Failed)]).await;
    h.factory
        .parser
        .validations
        .lock()
        .unwrap()
        .push_back(Err(ParseError("tasks contains a dependency cycle.".into())));
    let scheduler = root.get_service::<Scheduler>().unwrap();
    let error = bounded(
        scheduler
            .set_tasks(
                vec![dependent("a", "held")],
                root.selection().current().unwrap(),
                HistoryForkBoundary::AfterCompletedHistory,
            )
            .unwrap()
            .join(),
    )
    .await
    .unwrap_err();
    assert!(error.to_string().contains("dependency cycle"));
    assert_eq!(snapshot(&root).await, vec![state("held", Failed)]);
    {
        let graphs = h.factory.parser.graphs.lock().unwrap();
        assert_eq!(graphs.len(), 2);
        assert_eq!(graphs[1].1, "tasks");
        assert_eq!(
            graphs[1]
                .0
                .iter()
                .map(|task| task.name.as_str())
                .collect::<Vec<_>>(),
            ["held", "a"]
        );
    }
    assert_eq!(
        h.factory
            .build_calls
            .load(std::sync::atomic::Ordering::SeqCst),
        1
    );
    h.close().await;
}

#[tokio::test]
async fn running_visibility_updates_preserve_execution_detail_and_terminal_result() {
    for hidden in [false, true] {
        let h = Harness::new();
        let root = h.root("owner").await;
        let gate = Gate::new();
        h.provider.script("a", [Reply::held("answer", &gate)]);
        set(&root, vec![task("a"), state("blocker", Failed)]).await;
        gate.entered().await;
        let scope = h.factory.get("a");
        for value in [true, false, hidden] {
            set(
                &root,
                vec![Task {
                    state: Running,
                    hidden: value,
                    ..task("a")
                }],
            )
            .await;
            assert_eq!(snapshot(&root).await[0].hidden, value);
        }
        let scheduler = root.get_service::<Scheduler>().unwrap();
        assert_eq!(
            scheduler
                .capture_detail("a")
                .await
                .unwrap()
                .agent_name
                .as_deref(),
            Some("a")
        );
        assert!(scheduler.capture_detail("missing").await.is_none());
        let before = snapshot(&root).await;
        gate.open();
        let done = wait_tasks(&root, |tasks| tasks[0].state == Succeeded).await;
        assert_eq!(done[0].hidden, hidden);
        assert_eq!(done[0].result.as_deref(), Some("answer"));
        assert!(!done[1].hidden);
        let mut changed = before[0].clone();
        changed.hidden = !hidden;
        bounded(
            scheduler
                .apply_visibility_changes(before, vec![changed])
                .join(),
        )
        .await
        .unwrap();
        assert_eq!(snapshot(&root).await[0].hidden, !hidden);
        assert_eq!(snapshot(&root).await[0].state, Succeeded);
        assert_eq!(requests(&h.provider, "a").len(), 1);
        assert!(Arc::ptr_eq(&scope, &h.factory.get("a")));
        h.close().await;
    }
}

#[tokio::test]
async fn autohide_requires_nonempty_all_resolved_and_hides_once_per_transition() {
    for first in [Succeeded, Canceled] {
        for second in [Succeeded, Canceled] {
            let h = Harness::new();
            let root = h.root("owner").await;
            set(&root, vec![]).await;
            assert!(snapshot(&root).await.is_empty());
            set(&root, vec![state("a", first), state("b", second)]).await;
            assert!(snapshot(&root).await.iter().all(|task| task.hidden));
            set(&root, vec![state("a", first)]).await;
            assert!(!snapshot(&root).await[0].hidden);
            assert!(snapshot(&root).await[1].hidden);
            set(&root, vec![state("b", second)]).await;
            assert!(!snapshot(&root).await.iter().any(|task| task.hidden));
            set(&root, vec![state("b", Failed)]).await;
            assert!(!snapshot(&root).await.iter().any(|task| task.hidden));
            set(&root, vec![state("b", second)]).await;
            assert!(snapshot(&root).await.iter().all(|task| task.hidden));
            h.close().await;
        }
    }
    for unresolved in [Failed, Pending, Running] {
        let h = Harness::new();
        let root = h.root("owner").await;
        let gate = Gate::new();
        h.provider.script("b", [Reply::held("answer", &gate)]);
        set(&root, vec![state("a", Succeeded), state("b", unresolved)]).await;
        assert!(!snapshot(&root).await.iter().any(|task| task.hidden));
        h.close().await;
    }
}

#[tokio::test]
async fn unspawned_detail_and_recursive_visibility_never_create_workers() {
    let h = Harness::new();
    let root = h.root("owner").await;
    let mut blocked = composite("blocked", vec![task("child")]);
    blocked.dependencies.push("dependency".into());
    set(&root, vec![state("dependency", Failed), blocked]).await;
    let scheduler = root.get_service::<Scheduler>().unwrap();
    let detail = scheduler.capture_detail("blocked").await.unwrap();
    assert!(detail.agent_name.is_none());
    assert_eq!(detail.task.state, Pending);
    let before = snapshot(&root).await;
    let mut changed = before[1].clone();
    changed.payload = parrot_domain::values::agent_tasks::TaskPayload::Tasks(vec![Task {
        hidden: true,
        ..task("child")
    }]);
    bounded(
        scheduler
            .apply_visibility_changes(before, vec![changed])
            .join(),
    )
    .await
    .unwrap();
    assert!(inner(&snapshot(&root).await[1])[0].hidden);
    assert!(
        scheduler
            .capture_detail("blocked")
            .await
            .unwrap()
            .agent_name
            .is_none()
    );
    assert_eq!(h.registry.snapshot_scopes().len(), 1);
    h.close().await;
}
