use super::*;
use integrated_adapters::{ChildProfiles, reply, script};
use parrot_domain::{
    contracts::agent_tasks::TaskArtifactParser,
    values::{
        agent_state::{AGENT_STATE_FILE_NAME, AgentStateSnapshot},
        agent_tasks::{ExecutionStatus, Task},
        identifiers::UserSessionId,
    },
};
use parrot_runtime::{
    agent_turn::lifecycle::Signal,
    children::{CompletionDelivery, HistorySource, LaunchRequest},
};
use parrot_storage::{
    conversation::{HistoryForkBoundary, HistoryForkSelection},
    queue_files::Direction,
};
use tokio::sync::Semaphore;

type Scheduler = TaskScheduler<Security, TaskArtifactParserAdapter>;

async fn fixture() -> Fixture {
    Fixture::with_profiles(false, false, None, None, Arc::new(ChildProfiles)).await
}

async fn spawn(
    fixture: &Fixture,
    parent: &Arc<Scope<Security>>,
    name: &str,
) -> Arc<Scope<Security>> {
    bounded(
        parent
            .spawner()
            .spawn(LaunchRequest {
                parent: parent.session().clone(),
                requested_name: name.into(),
                requested_profile: "adapter".into(),
                model: fixture.router.resolve("local/model").unwrap(),
                requested_scope: format!("retained scope {name}"),
                delivery: CompletionDelivery::RetainedOnly,
                boundary: HistoryForkBoundary::AfterCompletedHistory,
                fork: HistoryForkSelection::parse("empty"),
                history_source: HistorySource::Parent,
            })
            .unwrap()
            .join(),
    )
    .await
    .unwrap()
}

async fn finish(scope: &Arc<Scope<Security>>, text: &str) {
    let sent = bounded(
        scope
            .get_service::<RuntimeSession<Security>>()
            .unwrap()
            .send_text_message(text.into(), Arc::new(NeverCancelled)),
    )
    .await
    .unwrap();
    bounded(sent.execution.join()).await.unwrap();
}

async fn set(scope: &Arc<Scope<Security>>, tasks: Value) {
    let tasks = TaskArtifactParserAdapter
        .parse_task_set(&tasks.to_string())
        .unwrap();
    bounded(
        scope
            .get_service::<Scheduler>()
            .unwrap()
            .set_tasks(
                tasks,
                scope.selection().current().unwrap(),
                HistoryForkBoundary::AfterCompletedHistory,
            )
            .unwrap()
            .join(),
    )
    .await
    .unwrap();
}

fn task(name: &str) -> Value {
    json!({"name": name, "description": format!("Do {name}"), "payload": "durable instruction", "acceptance_criteria": "verified"})
}

async fn wait_tasks(scope: &Arc<Scope<Security>>, predicate: impl Fn(&[Task]) -> bool) {
    let result = tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let tasks = scope.get_service::<Scheduler>().unwrap().snapshot().await;
            assert!(
                !tasks
                    .iter()
                    .any(|task| task.state == ExecutionStatus::Failed),
                "{tasks:?}"
            );
            if predicate(&tasks) {
                return;
            }
            tokio::task::yield_now().await;
        }
    })
    .await;
    assert!(
        result.is_ok(),
        "unfinished tasks: {:?}",
        scope.agent_state().load().await.unwrap().unwrap().tasks
    );
}

async fn idle(scope: &Arc<Scope<Security>>) {
    bounded(async {
        loop {
            let saved = scope.agent_state().load().await.unwrap().unwrap();
            if saved.tasks.notifications.is_empty() && !scope.session().is_active() {
                return;
            }
            tokio::task::yield_now().await;
        }
    })
    .await;
}

fn state_path(user: &UserServices<Security>, scope: &Scope<Security>) -> PathBuf {
    let names = scope
        .identity()
        .name_path
        .iter()
        .map(String::as_str)
        .collect::<Vec<_>>();
    user.resources
        .agent_scratch(&names)
        .unwrap()
        .root()
        .join(AGENT_STATE_FILE_NAME)
}

