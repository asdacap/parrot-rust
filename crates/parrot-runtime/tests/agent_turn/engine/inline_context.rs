use super::*;
fn tool() -> Step {
    let mut step = Step::complete("inline compact tool");
    step.completion.as_mut().unwrap().tool_calls = vec![sessions::ToolCall {
        id: "inline-call".into(),
        name: "compact_context".into(),
        arguments_json: "{}".into(),
    }];
    step
}
#[tokio::test]
async fn tool_reconcile_borrows_actual_controller_and_compacted_state_reaches_next_provider_without_second_drain()
 {
    let fixture = Fixture::new();
    fixture.seed().await;
    let tools = Arc::new(Tools::default());
    tools.inline_compact.store(true, Ordering::SeqCst);
    let trace = tools.trace.clone();
    let provider = Scripted::new(vec![
        tool(),
        Step::complete("inline durable summary"),
        Step::complete("answer using inline summary"),
    ]);
    let runtime = fixture
        .runtime(provider.clone(), tools, vec![], Arc::new(Signal::new()), 0)
        .await;
    let sent = runtime
        .send("current question", protocol::Delivery::DELIVERY_STEER)
        .await;
    assert_eq!(
        bounded(sent.execution.join()).await.unwrap().status,
        ExecutionStatus::Succeeded
    );
    assert!(
        trace
            .values()
            .iter()
            .any(|value| value == "inline-compacted")
    );
    let requests = provider.requests.lock().unwrap().clone();
    assert_eq!(requests.len(), 3);
    assert!(
        requests[2]
            .messages
            .iter()
            .any(|message| message.content().contains("inline durable summary"))
    );
    assert!(
        !requests[2]
            .messages
            .iter()
            .any(|message| message.content().starts_with("seed-0-"))
    );
    drop(requests);
    assert!(
        fixture
            .history
            .compaction_history()
            .await
            .unwrap()
            .is_some()
    );
    let events = fixture.events().await;
    assert_eq!(
        events
            .iter()
            .filter(|event| event.has_turn_started())
            .count(),
        1
    );
    assert_eq!(
        events.iter().filter(|event| event.has_turn_ended()).count(),
        1
    );
    assert_eq!(
        events
            .iter()
            .filter(|event| event.has_compaction_finished())
            .count(),
        1
    );
    let raw: Vec<serde_json::Value> = fixture
        .jsonl()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    assert_eq!(
        raw.iter()
            .filter(|entry| entry["type"] == "compaction")
            .count(),
        1
    );
    runtime.close().await;
}
#[tokio::test]
async fn interrupt_joins_actual_inline_compaction_cancellation_without_self_queue_or_second_turn() {
    let fixture = Fixture::new();
    fixture.seed().await;
    let tools = Arc::new(Tools::default());
    tools.inline_compact.store(true, Ordering::SeqCst);
    let summary = Gate::new();
    let mut step = Step::complete("never commit summary");
    step.gate = Some(summary.clone());
    let provider = Scripted::new(vec![tool(), step]);
    let runtime = fixture
        .runtime(provider.clone(), tools, vec![], Arc::new(Signal::new()), 0)
        .await;
    let sent = runtime
        .send("current question", protocol::Delivery::DELIVERY_STEER)
        .await;
    summary.entered().await;
    bounded(runtime.session.interrupt(&NeverCancelled))
        .await
        .unwrap();
    assert_eq!(
        bounded(sent.execution.join()).await.unwrap().status,
        ExecutionStatus::Canceled
    );
    assert_eq!(provider.requests.lock().unwrap().len(), 2);
    assert_eq!(provider.stream_closes.load(Ordering::SeqCst), 2);
    assert!(
        fixture
            .history
            .compaction_history()
            .await
            .unwrap()
            .is_none()
    );
    let events = fixture.events().await;
    assert_eq!(
        events
            .iter()
            .filter(|event| event.has_turn_started())
            .count(),
        1
    );
    let ended: Vec<_> = events
        .iter()
        .filter(|event| event.has_turn_ended())
        .collect();
    assert_eq!(ended.len(), 1);
    assert_eq!(ended[0].turn_ended().finish_reason, "interrupted");
    assert_eq!(
        events
            .iter()
            .filter(|event| event.has_compaction_failed())
            .count(),
        1
    );
    runtime.close().await;
}
