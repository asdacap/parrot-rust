use super::{fixture::*, harness::Harness, support::*};
use parrot_domain::protocol::messages::parrot::AgentTaskProgressSnapshot;
use parrot_domain::values::{
    agent_state::TaskExecutionSnapshot,
    agent_tasks::{ExecutionStatus::*, TaskAssignee},
};
use parrot_runtime::task_scheduler::{PROGRESS_KIND, TaskConcurrencyBudget};
use protobuf::Message;

#[tokio::test]
async fn solitary_leaf_runs_in_owner_without_creating_a_child() {
    let h = Harness::new();
    let root = h.root("owner").await;
    let held = Gate::new();
    h.provider
        .script("implement", [Reply::held("answer", &held)]);
    set(&root, vec![task("implement")]).await;
    assert_eq!(snapshot(&root).await[0].state, Running);
    assert!(root.children().snapshot_children().is_empty());
    assert!(task_requests(&h.provider, "implement").is_empty());
    let events = h.histories.repository.events().replay().await.unwrap();
    let progress = events
        .iter()
        .filter(|event| event.has_tool_snapshot())
        .map(|event| event.tool_snapshot())
        .filter(|snapshot| snapshot.kind == PROGRESS_KIND)
        .last()
        .unwrap();
    let progress = AgentTaskProgressSnapshot::parse_from_bytes(&progress.data).unwrap();
    assert_eq!(progress.root_nodes.len(), 1);
    assert!(progress.root_nodes[0].agent_session_id.is_empty());
    let scheduler = root.get_service::<Scheduler>().unwrap();
    let detail = scheduler.capture_detail("implement").await.unwrap();
    assert_eq!(detail.assigned_to, Some(TaskAssignee::Owner));
    assert_eq!(detail.agent_name, None);
    assert_eq!(
        scheduler.capture_views().await[0].assigned_to,
        Some(TaskAssignee::Owner)
    );
    let owner = h.factory.get("owner");
    owner
        .task_session
        .wait_calls(|calls| {
            calls.iter().any(|call| {
                call.kind == "send"
                    && call.text.contains("Work on implement")
                    && call.text.contains("implement works")
            })
        })
        .await;
    assert!(
        owner
            .task_session
            .calls
            .lock()
            .unwrap()
            .iter()
            .all(|call| call.kind != "enqueue")
    );
    h.close().await;
}

#[tokio::test]
async fn explicit_owner_completion_hands_off_serial_dependency_results() {
    let h = Harness::new();
    let root = h.root("owner").await;
    set(
        &root,
        vec![task("prepare"), dependent("implement", "prepare")],
    )
    .await;
    assert_eq!(
        snapshot(&root)
            .await
            .iter()
            .map(|task| task.state)
            .collect::<Vec<_>>(),
        [Running, Pending]
    );
    let mut prepared = state("prepare", Succeeded);
    prepared.result = Some("prepared contract".into());
    set(&root, vec![prepared]).await;
    let scheduler = root.get_service::<Scheduler>().unwrap();
    assert_eq!(
        snapshot(&root)
            .await
            .iter()
            .map(|task| task.state)
            .collect::<Vec<_>>(),
        [Succeeded, Running]
    );
    assert!(root.children().snapshot_children().is_empty());
    assert_eq!(
        scheduler
            .capture_detail("prepare")
            .await
            .unwrap()
            .assigned_to,
        Some(TaskAssignee::Owner)
    );
    assert_eq!(
        scheduler
            .capture_detail("implement")
            .await
            .unwrap()
            .assigned_to,
        Some(TaskAssignee::Owner)
    );
    let owner = h.factory.get("owner");
    owner
        .task_session
        .wait_calls(|calls| {
            calls.iter().any(|call| {
                call.kind == "send"
                    && call.text.contains("Work on implement")
                    && call.text.contains("implement works")
                    && call.text.contains("prepared contract")
            })
        })
        .await;
    let mut implemented = dependent("implement", "prepare");
    implemented.state = Succeeded;
    implemented.result = Some("verified implementation".into());
    set(&root, vec![implemented]).await;
    assert!(complete(&root).await.iter().all(|task| task.hidden));
    h.close().await;
}

