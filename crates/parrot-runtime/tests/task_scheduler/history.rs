use super::{fixture::*, harness::Harness, support::*};
use parrot_domain::{
    protocol::messages::parrot::Event,
    values::agent_tasks::{ExecutionStatus::*, Task},
};
use parrot_runtime::{children::*, task_scheduler::*};
use parrot_storage::{
    conversation::{
        AgentHistoryView, ConversationMessage, HistoryForkBoundary, MessageRole, ToolCall,
    },
    events::{ToolExecutionStatus, ToolExecutionTerminal},
};
use std::sync::Arc;

async fn append(history: &AgentHistoryView, message: ConversationMessage) -> i64 {
    history
        .append_conversation(
            Event {
                id: id(),
                agent_session_id: history.agent_session_id().into(),
                ..Default::default()
            },
            message,
        )
        .await
        .unwrap();
    history
        .conversation()
        .await
        .unwrap()
        .last()
        .unwrap()
        .sequence
}
async fn tool(history: &AgentHistoryView, text: &str, call: &str) -> i64 {
    let mut message = ConversationMessage::text(MessageRole::Assistant, text);
    message.tool_calls.push(ToolCall {
        id: call.into(),
        name: "set_agent_tasks".into(),
        arguments_json: "{}".into(),
    });
    append(history, message).await
}
fn configure(h: &Harness, mode: ForkHistoryMode) {
    let mut configuration = TaskConfiguration::defaults(templates());
    configuration.fork_history_mode = mode;
    *h.factory.configuration.lock().unwrap() = Some(configuration);
}

#[tokio::test]
async fn manual_retained_worker_reuses_original_selection_scope_delivery_history_and_lazy_model_callback()
 {
    let h = Harness::new();
    let root = h.root("owner").await;
    let mut request = h.request(&root, "existing-agent");
    request.delivery = CompletionDelivery::Automatic;
    request.requested_scope = "manual scope".into();
    request.requested_profile = "manual-worker".into();
    request.model = h.provider.selection("second");
    let manual = bounded(root.spawner().spawn(request).unwrap().join())
        .await
        .unwrap();
    h.provider.script(
        "existing-agent",
        [Reply::text("manual history"), Reply::text("answer")],
    );
    let sent = send(manual.session().as_ref(), "manual prompt").await;
    bounded(sent.execution.join()).await.unwrap();
    let original = manual.selection().current().unwrap();
    let prompt_scope = manual.identity().prompt_scope.format(1).unwrap();
    set(
        &root,
        vec![Task {
            model: Some("missing-provider/missing-model".into()),
            ..task("existing-agent")
        }],
    )
    .await;
    complete(&root).await;
    assert!(h.factory.router.selectors.lock().unwrap().is_empty());
    assert_eq!(
        manual.selection().current().unwrap().model.canonical_base(),
        original.model.canonical_base()
    );
    assert_eq!(
        manual.selection().current().unwrap().profile.id,
        "manual-worker"
    );
    assert_eq!(
        manual.parent_scope().delivery(),
        CompletionDelivery::Automatic
    );
    assert_eq!(
        manual.identity().prompt_scope.format(1).unwrap(),
        prompt_scope
    );
    assert_eq!(requests(&h.provider, "existing-agent").len(), 2);
    assert!(conversation(&requests(&h.provider, "existing-agent")[1]).contains("manual history"));
    set(
        &root,
        vec![Task {
            model: Some("missing-provider/missing-model".into()),
            ..task("new-agent")
        }],
    )
    .await;
    let tasks = wait_tasks(&root, |tasks| {
        tasks
            .iter()
            .any(|task| task.name == "new-agent" && task.state == Failed)
    })
    .await;
    assert!(
        tasks[1]
            .failure
            .as_deref()
            .unwrap()
            .contains("invalid selector")
    );
    assert_eq!(h.registry.snapshot_scopes().len(), 2);
    assert!(
        root.get_service::<Scheduler>()
            .unwrap()
            .capture_detail("new-agent")
            .await
            .unwrap()
            .agent_name
            .is_none()
    );
    set(&root, vec![task("new-agent")]).await;
    complete(&root).await;
    assert_eq!(h.registry.snapshot_scopes().len(), 3);
    h.close().await;
}

