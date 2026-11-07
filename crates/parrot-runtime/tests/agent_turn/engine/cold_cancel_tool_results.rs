use super::*;

#[tokio::test]
async fn saved_canceled_checkpoint_closes_unfinished_tools_without_activating_provider_or_tools() {
    let fixture = Fixture::new();
    let store = fixture.recovery_store().await;
    let original = fixture
        .save_cancelable_execution(store.as_ref(), ExecutionPhase::Canceled)
        .await;
    let provider = Scripted::default();
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
    let outcome = bounded(runtime.session.recover_owned().unwrap().join())
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
    let terminals = fixture
        .history
        .tool_terminals_of_calls(vec!["first-crashed".into(), "second-crashed".into()])
        .await
        .unwrap();
    assert_eq!(terminals.len(), 2);
    assert!(terminals.iter().all(|terminal| {
        terminal.status == parrot_storage::events::ToolExecutionStatus::Error
            && terminal
                .message
                .contains("The process crashed before the tool completed.")
    }));
    let tool_results: Vec<_> = fixture
        .history
        .conversation()
        .await
        .unwrap()
        .into_iter()
        .filter(|item| item.message.role == MessageRole::Tool)
        .collect();
    assert_eq!(tool_results.len(), 2);
    assert!(
        tool_results
            .iter()
            .all(|item| item.sequence <= saved.terminal.as_ref().unwrap().recovery_watermark)
    );
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
    assert!(provider.requests.lock().unwrap().is_empty());
    assert_eq!(tools.executions.load(Ordering::SeqCst), 0);
    assert_eq!(tools.materializations.load(Ordering::SeqCst), 0);
    assert_eq!(parent.messages.lock().unwrap().len(), 1);
    assert_eq!(
        bounded(runtime.session.recover_owned().unwrap().join())
            .await
            .unwrap(),
        outcome
    );
    assert_eq!(
        store.load().await.unwrap().unwrap().execution.unwrap(),
        saved
    );
    assert_eq!(fixture.events().await.len(), events.len());
    assert_eq!(
        fixture
            .history
            .tool_terminals_of_calls(vec!["first-crashed".into(), "second-crashed".into()])
            .await
            .unwrap(),
        terminals
    );
    assert!(provider.requests.lock().unwrap().is_empty());
    assert_eq!(tools.executions.load(Ordering::SeqCst), 0);
    assert_eq!(tools.materializations.load(Ordering::SeqCst), 0);
    assert_eq!(parent.messages.lock().unwrap().len(), 1);
    runtime.close().await;
}