async fn reopen(fixture: &Fixture, id: &UserSessionId) -> Arc<dyn HostedSession> {
    let opened = bounded(fixture.store.resume(id, true, false))
        .await
        .unwrap();
    bounded(fixture.factory.bind_opened(opened)).await.unwrap()
}

#[tokio::test]
async fn actual_reopen_retains_nested_queues_graphs_and_idle_public_results_without_requests() {
    let fixture = fixture().await;
    let hosted = bounded(fixture.open()).await.unwrap();
    let root = fixture.root();
    let child = spawn(&fixture, &root, "completed").await;
    finish(&child, "completed child conversation").await;
    let grandchild = spawn(&fixture, &child, "unstarted").await;
    let mut done = task("saved-result");
    done["state"] = "succeeded".into();
    done["result"] = "retained result".into();
    done["hidden"] = true.into();
    set(&root, json!([done.clone()])).await;
    set(&child, json!([done])).await;
    idle(&root).await;
    idle(&child).await;
    let expected = fixture
        .user()
        .registry()
        .unwrap()
        .snapshot_scopes()
        .into_iter()
        .map(|scope| (scope.identity().session.session_id.clone(), scope))
        .collect::<BTreeMap<_, _>>();
    for scope in [&root, &child, &grandchild] {
        let queues = scope.get_service::<AgentQueues<Security>>().unwrap();
        let name = format!("retained-{}", scope.identity().session.name);
        queues.create(&name, "metadata").unwrap();
        queues
            .push(
                &name,
                &[scope.identity().session.name.clone()],
                Direction::Back,
                true,
                &NeverCancelled,
            )
            .unwrap();
    }
    let root_tasks = root.get_service::<Scheduler>().unwrap().snapshot().await;
    let child_tasks = child.get_service::<Scheduler>().unwrap().snapshot().await;
    let id = UserSessionId::parse(&hosted.snapshot().id).unwrap();
    bounded(hosted.close()).await.unwrap();
    let requests = fixture.requests.lock().unwrap().len();
    let resumed = reopen(&fixture, &id).await;
    assert_eq!(fixture.requests.lock().unwrap().len(), requests);
    let scopes = fixture.user().registry().unwrap().snapshot_scopes();
    assert_eq!(scopes.len(), 3);
    for scope in &scopes {
        let original = &expected[&scope.identity().session.session_id];
        assert!(!Arc::ptr_eq(scope, original));
        let current = &scope.identity().session;
        let previous = &original.identity().session;
        assert_eq!(
            (&current.name, &current.parent_session_id, current.depth),
            (&previous.name, &previous.parent_session_id, previous.depth)
        );
        assert_eq!(
            scope
                .identity()
                .prompt_scope
                .format(scope.identity().session.depth)
                .unwrap(),
            original
                .identity()
                .prompt_scope
                .format(original.identity().session.depth)
                .unwrap()
        );
        let queues = scope.get_service::<AgentQueues<Security>>().unwrap();
        assert_eq!(queues.capture_inventory().queues.len(), 1);
        let name = format!("retained-{}", scope.identity().session.name);
        assert!(queues.get(&name).unwrap().closed);
        assert_eq!(
            queues.try_take(&name, 1, Direction::Front).unwrap().items,
            [scope.identity().session.name.clone()]
        );
        assert!(!scope.session().is_active());
    }
    let restored_root = fixture.root();
    let restored_child = restored_root.children().resolve("completed").unwrap();
    assert_eq!(
        restored_root
            .get_service::<Scheduler>()
            .unwrap()
            .snapshot()
            .await,
        root_tasks
    );
    assert_eq!(
        restored_child
            .get_service::<Scheduler>()
            .unwrap()
            .snapshot()
            .await,
        child_tasks
    );
    let result = bounded(
        restored_child
            .get_service::<RuntimeSession<Security>>()
            .unwrap()
            .wait(0, &NeverCancelled),
    )
    .await
    .unwrap();
    assert_eq!(result.execution.unwrap().output, "deterministic reply");
    let unstarted = restored_child.children().resolve("unstarted").unwrap();
    assert!(
        unstarted
            .get_service::<RuntimeSession<Security>>()
            .unwrap()
            .wait(1, &NeverCancelled)
            .await
            .is_err()
    );
    let constructors = fixture.observations.lock().unwrap().constructors;
    for _ in 0..2 {
        let cancel = Arc::new(Signal::new());
        let mut listener = resumed.listen(None, cancel.clone()).unwrap();
        assert!(
            bounded(listener.next(&NeverCancelled))
                .await
                .unwrap()
                .is_some()
        );
        cancel.cancel();
    }
    assert_eq!(
        fixture.observations.lock().unwrap().constructors,
        constructors
    );
    assert_eq!(fixture.requests.lock().unwrap().len(), requests);
    bounded(resumed.close()).await.unwrap();
    std::fs::remove_dir_all(&fixture.root).unwrap();
}

