use super::*;
use parrot_domain::values::{
    agent_state::{ExecutionOutcome, TaskGraphMigrationSnapshot},
    agent_tasks::TaskAssignee,
};

#[path = "../../../../../../tests/support/legacy_agent_state.rs"]
mod legacy_state;

fn task_requests(fixture: &Fixture, name: &str) -> usize {
    let marker = format!("Task: {name}\n");
    fixture
        .requests
        .lock()
        .unwrap()
        .iter()
        .filter(|request| {
            request
                .messages
                .iter()
                .any(|message| message.content().contains(&marker))
        })
        .count()
}

async fn wait_self_request(fixture: &Fixture, name: &str) {
    let marker = format!("AgentTask assigned to self: {name}");
    bounded(async {
        while !fixture.requests.lock().unwrap().iter().any(|request| {
            request
                .messages
                .iter()
                .any(|message| message.content().contains(&marker))
        }) {
            tokio::task::yield_now().await;
        }
    })
    .await;
}

#[tokio::test]
async fn actual_legacy_leaf_reassigns_to_self_preserves_outcomes_and_stays_idle_on_second_reopen() {
    let fixture = fixture().await;
    let hosted = bounded(fixture.open()).await.unwrap();
    let root = fixture.root();
    let held = Arc::new(Semaphore::new(0));
    script(
        &fixture,
        "Task: legacy-work\n",
        reply(r#"{"result":"unused","verdict":"accept"}"#),
        Some(held),
    );
    let mut done = task("done");
    done["state"] = "succeeded".into();
    done["result"] = "preserved result".into();
    let mut failed = task("failed");
    failed["state"] = "failed".into();
    failed["failure"] = "preserved failure".into();
    let mut canceled = task("canceled");
    canceled["state"] = "canceled".into();
    set(
        &root,
        json!([dedicated(task("legacy-work")), done, failed, canceled]),
    )
    .await;
    wait_task_request(&fixture, "legacy-work").await;
    let worker = root.children().resolve("legacy-work").unwrap();
    let worker_id = worker.identity().session.session_id.clone();
    let old_prompt = worker
        .agent_state()
        .load()
        .await
        .unwrap()
        .unwrap()
        .execution
        .unwrap()
        .initial_message_id;
    let path = state_path(&fixture.user(), &root);
    let id = UserSessionId::parse(&hosted.snapshot().id).unwrap();
    bounded(hosted.close()).await.unwrap();
    legacy_state::rewrite_snapshot_as_version_one(&path, &[0]);
    let owner_hold = Arc::new(Semaphore::new(0));
    script(
        &fixture,
        "AgentTask assigned to self: legacy-work",
        reply("owner finished"),
        Some(owner_hold.clone()),
    );
    let resumed = reopen(&fixture, &id).await;
    let restored = fixture.root();
    wait_self_request(&fixture, "legacy-work").await;
    let scheduler = restored.get_service::<Scheduler>().unwrap();
    assert_eq!(
        scheduler
            .capture_detail("legacy-work")
            .await
            .unwrap()
            .assigned_to,
        Some(TaskAssignee::Owner)
    );
    let tasks = scheduler.snapshot().await;
    assert_eq!(tasks[1].state, ExecutionStatus::Succeeded);
    assert_eq!(tasks[1].result.as_deref(), Some("preserved result"));
    assert_eq!(tasks[2].state, ExecutionStatus::Failed);
    assert_eq!(tasks[2].failure.as_deref(), Some("preserved failure"));
    assert_eq!(tasks[3].state, ExecutionStatus::Canceled);
    let worker = restored.children().resolve("legacy-work").unwrap();
    assert_eq!(worker.identity().session.session_id, worker_id);
    let execution = worker
        .agent_state()
        .load()
        .await
        .unwrap()
        .unwrap()
        .execution
        .unwrap();
    assert_eq!(execution.initial_message_id, old_prompt);
    assert_eq!(
        execution.terminal.unwrap().outcome,
        ExecutionOutcome::Canceled
    );
    assert_eq!(execution.result_claim, None);
    assert_eq!(execution.parent_notification, None);
    assert!(
        !worker
            .get_service::<RuntimeSession<Security>>()
            .unwrap()
            .has_recovery_work()
            .await
            .unwrap()
    );
    assert_eq!(task_requests(&fixture, "legacy-work"), 1);
    assert_eq!(
        scheduler.capture_migration().await,
        TaskGraphMigrationSnapshot::Current
    );
    let mut finished = task("legacy-work");
    finished["state"] = "succeeded".into();
    finished["result"] = "verified by owner".into();
    let mut accepted = task("failed");
    accepted["state"] = "succeeded".into();
    accepted["result"] = "accepted preserved failure".into();
    set(&restored, json!([finished, accepted])).await;
    owner_hold.add_permits(1);
    idle(&restored).await;
    bounded(resumed.close()).await.unwrap();
    let requests = fixture.requests.lock().unwrap().len();
    let reopened = reopen(&fixture, &id).await;
    let restored = fixture.root();
    idle(&restored).await;
    assert_eq!(
        restored
            .get_service::<Scheduler>()
            .unwrap()
            .capture_migration()
            .await,
        TaskGraphMigrationSnapshot::Current
    );
    assert_eq!(fixture.requests.lock().unwrap().len(), requests);
    assert_eq!(task_requests(&fixture, "legacy-work"), 1);
    bounded(reopened.close()).await.unwrap();
    std::fs::remove_dir_all(&fixture.root).unwrap();
}

#[tokio::test]
async fn actual_legacy_composite_stops_owned_grandchildren_and_reuses_its_scope_for_fresh_work() {
    let fixture = fixture().await;
    let hosted = bounded(fixture.open()).await.unwrap();
    let root = fixture.root();
    script(
        &fixture,
        "Task: legacy-group\n",
        reply(r#"{"result":"unused","verdict":"accept"}"#),
        Some(Arc::new(Semaphore::new(0))),
    );
    script(
        &fixture,
        "Task: legacy-grandchild\n",
        reply(r#"{"result":"unused","verdict":"accept"}"#),
        Some(Arc::new(Semaphore::new(0))),
    );
    let mut group = task("legacy-group");
    group["payload"] = json!([task("legacy-grandchild")]);
    set(&root, json!([group])).await;
    wait_task_request(&fixture, "legacy-group").await;
    let child = root.children().resolve("legacy-group").unwrap();
    set(&child, json!([dedicated(task("legacy-grandchild"))])).await;
    wait_task_request(&fixture, "legacy-grandchild").await;
    let grandchild = child.children().resolve("legacy-grandchild").unwrap();
    let child_id = child.identity().session.session_id.clone();
    let grandchild_id = grandchild.identity().session.session_id.clone();
    let old_group_prompt = child
        .agent_state()
        .load()
        .await
        .unwrap()
        .unwrap()
        .execution
        .unwrap()
        .initial_message_id;
    let pending = child.history().capture_pending_input_ids().await.unwrap();
    assert_eq!(pending.len(), 1);
    let events = fixture.user().lease.events.replay().await.unwrap();
    let admitted = events
        .iter()
        .find(|event| {
            event.agent_session_id == child_id
                && event.has_input_admitted()
                && event.input_admitted().message_id == pending[0]
        })
        .unwrap()
        .input_admitted();
    assert!(admitted.content.contains("legacy-grandchild: running"));
    let notice = parrot_domain::values::agent_state::TaskNotificationSnapshot {
        message_id: admitted.message_id.clone(),
        target_session_id: child_id.clone(),
        text: admitted.content.clone(),
        wake: false,
        ready: true,
        owner_assignment: None,
    };
    let paths = [
        state_path(&fixture.user(), &root),
        state_path(&fixture.user(), &child),
    ];
    let id = UserSessionId::parse(&hosted.snapshot().id).unwrap();
    bounded(hosted.close()).await.unwrap();
    let mut saved: AgentStateSnapshot =
        serde_json::from_slice(&std::fs::read(&paths[1]).unwrap()).unwrap();
    assert!(saved.tasks.notifications.is_empty());
    saved.tasks.notifications.push(notice);
    std::fs::write(&paths[1], serde_json::to_vec(&saved).unwrap()).unwrap();
    legacy_state::rewrite_snapshot_as_version_one(&paths[0], &[]);
    legacy_state::rewrite_snapshot_as_version_one(&paths[1], &[0]);
    let new_group = Arc::new(Semaphore::new(0));
    script(
        &fixture,
        "Task: legacy-group\n",
        reply(r#"{"result":"fresh verified work","verdict":"accept"}"#),
        Some(new_group.clone()),
    );
    let resumed = reopen(&fixture, &id).await;
    let restored = fixture.root();
    bounded(async {
        while task_requests(&fixture, "legacy-group") < 2 {
            tokio::task::yield_now().await;
        }
    })
    .await;
    let child = restored.children().resolve("legacy-group").unwrap();
    assert_eq!(child.identity().session.session_id, child_id);
    let current = child.agent_state().load().await.unwrap().unwrap();
    assert_eq!(current.tasks.migration, TaskGraphMigrationSnapshot::Current);
    assert_eq!(
        current.tasks.entries[0].task.state,
        ExecutionStatus::Canceled
    );
    assert_ne!(
        current.execution.unwrap().initial_message_id,
        old_group_prompt
    );
    let grandchild = child.children().resolve("legacy-grandchild").unwrap();
    assert_eq!(grandchild.identity().session.session_id, grandchild_id);
    let execution = grandchild
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
    assert_eq!(task_requests(&fixture, "legacy-grandchild"), 1);
    assert!(
        !grandchild
            .get_service::<RuntimeSession<Security>>()
            .unwrap()
            .has_recovery_work()
            .await
            .unwrap()
    );
    set(&child, json!([task("fresh-inner")])).await;
    assert_eq!(
        child
            .get_service::<Scheduler>()
            .unwrap()
            .capture_detail("fresh-inner")
            .await
            .unwrap()
            .assigned_to,
        Some(TaskAssignee::Owner)
    );
    let mut finished = task("fresh-inner");
    finished["state"] = "succeeded".into();
    finished["result"] = "fresh scope remains usable".into();
    set(&child, json!([finished])).await;
    new_group.add_permits(1);
    wait_tasks(&restored, |tasks| {
        tasks[0].state == ExecutionStatus::Succeeded
    })
    .await;
    idle(&restored).await;
    assert_eq!(task_requests(&fixture, "legacy-grandchild"), 1);
    bounded(resumed.close()).await.unwrap();
    std::fs::remove_dir_all(&fixture.root).unwrap();
}

#[tokio::test]
async fn actual_legacy_waiter_preserves_an_unrelated_manual_predecessor_and_its_graph() {
    let fixture = fixture().await;
    let hosted = bounded(fixture.open()).await.unwrap();
    let root = fixture.root();
    let manual = spawn(&fixture, &root, "manual-worker").await;
    script(
        &fixture,
        "manual predecessor",
        reply("unused predecessor"),
        Some(Arc::new(Semaphore::new(0))),
    );
    let manual_session = manual.get_service::<RuntimeSession<Security>>().unwrap();
    let predecessor = bounded(
        manual_session.send_text_message("manual predecessor".into(), Arc::new(NeverCancelled)),
    )
    .await
    .unwrap();
    bounded(async {
        while fixture.requests.lock().unwrap().is_empty() {
            tokio::task::yield_now().await;
        }
    })
    .await;
    let mut done = task("manual-result");
    done["state"] = "succeeded".into();
    done["result"] = "preserved unrelated graph".into();
    set(&manual, json!([done])).await;
    set(&root, json!([dedicated(task("manual-worker"))])).await;
    assert!(!predecessor.execution.is_finished());
    let old = manual.agent_state().load().await.unwrap().unwrap();
    let path = state_path(&fixture.user(), &root);
    let id = UserSessionId::parse(&hosted.snapshot().id).unwrap();
    bounded(hosted.close()).await.unwrap();
    legacy_state::rewrite_snapshot_as_version_one(&path, &[0]);
    let manual_hold = Arc::new(Semaphore::new(0));
    let owner_hold = Arc::new(Semaphore::new(0));
    script(
        &fixture,
        "manual predecessor",
        reply("manual predecessor completed"),
        Some(manual_hold.clone()),
    );
    script(
        &fixture,
        "AgentTask assigned to self: manual-worker",
        reply("owner completed"),
        Some(owner_hold.clone()),
    );
    let resumed = reopen(&fixture, &id).await;
    let restored = fixture.root();
    wait_self_request(&fixture, "manual-worker").await;
    bounded(async {
        while fixture
            .requests
            .lock()
            .unwrap()
            .iter()
            .filter(|request| {
                request
                    .messages
                    .iter()
                    .any(|message| message.content().contains("manual predecessor"))
            })
            .count()
            < 2
        {
            tokio::task::yield_now().await;
        }
    })
    .await;
    let manual = restored.children().resolve("manual-worker").unwrap();
    let current = manual.agent_state().load().await.unwrap().unwrap();
    assert_eq!(
        current.execution.as_ref().unwrap().initial_message_id,
        old.execution.unwrap().initial_message_id
    );
    assert!(current.execution.unwrap().terminal.is_none());
    assert_eq!(current.tasks.entries, old.tasks.entries);
    assert_eq!(task_requests(&fixture, "manual-worker"), 0);
    assert_eq!(
        restored
            .get_service::<Scheduler>()
            .unwrap()
            .capture_detail("manual-worker")
            .await
            .unwrap()
            .assigned_to,
        Some(TaskAssignee::Owner)
    );
    let mut finished = task("manual-worker");
    finished["state"] = "succeeded".into();
    finished["result"] = "owner performed its todo".into();
    set(&restored, json!([finished])).await;
    manual_hold.add_permits(1);
    owner_hold.add_permits(1);
    idle(&manual).await;
    idle(&restored).await;
    bounded(resumed.close()).await.unwrap();
    std::fs::remove_dir_all(&fixture.root).unwrap();
}

#[tokio::test]
async fn actual_legacy_startup_recovers_canceled_child_with_unacknowledged_claim() {
    let fixture = fixture().await;
    let hosted = bounded(fixture.open()).await.unwrap();
    let root = fixture.root();
    script(
        &fixture,
        "Task: interrupted-legacy\n",
        reply(r#"{"result":"unused","verdict":"accept"}"#),
        Some(Arc::new(Semaphore::new(0))),
    );
    set(&root, json!([dedicated(task("interrupted-legacy"))])).await;
    wait_task_request(&fixture, "interrupted-legacy").await;
    let worker = root.children().resolve("interrupted-legacy").unwrap();
    let original = worker
        .agent_state()
        .load()
        .await
        .unwrap()
        .unwrap()
        .execution
        .unwrap();
    let original_graph = root.agent_state().load().await.unwrap().unwrap().tasks;
    let path = state_path(&fixture.user(), &root);
    let worker_path = state_path(&fixture.user(), &worker);
    let failure_hold = Arc::new(Semaphore::new(0));
    script(
        &fixture,
        "AgentTask failed: interrupted-legacy",
        reply("waiting for resolution"),
        Some(failure_hold.clone()),
    );
    bounded(
        worker
            .get_service::<RuntimeSession<Security>>()
            .unwrap()
            .interrupt(&NeverCancelled),
    )
    .await
    .unwrap();
    bounded(async {
        while root.get_service::<Scheduler>().unwrap().snapshot().await[0].state
            != ExecutionStatus::Failed
        {
            tokio::task::yield_now().await;
        }
    })
    .await;
    let id = UserSessionId::parse(&hosted.snapshot().id).unwrap();
    bounded(hosted.close()).await.unwrap();
    let mut saved: AgentStateSnapshot =
        serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    saved.tasks = original_graph;
    std::fs::write(&path, serde_json::to_vec(&saved).unwrap()).unwrap();
    let mut saved: AgentStateSnapshot =
        serde_json::from_slice(&std::fs::read(&worker_path).unwrap()).unwrap();
    let stopped = saved.execution.as_mut().unwrap();
    assert_eq!(stopped.initial_message_id, original.initial_message_id);
    assert_eq!(
        stopped.terminal.as_ref().unwrap().outcome,
        ExecutionOutcome::Canceled
    );
    stopped.result_claim = original.result_claim;
    std::fs::write(&worker_path, serde_json::to_vec(&saved).unwrap()).unwrap();
    legacy_state::rewrite_snapshot_as_version_one(&path, &[0]);
    let owner_hold = Arc::new(Semaphore::new(0));
    script(
        &fixture,
        "AgentTask assigned to self: interrupted-legacy",
        reply("owner completed after recovery"),
        Some(owner_hold.clone()),
    );
    let resumed = reopen(&fixture, &id).await;
    let restored = fixture.root();
    wait_self_request(&fixture, "interrupted-legacy").await;
    let scheduler = restored.get_service::<Scheduler>().unwrap();
    assert_eq!(
        scheduler.capture_migration().await,
        TaskGraphMigrationSnapshot::Current
    );
    assert_eq!(
        scheduler
            .capture_detail("interrupted-legacy")
            .await
            .unwrap()
            .assigned_to,
        Some(TaskAssignee::Owner)
    );
    let execution = restored
        .children()
        .resolve("interrupted-legacy")
        .unwrap()
        .agent_state()
        .load()
        .await
        .unwrap()
        .unwrap()
        .execution
        .unwrap();
    assert_eq!(execution.initial_message_id, original.initial_message_id);
    assert_eq!(
        execution.terminal.unwrap().outcome,
        ExecutionOutcome::Canceled
    );
    assert_eq!(execution.result_claim, None);
    assert_eq!(task_requests(&fixture, "interrupted-legacy"), 1);
    let mut finished = task("interrupted-legacy");
    finished["state"] = "succeeded".into();
    finished["result"] = "verified recovered todo".into();
    set(&restored, json!([finished])).await;
    failure_hold.add_permits(1);
    owner_hold.add_permits(1);
    idle(&restored).await;
    bounded(resumed.close()).await.unwrap();
    std::fs::remove_dir_all(&fixture.root).unwrap();
}

#[tokio::test]
async fn actual_legacy_ambiguous_input_blocks_open_without_changes_and_allows_resolved_retry() {
    use parrot_domain::protocol::messages::parrot::{Delivery, Event, InputCanceled};
    use parrot_storage::{database::SessionDatabase, events::EventRepository};

    let fixture = fixture().await;
    let hosted = bounded(fixture.open()).await.unwrap();
    let root = fixture.root();
    script(
        &fixture,
        "Task: legacy-ambiguous\n",
        reply(r#"{"result":"unused","verdict":"accept"}"#),
        Some(Arc::new(Semaphore::new(0))),
    );
    set(&root, json!([dedicated(task("legacy-ambiguous"))])).await;
    wait_task_request(&fixture, "legacy-ambiguous").await;
    let worker = root.children().resolve("legacy-ambiguous").unwrap();
    let worker_id = worker.identity().session.session_id.clone();
    script(
        &fixture,
        "Task: completed-inner\n",
        reply(r#"{"result":"inner completed","verdict":"accept"}"#),
        None,
    );
    set(&worker, json!([dedicated(task("completed-inner"))])).await;
    wait_tasks(&worker, |tasks| {
        tasks[0].state == ExecutionStatus::Succeeded
    })
    .await;
    bounded(async {
        loop {
            let saved = worker.agent_state().load().await.unwrap().unwrap();
            let events = fixture.user().lease.events.replay().await.unwrap();
            if saved.tasks.notifications.is_empty()
                && events.iter().any(|event| {
                    event.agent_session_id == worker_id
                        && event.has_input_admitted()
                        && event
                            .input_admitted()
                            .content
                            .contains("AgentTask succeeded: completed-inner")
                })
            {
                return;
            }
            tokio::task::yield_now().await;
        }
    })
    .await;
    worker
        .get_service::<RuntimeSession<Security>>()
        .unwrap()
        .record(
            vec![ConversationPart::text("unrelated queued input")],
            "unrelated-legacy-queue".into(),
            Delivery::DELIVERY_QUEUE,
            Arc::new(NeverCancelled),
        )
        .await
        .unwrap();
    let pending = worker.history().capture_pending_input_ids().await.unwrap();
    assert!(pending.contains(&"unrelated-legacy-queue".to_owned()));
    assert!(pending.len() > 1);
    let path = state_path(&fixture.user(), &root);
    let worker_path = state_path(&fixture.user(), &worker);
    let selected_root = fixture.user().lease.selected_root().clone();
    let id = UserSessionId::parse(&hosted.snapshot().id).unwrap();
    bounded(hosted.close()).await.unwrap();
    let mut saved: AgentStateSnapshot =
        serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    saved.descriptor.selection.model_selector = "local/model-next".into();
    std::fs::write(&path, serde_json::to_vec(&saved).unwrap()).unwrap();
    let mut saved: AgentStateSnapshot =
        serde_json::from_slice(&std::fs::read(&worker_path).unwrap()).unwrap();
    saved.execution.as_mut().unwrap().phase =
        parrot_domain::values::agent_state::ExecutionPhase::Active;
    std::fs::write(&worker_path, serde_json::to_vec(&saved).unwrap()).unwrap();
    legacy_state::rewrite_snapshot_as_version_one(&worker_path, &[]);
    legacy_state::rewrite_snapshot_as_version_one(&path, &[0]);
    let original_root = std::fs::read(&path).unwrap();
    let original_worker = std::fs::read(&worker_path).unwrap();
    let requests = fixture.requests.lock().unwrap().len();
    for _ in 0..2 {
        let error = bounded(fixture.store.resume(&id, true, false))
            .await
            .err()
            .expect("ambiguous legacy input must block opening");
        assert!(
            error.to_string().contains("legacy task migration blocked"),
            "{error}"
        );
        assert_eq!(fixture.requests.lock().unwrap().len(), requests);
        assert!(
            std::fs::read(&path).unwrap() == original_root,
            "blocked opening must preserve the original version-one root bytes"
        );
        assert!(
            std::fs::read(&worker_path).unwrap() == original_worker,
            "blocked opening must preserve the original version-one running worker bytes"
        );
        let repository =
            EventRepository::new(SessionDatabase::open(selected_root.clone()).unwrap());
        assert_eq!(
            repository
                .capture_pending_input_ids(worker_id.clone())
                .await
                .unwrap(),
            pending
        );
        repository.database().close().await.unwrap();
    }
    let repository = EventRepository::new(SessionDatabase::open(selected_root).unwrap());
    for message_id in pending {
        let input = repository
            .find_input(worker_id.clone(), message_id.clone())
            .await
            .unwrap()
            .unwrap();
        let event_worker = worker_id.clone();
        let input_id = input.id.clone();
        repository
            .cancel_pending_input(worker_id.clone(), input.id, move || {
                let mut event = Event::new();
                event.id = format!("operator-resolved-{message_id}");
                event.agent_session_id = event_worker;
                event.set_input_canceled(InputCanceled {
                    input_id,
                    ..Default::default()
                });
                Ok(event)
            })
            .await
            .unwrap()
            .unwrap();
    }
    repository.database().close().await.unwrap();
    let owner_hold = Arc::new(Semaphore::new(0));
    script(
        &fixture,
        "AgentTask assigned to self: legacy-ambiguous",
        reply("owner completed resolved task"),
        Some(owner_hold.clone()),
    );
    let resumed = reopen(&fixture, &id).await;
    let restored = fixture.root();
    wait_self_request(&fixture, "legacy-ambiguous").await;
    assert_eq!(
        restored
            .selection()
            .current()
            .unwrap()
            .model
            .requested_selector
            .value,
        "local/model"
    );
    let saved = restored.agent_state().load().await.unwrap().unwrap();
    assert_eq!(saved.descriptor.selection.model_selector, "local/model");
    assert_eq!(saved.tasks.migration, TaskGraphMigrationSnapshot::Current);
    let mut finished = task("legacy-ambiguous");
    finished["state"] = "succeeded".into();
    finished["result"] = "owner completed after explicit resolution".into();
    set(&restored, json!([finished])).await;
    owner_hold.add_permits(1);
    idle(&restored).await;
    bounded(resumed.close()).await.unwrap();
    std::fs::remove_dir_all(&fixture.root).unwrap();
}
