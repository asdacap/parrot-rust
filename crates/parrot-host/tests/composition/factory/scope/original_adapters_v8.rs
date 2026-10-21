//! Original TaskRunner integration follow-up; frozen v7's broader rejection remains.
//! Only provider replies are scripted. Catalog, forks, workers, history and joins are real.
use super::{
    integrated_adapters::{ChildProfiles, reply, script},
    *,
};
use parrot_domain::{
    protocol::messages::parrot::{AgentTaskProgressSnapshot, AgentTaskProgressStatus, Delivery},
    values::agent_tasks::ExecutionStatus,
};
use parrot_runtime::task_scheduler::PROGRESS_KIND;
use protobuf::Message as _;
use tokio::sync::Semaphore;

const OWNER_PREFIX: &str = "original-owner-prefix-v8";
const INVOKING_BATCH: &str = "original-invoking-batch-not-forked-v8";
const CALL_ID: &str = "original-catalog-task-call-v8";
const FIRST: &str = "original-first-v8";
const SECOND: &str = "original-second-v8";
const FIRST_PAYLOAD: &str = "original-first-worker-payload-v8";
const SECOND_PAYLOAD: &str = "original-second-worker-payload-v8";
const FIRST_RESULT: &str = "original-first-transcript-result-v8";
const SECOND_RESULT: &str = "original-second-transcript-result-v8";

fn conversation(request: &Request) -> String {
    request
        .messages
        .iter()
        .map(|message| message.content())
        .collect::<Vec<_>>()
        .join("\n")
}
fn accepted(result: &str) -> String {
    json!({"result":result,"verdict":"accept"}).to_string()
}