#[tokio::test]
async fn self_uses_no_child_capacity_and_does_not_steal_running_dedicated_work() {
    let h = Harness::new();
    *h.factory.concurrency.lock().unwrap() = Some(TaskConcurrencyBudget::new(0));
    let root = h.root("owner").await;
    let held = Gate::new();
    h.provider
        .script("parallel", [Reply::held("answer", &held)]);
    set(&root, vec![task("self"), task("parallel"), task("waiting")]).await;
    held.entered().await;
    assert_eq!(
        snapshot(&root)
            .await
            .iter()
            .map(|task| task.state)
            .collect::<Vec<_>>(),
        [Running, Running, Pending]
    );
    let scheduler = root.get_service::<Scheduler>().unwrap();
    let dedicated = Some(TaskAssignee::Dedicated {
        agent_path: "/owner/parallel".into(),
    });
    assert_eq!(
        scheduler
            .capture_detail("parallel")
            .await
            .unwrap()
            .assigned_to,
        dedicated
    );
    assert_eq!(root.children().snapshot_children().len(), 1);
    set(&root, vec![state("self", Succeeded)]).await;
    assert_eq!(
        scheduler
            .capture_detail("waiting")
            .await
            .unwrap()
            .assigned_to,
        Some(TaskAssignee::Owner)
    );
    assert_eq!(
        scheduler
            .capture_detail("parallel")
            .await
            .unwrap()
            .assigned_to,
        dedicated
    );
    assert_eq!(
        scheduler
            .capture_views()
            .await
            .iter()
            .filter(|view| {
                view.task.state == Running && view.assigned_to == Some(TaskAssignee::Owner)
            })
            .count(),
        1
    );
    set(&root, vec![state("waiting", Succeeded)]).await;
    held.open();
    complete(&root).await;
    assert_eq!(task_requests(&h.provider, "parallel").len(), 1);
    h.close().await;
}

#[tokio::test]
async fn composites_and_model_overrides_always_use_dedicated_agents() {
    let h = Harness::new();
    let root = h.root("owner").await;
    let composite_gate = Gate::new();
    let model_gate = Gate::new();
    h.provider
        .script("group", [Reply::held("answer", &composite_gate)]);
    h.provider
        .script("specialist", [Reply::held("answer", &model_gate)]);
    let mut specialist = task("specialist");
    specialist.model = Some("second".into());
    set(
        &root,
        vec![composite("group", vec![task("inner")]), specialist],
    )
    .await;
    composite_gate.entered().await;
    model_gate.entered().await;
    let scheduler = root.get_service::<Scheduler>().unwrap();
    assert!(
        scheduler
            .capture_views()
            .await
            .iter()
            .all(|view| matches!(view.assigned_to, Some(TaskAssignee::Dedicated { .. })))
    );
    assert_eq!(requests(&h.provider, "specialist")[0].model, "second");
    assert_eq!(root.children().snapshot_children().len(), 2);
    composite_gate.open();
    model_gate.open();
    complete(&root).await;
    h.close().await;
}

