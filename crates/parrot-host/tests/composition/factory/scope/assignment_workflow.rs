use super::*;
use parrot_domain::values::agent_tasks::TaskAssignee;
use parrot_tools::tasks_queues::tasks::{TaskTool, TaskToolKind};

async fn invoke_task(
    fixture: &Fixture,
    scope: &Arc<Scope<Security>>,
    kind: TaskToolKind,
    invocation: ToolInvocation,
) -> String {
    let tool = TaskTool::new(
        kind,
        &catalog::configured_definitions(&fixture.configuration),
        scope
            .get_service::<ScopedTaskController<Security, ContextSelection<Security>>>()
            .unwrap(),
        Arc::new(Files::<SharedSecurity<Security>>(PhantomData)),
    )
    .unwrap();
    let turn = scope.selection().capture().unwrap();
    bounded(tool.invoke(
        invocation,
        InvocationContext::new(
            SharedSecurity(turn.security.clone()),
            ContextSelection::external(turn),
            &NeverCancelled,
        ),
    ))
    .await
    .unwrap()
    .text
}

fn count_requests(fixture: &Fixture, marker: &str) -> usize {
    fixture
        .requests
        .lock()
        .unwrap()
        .iter()
        .filter(|request| {
            request
                .messages
                .iter()
                .any(|message| message.content().contains(marker))
        })
        .count()
}

