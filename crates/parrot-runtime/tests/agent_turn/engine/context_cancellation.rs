use super::*;
fn assert_interrupted(events: &[Event]) {
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
    assert!(!events.iter().any(|event| event.has_turn_failed()));
}
fn tool_step() -> Step {
    let mut step = Step::complete("context control");
    step.completion.as_mut().unwrap().tool_calls = vec![sessions::ToolCall {
        id: "context-control".into(),
        name: "compact_context".into(),
        arguments_json: "{}".into(),
    }];
    step
}
#[tokio::test]
async fn awaited_actual_epoch_prompt_cancellation_closes_interrupted_without_provider_or_turn_failed()
 {
    let fixture = Fixture::new();
    let tools = Arc::new(Tools::default());
    tools.prompt_cancelled.store(true, Ordering::SeqCst);
    let trace = tools.trace.clone();
    let provider = Scripted::new(vec![]);
    let runtime = fixture
        .runtime(provider.clone(), tools, vec![], Arc::new(Signal::new()), 0)
        .await;
    let sent = runtime
        .send("question", protocol::Delivery::DELIVERY_STEER)
        .await;
    assert_eq!(
        bounded(sent.execution.join()).await.unwrap().status,
        ExecutionStatus::Canceled
    );
    assert!(provider.requests.lock().unwrap().is_empty());
    assert_interrupted(&fixture.events().await);
    assert!(trace.values().iter().any(|entry| entry == "epoch-start"));
    assert!(!trace.values().iter().any(|entry| entry == "epoch-done"));
    runtime.close().await;
}
#[tokio::test]
async fn actual_inline_summary_provider_typed_cancellations_are_interrupted_and_join_stream_cleanup()
 {
    for error in [
        Error::Cancelled,
        Error::Cancellation("transport-owned cancellation text".into()),
    ] {
        let fixture = Fixture::new();
        fixture.seed().await;
        let tools = Arc::new(Tools::default());
        tools.inline_compact.store(true, Ordering::SeqCst);
        let mut summary = Step::complete("not committed");
        summary.completion = Err(error);
        let provider = Scripted::new(vec![tool_step(), summary]);
        let runtime = fixture
            .runtime(provider.clone(), tools, vec![], Arc::new(Signal::new()), 0)
            .await;
        let sent = runtime
            .send("question", protocol::Delivery::DELIVERY_STEER)
            .await;
        assert_eq!(
            bounded(sent.execution.join()).await.unwrap().status,
            ExecutionStatus::Canceled
        );
        assert_interrupted(&fixture.events().await);
        assert_eq!(provider.stream_closes.load(Ordering::SeqCst), 2);
        assert!(
            fixture
                .history
                .compaction_history()
                .await
                .unwrap()
                .is_none()
        );
        assert_eq!(
            fixture
                .events()
                .await
                .iter()
                .filter(|event| event.has_compaction_failed())
                .count(),
            1
        );
        runtime.close().await;
    }
}
#[tokio::test]
async fn actual_context_summary_retry_publication_cancellation_retains_cancel_kind_and_joins_cleanup()
 {
    let fixture = Fixture::new();
    fixture.seed().await;
    let tools = Arc::new(Tools::default());
    tools.inline_compact.store(true, Ordering::SeqCst);
    let mut summary = Step::complete("must not commit summary");
    summary.events.push_back(sessions::Event::Retry {
        attempt: 1,
        retry_after: Duration::ZERO,
        reason: "provider retry".into(),
    });
    let provider = Scripted::new(vec![tool_step(), summary]);
    let runtime = fixture
        .runtime(provider.clone(), tools, vec![], Arc::new(Signal::new()), 0)
        .await;
    runtime.publisher.cancel_retry.store(true, Ordering::SeqCst);
    let sent = runtime
        .send("question", protocol::Delivery::DELIVERY_STEER)
        .await;
    assert_eq!(
        bounded(sent.execution.join()).await.unwrap().status,
        ExecutionStatus::Canceled
    );
    let events = fixture.events().await;
    assert_interrupted(&events);
    assert_eq!(
        events
            .iter()
            .filter(|event| event.has_retry_notice())
            .count(),
        1
    );
    assert_eq!(provider.stream_closes.load(Ordering::SeqCst), 2);
    assert!(
        fixture
            .history
            .compaction_history()
            .await
            .unwrap()
            .is_none()
    );
    runtime.close().await;
}
