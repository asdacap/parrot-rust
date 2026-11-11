use super::*;
use parrot_domain::{
    protocol::messages::parrot::{AgentTaskProgressSnapshot, AgentTaskProgressStatus},
    values::agent_tasks::ExecutionStatus,
};
use protobuf::Message as _;
use tokio::sync::Semaphore;

#[tokio::test]
async fn catalog_task_worker_nested_child_wait_and_durable_progress_join_together() {
    let fixture = fixture().await;
    let hosted = fixture.open().await.unwrap();
    let root = fixture.root();
    let user = fixture.user();
    let registry = user.registry().unwrap();
    let controller = root
        .get_service::<ScopedTaskController<Security, ContextSelection<Security>>>()
        .unwrap();
    let gate = Arc::new(Semaphore::new(0));
    script(
        &fixture,
        "t76-workflow-owner",
        tool_reply(
            "t76-task-set",
            "set_agent_tasks",
            json!({"tasks":[{
                "name":"queue-workflow", "description":"Deliver a nested receipt", "payload":"t76-workflow-worker",
                "acceptance_criteria":"return strict accept JSON after nested child finishes", "model":"local/model"
            }]}),
        ),
        None,
    );
    script(
        &fixture,
        "t76-workflow-worker",
        tool_reply(
            "t76-worker-spawn",
            "agent_spawn",
            json!({"name":"receipt-child","agent":"adapter","prompt":"t76-workflow-nested","fork":"empty"}),
        ),
        None,
    );
    script(
        &fixture,
        "t76-workflow-nested",
        reply("t76-nested-complete"),
        Some(gate.clone()),
    );
    script(
        &fixture,
        "t76-workflow-worker",
        tool_reply("t76-worker-wait", "wait", json!({"duration_ms":10000})),
        None,
    );
    let verdict = json!({"result":"t76-workflow-delivered","verdict":"accept"}).to_string();
    script(&fixture, "t76-workflow-worker", reply(&verdict), None);
    script(
        &fixture,
        "t76-workflow-owner",
        reply("t76-owner-observed-workflow"),
        None,
    );
    send(&hosted, "t76-workflow-owner", "t76-workflow-input").await;
    let (worker, nested) = bounded(async {
        loop {
            if let Some(name) = controller
                .scheduler()
                .capture_detail("queue-workflow")
                .await
                .and_then(|task| task.agent_name)
                && let Some(worker) = root.children().find(&name)
                && let Some(nested) = worker.children().find("receipt-child")
            {
                let history = worker.history().conversation().await.unwrap();
                if history.iter().any(|item| {
                    item.message
                        .tool_calls
                        .iter()
                        .any(|call| call.id == "t76-worker-wait")
                }) {
                    break (worker, nested);
                }
            }
            tokio::task::yield_now().await;
        }
    })
    .await;
    let registered = registry.contains(&worker) && registry.contains(&nested);
    let nested_parent = Arc::ptr_eq(&nested.parent_scope().parent().unwrap(), &worker);
    let wait =
        integrated_adapters::agent_tool(&fixture, &root, parrot_tools::agents::AgentToolKind::Wait);
    let waiting =
        integrated_adapters::agent_invoke(&wait, json!({"duration_ms":10000}), &NeverCancelled);
    tokio::pin!(waiting);
    let premature = tokio::select! {
        result = &mut waiting => Some(result),
        _ = tokio::task::yield_now() => None,
    };
    let woke_before_release = premature.is_some();
    gate.add_permits(1);
    let wake = if let Some(result) = premature {
        result
    } else {
        bounded(waiting).await
    }
    .unwrap();
    let tasks = bounded(async {
        loop {
            let tasks = controller.scheduler().snapshot().await;
            if tasks.iter().all(|task| {
                matches!(
                    task.state,
                    ExecutionStatus::Succeeded | ExecutionStatus::Failed
                )
            }) {
                break tasks;
            }
            tokio::task::yield_now().await;
        }
    })
    .await;
    idle(&root).await;
    idle(&worker).await;
    idle(&nested).await;
    let worker_history = worker.history().conversation().await.unwrap();
    let nested_history = nested.history().conversation().await.unwrap();
    let durable = user.lease.events.replay().await.unwrap();
    bounded(hosted.close()).await.unwrap();
    assert_database_closed(&user);

    assert!(
        nested_history
            .iter()
            .any(|item| parrot_storage::events::content(&item.message.parts)
                == "t76-nested-complete")
    );
    assert!(registered && nested_parent);
    assert!(!woke_before_release);
    assert!(wake.contains("AgentTask update"), "{wake}");
    assert_eq!(tasks.len(), 1);
    assert_eq!(tasks[0].state, ExecutionStatus::Succeeded, "{tasks:?}");
    assert_eq!(tasks[0].result.as_deref(), Some("t76-workflow-delivered"));
    let result = worker_history
        .iter()
        .find(|item| item.message.tool_call_id == "t76-worker-wait")
        .unwrap();
    assert!(
        parrot_storage::events::content(&result.message.parts).contains("receipt-child completion"),
        "{}",
        parrot_storage::events::content(&result.message.parts)
    );
    assert!(
        worker_history
            .iter()
            .any(|item| parrot_storage::events::content(&item.message.parts) == verdict)
    );
    let progress: Vec<_> = durable
        .iter()
        .filter(|event| {
            event.has_tool_snapshot()
                && event.tool_snapshot().kind == parrot_runtime::task_scheduler::PROGRESS_KIND
        })
        .collect();
    let terminal =
        AgentTaskProgressSnapshot::parse_from_bytes(&progress.last().unwrap().tool_snapshot().data)
            .unwrap();
    assert_eq!(terminal.root_nodes.len(), 1);
    assert_eq!(
        terminal.root_nodes[0].status.enum_value().unwrap(),
        AgentTaskProgressStatus::AGENT_TASK_PROGRESS_STATUS_SUCCEEDED
    );
    assert_eq!(
        terminal.root_nodes[0].agent_session_id,
        worker.identity().session.session_id
    );
    assert!(!registry.contains(&worker));
    assert!(!registry.contains(&nested));
    assert!(fixture.scripts.lock().unwrap().is_empty());
}