#[tokio::test]
async fn actual_tool_workflow_at_zero_budget_hands_off_reopens_and_replays_original_response() {
    let fixture = Fixture::with_config(
        false,
        false,
        None,
        None,
        Arc::new(ChildProfiles),
        |config| config.extra_concurrent_tasks = 0,
    )
    .await;
    let hosted = bounded(fixture.open()).await.unwrap();
    let root = fixture.root();
    let worker_hold = Arc::new(Semaphore::new(0));
    script(
        &fixture,
        "Task: workflow-parallel\n",
        reply(r#"{"result":"parallel verified","verdict":"accept"}"#),
        Some(worker_hold.clone()),
    );
    let mut own = task("workflow-owner");
    own["payload"] = "Implement the first owner instruction".into();
    own["acceptance_criteria"] = "The first owner behavior passes".into();
    let mut next = task("workflow-next");
    next["dependencies"] = json!(["workflow-owner", "workflow-parallel"]);
    next["payload"] = "Verify both dependency results in the owner".into();
    next["acceptance_criteria"] = "Both dependency checks pass".into();
    let (original_call, accepted) = integrated_adapters::set_tasks(
        &fixture,
        &root,
        json!([own.clone(), task("workflow-parallel"), next.clone()]),
    )
    .await;
    wait_task_request(&fixture, "workflow-parallel").await;
    assert!(accepted.contains("Implement the first owner instruction"));
    assert!(accepted.contains("The first owner behavior passes"));
    let worker = root.children().resolve("workflow-parallel").unwrap();
    assert!(accepted.contains(&format!("/{}", worker.identity().name_path.join("/"))));
    assert_eq!(root.children().snapshot_children().len(), 1);
    let scheduler = root.get_service::<Scheduler>().unwrap();
    let tasks = scheduler.snapshot().await;
    assert_eq!(tasks[0].state, ExecutionStatus::Running);
    assert_eq!(tasks[1].state, ExecutionStatus::Running);
    assert_eq!(tasks[2].state, ExecutionStatus::Pending);
    own["state"] = "succeeded".into();
    own["result"] = "owner verified".into();
    invoke_task(
        &fixture,
        &root,
        TaskToolKind::Set,
        ToolInvocation {
            call_id: "workflow-complete-owner".into(),
            assistant_sequence: original_call.assistant_sequence,
            arguments_json: json!({"tasks":[own]}).to_string(),
        },
    )
    .await;
    let owner_hold = Arc::new(Semaphore::new(0));
    script(
        &fixture,
        "AgentTask assigned to self: workflow-next",
        reply("owner awaits explicit completion"),
        Some(owner_hold),
    );
    worker_hold.add_permits(1);
    wait_tasks(&root, |tasks| {
        tasks[1].state == ExecutionStatus::Succeeded && tasks[2].state == ExecutionStatus::Running
    })
    .await;
    bounded(async {
        loop {
            if fixture.requests.lock().unwrap().iter().any(|request| {
                request.messages.iter().any(|message| {
                    let text = message.content();
                    text.contains("AgentTask assigned to self: workflow-next")
                        && text.contains("Verify both dependency results in the owner")
                        && text.contains("Both dependency checks pass")
                        && text.contains("owner verified")
                        && text.contains("parallel verified")
                })
            }) {
                return;
            }
            tokio::task::yield_now().await;
        }
    })
    .await;
    let id = UserSessionId::parse(&hosted.snapshot().id).unwrap();
    bounded(hosted.close()).await.unwrap();
    let resumed_hold = Arc::new(Semaphore::new(0));
    script(
        &fixture,
        "AgentTask assigned to self: workflow-next",
        reply("owner completed"),
        Some(resumed_hold.clone()),
    );
    let reopened = reopen(&fixture, &id).await;
    let restored = fixture.root();
    let scheduler = restored.get_service::<Scheduler>().unwrap();
    assert_eq!(
        scheduler
            .capture_detail("workflow-next")
            .await
            .unwrap()
            .assigned_to,
        Some(TaskAssignee::Owner)
    );
    let mut replay = original_call.clone();
    replay.arguments_json = "not valid task arguments".into();
    assert_eq!(
        invoke_task(&fixture, &restored, TaskToolKind::Set, replay).await,
        accepted
    );
    let summaries: Value = serde_json::from_str(
        &invoke_task(
            &fixture,
            &restored,
            TaskToolKind::Get,
            ToolInvocation {
                arguments_json: "{}".into(),
                ..original_call
            },
        )
        .await,
    )
    .unwrap();
    assert_eq!(summaries[0]["state"], "succeeded");
    assert_eq!(summaries[0]["assigned_to"], "self");
    assert_eq!(summaries[1]["state"], "succeeded");
    assert_eq!(summaries[2]["assigned_to"], "self");
    assert_eq!(count_requests(&fixture, "Task: workflow-parallel\n"), 1);
    next["state"] = "succeeded".into();
    next["result"] = "both dependencies verified after reopen".into();
    integrated_adapters::set_tasks(&fixture, &restored, json!([next])).await;
    resumed_hold.add_permits(1);
    idle(&restored).await;
    assert!(
        scheduler
            .snapshot()
            .await
            .iter()
            .all(|task| task.state == ExecutionStatus::Succeeded)
    );
    bounded(reopened.close()).await.unwrap();
    std::fs::remove_dir_all(&fixture.root).unwrap();
}

#[tokio::test]
async fn actual_composite_final_response_waits_for_explicit_inner_self_completion() {
    let fixture = fixture().await;
    let hosted = bounded(fixture.open()).await.unwrap();
    let root = fixture.root();
    let premature = Arc::new(Semaphore::new(0));
    let resolved = Arc::new(Semaphore::new(0));
    let mut inner = task("workflow-inner");
    let mut register = reply("");
    register
        .tool_calls
        .push(parrot_provider::sessions::ToolCall {
            id: "register-inner-workflow".into(),
            name: "set_agent_tasks".into(),
            arguments_json: json!({"tasks":[inner.clone()]}).to_string(),
        });
    script(&fixture, "Task: workflow-composite\n", register, None);
    script(
        &fixture,
        "Task: workflow-composite\n",
        reply(r#"{"result":"premature result","verdict":"accept"}"#),
        Some(premature.clone()),
    );
    inner["state"] = "succeeded".into();
    inner["result"] = "explicit inner verification".into();
    let mut complete = reply("");
    complete
        .tool_calls
        .push(parrot_provider::sessions::ToolCall {
            id: "complete-inner-workflow".into(),
            name: "set_agent_tasks".into(),
            arguments_json: json!({"tasks":[inner]}).to_string(),
        });
    script(
        &fixture,
        "Task: workflow-composite\n",
        complete,
        Some(resolved.clone()),
    );
    script(
        &fixture,
        "Task: workflow-composite\n",
        reply(r#"{"result":"resolved group","verdict":"accept"}"#),
        None,
    );
    let mut group = task("workflow-composite");
    group["payload"] = json!([task("workflow-inner")]);
    integrated_adapters::set_tasks(&fixture, &root, json!([group])).await;
    wait_task_request(&fixture, "workflow-composite").await;
    let child = root.children().resolve("workflow-composite").unwrap();
    wait_tasks(&child, |tasks| {
        !tasks.is_empty() && tasks[0].state == ExecutionStatus::Running
    })
    .await;
    assert_eq!(
        child
            .get_service::<Scheduler>()
            .unwrap()
            .capture_detail("workflow-inner")
            .await
            .unwrap()
            .assigned_to,
        Some(TaskAssignee::Owner)
    );
    assert!(child.children().snapshot_children().is_empty());
    premature.add_permits(1);
    bounded(async {
        while count_requests(&fixture, "Task: workflow-composite\n") < 3 {
            tokio::task::yield_now().await;
        }
    })
    .await;
    assert!(
        fixture
            .requests
            .lock()
            .unwrap()
            .iter()
            .any(|request| request.messages.iter().any(|message| {
                let text = message.content();
                text.contains("Unresolved AgentTasks") && text.contains("workflow-inner [running]")
            }))
    );
    assert_eq!(
        root.get_service::<Scheduler>().unwrap().snapshot().await[0].state,
        ExecutionStatus::Running
    );
    assert!(
        child
            .agent_state()
            .load()
            .await
            .unwrap()
            .unwrap()
            .execution
            .unwrap()
            .terminal
            .is_none()
    );
    resolved.add_permits(1);
    wait_tasks(&root, |tasks| tasks[0].state == ExecutionStatus::Succeeded).await;
    assert_eq!(
        root.get_service::<Scheduler>().unwrap().snapshot().await[0]
            .result
            .as_deref(),
        Some("resolved group")
    );
    idle(&root).await;
    bounded(hosted.close()).await.unwrap();
    std::fs::remove_dir_all(&fixture.root).unwrap();
}
