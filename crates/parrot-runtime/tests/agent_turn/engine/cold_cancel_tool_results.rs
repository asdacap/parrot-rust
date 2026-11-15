use super::*;

#[tokio::test]
async fn saved_canceled_checkpoint_leaves_unfinished_tools_to_the_next_turn() {
    let fixture = Fixture::new();
    let store = fixture.recovery_store().await;
    let original = fixture
        .save_cancelable_execution(store.as_ref(), ExecutionPhase::Canceled)
        .await;
    let provider = Scripted::new(vec![
        Step::complete("settled answer"),
        Step::complete("later answer"),
    ]);
    let tools = Arc::new(Tools::default());
    let parent = Arc::new(RecoveryParent::default());
    let runtime = fixture
        .runtime_with_overrides(
            provider.clone(),
            tools.clone(),
            vec![],
            Arc::new(Signal::new()),
            0,
            RuntimeOverrides {
                state: Some(store.clone()),
                parent_completion: Some(parent.clone()),
                ..RuntimeOverrides::default()
            },
        )
        .await;
    runtime
        .session
        .record(
            vec![ConversationPart::text("saved question")],
            original.initial_message_id.clone(),
            protocol::Delivery::DELIVERY_STEER,
            Arc::new(NeverCancelled),
        )
        .await
        .unwrap();
    let mut message = ConversationMessage::text(MessageRole::Assistant, "unfinished tool requests");
    message.tool_calls = ["first-crashed", "second-crashed"]
        .into_iter()
        .map(|id| parrot_storage::events::ToolCall {
            id: id.into(),
            name: "must-not-run".into(),
            arguments_json: "{}".into(),
        })
        .collect();
    fixture
        .history
        .append_conversation(
            Event {
                id: id(),
                agent_session_id: "agent".into(),
                ..Event::new()
            },
            message,
        )
        .await
        .unwrap();
    let outcome = bounded(
        runtime
            .session
            .recover_owned_reserved()
            .unwrap()
            .execution
            .join(),
    )
    .await
    .unwrap();
    assert_eq!(outcome.status, ExecutionStatus::Canceled);
    let saved = store.load().await.unwrap().unwrap().execution.unwrap();
    assert_eq!(saved.phase, ExecutionPhase::Canceled);
    assert_eq!(saved.execution_id, original.execution_id);
    assert_eq!(
        saved.terminal.as_ref().unwrap().outcome,
        ExecutionOutcome::Canceled
    );
    let calls = vec!["first-crashed".to_string(), "second-crashed".to_string()];
    assert!(
        fixture
            .history
            .tool_terminals_of_calls(calls.clone())
            .await
            .unwrap()
            .is_empty()
    );
    let events = fixture.events().await;
    assert!(!events.iter().any(|event| event.has_tool_error()));
    assert!(
        !events
            .iter()
            .any(|event| event.has_tool_started() || event.has_tool_execution_started())
    );
    assert!(provider.requests.lock().unwrap().is_empty());
    assert_eq!(tools.executions.load(Ordering::SeqCst), 0);
    assert_eq!(parent.messages.lock().unwrap().len(), 1);
    assert_eq!(
        bounded(
            runtime
                .session
                .recover_owned_reserved()
                .unwrap()
                .execution
                .join()
        )
        .await
        .unwrap(),
        outcome
    );
    assert_eq!(
        store.load().await.unwrap().unwrap().execution.unwrap(),
        saved
    );
    assert!(
        fixture
            .history
            .tool_terminals_of_calls(calls.clone())
            .await
            .unwrap()
            .is_empty()
    );
    assert_eq!(fixture.events().await.len(), events.len());
    assert!(provider.requests.lock().unwrap().is_empty());
    assert_eq!(tools.executions.load(Ordering::SeqCst), 0);
    let later = runtime
        .session
        .enqueue_execution(
            vec![ConversationPart::text("later question")],
            "later-message".into(),
            None,
            None,
            Arc::new(NeverCancelled),
        )
        .unwrap();
    assert_eq!(
        bounded(later.join()).await.unwrap(),
        Execution::succeeded("settled answer")
    );
    let requests = provider.requests.lock().unwrap().clone();
    assert_eq!(requests.len(), 1);
    let crash = "The process crashed before the tool completed.";
    for call in ["first-crashed", "second-crashed"] {
        assert!(
            requests[0]
                .messages
                .iter()
                .any(|message| message.tool_call_id == call && message.content().contains(crash)),
            "the request must carry the crash result for {call}"
        );
    }
    let batch_index = requests[0]
        .messages
        .iter()
        .position(|message| message.content().contains("unfinished tool requests"))
        .expect("the dangling batch stays in the request");
    let first_result_index = requests[0]
        .messages
        .iter()
        .position(|message| message.tool_call_id == "first-crashed")
        .expect("the crash result follows the batch");
    let input_index = requests[0]
        .messages
        .iter()
        .position(|message| message.content().contains("later question"))
        .expect("the next input joins the same request");
    assert!(batch_index < first_result_index && first_result_index < input_index);
    let terminals = fixture
        .history
        .tool_terminals_of_calls(calls)
        .await
        .unwrap();
    assert_eq!(terminals.len(), 2);
    assert!(terminals.iter().all(|terminal| {
        terminal.status == parrot_storage::events::ToolExecutionStatus::Error
            && terminal
                .content()
                .contains("The process crashed before the tool completed.")
    }));
    let events = fixture.events().await;
    assert_eq!(
        events.iter().filter(|event| event.has_tool_error()).count(),
        2
    );
    assert!(
        !events
            .iter()
            .any(|event| event.has_tool_started() || event.has_tool_execution_started())
    );
    assert_eq!(tools.executions.load(Ordering::SeqCst), 0);
    runtime.close().await;
}