#[tokio::test]
async fn fork_modes_use_first_declared_dependency_not_completion_order_and_retained_history_never_reforks()
 {
    for mode in [
        ForkHistoryMode::Dependency,
        ForkHistoryMode::Parent,
        ForkHistoryMode::Empty,
    ] {
        let h = Harness::new();
        configure(&h, mode);
        let root = h.root("owner").await;
        h.histories
            .append(root.history(), "owner-history-marker")
            .await;
        let first = Gate::new();
        h.provider
            .script("first", [Reply::held("first-transcript", &first)]);
        h.provider
            .script("second", [Reply::text("second-transcript")]);
        h.provider.script(
            "next",
            [Reply::text("next-transcript"), Reply::text("answer")],
        );
        for name in ["first", "second", "next"] {
            h.factory.parser.answer(
                &format!("{name}-transcript"),
                accept(&format!("{name}-marker")),
            );
        }
        let mut next = dependent("next", "second");
        next.dependencies.push("first".into());
        set(&root, vec![next, task("second"), task("first")]).await;
        first.entered().await;
        wait_tasks(&root, |tasks| tasks[1].state == Succeeded).await;
        let mut updated = dependent("next", "first");
        updated.dependencies.push("second".into());
        set(&root, vec![updated]).await;
        assert!(
            root.get_service::<Scheduler>()
                .unwrap()
                .capture_detail("next")
                .await
                .unwrap()
                .agent_name
                .is_none()
        );
        first.open();
        complete(&root).await;
        let requests = requests(&h.provider, "next");
        let next = &requests[0];
        assert_eq!(
            next.messages
                .iter()
                .any(|message| message.content() == "first-transcript"),
            mode == ForkHistoryMode::Dependency
        );
        assert!(
            !next
                .messages
                .iter()
                .any(|message| message.content() == "second-transcript")
        );
        assert_eq!(
            next.messages
                .iter()
                .any(|message| message.content() == "owner-history-marker"),
            mode != ForkHistoryMode::Empty
        );
        assert!(prompt(next).contains("[first] Do first\nResult: first-marker"));
        assert!(prompt(next).contains("[second] Do second\nResult: second-marker"));
        let retained = root.children().find("next").unwrap();
        let mut updated = dependent("next", "second");
        updated.dependencies.push("first".into());
        set(&root, vec![updated]).await;
        complete(&root).await;
        assert!(Arc::ptr_eq(
            &retained,
            &root.children().find("next").unwrap()
        ));
        let restarted = super::fixture::requests(&h.provider, "next");
        assert!(conversation(restarted.last().unwrap()).contains("next-transcript"));
        assert!(
            !restarted
                .last()
                .unwrap()
                .messages
                .iter()
                .any(|message| message.content() == "second-transcript")
        );
        h.close().await;
    }
}

#[tokio::test]
async fn deferred_entry_preserves_original_selection_and_before_invoking_batch_boundary_across_updates()
 {
    let h = Harness::new();
    configure(&h, ForkHistoryMode::Dependency);
    let root = h.root("owner").await;
    h.histories
        .append(root.history(), "owner-safe-marker")
        .await;
    let invocation = tool(root.history(), "invoking-batch-marker", "set-tasks").await;
    let scheduler = root.get_service::<Scheduler>().unwrap();
    bounded(
        scheduler
            .set_tasks(
                vec![state("manual", Failed), dependent("next", "manual")],
                root.selection().current().unwrap(),
                HistoryForkBoundary::BeforeToolBatch {
                    assistant_sequence: invocation,
                    tool_call_id: "set-tasks".into(),
                },
            )
            .unwrap()
            .join(),
    )
    .await
    .unwrap();
    root.session()
        .update_selection(selection(&h.provider, "second", "later-mode", 0x03))
        .unwrap();
    h.histories
        .append(root.history(), "later-owner-marker")
        .await;
    set(
        &root,
        vec![Task {
            result: Some("manual accepted".into()),
            ..state("manual", Succeeded)
        }],
    )
    .await;
    complete(&root).await;
    let next = requests(&h.provider, "next");
    assert_eq!(next[0].model, "first");
    assert!(conversation(&next[0]).contains("owner-safe-marker"));
    assert!(!conversation(&next[0]).contains("invoking-batch-marker"));
    assert!(!conversation(&next[0]).contains("later-owner-marker"));
    assert!(prompt(&next[0]).contains("Result: manual accepted"));
    assert_eq!(h.registry.snapshot_scopes().len(), 2);
    h.close().await;
}

