use super::{fixture::*, harness::Harness, support::*};
use parrot_domain::values::agent_tasks::{ExecutionStatus::*, Task, TaskPayload};
use std::sync::Arc;

#[tokio::test]
async fn composite_nested_visibility_changes_remain_local_without_restarting_execution_a002() {
    let h = Harness::new();
    let root = h.root("owner").await;
    let gate = Gate::new();
    h.provider.script("parent", [Reply::held("answer", &gate)]);
    let declaration = composite("parent", vec![state("shared", Canceled)]);
    set(&root, vec![declaration]).await;
    gate.entered().await;
    let parent = h
        .registry
        .snapshot_scopes()
        .into_iter()
        .find(|scope| scope.identity().session.name == "parent")
        .unwrap();
    let child = parent.get_service::<Scheduler>().unwrap();
    assert!(child.snapshot().await.is_empty());
    assert_eq!(h.registry.snapshot_scopes().len(), 2);
    set(&parent, vec![state("shared", Canceled)]).await;
    // A002: preserve auto-hide first, then explicitly unhide the child's OWN graph.
    assert!(child.snapshot().await[0].hidden);
    set(&parent, vec![state("shared", Canceled)]).await;
    assert!(!child.snapshot().await[0].hidden);
    let scheduler = root.get_service::<Scheduler>().unwrap();
    let original = snapshot(&root).await[0].clone();
    let hidden_payload = TaskPayload::Tasks(vec![Task {
        hidden: true,
        ..state("shared", Canceled)
    }]);
    set(
        &root,
        vec![Task {
            state: Running,
            payload: hidden_payload.clone(),
            ..original.clone()
        }],
    )
    .await;
    assert!(inner(&snapshot(&root).await[0])[0].hidden);
    assert!(!child.snapshot().await[0].hidden);
    for payload in [original.payload.clone(), hidden_payload] {
        let before = snapshot(&root).await;
        let mut update = before[0].clone();
        update.payload = payload.clone();
        bounded(
            scheduler
                .apply_visibility_changes(before, vec![update])
                .join(),
        )
        .await
        .unwrap();
        assert_eq!(
            inner(&snapshot(&root).await[0])[0].hidden,
            inner(&Task {
                payload,
                ..original.clone()
            })[0]
                .hidden
        );
        assert!(!child.snapshot().await[0].hidden);
    }
    assert_eq!(snapshot(&root).await[0].state, Running);
    assert_eq!(
        scheduler
            .capture_detail("parent")
            .await
            .unwrap()
            .agent_name
            .as_deref(),
        Some("parent")
    );
    assert_eq!(task_requests(&h.provider, "parent").len(), 1);
    gate.open();
    complete(&root).await;
    assert_eq!(task_requests(&h.provider, "parent").len(), 1);
    assert_eq!(
        h.factory
            .get("parent")
            .task_session
            .calls
            .lock()
            .unwrap()
            .iter()
            .filter(|call| call.kind == "enqueue")
            .count(),
        1
    );
    assert!(!child.snapshot().await[0].hidden);
    h.close().await;
}

#[tokio::test]
async fn nested_graph_is_owned_by_child_and_receives_dependency_history_only_when_declared() {
    let h = Harness::new();
    let mut configuration =
        parrot_runtime::task_scheduler::TaskConfiguration::defaults(templates());
    configuration.fork_history_mode = parrot_runtime::task_scheduler::ForkHistoryMode::Dependency;
    *h.factory.configuration.lock().unwrap() = Some(configuration);
    let root = h.root("owner").await;
    let parent_gate = Gate::new();
    let source_gate = Gate::new();
    h.factory
        .parser
        .answer("source-reply", accept("nested-source-marker"));
    h.provider
        .script("parent", [Reply::held("answer", &parent_gate)]);
    h.provider
        .script("source", [Reply::held("source-reply", &source_gate)]);
    let children = vec![dependent("dependent", "source"), task("source")];
    set(&root, vec![composite("parent", children.clone())]).await;
    parent_gate.entered().await;
    let parent = h
        .registry
        .snapshot_scopes()
        .into_iter()
        .find(|scope| scope.identity().session.name == "parent")
        .unwrap();
    assert!(snapshot(&parent).await.is_empty());
    assert_eq!(h.registry.snapshot_scopes().len(), 2);
    let first = task_requests(&h.provider, "parent");
    assert!(prompt(&first[0]).contains("AgentTask role: composite owner"));
    assert!(
        prompt(&first[0])
            .contains("use set_agent_tasks to declare and manage the supplied provisional work")
    );
    // Explicit child declaration, never auto-execution of the parent's delivered payload.
    set(&parent, children).await;
    source_gate.entered().await;
    assert!(requests(&h.provider, "dependent").is_empty());
    source_gate.open();
    complete(&parent).await;
    let next = requests(&h.provider, "dependent");
    assert!(
        next[0]
            .messages
            .iter()
            .any(|message| message.content() == "source-reply")
    );
    assert!(prompt(&next[0]).contains("Result: nested-source-marker"));
    for name in ["source", "dependent"] {
        let scope = h
            .registry
            .snapshot_scopes()
            .into_iter()
            .find(|scope| scope.identity().session.name == name)
            .unwrap();
        assert_eq!(
            scope.identity().session.parent_session_id,
            parent.identity().session.session_id
        );
        assert!(!Arc::ptr_eq(
            &root.get_service::<Scheduler>().unwrap(),
            &scope.get_service::<Scheduler>().unwrap()
        ));
    }
    parent_gate.open();
    complete(&root).await;
    assert_eq!(snapshot(&root).await.len(), 1);
    assert_eq!(snapshot(&parent).await.len(), 2);
    h.close().await;
}

