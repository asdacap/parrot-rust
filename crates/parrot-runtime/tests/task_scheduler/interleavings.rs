use super::{fixture::*, harness::Harness, support::*};
use parrot_domain::values::agent_tasks::{ExecutionStatus::*, Task, TaskPayload};

#[tokio::test]
async fn recursive_declarations_do_not_control_child_visibility_or_independent_success_hiding() {
    let h = Harness::new();
    let owner = h.root("owner").await;
    let root_gate = Gate::new();
    let branch_gate = Gate::new();
    let peer_gate = Gate::new();
    let leaf_gate = Gate::new();
    for (name, gate) in [
        ("root-task", &root_gate),
        ("branch", &branch_gate),
        ("peer", &peer_gate),
        ("leaf", &leaf_gate),
    ] {
        h.provider.script(name, [Reply::held("answer", gate)]);
    }
    let leaf = task("leaf");
    let branch = composite("branch", vec![leaf.clone()]);
    let root_task = composite("root-task", vec![branch.clone(), task("peer")]);
    set(&owner, vec![root_task.clone()]).await;
    root_gate.entered().await;
    let root_scope = owner.children().find("root-task").unwrap();
    set(&root_scope, vec![branch.clone(), task("peer")]).await;
    branch_gate.entered().await;
    peer_gate.entered().await;
    let branch_scope = root_scope.children().find("branch").unwrap();
    set(&branch_scope, vec![leaf]).await;
    leaf_gate.entered().await;
    let hidden_leaf = Task {
        hidden: true,
        ..task("leaf")
    };
    let hidden_branch = Task {
        hidden: true,
        payload: TaskPayload::Tasks(vec![hidden_leaf]),
        ..branch.clone()
    };
    set(
        &owner,
        vec![Task {
            state: Running,
            hidden: true,
            payload: TaskPayload::Tasks(vec![hidden_branch, task("peer")]),
            ..root_task.clone()
        }],
    )
    .await;
    assert!(!snapshot(&root_scope).await.iter().any(|task| task.hidden));
    assert!(!snapshot(&branch_scope).await[0].hidden);
    assert!(
        snapshot(&root_scope)
            .await
            .iter()
            .all(|task| task.state == Running)
    );
    assert_eq!(snapshot(&branch_scope).await[0].state, Running);
    set(
        &owner,
        vec![Task {
            state: Running,
            ..root_task
        }],
    )
    .await;
    for name in ["root-task", "branch", "peer", "leaf"] {
        assert_eq!(requests(&h.provider, name).len(), 1);
    }
    leaf_gate.open();
    complete(&branch_scope).await;
    assert!(snapshot(&branch_scope).await[0].hidden);
    assert!(!snapshot(&root_scope).await.iter().any(|task| task.hidden));
    branch_gate.open();
    peer_gate.open();
    complete(&root_scope).await;
    assert!(snapshot(&root_scope).await.iter().all(|task| task.hidden));
    assert!(!snapshot(&owner).await[0].hidden);
    root_gate.open();
    complete(&owner).await;
    assert!(snapshot(&owner).await[0].hidden);
    assert_eq!(h.registry.snapshot_scopes().len(), 5);
    h.close().await;
}

#[tokio::test]
async fn dependency_cancel_and_reset_stop_actual_running_dependent_and_restart_on_same_worker() {
    for cancel in [false, true] {
        let h = Harness::new();
        let owner = h.root("owner").await;
        let first = Gate::new();
        let dependent_gate = Gate::new();
        let second = Gate::new();
        let resumed = Gate::new();
        h.provider.script(
            "a",
            [
                Reply::held("answer", &first),
                Reply::held("answer", &second),
            ],
        );
        h.provider.script(
            "b",
            [
                Reply::held("answer", &dependent_gate),
                Reply::held("answer", &resumed),
            ],
        );
        set(
            &owner,
            vec![task("a"), dependent("b", "a"), dependent("c", "b")],
        )
        .await;
        first.entered().await;
        first.open();
        dependent_gate.entered().await;
        let retained = owner.children().find("b").unwrap();
        set(
            &owner,
            vec![state("a", if cancel { Canceled } else { Pending })],
        )
        .await;
        let reason = if cancel {
            "a task this task depends on was canceled."
        } else {
            "a task this task depends on was reset; it restarts in this session once that task succeeds again."
        };
        h.factory
            .get("b")
            .task_session
            .wait_calls(|calls| {
                calls
                    .iter()
                    .any(|call| call.kind == "record" && call.text.contains(reason))
            })
            .await;
        if cancel {
            assert!(
                snapshot(&owner)
                    .await
                    .iter()
                    .all(|task| task.state == Canceled)
            );
            set(&owner, vec![task("a")]).await;
        }
        second.entered().await;
        assert_eq!(
            snapshot(&owner)
                .await
                .iter()
                .map(|task| task.state)
                .collect::<Vec<_>>(),
            [Running, Pending, Pending]
        );
        second.open();
        resumed.entered().await;
        assert!(std::sync::Arc::ptr_eq(
            &retained,
            &owner.children().find("b").unwrap()
        ));
        resumed.open();
        complete(&owner).await;
        assert_eq!(requests(&h.provider, "b").len(), 2);
        assert_eq!(requests(&h.provider, "c").len(), 1);
        h.close().await;
    }
}