#[tokio::test]
async fn dedicated_completion_assigns_new_self_work_without_another_update() {
    let h = Harness::new();
    let root = h.root("owner").await;
    let held = Gate::new();
    h.provider
        .script("dependency", [Reply::held("answer", &held)]);
    set(
        &root,
        vec![
            dedicated(task("dependency")),
            dependent("next", "dependency"),
        ],
    )
    .await;
    held.entered().await;
    let scheduler = root.get_service::<Scheduler>().unwrap();
    assert_eq!(
        scheduler.capture_detail("next").await.unwrap().assigned_to,
        None
    );
    held.open();
    wait_tasks(&root, |tasks| tasks[1].state == Running).await;
    assert_eq!(
        scheduler.capture_detail("next").await.unwrap().assigned_to,
        Some(TaskAssignee::Owner)
    );
    assert_eq!(root.children().snapshot_children().len(), 1);
    let owner = h.factory.get("owner");
    owner
        .task_session
        .wait_calls(|calls| {
            calls.iter().any(|call| {
                call.kind == "send"
                    && call.text.contains("Work on next")
                    && call.text.contains("next works")
                    && call.text.contains("Result: answer")
            })
        })
        .await;
    let mut next = dependent("next", "dependency");
    next.state = Succeeded;
    next.result = Some("verified next".into());
    set(&root, vec![next]).await;
    complete(&root).await;
    h.close().await;
}

#[tokio::test]
async fn self_visibility_preserves_assignment_while_redefinition_and_reset_replace_it() {
    let h = Harness::new();
    let root = h.root("owner").await;
    set(&root, vec![task("work")]).await;
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
    let mut visible = state("work", Running);
    visible.hidden = true;
    set(&root, vec![visible]).await;
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
    let mut changed = task("work");
    changed.payload = parrot_domain::values::agent_tasks::TaskPayload::Instruction(
        "A revised instruction".into(),
    );
    set(&root, vec![changed.clone()]).await;
    let revised = root
        .agent_state()
        .load()
        .await
        .unwrap()
        .unwrap()
        .tasks
        .entries[0]
        .execution
        .clone();
    assert_ne!(revised, before);
    changed.state = Failed;
    changed.failure = Some("needs another attempt".into());
    set(&root, vec![changed.clone()]).await;
    assert_eq!(
        root.get_service::<Scheduler>()
            .unwrap()
            .capture_detail("work")
            .await
            .unwrap()
            .assigned_to,
        Some(TaskAssignee::Owner)
    );
    changed.state = Pending;
    changed.failure = None;
    set(&root, vec![changed.clone()]).await;
    assert_ne!(
        root.agent_state()
            .load()
            .await
            .unwrap()
            .unwrap()
            .tasks
            .entries[0]
            .execution,
        revised
    );
    changed.state = Canceled;
    set(&root, vec![changed]).await;
    assert_eq!(snapshot(&root).await[0].state, Canceled);
    assert!(root.children().snapshot_children().is_empty());
    let owner = h.factory.get("owner");
    assert!(owner.task_session.calls.lock().unwrap().iter().all(|call| {
        call.kind != "enqueue" && !call.text.contains("Your AgentTask turn was stopped:")
    }));
    h.close().await;
}

#[tokio::test]
async fn dependency_reset_and_cancellation_invalidate_owner_work_without_stopping_owner() {
    let h = Harness::new();
    let root = h.root("owner").await;
    set(
        &root,
        vec![
            state("dependency", Succeeded),
            dependent("work", "dependency"),
        ],
    )
    .await;
    let scheduler = root.get_service::<Scheduler>().unwrap();
    assert_eq!(
        scheduler.capture_detail("work").await.unwrap().assigned_to,
        Some(TaskAssignee::Owner)
    );
    set(&root, vec![task("dependency")]).await;
    assert_eq!(
        snapshot(&root)
            .await
            .iter()
            .map(|task| task.state)
            .collect::<Vec<_>>(),
        [Running, Pending]
    );
    assert_eq!(
        scheduler.capture_detail("work").await.unwrap().assigned_to,
        None
    );
    set(&root, vec![state("dependency", Succeeded)]).await;
    assert_eq!(snapshot(&root).await[1].state, Running);
    set(&root, vec![state("dependency", Canceled)]).await;
    assert_eq!(snapshot(&root).await[1].state, Canceled);
    assert!(root.children().snapshot_children().is_empty());
    h.close().await;
}

