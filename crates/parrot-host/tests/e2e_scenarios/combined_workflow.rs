use super::*;
use parrot_domain::values::context::ContextSize;
use parrot_runtime::interactions::{PermissionDecision, QuestionReply};
use parrot_storage::{conversation::ConversationRepository, events::MessageRole};
use tokio::sync::Semaphore;

#[tokio::test]
async fn compacted_full_fork_pending_interactions_process_wait_and_same_session_resume_join_together()
 {
    let fixture = fixture().await;
    let hosted = fixture.open().await.unwrap();
    let root = fixture.root();
    let user = fixture.user();
    let id = UserSessionId::parse(&user.id).unwrap();
    let registry = user.registry().unwrap();
    let queues = root.get_service::<AgentQueues<Security>>().unwrap();
    let source = root
        .get_service::<dyn parrot_domain::contracts::status::ProcessStatusSource>()
        .unwrap();
    send(
        &hosted,
        &format!(
            "t76-combined-seed {}",
            "retained compaction input ".repeat(3000)
        ),
        "t76-combined-prefill",
    )
    .await;
    idle(&root).await;

    let execution = integrated_adapters::start_process(&root, "t76-combined-process").await;
    let child_gate = Arc::new(Semaphore::new(0));
    let final_root_gate = Arc::new(Semaphore::new(0));
    // Even an observation failure releases the supplied work naturally, never by signal.
    struct Release(
        Arc<integrated_adapters::NaturalExecution>,
        Arc<Semaphore>,
        Arc<Semaphore>,
    );
    impl Drop for Release {
        fn drop(&mut self) {
            self.0.finish();
            self.1.add_permits(1);
            self.2.add_permits(1);
        }
    }
    let release = Release(execution, child_gate.clone(), final_root_gate.clone());
    let process_before = source.active();
    let process_snapshot = source.snapshot();
    let mut stream = hosted
        .listen(Some(String::new()), Arc::new(NeverCancelled))
        .unwrap();
    let requesting_user = user.clone();
    let permission = user.lease.runtime_handle().spawn(async move {
        requesting_user
            .permissions
            .request(
                permission_request(&requesting_user, "t76-combined-denied-write"),
                &NeverCancelled,
            )
            .await
    });
    let pending_event = bounded(async {
        loop {
            let event = stream.next(&NeverCancelled).await.unwrap().unwrap();
            if event.has_permission_pending() {
                break event;
            }
        }
    })
    .await;
    drop(stream);
    let pending_before = hosted.pending_permissions().await.unwrap();
    let permission_id = pending_event.permission_pending().id.clone();

    // This is the real compactor's sole summary request, not a synthetic tool result.
    script(
        &fixture,
        "t76-combined-seed",
        reply("t76-combined-summary"),
        None,
    );
    bounded(hosted.compact(
        Some(ContextSize::parse("16k").unwrap()),
        Arc::new(NeverCancelled),
    ))
    .await
    .unwrap();
    let compacted = root.history().compaction_history().await.unwrap();
    let events_after_compaction = user.lease.events.replay().await.unwrap();
    let pending_after_compaction = hosted.pending_permissions().await.unwrap();
    let process_after_compaction = source.active();
    let same_scope = Arc::ptr_eq(&root, &fixture.root());

    let mut spawn = tool_reply(
        "t76-combined-spawn",
        "agent_spawn",
        json!({
            "name":"combined-child", "agent":"adapter", "model":"local/model",
            "prompt":"t76-combined-child-input", "fork":"full"
        }),
    );
    spawn.assistant_text = "t76-combined-invoking-batch".into();
    script(&fixture, "t76-combined-root-input", spawn, None);
    // Full fork inherits the root marker: match the child's own prompt first.
    script(
        &fixture,
        "t76-combined-child-input",
        reply("t76-combined-child-complete"),
        Some(child_gate),
    );
    script(
        &fixture,
        "t76-combined-root-input",
        tool_reply(
            "t76-combined-question",
            "question",
            json!({"questions":[{"prompt":"t76-combined-proceed?","custom":true}]}),
        ),
        None,
    );
    script(
        &fixture,
        "t76-combined-root-input",
        tool_reply("t76-combined-wait", "wait", json!({"duration_ms":10000})),
        None,
    );
    script(
        &fixture,
        "t76-combined-root-input",
        reply("t76-combined-root-complete"),
        Some(final_root_gate),
    );
    send(&hosted, "t76-combined-root-input", "t76-combined-dispatch").await;
    let (child, question, child_request) = bounded(async {
        loop {
            let child = root.children().find("combined-child");
            let question = hosted.pending_questions().unwrap().into_iter().next();
            let request = fixture
                .requests
                .lock()
                .unwrap()
                .iter()
                .find(|request| {
                    request
                        .messages
                        .iter()
                        .any(|message| message.content() == "t76-combined-child-input")
                })
                .cloned();
            if let (Some(child), Some(question), Some(request)) = (child, question, request) {
                break (child, question, request);
            }
            tokio::task::yield_now().await;
        }
    })
    .await;
    let child_id = child.identity().session.session_id.clone();
    let child_queues = child.get_service::<AgentQueues<Security>>().unwrap();
    let forked = child.history().compaction_history().await.unwrap();
    let child_prefix = child.history().conversation().await.unwrap();
    let registered = registry.contains(&child);
    let live_parent = Arc::ptr_eq(&child.parent_scope().parent().unwrap(), &root);
    let question_active = root.session().is_active() && child.session().is_active();
    let pending_during_question = hosted.pending_permissions().await.unwrap();
    let process_during_question = source.active();
    let status = bounded(hosted.observe_status(&NeverCancelled))
        .await
        .unwrap();
    hosted
        .reply_question(
            &question.id,
            QuestionReply::answered(vec!["t76-combined-answer".into()]),
        )
        .unwrap();
    // The plain reject choice accepts no reason and can never call NeverGrant::approve.
    bounded(hosted.reply_permission(&permission_id, "reject", ""))
        .await
        .unwrap();
    let permission_reply = bounded(permission).await.unwrap().unwrap();
    let before_release = bounded(async {
        loop {
            let events = user.lease.events.replay().await.unwrap();
            if events.iter().any(|event| {
                event.has_tool_started() && event.tool_started().tool_call_id == "t76-combined-wait"
            }) {
                break events;
            }
            tokio::task::yield_now().await;
        }
    })
    .await;
    let waiting_history = root.history().conversation().await.unwrap();
    let work_still_active = child.session().is_active() && !source.active().is_empty();
    release.0.finish();
    release.1.add_permits(1);
    idle(&child).await;
    bounded(async {
        while !source.snapshot().is_empty() {
            tokio::task::yield_now().await;
        }
    })
    .await;
    // Completion callbacks must observe settled owned work before accepting this candidate.
    release.2.add_permits(1);
    idle(&root).await;
    let history = root.history().conversation().await.unwrap();
    let child_history = child.history().conversation().await.unwrap();
    let child_compaction = child.history().compaction_history().await.unwrap();
    let root_compaction = root.history().compaction_history().await.unwrap();
    let durable = user.lease.events.replay_raw().await.unwrap();
    let pending_questions = hosted.pending_questions().unwrap();
    let pending_permissions = hosted.pending_permissions().await.unwrap();
    let selected_root = user.resources.root().to_owned();
    bounded(hosted.close()).await.unwrap();
    assert_database_closed(&user);
    let old_child_removed = !registry.contains(&child);
    let old_root_inactive = !root.session().is_active();
    let old_child_inactive = !child.session().is_active();
    let old_processes_settled = source.active().is_empty() && source.snapshot().is_empty();
    let old_queues_disposed = queues.is_disposed() && child_queues.is_disposed();

    let opened = bounded(fixture.store.resume(&id, true, false))
        .await
        .unwrap();
    let resumed = bounded(fixture.factory.bind_opened(opened)).await.unwrap();
    let resumed_root = fixture.root();
    let resumed_user = fixture.user();
    let restored = ConversationRepository::new(resumed_user.lease.events.clone());
    let restored_child_history = restored.conversation(child_id.clone()).await.unwrap();
    let restored_child_compaction = restored.compaction_history(child_id).await.unwrap();
    let restored_history = resumed_root.history().conversation().await.unwrap();
    let restored_compaction = resumed_root.history().compaction_history().await.unwrap();
    let restored_events = resumed_user.lease.events.replay_raw().await.unwrap();
    let restored_questions = resumed.pending_questions().unwrap();
    let restored_permissions = resumed.pending_permissions().await.unwrap();
    let resumed_source = resumed_root
        .get_service::<dyn parrot_domain::contracts::status::ProcessStatusSource>()
        .unwrap();
    let resumed_queues = resumed_root.get_service::<AgentQueues<Security>>().unwrap();
    let resumed_id = resumed.snapshot().id;
    let resumed_selected_root = resumed_user.resources.root().to_owned();
    let new_scope = !Arc::ptr_eq(&root, &resumed_root);
    let resumed_processes_empty =
        resumed_source.active().is_empty() && resumed_source.snapshot().is_empty();
    bounded(resumed.close()).await.unwrap();
    assert_database_closed(&resumed_user);

    assert!(same_scope && new_scope);
    assert_eq!(resumed_id, id.value());
    assert_eq!(resumed_selected_root, selected_root);
    assert!(old_root_inactive && old_child_inactive && old_child_removed);
    assert!(old_processes_settled && resumed_processes_empty);
    assert!(old_queues_disposed && resumed_queues.is_disposed());
    assert_eq!(process_before.len(), 1);
    assert_eq!(process_before[0].name, "t76-combined-process");
    assert_eq!(process_after_compaction.len(), 1);
    assert_eq!(process_during_question.len(), 1);
    assert_eq!(process_before[0].id, process_after_compaction[0].id);
    assert_eq!(process_before[0].id, process_during_question[0].id);
    assert_eq!(process_snapshot.len(), 1);
    assert_eq!(
        process_snapshot[0].owner_session_id,
        root.identity().session.session_id
    );
    assert_eq!(pending_before.len(), 1);
    assert_eq!(pending_before[0].id, permission_id);
    assert_eq!(pending_after_compaction, pending_before);
    assert_eq!(pending_during_question, pending_before);
    assert_eq!(permission_reply.decision, PermissionDecision::Reject);
    assert!(permission_reply.reason.is_empty());
    assert!(pending_questions.is_empty() && pending_permissions.is_empty());
    assert!(restored_questions.is_empty() && restored_permissions.is_empty());
    assert!(registered && live_parent && question_active && work_still_active);
    assert_eq!(question.questions[0].prompt, "t76-combined-proceed?");
    assert!(
        status.contains("combined-child") && status.contains("t76-combined-process"),
        "{status}"
    );
    assert!(events_after_compaction.iter().any(|event| {
        event.has_compaction_finished()
            && event.agent_session_id == root.identity().session.session_id
    }));
    let compacted = compacted.unwrap();
    let forked = forked.unwrap();
    assert!(compacted.snapshot.summary.contains("t76-combined-summary"));
    assert!(compacted.snapshot.watermark > 0);
    assert_eq!(forked.snapshot.summary, compacted.snapshot.summary);
    assert_eq!(forked.snapshot.watermark, 0);
    assert!(
        child_request
            .messages
            .iter()
            .any(|message| message.content().contains("t76-combined-summary"))
    );
    assert!(
        child_request
            .messages
            .iter()
            .any(|message| message.content() == "t76-combined-root-input")
    );
    assert!(!child_request.messages.iter().any(|message| {
        message.content().contains("t76-combined-invoking-batch")
            || message.content().contains("retained compaction input ")
            || message
                .tool_calls
                .iter()
                .any(|call| call.id == "t76-combined-spawn")
    }));
    assert!(!child_prefix.iter().any(|item| {
        parrot_storage::events::content(&item.message.parts).contains("t76-combined-invoking-batch")
            || item
                .message
                .tool_calls
                .iter()
                .any(|call| call.id == "t76-combined-spawn")
    }));
    assert!(!before_release.iter().any(|event| {
        event.has_tool_finished() && event.tool_finished().tool_call_id == "t76-combined-wait"
    }));
    assert!(
        !waiting_history
            .iter()
            .any(|item| item.message.tool_call_id == "t76-combined-wait")
    );
    for (call, expected) in [
        ("t76-combined-spawn", "combined-child"),
        ("t76-combined-question", "t76-combined-answer"),
        ("t76-combined-wait", "wait interrupted due to"),
    ] {
        let result = history
            .iter()
            .find(|item| item.message.tool_call_id == call)
            .unwrap();
        assert_eq!(result.message.role, MessageRole::Tool);
        assert!(
            parrot_storage::events::content(&result.message.parts).contains(expected),
            "{result:?}"
        );
        assert!(restored_history.iter().any(|item| item == result));
    }
    assert!(
        history
            .iter()
            .any(|item| parrot_storage::events::content(&item.message.parts)
                == "t76-combined-root-complete")
    );
    assert!(
        child_history
            .iter()
            .any(|item| parrot_storage::events::content(&item.message.parts)
                == "t76-combined-child-complete")
    );
    assert_eq!(restored_child_history, child_history);
    assert_eq!(restored_child_compaction, child_compaction);
    assert_eq!(restored_compaction, root_compaction);
    assert!(restored_history.starts_with(&history));
    assert!(restored_events.len() >= durable.len());
    assert_eq!(
        restored_events[..durable.len()]
            .iter()
            .map(|row| (row.sequence, &row.raw))
            .collect::<Vec<_>>(),
        durable
            .iter()
            .map(|row| (row.sequence, &row.raw))
            .collect::<Vec<_>>()
    );
    assert_eq!(
        durable
            .iter()
            .filter(|row| row.event.has_permission_pending())
            .map(|row| &row.event)
            .collect::<Vec<_>>(),
        vec![&pending_event]
    );
    assert!(fixture.scripts.lock().unwrap().is_empty());
}