#[tokio::test]
async fn production_catalog_task_runner_forks_execute_publish_success_and_join_actual_owner() {
    let fixture = Fixture::with_profiles(false, false, None, None, Arc::new(ChildProfiles)).await;
    let hosted = fixture.open().await.unwrap();
    let owner = fixture.root();
    let user = fixture.user();
    let controller = owner
        .get_service::<ScopedTaskController<Security, ContextSelection<Security>>>()
        .unwrap();
    let first_gate = Arc::new(Semaphore::new(0));
    let second_gate = Arc::new(Semaphore::new(0));
    let before_notification = Arc::new(Semaphore::new(0));
    let after_notification = Arc::new(Semaphore::new(0));
    let mut dispatch = reply(INVOKING_BATCH);
    dispatch.tool_calls.push(parrot_provider::sessions::ToolCall {
        id: CALL_ID.into(),
        name: "set_agent_tasks".into(),
        arguments_json: json!({"tasks":[
            {"name":FIRST,"description":"Do first","payload":FIRST_PAYLOAD,"acceptance_criteria":"return strict accept JSON","model":"local/model"},
            {"name":SECOND,"description":"Do second","payload":SECOND_PAYLOAD,"acceptance_criteria":"return strict accept JSON","dependencies":[FIRST],"model":"local/model"}
        ]}).to_string(),
    });
    script(&fixture, OWNER_PREFIX, dispatch, None);
    // Worker steps precede owner follow-ups because workers inherit the safe owner prefix.
    script(
        &fixture,
        FIRST_PAYLOAD,
        reply(&accepted(FIRST_RESULT)),
        Some(first_gate.clone()),
    );
    script(
        &fixture,
        SECOND_PAYLOAD,
        reply(&accepted(SECOND_RESULT)),
        Some(second_gate.clone()),
    );
    script(
        &fixture,
        OWNER_PREFIX,
        reply("owner noted admission"),
        Some(before_notification.clone()),
    );
    script(
        &fixture,
        OWNER_PREFIX,
        reply("owner noted first success"),
        Some(after_notification.clone()),
    );
    hosted
        .send(
            vec![ConversationPart::text(OWNER_PREFIX)],
            "original-catalog-task-input-v8".into(),
            Delivery::DELIVERY_STEER,
            Arc::new(NeverCancelled),
        )
        .await
        .unwrap();

    // Initial catalog invocation, first worker and held owner follow-up have arrived;
    // dependency worker and second owner follow-up have not yet been requested.
    bounded(async {
        while fixture.scripts.lock().unwrap().len() != 2 {
            tokio::task::yield_now().await;
        }
    })
    .await;
    let running = controller.scheduler().snapshot().await;
    assert_eq!(running.len(), 2);
    assert_eq!(running[0].state, ExecutionStatus::Running);
    assert_eq!(running[1].state, ExecutionStatus::Pending);
    let first_detail = controller.scheduler().capture_detail(FIRST).await.unwrap();
    let first = owner
        .children()
        .find(first_detail.agent_name.as_deref().unwrap())
        .unwrap();
    assert!(user.registry().unwrap().contains(&first));
    assert!(Arc::ptr_eq(&first.parent_scope().parent().unwrap(), &owner));
    assert_eq!(
        first.identity().session.parent_session_id,
        owner.identity().session.session_id
    );
    assert_eq!(
        first.selection().capture().unwrap().profile.id,
        "agent-task-payload"
    );
    assert!(first.session().is_active());
    let first_request = fixture
        .requests
        .lock()
        .unwrap()
        .iter()
        .find(|request| conversation(request).contains(FIRST_PAYLOAD))
        .unwrap()
        .clone();
    let first_history = conversation(&first_request);
    assert!(first_history.contains(OWNER_PREFIX), "{first_history}");
    assert!(!first_history.contains(INVOKING_BATCH), "{first_history}");
    assert!(
        !first_request
            .messages
            .iter()
            .any(|message| message.tool_calls.iter().any(|call| call.id == CALL_ID))
    );
    let owner_history = owner.history().conversation().await.unwrap();
    let invoking = owner_history
        .iter()
        .find(|item| {
            item.message
                .tool_calls
                .iter()
                .any(|call| call.id == CALL_ID)
        })
        .unwrap();
    assert_eq!(invoking.message.tool_calls[0].name, "set_agent_tasks");
    assert!(parrot_storage::events::content(&invoking.message.parts).contains(INVOKING_BATCH));
    let tool_result = owner_history
        .iter()
        .find(|item| item.message.tool_call_id == CALL_ID)
        .unwrap();
    assert!(tool_result.sequence > invoking.sequence);
    assert!(!parrot_storage::events::content(&tool_result.message.parts).starts_with("error:"));
    let admitted = user.lease.events.replay().await.unwrap();
    assert!(
        admitted
            .iter()
            .any(|event| event.has_tool_started() && event.tool_started().tool_call_id == CALL_ID)
    );
    let finished = admitted
        .iter()
        .find(|event| event.has_tool_finished() && event.tool_finished().tool_call_id == CALL_ID)
        .unwrap();
    assert_eq!(
        finished.agent_session_id,
        owner.identity().session.session_id
    );
    assert!(
        !finished
            .tool_finished()
            .result
            .as_deref()
            .unwrap()
            .starts_with("error:")
    );
    assert!(fixture.requests.lock().unwrap().iter().any(|request| {
        request
            .tools
            .iter()
            .any(|tool| tool.name == "set_agent_tasks")
    }));

    let wait = integrated_adapters::agent_tool(
        &fixture,
        &owner,
        parrot_tools::agents::AgentToolKind::Wait,
    );
    let waiting =
        integrated_adapters::agent_invoke(&wait, json!({"duration_ms":10000}), &NeverCancelled);
    tokio::pin!(waiting);
    tokio::select! {
        result = &mut waiting => panic!("wait returned before successful TaskRunner completion: {result:?}"),
        _ = tokio::task::yield_now() => {}
    }
    first_gate.add_permits(1);
    assert_eq!(
        bounded(waiting).await.unwrap(),
        "wait interrupted due to AgentTask update"
    );
    bounded(async {
        loop {
            let tasks = controller.scheduler().snapshot().await;
            assert_ne!(tasks[0].state, ExecutionStatus::Failed, "{tasks:?}");
            assert_ne!(tasks[1].state, ExecutionStatus::Failed, "{tasks:?}");
            if tasks[0].state == ExecutionStatus::Succeeded
                && tasks[1].state == ExecutionStatus::Running
            {
                assert_eq!(tasks[0].result.as_deref(), Some(FIRST_RESULT));
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await;
    let second_detail = controller.scheduler().capture_detail(SECOND).await.unwrap();
    let second = owner
        .children()
        .find(second_detail.agent_name.as_deref().unwrap())
        .unwrap();
    assert!(Arc::ptr_eq(
        &second.parent_scope().parent().unwrap(),
        &owner
    ));
    assert_ne!(
        first.identity().session.session_id,
        second.identity().session.session_id
    );
    assert_eq!(
        second.selection().capture().unwrap().profile.id,
        "agent-task-payload"
    );
    bounded(async {
        while fixture.scripts.lock().unwrap().len() != 1 {
            tokio::task::yield_now().await;
        }
    })
    .await;
    let second_request = fixture
        .requests
        .lock()
        .unwrap()
        .iter()
        .find(|request| conversation(request).contains(SECOND_PAYLOAD))
        .unwrap()
        .clone();
    let second_history = conversation(&second_request);
    assert!(second_history.contains(OWNER_PREFIX), "{second_history}");
    assert!(second_request.messages.iter().any(|message| message.role
        == parrot_provider::sessions::Role::Assistant
        && message.content() == accepted(FIRST_RESULT)));
    assert!(!second_history.contains(INVOKING_BATCH), "{second_history}");
    assert!(
        second_history.contains(&format!("[{FIRST}] Do first\nResult: {FIRST_RESULT}")),
        "{second_history}"
    );

    let expected_first_notice =
        format!("AgentTask succeeded: {FIRST}\nDescription: Do first\n\nResult:\n{FIRST_RESULT}");
    bounded(async {
        loop {
            let events = user.lease.events.replay().await.unwrap();
            if let Some(notice) = events.iter().find(|event| {
                event.has_input_admitted()
                    && event
                        .input_admitted()
                        .content
                        .contains(&expected_first_notice)
            }) {
                assert_eq!(notice.agent_session_id, owner.identity().session.session_id);
                assert!(!events.iter().any(|event| {
                    event.has_input_admitted()
                        && event
                            .input_admitted()
                            .content
                            .contains("All AgentTasks are complete:")
                }));
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await;
    before_notification.add_permits(1);
    bounded(async {
        while !fixture.scripts.lock().unwrap().is_empty() {
            tokio::task::yield_now().await;
        }
    })
    .await;
    {
        let requests = fixture.requests.lock().unwrap();
        let notified_owner = requests
            .iter()
            .find(|request| conversation(request).contains(&expected_first_notice))
            .unwrap();
        assert!(!conversation(notified_owner).contains("All AgentTasks are complete:"));
        assert!(!conversation(notified_owner).contains(SECOND_PAYLOAD));
    }
    assert_eq!(
        controller.scheduler().snapshot().await[1].state,
        ExecutionStatus::Running
    );

    second_gate.add_permits(1);
    bounded(async {
        loop {
            let tasks = controller.scheduler().snapshot().await;
            assert!(
                tasks
                    .iter()
                    .all(|task| task.state != ExecutionStatus::Failed),
                "{tasks:?}"
            );
            if tasks
                .iter()
                .all(|task| task.state == ExecutionStatus::Succeeded)
            {
                assert_eq!(tasks[0].result.as_deref(), Some(FIRST_RESULT));
                assert_eq!(tasks[1].result.as_deref(), Some(SECOND_RESULT));
                assert!(tasks.iter().all(|task| task.hidden));
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await;
    let expected_complete =
        format!("All AgentTasks are complete:\n- {FIRST}: succeeded\n- {SECOND}: succeeded");
    bounded(async {
        while !user
            .lease
            .events
            .replay()
            .await
            .unwrap()
            .iter()
            .any(|event| {
                event.has_input_admitted()
                    && event.agent_session_id == owner.identity().session.session_id
                    && event.input_admitted().content == expected_complete
            })
        {
            tokio::task::yield_now().await;
        }
    })
    .await;
    after_notification.add_permits(1);
    bounded(async {
        while owner.session().is_active()
            || first.session().is_active()
            || second.session().is_active()
        {
            tokio::task::yield_now().await;
        }
    })
    .await;
    bounded(controller.scheduler().settle().join())
        .await
        .unwrap();
    for (worker, result) in [(&first, FIRST_RESULT), (&second, SECOND_RESULT)] {
        assert!(worker.history().conversation().await.unwrap().iter().any(
            |item| item.message.role == parrot_storage::conversation::MessageRole::Assistant
                && parrot_storage::events::content(&item.message.parts) == accepted(result)
        ));
    }
    let durable = user.lease.events.replay().await.unwrap();
    let progress: Vec<_> = durable
        .iter()
        .filter(|event| event.has_tool_snapshot() && event.tool_snapshot().kind == PROGRESS_KIND)
        .collect();
    assert!(progress.len() >= 3);
    for event in &progress {
        assert_eq!(event.agent_session_id, owner.identity().session.session_id);
        assert_eq!(
            event.tool_snapshot().owner_agent_session_id,
            owner.identity().session.session_id
        );
    }
    let terminal =
        AgentTaskProgressSnapshot::parse_from_bytes(&progress.last().unwrap().tool_snapshot().data)
            .unwrap();
    assert_eq!(terminal.root_nodes.len(), 2);
    for (node, worker) in terminal.root_nodes.iter().zip([&first, &second]) {
        assert_eq!(node.agent_session_id, worker.identity().session.session_id);
        assert_eq!(
            node.status.enum_value().unwrap(),
            AgentTaskProgressStatus::AGENT_TASK_PROGRESS_STATUS_SUCCEEDED
        );
        assert!(node.hidden);
    }
    bounded(hosted.close()).await.unwrap();
    assert!(!owner.session().is_active());
    assert!(!first.session().is_active());
    assert!(!second.session().is_active());
}

#[tokio::test]
async fn idle_authoritative_owner_wakes_on_real_task_success_before_sibling_fanout_finishes_v9() {
    let fixture = Fixture::with_profiles(false, false, None, None, Arc::new(ChildProfiles)).await;
    let hosted = fixture.open().await.unwrap();
    let owner = fixture.root();
    let user = fixture.user();
    let controller = owner
        .get_service::<ScopedTaskController<Security, ContextSelection<Security>>>()
        .unwrap();
    let missing = parrot_tools::task_artifacts::parse_task_set(&json!([
        {"name":"b","description":"Do b","payload":"not executed","acceptance_criteria":"unused","dependencies":["missing"]}
    ]).to_string()).unwrap();
    let error = bounded(
        controller
            .scheduler()
            .set_tasks(
                missing,
                owner.selection().capture().unwrap(),
                parrot_storage::conversation::HistoryForkBoundary::AfterCompletedHistory,
            )
            .unwrap()
            .join(),
    )
    .await
    .unwrap_err();
    assert_eq!(
        error.to_string(),
        "tasks task 'b' has missing sibling dependency 'missing'."
    );
    assert!(controller.scheduler().snapshot().await.is_empty());
    assert_eq!(fixture.observations.lock().unwrap().constructors, 1);
    let gates: Vec<_> = (0..3).map(|_| Arc::new(Semaphore::new(0))).collect();
    for (name, gate) in ["a", "b", "c"].into_iter().zip(&gates) {
        script(
            &fixture,
            &format!("v9-idle-worker-{name}"),
            reply(&accepted(&format!("{name} done 1"))),
            Some(gate.clone()),
        );
    }
    let first_notice = "AgentTask succeeded: a\nDescription: Do a\n\nResult:\na done 1";
    let owner_gate = Arc::new(Semaphore::new(0));
    script(
        &fixture,
        first_notice,
        reply("owner naturally noted success"),
        Some(owner_gate.clone()),
    );
    // Match original Set(service, ...) without starting an owner tool turn.
    // The existing v8 catalog dispatch is deliberately not replayed here.
    let declarations = parrot_tools::task_artifacts::parse_task_set(&json!([
        {"name":"a","description":"Do a","payload":"v9-idle-worker-a","acceptance_criteria":"return strict accept JSON","model":"local/model"},
        {"name":"b","description":"Do b","payload":"v9-idle-worker-b","acceptance_criteria":"return strict accept JSON","dependencies":["a"],"model":"local/model"},
        {"name":"c","description":"Do c","payload":"v9-idle-worker-c","acceptance_criteria":"return strict accept JSON","dependencies":["a"],"model":"local/model"}
    ]).to_string()).unwrap();
    bounded(
        controller
            .scheduler()
            .set_tasks(
                declarations,
                owner.selection().capture().unwrap(),
                parrot_storage::conversation::HistoryForkBoundary::AfterCompletedHistory,
            )
            .unwrap()
            .join(),
    )
    .await
    .unwrap();
    let initial_notice =
        "AgentTask states changed by your update:\n- a: running\n- b: pending\n- c: pending";
    bounded(async {
        loop {
            let events = user.lease.events.replay().await.unwrap();
            if events.iter().any(|event| {
                event.has_input_admitted() && event.input_admitted().content == initial_notice
            }) && fixture.scripts.lock().unwrap().len() == 3
            {
                assert!(!events.iter().any(|event| event.agent_session_id
                    == owner.identity().session.session_id
                    && event.has_turn_started()));
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await;
    assert!(
        !owner.session().is_active(),
        "record-only graph update must leave the actual owner idle"
    );
    assert_eq!(
        fixture.requests.lock().unwrap().len(),
        1,
        "only the first worker may have called the provider"
    );
    let before_rejected_cycle = controller.scheduler().snapshot().await;
    let cycle = parrot_tools::task_artifacts::parse_task_set(&json!([
        {"name":"a","description":"Do a","payload":"not executed","acceptance_criteria":"unused","dependencies":["b"]}
    ]).to_string()).unwrap();
    let error = bounded(
        controller
            .scheduler()
            .set_tasks(
                cycle,
                owner.selection().capture().unwrap(),
                parrot_storage::conversation::HistoryForkBoundary::AfterCompletedHistory,
            )
            .unwrap()
            .join(),
    )
    .await
    .unwrap_err();
    assert_eq!(error.to_string(), "tasks contains a dependency cycle.");
    assert_eq!(
        controller.scheduler().snapshot().await,
        before_rejected_cycle
    );
    gates[0].add_permits(1);
    bounded(async {
        while !fixture.scripts.lock().unwrap().is_empty() {
            tokio::task::yield_now().await;
        }
    })
    .await;
    assert!(
        owner.session().is_active(),
        "real successful-worker notification must start the idle owner's drain"
    );
    let running = controller.scheduler().snapshot().await;
    assert_eq!(
        running.iter().map(|task| task.state).collect::<Vec<_>>(),
        [
            ExecutionStatus::Succeeded,
            ExecutionStatus::Running,
            ExecutionStatus::Running
        ]
    );
    {
        let requests = fixture.requests.lock().unwrap();
        let notification_request = requests
            .iter()
            .find(|request| conversation(request).contains(first_notice))
            .unwrap();
        assert!(!conversation(notification_request).contains("All AgentTasks are complete:"));
        assert!(!conversation(notification_request).contains("v9-idle-worker-"));
    }
    let mut workers = Vec::new();
    for name in ["a", "b", "c"] {
        let detail = controller.scheduler().capture_detail(name).await.unwrap();
        let worker = owner
            .children()
            .find(detail.agent_name.as_deref().unwrap())
            .unwrap();
        assert!(Arc::ptr_eq(
            &worker.parent_scope().parent().unwrap(),
            &owner
        ));
        assert_eq!(
            worker.selection().capture().unwrap().profile.id,
            "agent-task-payload"
        );
        workers.push(worker);
    }
    assert_eq!(user.registry().unwrap().snapshot_scopes().len(), 4);
    assert_eq!(fixture.observations.lock().unwrap().constructors, 4);
    gates[1].add_permits(1);
    let second_notice = "AgentTask succeeded: b\nDescription: Do b\n\nResult:\nb done 1";
    bounded(async {
        loop {
            let events = user.lease.events.replay().await.unwrap();
            if let Some(notice) = events.iter().find(|event| {
                event.has_input_admitted() && event.input_admitted().content == second_notice
            }) {
                assert_eq!(notice.agent_session_id, owner.identity().session.session_id);
                assert!(!events.iter().any(|event| {
                    event.has_input_admitted()
                        && event
                            .input_admitted()
                            .content
                            .starts_with("All AgentTasks are complete:")
                }));
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await;
    assert_eq!(
        controller.scheduler().snapshot().await[2].state,
        ExecutionStatus::Running
    );
    gates[2].add_permits(1);
    let complete = "All AgentTasks are complete:\n- a: succeeded\n- b: succeeded\n- c: succeeded";
    bounded(async {
        loop {
            let events = user.lease.events.replay().await.unwrap();
            if events.iter().any(|event| {
                event.has_input_admitted()
                    && event.agent_session_id == owner.identity().session.session_id
                    && event.input_admitted().content == complete
            }) {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await;
    let terminal = controller.scheduler().snapshot().await;
    assert!(
        terminal
            .iter()
            .all(|task| task.state == ExecutionStatus::Succeeded && task.hidden)
    );
    assert_eq!(
        terminal
            .iter()
            .map(|task| task.result.as_deref().unwrap())
            .collect::<Vec<_>>(),
        ["a done 1", "b done 1", "c done 1"]
    );
    let events = user.lease.events.replay().await.unwrap();
    let progress = events
        .iter()
        .rev()
        .find(|event| event.has_tool_snapshot() && event.tool_snapshot().kind == PROGRESS_KIND)
        .unwrap();
    let progress =
        AgentTaskProgressSnapshot::parse_from_bytes(&progress.tool_snapshot().data).unwrap();
    assert_eq!(progress.root_nodes.len(), 3);
    for (node, worker) in progress.root_nodes.iter().zip(&workers) {
        assert_eq!(node.agent_session_id, worker.identity().session.session_id);
        assert_eq!(
            node.status.enum_value().unwrap(),
            AgentTaskProgressStatus::AGENT_TASK_PROGRESS_STATUS_SUCCEEDED
        );
        assert!(node.hidden);
    }
    // Accepted runtime tests already prove reset rules and retained Arc reuse.
    // This host-only extension distinguishes real parsed fresh results from stale ones.
    for name in ["a", "b", "c"] {
        script(
            &fixture,
            &format!("v9-idle-worker-{name}"),
            reply(&accepted(&format!("{name} done 2"))),
            None,
        );
    }
    let reset = parrot_tools::task_artifacts::parse_task_set(&json!([
        {"name":"a","description":"Do a","payload":"v9-idle-worker-a","acceptance_criteria":"return strict accept JSON","model":"local/model","state":"pending"}
    ]).to_string()).unwrap();
    bounded(
        controller
            .scheduler()
            .set_tasks(
                reset,
                owner.selection().capture().unwrap(),
                parrot_storage::conversation::HistoryForkBoundary::AfterCompletedHistory,
            )
            .unwrap()
            .join(),
    )
    .await
    .unwrap();
    bounded(async {
        loop {
            let tasks = controller.scheduler().snapshot().await;
            assert!(
                tasks
                    .iter()
                    .all(|task| task.state != ExecutionStatus::Failed),
                "{tasks:?}"
            );
            if tasks
                .iter()
                .map(|task| task.result.as_deref())
                .collect::<Vec<_>>()
                == [Some("a done 2"), Some("b done 2"), Some("c done 2")]
            {
                assert!(
                    tasks
                        .iter()
                        .all(|task| task.state == ExecutionStatus::Succeeded && task.hidden)
                );
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await;
    bounded(async {
        loop {
            let events = user.lease.events.replay().await.unwrap();
            if ["a", "b", "c"].into_iter().all(|name| {
                let fresh = format!(
                    "AgentTask succeeded: {name}\nDescription: Do {name}\n\nResult:\n{name} done 2"
                );
                events.iter().any(|event| {
                    event.agent_session_id == owner.identity().session.session_id
                        && event.has_input_admitted()
                        && event.input_admitted().content == fresh
                })
            }) {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await;
    assert_eq!(fixture.observations.lock().unwrap().constructors, 4);
    owner_gate.add_permits(1);
    bounded(async {
        while owner.session().is_active()
            || workers.iter().any(|worker| worker.session().is_active())
        {
            tokio::task::yield_now().await;
        }
    })
    .await;
    bounded(controller.scheduler().settle().join())
        .await
        .unwrap();
    bounded(hosted.close()).await.unwrap();
}