#[tokio::test]
async fn explicit_success_of_active_dependency_forks_safe_prefix_not_incomplete_batch_or_following_history()
 {
    let h = Harness::new();
    configure(&h, ForkHistoryMode::Dependency);
    let root = h.root("owner").await;
    let gate = Gate::new();
    h.provider.script("source", [Reply::held("answer", &gate)]);
    set(&root, vec![task("source"), dependent("next", "source")]).await;
    gate.entered().await;
    let source = root.children().find("source").unwrap();
    h.histories
        .append(source.history(), "source-safe-marker")
        .await;
    tool(source.history(), "incomplete-marker", "unfinished").await;
    h.histories
        .append(source.history(), "after-incomplete-marker")
        .await;
    set(
        &root,
        vec![Task {
            result: Some("accepted while active".into()),
            ..state("source", Succeeded)
        }],
    )
    .await;
    complete(&root).await;
    let next = requests(&h.provider, "next");
    let history = conversation(&next[0]);
    assert!(history.contains("source-safe-marker"));
    assert!(!history.contains("incomplete-marker"));
    assert!(!history.contains("after-incomplete-marker"));
    assert!(
        !next[0]
            .messages
            .iter()
            .any(|message| !message.tool_calls.is_empty())
    );
    assert!(prompt(&next[0]).contains("accepted while active"));
    h.close().await;
}

#[tokio::test]
async fn compacted_original_boundary_fails_deferred_task_without_creating_a_worker() {
    let h = Harness::new();
    configure(&h, ForkHistoryMode::Dependency);
    let root = h.root("owner").await;
    let launch = tool(root.history(), "owner launch", "launch").await;
    root.history()
        .append_tool_settlement(
            Event {
                id: id(),
                agent_session_id: root.history().agent_session_id().into(),
                ..Default::default()
            },
            launch,
            ToolExecutionTerminal {
                tool_call_id: "launch".into(),
                tool_name: "set_agent_tasks".into(),
                status: ToolExecutionStatus::Finished,
                result_parts: vec![],
                message: "declared".into(),
            },
        )
        .await
        .unwrap();
    let scheduler = root.get_service::<Scheduler>().unwrap();
    bounded(
        scheduler
            .set_tasks(
                vec![state("manual", Failed), dependent("next", "manual")],
                root.selection().current().unwrap(),
                HistoryForkBoundary::BeforeToolBatch {
                    assistant_sequence: launch,
                    tool_call_id: "launch".into(),
                },
            )
            .unwrap()
            .join(),
    )
    .await
    .unwrap();
    let last = root
        .history()
        .conversation()
        .await
        .unwrap()
        .last()
        .unwrap()
        .sequence;
    root.history()
        .append_compaction_status(
            Event {
                id: id(),
                agent_session_id: root.history().agent_session_id().into(),
                ..Default::default()
            },
            parrot_storage::conversation::CompactionSnapshot {
                summary: "later owner summary".into(),
                watermark: last,
            },
            "later status".into(),
        )
        .await
        .unwrap();
    set(
        &root,
        vec![Task {
            result: Some("accepted".into()),
            ..state("manual", Succeeded)
        }],
    )
    .await;
    let tasks = snapshot(&root).await;
    assert_eq!(tasks[1].state, Failed);
    assert!(
        tasks[1]
            .failure
            .as_deref()
            .unwrap()
            .contains("captured fork tool batch is unavailable")
    );
    assert_eq!(h.registry.snapshot_scopes().len(), 1);
    assert!(
        scheduler
            .capture_detail("next")
            .await
            .unwrap()
            .agent_name
            .is_none()
    );
    h.close().await;
}