#[tokio::test]
async fn self_assignment_reopens_without_a_worker_checkpoint_or_duplicate_assignment() {
    let first = Harness::new();
    let root = first.root("owner").await;
    set(&root, vec![task("work")]).await;
    let scheduler = root.get_service::<Scheduler>().unwrap();
    scheduler.suspend().join().await.unwrap();
    first.registry.shutdown().join().await.unwrap();
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
    assert!(matches!(before, TaskExecutionSnapshot::Owner { .. }));
    let second = Harness::with_history(32, first.histories.clone(), first.provider.clone());
    let restored = second.reopen_root(root.identity().clone()).await;
    let scheduler = restored.get_service::<Scheduler>().unwrap();
    scheduler.activate_executions().join().await.unwrap();
    assert_eq!(
        scheduler.capture_detail("work").await.unwrap().assigned_to,
        Some(TaskAssignee::Owner)
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
            .execution,
        before
    );
    assert!(restored.children().snapshot_children().is_empty());
    assert!(task_requests(&second.provider, "work").is_empty());
    set(&restored, vec![state("work", Succeeded)]).await;
    complete(&restored).await;
    second.close().await;
}

#[tokio::test]
async fn dedicated_composite_owns_its_inner_self_assignment() {
    let h = Harness::new();
    let root = h.root("owner").await;
    let held = Gate::new();
    h.provider.script("group", [Reply::held("answer", &held)]);
    set(&root, vec![composite("group", vec![task("inner")])]).await;
    held.entered().await;
    let child = root.children().resolve("group").unwrap();
    set(&child, vec![task("inner")]).await;
    assert_eq!(
        child
            .get_service::<Scheduler>()
            .unwrap()
            .capture_detail("inner")
            .await
            .unwrap()
            .assigned_to,
        Some(TaskAssignee::Owner)
    );
    assert!(child.children().snapshot_children().is_empty());
    set(&child, vec![state("inner", Succeeded)]).await;
    complete(&child).await;
    assert_eq!(snapshot(&root).await[0].state, Running);
    held.open();
    complete(&root).await;
    h.close().await;
}

#[tokio::test]
async fn resetting_a_dedicated_leaf_can_handoff_to_self_after_its_old_stop() {
    let h = Harness::new();
    let root = h.root("owner").await;
    let held = Gate::new();
    h.provider
        .script("parallel", [Reply::held("answer", &held)]);
    set(&root, vec![task("owner-work"), task("parallel")]).await;
    held.entered().await;
    let mut replacement = task("parallel");
    replacement.payload = parrot_domain::values::agent_tasks::TaskPayload::Instruction(
        "Retry in the owning agent".into(),
    );
    set(
        &root,
        vec![state("owner-work", Succeeded), replacement.clone()],
    )
    .await;
    let scheduler = root.get_service::<Scheduler>().unwrap();
    bounded(async {
        while scheduler
            .capture_detail("parallel")
            .await
            .unwrap()
            .assigned_to
            != Some(TaskAssignee::Owner)
        {
            tokio::task::yield_now().await;
        }
    })
    .await;
    let owner = h.factory.get("owner");
    owner
        .task_session
        .wait_calls(|calls| {
            calls
                .iter()
                .any(|call| call.kind == "send" && call.text.contains("Retry in the owning agent"))
        })
        .await;
    assert_eq!(task_requests(&h.provider, "parallel").len(), 1);
    replacement.state = Succeeded;
    replacement.result = Some("completed in the owner".into());
    set(&root, vec![replacement]).await;
    complete(&root).await;
    h.close().await;
}