#[tokio::test]
async fn recursive_composite_prompt_preserves_all_fields_without_auto_registering() {
    let h = Harness::new();
    let root = h.root("owner").await;
    let mut source = state("source", Succeeded);
    source.description = "Quote \" and braces {}".into();
    source.payload = TaskPayload::Instruction("Line one\nLine two \"{}\"".into());
    source.model = Some("chosen-model".into());
    source.hidden = true;
    source.result = Some("Source result".into());
    let mut failed = state("failed", Failed);
    failed.result = Some("Partial result".into());
    failed.failure = Some("Failure reason".into());
    let mut nested = composite("nested", vec![failed]);
    nested.state = Canceled;
    nested.dependencies.push("source".into());
    set(&root, vec![composite("parent", vec![source, nested])]).await;
    complete(&root).await;
    let request = task_requests(&h.provider, "parent");
    let prompt = prompt(&request[0]);
    let start = prompt
        .find("The following inner task list is provided:\n")
        .unwrap()
        + "The following inner task list is provided:\n".len();
    let end = prompt[start..]
        .find("\nReuse supplied context and preparation results.")
        .unwrap()
        + start;
    // JSON decoding here inspects the serializer's output, not T49's task parser.
    let delivered: serde_json::Value = serde_json::from_str(&prompt[start..end]).unwrap();
    assert_eq!(delivered[0]["description"], "Quote \" and braces {}");
    assert_eq!(delivered[0]["payload"], "Line one\nLine two \"{}\"");
    assert_eq!(delivered[0]["model"], "chosen-model");
    assert_eq!(delivered[0]["hidden"], true);
    assert_eq!(delivered[0]["state"], "succeeded");
    assert_eq!(delivered[0]["result"], "Source result");
    assert_eq!(delivered[1]["dependencies"], serde_json::json!(["source"]));
    assert_eq!(delivered[1]["state"], "canceled");
    assert_eq!(delivered[1]["payload"][0]["state"], "failed");
    assert_eq!(delivered[1]["payload"][0]["result"], "Partial result");
    assert_eq!(delivered[1]["payload"][0]["failure"], "Failure reason");
    assert!(!prompt.contains("agent_name"));
    assert_eq!(h.registry.snapshot_scopes().len(), 2);
    let parent = h
        .registry
        .snapshot_scopes()
        .into_iter()
        .find(|scope| scope.identity().session.name == "parent")
        .unwrap();
    assert!(snapshot(&parent).await.is_empty());
    h.close().await;
}

#[tokio::test]
async fn retries_and_definition_restarts_preserve_independently_declared_child_graph() {
    let h = Harness::new();
    let root = h.root("owner").await;
    let gate = Gate::new();
    h.factory.parser.answer(
        "retry",
        parrot_domain::values::agent_tasks::LeafResponse {
            result: "try again".into(),
            verdict: parrot_domain::values::agent_tasks::AcceptanceVerdict::RejectAndRetry {
                feedback: "Retry without touching my graph".into(),
                payload: None,
                replacement_result: None,
            },
        },
    );
    h.provider.script(
        "parent",
        [
            Reply::held("retry", &gate),
            Reply::text("retry"),
            Reply::text("answer"),
            Reply::text("answer"),
        ],
    );
    let declaration = composite("parent", vec![task("shared"), task("missing")]);
    set(&root, vec![declaration.clone()]).await;
    gate.entered().await;
    let parent = h
        .registry
        .snapshot_scopes()
        .into_iter()
        .find(|scope| scope.identity().session.name == "parent")
        .unwrap();
    let mut shared = state("shared", Canceled);
    shared.payload = TaskPayload::Instruction("Child-owned edit".into());
    set(&parent, vec![shared.clone(), state("extra", Canceled)]).await;
    let owned = snapshot(&parent).await;
    // Queue the actual child completion before releasing the parent's first turn.
    // Both its initial request and notification follow-up return the same rejected
    // result, so the real session drain cannot accidentally erase the scripted retry.
    bounded(async {
        loop {
            let events = h.histories.repository.events().replay().await.unwrap();
            if events.iter().any(|event| {
                event.agent_session_id == parent.history().agent_session_id()
                    && event.has_input_admitted()
                    && event
                        .input_admitted()
                        .content
                        .contains("All AgentTasks are complete:")
            }) {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await;
    gate.open();
    complete(&root).await;
    assert_eq!(snapshot(&parent).await, owned);
    assert!(
        prompt(&task_requests(&h.provider, "parent")[1])
            .contains("Retry without touching my graph")
    );
    let mut changed = declaration;
    changed.acceptance_criteria = "Updated acceptance".into();
    set(&root, vec![changed]).await;
    complete(&root).await;
    assert_eq!(snapshot(&parent).await, owned);
    assert_eq!(h.registry.snapshot_scopes().len(), 2);
    assert_eq!(task_requests(&h.provider, "parent").len(), 3);
    assert_eq!(
        h.factory.parser.leaf_inputs.lock().unwrap().as_slice(),
        ["retry", "answer", "answer"]
    );
    h.close().await;
}