#[tokio::test]
async fn actual_running_task_reopens_same_retry_prompt_attempt_and_execution_before_activation() {
    let fixture = fixture().await;
    let hosted = bounded(fixture.open()).await.unwrap();
    let root = fixture.root();
    let held = Arc::new(Semaphore::new(0));
    script(
        &fixture,
        "Task: durable-task\n",
        reply(
            r#"{"result":"first","verdict":"reject_and_retry","feedback":"fix","payload":"replacement instruction"}"#,
        ),
        None,
    );
    script(
        &fixture,
        "Task: durable-task\n",
        reply(r#"{"result":"unused","verdict":"accept"}"#),
        Some(held),
    );
    let mut dependent = task("dependent");
    dependent["dependencies"] = json!(["durable-task"]);
    set(&root, json!([task("durable-task"), dependent])).await;
    bounded(async {
        while !fixture.scripts.lock().unwrap().is_empty() {
            tokio::task::yield_now().await;
        }
    })
    .await;
    let before = root.agent_state().load().await.unwrap().unwrap();
    let attempt = before.tasks.entries[0].runner.clone().unwrap();
    assert_eq!(attempt.attempt, 2);
    assert_eq!(attempt.feedback, ["fix"]);
    let worker = root.children().resolve("durable-task").unwrap();
    let execution = worker
        .agent_state()
        .load()
        .await
        .unwrap()
        .unwrap()
        .execution
        .unwrap();
    let id = UserSessionId::parse(&hosted.snapshot().id).unwrap();
    bounded(hosted.close()).await.unwrap();
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
            .unwrap(),
        &attempt
    );
    let requests = fixture.requests.lock().unwrap().len();
    let release = Arc::new(Semaphore::new(0));
    script(
        &fixture,
        "Task: durable-task\n",
        reply(r#"{"result":"durable-result","verdict":"accept"}"#),
        Some(release.clone()),
    );
    script(
        &fixture,
        "Task: dependent\n",
        reply(r#"{"result":"dependent-result","verdict":"accept"}"#),
        None,
    );
    let opened = bounded(fixture.store.resume(&id, true, false))
        .await
        .unwrap();
    let restored_root = fixture.root();
    let restored_worker = restored_root.children().resolve("durable-task").unwrap();
    assert_eq!(
        fixture.requests.lock().unwrap().len(),
        requests,
        "storage preparation must keep provider activation closed"
    );
    let saved = restored_root.agent_state().load().await.unwrap().unwrap();
    assert_eq!(saved.tasks.entries[0].runner.as_ref().unwrap(), &attempt);
    let restored_execution = restored_worker
        .agent_state()
        .load()
        .await
        .unwrap()
        .unwrap()
        .execution
        .unwrap();
    assert_eq!(restored_execution.execution_id, execution.execution_id);
    assert_eq!(
        restored_execution.initial_message_id,
        execution.initial_message_id
    );
    assert_eq!(
        restored_worker.identity().session.session_id,
        worker.identity().session.session_id
    );
    let resumed = bounded(fixture.factory.bind_opened(opened)).await.unwrap();
    bounded(async {
        while fixture.scripts.lock().unwrap().len() != 1 {
            tokio::task::yield_now().await;
        }
    })
    .await;
    assert_eq!(fixture.requests.lock().unwrap().len(), requests + 1);
    release.add_permits(1);
    wait_tasks(&restored_root, |tasks| {
        tasks
            .iter()
            .all(|task| task.state == ExecutionStatus::Succeeded)
    })
    .await;
    let events = bounded(fixture.user().lease.events.replay()).await.unwrap();
    assert_eq!(
        events
            .iter()
            .filter(|event| event.has_input_admitted()
                && event.input_admitted().message_id == attempt.prompt_message_id)
            .count(),
        1
    );
    assert!(fixture.requests.lock().unwrap().iter().any(|request| {
        request.messages.iter().any(|message| {
            message.content().contains("Task: dependent\n")
                && message.content().contains("durable-result")
        })
    }));
    bounded(resumed.close()).await.unwrap();
    std::fs::remove_dir_all(&fixture.root).unwrap();
}

#[tokio::test]
async fn actual_resumed_provider_tool_sends_to_another_restored_owned_execution_after_global_reservation()
 {
    let fixture = fixture().await;
    let hosted = bounded(fixture.open()).await.unwrap();
    let root = fixture.root();
    let source = spawn(&fixture, &root, "source").await;
    let target = spawn(&fixture, &root, "target").await;
    let mut executions = Vec::new();
    for (scope, marker) in [
        (&source, "cross-child-source"),
        (&target, "cross-child-target"),
    ] {
        let barrier = Arc::new(TurnBarrier::default());
        fixture
            .turn_barriers
            .lock()
            .unwrap()
            .insert(marker.into(), barrier.clone());
        let _sent = bounded(
            scope
                .get_service::<RuntimeSession<Security>>()
                .unwrap()
                .send_text_message(marker.into(), Arc::new(NeverCancelled)),
        )
        .await
        .unwrap();
        bounded(barrier.entered.notified()).await;
        executions.push(
            scope
                .agent_state()
                .load()
                .await
                .unwrap()
                .unwrap()
                .execution
                .unwrap(),
        );
    }
    let id = UserSessionId::parse(&hosted.snapshot().id).unwrap();
    bounded(hosted.close()).await.unwrap();
    let requests = fixture.requests.lock().unwrap().len();
    let target_release = Arc::new(Semaphore::new(0));
    let mut send = reply("");
    send.tool_calls.push(parrot_provider::sessions::ToolCall {
        id: "cross-child-send".into(), name: "agent_send".into(),
        arguments_json: json!({"name":format!("/{}/target", root.identity().session.name), "message":"resumed sibling relay"}).to_string(),
    });
    script(&fixture, "cross-child-source", send, None);
    script(
        &fixture,
        "cross-child-source",
        reply("source complete"),
        None,
    );
    script(
        &fixture,
        "cross-child-target",
        reply("target complete"),
        Some(target_release.clone()),
    );
    let opened = bounded(fixture.store.resume(&id, true, false))
        .await
        .unwrap();
    let restored_root = fixture.root();
    let restored_source = restored_root.children().resolve("source").unwrap();
    let restored_target = restored_root.children().resolve("target").unwrap();
    assert_eq!(fixture.requests.lock().unwrap().len(), requests);
    for (scope, execution) in [
        (&restored_source, &executions[0]),
        (&restored_target, &executions[1]),
    ] {
        assert_eq!(
            scope
                .agent_state()
                .load()
                .await
                .unwrap()
                .unwrap()
                .execution
                .unwrap()
                .execution_id,
            execution.execution_id
        );
        assert!(
            scope
                .get_service::<RuntimeSession<Security>>()
                .unwrap()
                .wait(1, &NeverCancelled)
                .await
                .unwrap()
                .yielded
        );
    }
    let resumed = bounded(fixture.factory.bind_opened(opened)).await.unwrap();
    bounded(async {
        loop {
            let events = fixture.user().lease.events.replay().await.unwrap();
            if events.iter().any(|event| {
                event.has_tool_finished()
                    && event.tool_finished().tool_call_id == "cross-child-send"
            }) {
                return;
            }
            tokio::task::yield_now().await;
        }
    })
    .await;
    let history = restored_source.history().conversation().await.unwrap();
    let receipt = history
        .iter()
        .find(|item| item.message.tool_call_id == "cross-child-send")
        .unwrap();
    assert!(!parrot_storage::events::content(&receipt.message.parts).contains("error:"));
    let events = fixture.user().lease.events.replay().await.unwrap();
    assert!(events.iter().any(|event| event.agent_session_id
        == restored_target.identity().session.session_id
        && event.has_input_admitted()
        && event.input_admitted().content == "resumed sibling relay"));
    for execution in executions {
        assert_eq!(
            events
                .iter()
                .filter(|event| event.has_input_admitted()
                    && event.input_admitted().message_id == execution.initial_message_id)
                .count(),
            1
        );
    }
    target_release.add_permits(1);
    idle(&restored_source).await;
    idle(&restored_target).await;
    bounded(resumed.close()).await.unwrap();
    std::fs::remove_dir_all(&fixture.root).unwrap();
}

#[tokio::test]
async fn actual_terminal_checkpoint_and_absent_checkpoint_recover_new_durable_inputs() {
    for terminal in [false, true] {
        let fixture = fixture().await;
        let hosted = bounded(fixture.open()).await.unwrap();
        let root = fixture.root();
        if terminal {
            finish(&root, "old completed execution").await;
        }
        let runtime = root.get_service::<RuntimeSession<Security>>().unwrap();
        bounded(runtime.record(
            vec![ConversationPart::text("new durable pending input")],
            "resume-pending-input".into(),
            parrot_domain::protocol::messages::parrot::Delivery::DELIVERY_QUEUE,
            Arc::new(NeverCancelled),
        ))
        .await
        .unwrap();
        let id = UserSessionId::parse(&hosted.snapshot().id).unwrap();
        bounded(hosted.close()).await.unwrap();
        let requests = fixture.requests.lock().unwrap().len();
        let resumed = reopen(&fixture, &id).await;
        let restored = fixture.root();
        idle(&restored).await;
        assert_eq!(fixture.requests.lock().unwrap().len(), requests + 1);
        assert!(
            !restored
                .get_service::<RuntimeSession<Security>>()
                .unwrap()
                .has_recovery_work()
                .await
                .unwrap()
        );
        bounded(resumed.close()).await.unwrap();
        std::fs::remove_dir_all(&fixture.root).unwrap();
    }
}

#[tokio::test]
async fn actual_damaged_referenced_snapshots_fail_before_any_resumed_provider_request() {
    for damage in ["missing", "corrupt", "owner", "selector"] {
        let fixture = fixture().await;
        let hosted = bounded(fixture.open()).await.unwrap();
        let root = fixture.root();
        let child = spawn(&fixture, &root, "retained").await;
        let barrier = Arc::new(TurnBarrier::default());
        fixture
            .turn_barriers
            .lock()
            .unwrap()
            .insert("held-root-recovery".into(), barrier.clone());
        let _sent = bounded(
            root.get_service::<RuntimeSession<Security>>()
                .unwrap()
                .send_text_message("held-root-recovery".into(), Arc::new(NeverCancelled)),
        )
        .await
        .unwrap();
        bounded(barrier.entered.notified()).await;
        let path = state_path(&fixture.user(), &child);
        let id = UserSessionId::parse(&hosted.snapshot().id).unwrap();
        bounded(hosted.close()).await.unwrap();
        match damage {
            "missing" => std::fs::remove_file(&path).unwrap(),
            "corrupt" => std::fs::write(&path, b"{").unwrap(),
            _ => {
                let mut saved: AgentStateSnapshot =
                    serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
                if damage == "owner" {
                    saved.owner_session_id = "wrong-owner".into();
                } else {
                    saved.descriptor.selection.model_selector = "missing/model".into();
                }
                std::fs::write(&path, serde_json::to_vec(&saved).unwrap()).unwrap();
            }
        }
        let requests = fixture.requests.lock().unwrap().len();
        assert!(
            bounded(fixture.store.resume(&id, true, false))
                .await
                .is_err(),
            "{damage}"
        );
        assert_eq!(fixture.requests.lock().unwrap().len(), requests, "{damage}");
        assert!(fixture.user().lifetime.is_cancelled());
        if damage != "missing" {
            assert!(path.is_file());
        }
    }
}

#[tokio::test]
async fn actual_close_joins_admitted_task_worker_construction_and_reopens_pending_generation() {
    let fixture = fixture().await;
    let hosted = bounded(fixture.open()).await.unwrap();
    let root = fixture.root();
    let gate = Arc::new(TurnBarrier::default());
    fixture.observations.lock().unwrap().construction = Some(("constructing".into(), gate.clone()));
    let tasks = TaskArtifactParserAdapter
        .parse_task_set(&json!([task("constructing")]).to_string())
        .unwrap();
    let update = root
        .get_service::<Scheduler>()
        .unwrap()
        .set_tasks(
            tasks,
            root.selection().current().unwrap(),
            HistoryForkBoundary::AfterCompletedHistory,
        )
        .unwrap();
    bounded(gate.entered.notified()).await;
    let id = UserSessionId::parse(&hosted.snapshot().id).unwrap();
    let closing = {
        let hosted = hosted.clone();
        tokio::spawn(async move { hosted.close().await })
    };
    bounded(async {
        while fixture.user().registry().unwrap().is_accepting() {
            tokio::task::yield_now().await;
        }
    })
    .await;
    assert!(!closing.is_finished());
    assert!(!fixture.user().lifetime.is_cancelled());
    gate.release.add_permits(1);
    bounded(closing).await.unwrap().unwrap();
    bounded(update.join()).await.unwrap();
    let saved = root.agent_state().load().await.unwrap().unwrap();
    assert_eq!(saved.tasks.entries[0].task.state, ExecutionStatus::Pending);
    assert!(saved.tasks.entries[0].task.failure.is_none());
    assert!(fixture.requests.lock().unwrap().is_empty());
    script(
        &fixture,
        "Task: constructing\n",
        reply(r#"{"result":"constructed after reopen","verdict":"accept"}"#),
        None,
    );
    let resumed = reopen(&fixture, &id).await;
    wait_tasks(&fixture.root(), |tasks| {
        tasks[0].state == ExecutionStatus::Succeeded
    })
    .await;
    assert_eq!(
        fixture
            .root()
            .agent_state()
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
        saved.tasks.entries[0]
            .runner
            .as_ref()
            .unwrap()
            .generation_id
    );
    bounded(resumed.close()).await.unwrap();
    std::fs::remove_dir_all(&fixture.root).unwrap();
}

#[tokio::test]
async fn actual_nested_running_graphs_suspend_together_and_reopen_without_duplicate_owners() {
    let fixture = fixture().await;
    let hosted = bounded(fixture.open()).await.unwrap();
    let root = fixture.root();
    script(
        &fixture,
        "Task: outer\n",
        reply(r#"{"result":"outer","verdict":"accept"}"#),
        Some(Arc::new(Semaphore::new(0))),
    );
    set(&root, json!([task("outer")])).await;
    bounded(async {
        while !fixture.scripts.lock().unwrap().is_empty() {
            tokio::task::yield_now().await;
        }
    })
    .await;
    let outer = root.children().resolve("outer").unwrap();
    script(
        &fixture,
        "Task: inner\n",
        reply(r#"{"result":"inner","verdict":"accept"}"#),
        Some(Arc::new(Semaphore::new(0))),
    );
    set(&outer, json!([task("inner")])).await;
    bounded(async {
        while !fixture.scripts.lock().unwrap().is_empty() {
            tokio::task::yield_now().await;
        }
    })
    .await;
    let inner = outer.children().resolve("inner").unwrap();
    let outer_execution = outer
        .agent_state()
        .load()
        .await
        .unwrap()
        .unwrap()
        .execution
        .unwrap();
    let inner_execution = inner
        .agent_state()
        .load()
        .await
        .unwrap()
        .unwrap()
        .execution
        .unwrap();
    let id = UserSessionId::parse(&hosted.snapshot().id).unwrap();
    bounded(hosted.close()).await.unwrap();
    for scope in [&root, &outer] {
        assert_eq!(
            scope
                .agent_state()
                .load()
                .await
                .unwrap()
                .unwrap()
                .tasks
                .entries[0]
                .task
                .state,
            ExecutionStatus::Running
        );
    }
    let requests = fixture.requests.lock().unwrap().len();
    let release = Arc::new(Semaphore::new(0));
    script(
        &fixture,
        "Task: inner\n",
        reply(r#"{"result":"inner","verdict":"accept"}"#),
        None,
    );
    script(
        &fixture,
        "Task: outer\n",
        reply(r#"{"result":"outer","verdict":"accept"}"#),
        Some(release.clone()),
    );
    script(
        &fixture,
        "Task: outer\n",
        reply(r#"{"result":"outer","verdict":"accept"}"#),
        None,
    );
    let opened = bounded(fixture.store.resume(&id, true, false))
        .await
        .unwrap();
    assert_eq!(fixture.requests.lock().unwrap().len(), requests);
    let restored_root = fixture.root();
    let restored_outer = restored_root.children().resolve("outer").unwrap();
    let restored_inner = restored_outer.children().resolve("inner").unwrap();
    assert_eq!(
        restored_outer
            .agent_state()
            .load()
            .await
            .unwrap()
            .unwrap()
            .execution
            .unwrap()
            .execution_id,
        outer_execution.execution_id
    );
    assert_eq!(
        restored_inner
            .agent_state()
            .load()
            .await
            .unwrap()
            .unwrap()
            .execution
            .unwrap()
            .execution_id,
        inner_execution.execution_id
    );
    let resumed = bounded(fixture.factory.bind_opened(opened)).await.unwrap();
    wait_tasks(&restored_outer, |tasks| {
        tasks[0].state == ExecutionStatus::Succeeded
    })
    .await;
    bounded(async {
        while !restored_outer
            .agent_state()
            .load()
            .await
            .unwrap()
            .unwrap()
            .tasks
            .notifications
            .is_empty()
        {
            tokio::task::yield_now().await;
        }
    })
    .await;
    release.add_permits(1);
    wait_tasks(&restored_root, |tasks| {
        tasks[0].state == ExecutionStatus::Succeeded
    })
    .await;
    let events = fixture.user().lease.events.replay().await.unwrap();
    let completed = restored_root.agent_state().load().await.unwrap().unwrap();
    let runner = completed.tasks.entries[0].runner.as_ref().unwrap();
    assert_eq!(runner.attempt, 1);
    assert_eq!(runner.repair_attempt, 0);
    for execution in [&outer_execution, &inner_execution] {
        assert_eq!(
            events
                .iter()
                .filter(|event| event.has_input_admitted()
                    && event.input_admitted().message_id == execution.initial_message_id)
                .count(),
            1
        );
    }
    bounded(resumed.close()).await.unwrap();
    std::fs::remove_dir_all(&fixture.root).unwrap();
}

#[tokio::test]
async fn actual_unstarted_full_fork_does_not_activate_inherited_unfinished_parent_context() {
    let fixture = fixture().await;
    let hosted = bounded(fixture.open()).await.unwrap();
    let root = fixture.root();
    let barrier = Arc::new(TurnBarrier::default());
    fixture
        .turn_barriers
        .lock()
        .unwrap()
        .insert("parent unfinished context".into(), barrier.clone());
    let _sent = bounded(
        root.get_service::<RuntimeSession<Security>>()
            .unwrap()
            .send_text_message("parent unfinished context".into(), Arc::new(NeverCancelled)),
    )
    .await
    .unwrap();
    bounded(barrier.entered.notified()).await;
    let child = bounded(
        root.spawner()
            .spawn(LaunchRequest {
                parent: root.session().clone(),
                requested_name: "never-started-fork".into(),
                requested_profile: "adapter".into(),
                model: fixture.router.resolve("local/model").unwrap(),
                requested_scope: String::new(),
                delivery: CompletionDelivery::RetainedOnly,
                boundary: HistoryForkBoundary::AfterSafeHistoryPrefix,
                fork: HistoryForkSelection::parse("full"),
                history_source: HistorySource::Parent,
            })
            .unwrap()
            .join(),
    )
    .await
    .unwrap();
    assert_eq!(
        child
            .history()
            .conversation()
            .await
            .unwrap()
            .last()
            .unwrap()
            .message
            .role,
        parrot_storage::conversation::MessageRole::User
    );
    let id = UserSessionId::parse(&hosted.snapshot().id).unwrap();
    bounded(hosted.close()).await.unwrap();
    let requests = fixture.requests.lock().unwrap().len();
    let resumed = reopen(&fixture, &id).await;
    idle(&fixture.root()).await;
    let restored = fixture
        .root()
        .children()
        .resolve("never-started-fork")
        .unwrap();
    idle(&restored).await;
    assert_eq!(
        fixture.requests.lock().unwrap().len(),
        requests + 1,
        "only the unfinished parent owns recoverable work"
    );
    assert!(
        restored
            .get_service::<RuntimeSession<Security>>()
            .unwrap()
            .wait(1, &NeverCancelled)
            .await
            .is_err()
    );
    bounded(resumed.close()).await.unwrap();
    std::fs::remove_dir_all(&fixture.root).unwrap();
}

#[tokio::test]
async fn actual_cold_canceled_consumed_input_does_not_restart_from_answerable_history() {
    let fixture = fixture().await;
    let hosted = bounded(fixture.open()).await.unwrap();
    let root = fixture.root();
    let barrier = Arc::new(TurnBarrier::default());
    fixture
        .turn_barriers
        .lock()
        .unwrap()
        .insert("cold canceled consumed input".into(), barrier.clone());
    let _sent = bounded(
        root.get_service::<RuntimeSession<Security>>()
            .unwrap()
            .send_text_message(
                "cold canceled consumed input".into(),
                Arc::new(NeverCancelled),
            ),
    )
    .await
    .unwrap();
    bounded(barrier.entered.notified()).await;
    let id = UserSessionId::parse(&hosted.snapshot().id).unwrap();
    bounded(hosted.close()).await.unwrap();
    let mut execution = root
        .agent_state()
        .load()
        .await
        .unwrap()
        .unwrap()
        .execution
        .unwrap();
    execution.phase = parrot_domain::values::agent_state::ExecutionPhase::Canceled;
    root.agent_state()
        .update(parrot_domain::values::agent_state::AgentStateUpdate::Execution(Some(execution)))
        .await
        .unwrap();
    let requests = fixture.requests.lock().unwrap().len();
    let resumed = reopen(&fixture, &id).await;
    idle(&fixture.root()).await;
    assert_eq!(fixture.requests.lock().unwrap().len(), requests);
    bounded(resumed.close()).await.unwrap();
    std::fs::remove_dir_all(&fixture.root).unwrap();
}

#[tokio::test]
async fn actual_absent_legacy_root_snapshot_warns_and_does_not_reconstruct_lineage_children() {
    let fixture = fixture().await;
    let hosted = bounded(fixture.open()).await.unwrap();
    let root = fixture.root();
    finish(&root, "root history").await;
    let child = spawn(&fixture, &root, "historical").await;
    finish(&child, "historical child lineage").await;
    let user = fixture.user();
    let path = state_path(&user, &root);
    let child_path = state_path(&user, &child);
    let id = UserSessionId::parse(&hosted.snapshot().id).unwrap();
    assert!(
        !user
            .lease
            .events
            .replay()
            .await
            .unwrap()
            .iter()
            .any(|event| event.has_agent_notice()
                && event.agent_notice().kind == "legacy_root_only_resume")
    );
    bounded(hosted.close()).await.unwrap();
    std::fs::remove_file(path).unwrap();
    let requests = fixture.requests.lock().unwrap().len();
    let resumed = reopen(&fixture, &id).await;
    assert_eq!(
        fixture.user().registry().unwrap().snapshot_scopes().len(),
        1
    );
    assert!(child_path.is_file());
    assert_eq!(fixture.requests.lock().unwrap().len(), requests);
    let events = fixture.user().lease.events.replay().await.unwrap();
    let notices = events
        .iter()
        .filter(|event| {
            event.has_agent_notice() && event.agent_notice().kind == "legacy_root_only_resume"
        })
        .collect::<Vec<_>>();
    assert_eq!(notices.len(), 1);
    assert!(
        notices[0]
            .agent_notice()
            .notice
            .contains("Only the root agent was reopened")
    );
    bounded(resumed.close()).await.unwrap();
    std::fs::remove_dir_all(&fixture.root).unwrap();
}