#[tokio::test]
async fn self_assignment_retry_does_not_deliver_a_canceled_or_replaced_instruction() {
    for replace in [false, true] {
        let h = Harness::new();
        let root = h.root("owner").await;
        let owner = h.factory.get("owner");
        owner
            .task_session
            .send_failures
            .store(1, std::sync::atomic::Ordering::SeqCst);
        set(&root, vec![task("work")]).await;
        owner
            .task_session
            .wait_calls(|calls| {
                calls
                    .iter()
                    .any(|call| call.kind == "send" && call.text.contains("Work on work"))
            })
            .await;
        let mut update = task("work");
        if replace {
            update.payload = parrot_domain::values::agent_tasks::TaskPayload::Instruction(
                "Replacement instruction".into(),
            );
        } else {
            update.state = Canceled;
        }
        set(&root, vec![update]).await;
        owner
            .task_session
            .wait_calls(|calls| {
                calls.iter().any(|call| {
                    call.kind == "send"
                        && call.text.contains(if replace {
                            "Replacement instruction"
                        } else {
                            "All AgentTasks are complete:"
                        })
                })
            })
            .await;
        let events = h.histories.repository.events().replay().await.unwrap();
        assert!(
            !events.iter().any(|event| {
                event.agent_session_id == root.identity().session.session_id
                    && event.has_input_admitted()
                    && event.input_admitted().content.contains("Work on work")
            }),
            "a stale executable assignment was admitted after invalidation"
        );
        h.close().await;
    }
}

#[tokio::test]
async fn recovered_self_notice_does_not_deliver_an_invalidated_assignment() {
    let first = Harness::new();
    let root = first.root("owner").await;
    let owner = first.factory.get("owner");
    owner
        .task_session
        .send_failures
        .store(1, std::sync::atomic::Ordering::SeqCst);
    set(&root, vec![task("work")]).await;
    owner
        .task_session
        .wait_calls(|calls| {
            calls
                .iter()
                .any(|call| call.kind == "send" && call.text.contains("Work on work"))
        })
        .await;
    root.get_service::<Scheduler>()
        .unwrap()
        .suspend()
        .join()
        .await
        .unwrap();
    first.registry.shutdown().join().await.unwrap();
    let second = Harness::with_history(32, first.histories.clone(), first.provider.clone());
    let restored = second.reopen_root(root.identity().clone()).await;
    set(&restored, vec![state("work", Canceled)]).await;
    let scheduler = restored.get_service::<Scheduler>().unwrap();
    scheduler.activate_executions().join().await.unwrap();
    scheduler.activate_notifications().join().await.unwrap();
    let owner = second.factory.get("owner");
    owner
        .task_session
        .wait_calls(|calls| {
            calls.iter().any(|call| {
                call.kind == "send" && call.text.contains("All AgentTasks are complete:")
            })
        })
        .await;
    let events = second.histories.repository.events().replay().await.unwrap();
    assert!(!events.iter().any(|event| {
        event.agent_session_id == restored.identity().session.session_id
            && event.has_input_admitted()
            && event.input_admitted().content.contains("Work on work")
    }));
    second.close().await;
}

#[tokio::test]
async fn current_self_assignment_still_retries_with_the_same_message_identity() {
    let h = Harness::new();
    let root = h.root("owner").await;
    let owner = h.factory.get("owner");
    owner
        .task_session
        .send_failures
        .store(1, std::sync::atomic::Ordering::SeqCst);
    set(&root, vec![task("work")]).await;
    owner
        .task_session
        .wait_calls(|calls| {
            calls
                .iter()
                .filter(|call| call.kind == "send" && call.text.contains("Work on work"))
                .count()
                == 2
        })
        .await;
    let calls = owner.task_session.calls.lock().unwrap().clone();
    let attempts = calls
        .iter()
        .filter(|call| call.kind == "send" && call.text.contains("Work on work"))
        .collect::<Vec<_>>();
    assert_eq!(attempts[0].id, attempts[1].id);
    assert_eq!(attempts[0].text, attempts[1].text);
    let events = h.histories.repository.events().replay().await.unwrap();
    assert_eq!(
        events
            .iter()
            .filter(|event| event.has_input_admitted()
                && event.input_admitted().message_id == attempts[0].id)
            .count(),
        1
    );
    h.close().await;
}
