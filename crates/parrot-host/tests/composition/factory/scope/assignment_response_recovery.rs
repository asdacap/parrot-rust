use super::*;
use parrot_domain::values::{agent_state::TaskCallReceipt, agent_tasks::TaskCallIdentity};

#[tokio::test]
async fn actual_startup_seals_preparing_response_before_merging_saved_child_completion() {
    let fixture = fixture().await;
    let hosted = bounded(fixture.open()).await.unwrap();
    let root = fixture.root();
    let held = Arc::new(Semaphore::new(0));
    script(
        &fixture,
        "Task: parallel-response\n",
        reply(r#"{"result":"actual completed child","verdict":"accept"}"#),
        Some(held.clone()),
    );
    let key = TaskCallIdentity {
        assistant_sequence: 71,
        call_id: "saved-preparing-response".into(),
    };
    let declarations = TaskArtifactParserAdapter
        .parse_task_set(
            &json!([task("owner-response"), dedicated(task("parallel-response"))]).to_string(),
        )
        .unwrap();
    let scheduler = root.get_service::<Scheduler>().unwrap();
    let accepted = bounded(
        scheduler
            .set_tool_tasks(
                declarations,
                root.selection().current().unwrap(),
                HistoryForkBoundary::AfterCompletedHistory,
                key.clone(),
            )
            .unwrap()
            .join(),
    )
    .await
    .unwrap();
    wait_task_request(&fixture, "parallel-response").await;
    held.add_permits(1);
    wait_tasks(&root, |tasks| tasks[1].state == ExecutionStatus::Succeeded).await;
    let path = state_path(&fixture.user(), &root);
    let id = UserSessionId::parse(&hosted.snapshot().id).unwrap();
    bounded(hosted.close()).await.unwrap();
    let mut saved: AgentStateSnapshot =
        serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    assert_eq!(
        saved.tasks.entries[1].task.result.as_deref(),
        Some("actual completed child")
    );
    assert!(
        saved.tasks.entries[1]
            .borrow_worker()
            .runner
            .as_ref()
            .unwrap()
            .outcome
            .is_some()
    );
    saved.tasks.entries[1].task.state = ExecutionStatus::Running;
    saved.tasks.entries[1].task.result = None;
    saved.tasks.accepted_calls[0].receipt = TaskCallReceipt::Preparing { accepted_count: 2 };
    std::fs::write(&path, serde_json::to_vec(&saved).unwrap()).unwrap();
    let requests = {
        let requests = fixture.requests.lock().unwrap();
        requests
            .iter()
            .filter(|request| {
                request
                    .messages
                    .iter()
                    .any(|message| message.content().contains("Task: parallel-response\n"))
            })
            .count()
    };
    assert_eq!(requests, 1);
    let resumed = reopen(&fixture, &id).await;
    let restored = fixture.root();
    let scheduler = restored.get_service::<Scheduler>().unwrap();
    wait_tasks(&restored, |tasks| {
        tasks[1].state == ExecutionStatus::Succeeded
    })
    .await;
    assert_eq!(
        scheduler.lookup_accepted(&key).await.unwrap(),
        Some(accepted)
    );
    let current = scheduler.snapshot().await;
    assert_eq!(current[1].result.as_deref(), Some("actual completed child"));
    assert_eq!(
        fixture
            .requests
            .lock()
            .unwrap()
            .iter()
            .filter(|request| request
                .messages
                .iter()
                .any(|message| message.content().contains("Task: parallel-response\n")))
            .count(),
        1
    );
    let mut finished = task("owner-response");
    finished["state"] = "succeeded".into();
    finished["result"] = "owner completed after response recovery".into();
    set(&restored, json!([finished])).await;
    idle(&restored).await;
    bounded(resumed.close()).await.unwrap();
    std::fs::remove_dir_all(&fixture.root).unwrap();
}
