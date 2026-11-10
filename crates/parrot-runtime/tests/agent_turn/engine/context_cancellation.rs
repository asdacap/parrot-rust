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
    assert_eq!(provider.turn_begins.load(Ordering::SeqCst), 0);
    let events = fixture.events().await;
    assert_interrupted(&events);
    let started = events
        .iter()
        .find(|event| event.has_turn_started())
        .unwrap();
    let diagnostic = runtime.activity.diagnostic.lock().unwrap().clone();
    assert_eq!(diagnostic.len(), 2);
    assert_eq!(diagnostic[0], format!("started:{}", started.id));
    assert!(diagnostic[1].starts_with(&format!("finished:{}:cancelled:", started.id)));
    assert!(trace.values().iter().any(|entry| entry == "epoch-start"));
    assert!(!trace.values().iter().any(|entry| entry == "epoch-done"));
    assert!(!trace.values().iter().any(|entry| entry == "skills-begin"));
    runtime.close().await;
}
struct FailingEpochPrompt;
impl SystemPrompt for FailingEpochPrompt {
    fn renew_epoch(&mut self) {}

    fn sample_epoch<'a>(&'a mut self, _: &'a dyn Cancellation) -> BoxFuture<'a, PromptResult<()>> {
        Box::pin(async {
            Err(parrot_runtime::prompt_context::PromptError::Context(
                "epoch initialization failed".into(),
            ))
        })
    }

    fn read_system_prompts(
        &mut self,
        _: &parrot_provider::routing::aliases::ResolvedModelSelection,
    ) -> PromptResult<Vec<(String, String)>> {
        panic!("failed epoch must not be built")
    }
}

#[tokio::test]
async fn native_initialization_failure_finishes_its_started_activity_without_entering_core() {
    let fixture = Fixture::new();
    let tools = Arc::new(Tools::default());
    let provider = Scripted::default();
    let runtime = fixture
        .runtime_with_overrides(
            provider.clone(),
            tools.clone(),
            vec![],
            Arc::new(Signal::new()),
            0,
            RuntimeOverrides {
                prompt: Some(Box::new(FailingEpochPrompt)),
                ..Default::default()
            },
        )
        .await;
    let sent = runtime
        .send("question", protocol::Delivery::DELIVERY_STEER)
        .await;
    assert_eq!(
        bounded(sent.execution.join()).await.unwrap().status,
        ExecutionStatus::Failed
    );
    assert_eq!(provider.turn_begins.load(Ordering::SeqCst), 0);
    assert!(provider.requests.lock().unwrap().is_empty());
    assert_eq!(tools.executions.load(Ordering::SeqCst), 0);
    assert!(
        !tools
            .trace
            .values()
            .iter()
            .any(|entry| entry == "skills-begin")
    );
    let events = fixture.events().await;
    let starts: Vec<_> = events
        .iter()
        .filter(|event| event.has_turn_started())
        .collect();
    let failures: Vec<_> = events
        .iter()
        .filter(|event| event.has_turn_failed())
        .collect();
    assert_eq!(starts.len(), 1);
    assert_eq!(failures.len(), 1);
    assert!(
        failures[0]
            .turn_failed()
            .message
            .contains("epoch initialization failed")
    );
    assert!(!events.iter().any(|event| event.has_turn_ended()));
    let diagnostic = runtime.activity.diagnostic.lock().unwrap().clone();
    assert_eq!(diagnostic.len(), 2);
    assert_eq!(diagnostic[0], format!("started:{}", starts[0].id));
    assert!(diagnostic[1].starts_with(&format!("finished:{}:failed:", starts[0].id)));
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

#[tokio::test]
async fn oversized_recovery_uses_the_shared_core_summary_without_publishing_private_deltas() {
    let fixture = Fixture::new();
    fixture.seed().await;
    let mut oversized = Step::complete("rejected");
    oversized.completion = Err(Error::Http(
        parrot_provider::http_stream::HttpError::RequestTooLarge(1),
    ));
    let mut summary = Step::complete("oversized durable summary");
    summary
        .events
        .push_back(sessions::Event::TextDelta("private summary delta".into()));
    summary.events.push_back(sessions::Event::ReasoningDelta {
        text: "private summary reasoning".into(),
        kind: sessions::ReasoningKind::Raw,
        part_id: "reasoning".into(),
        completed: false,
    });
    let provider = Scripted::new(vec![oversized, summary, Step::complete("final answer")]);
    let runtime = fixture
        .runtime(
            provider.clone(),
            Arc::new(Tools::default()),
            vec![],
            Arc::new(Signal::new()),
            0,
        )
        .await;
    let sent = runtime
        .send("current question", protocol::Delivery::DELIVERY_STEER)
        .await;
    assert_eq!(
        bounded(sent.execution.join()).await.unwrap().status,
        ExecutionStatus::Succeeded
    );
    let requests = provider.requests.lock().unwrap().clone();
    assert_eq!(requests.len(), 3);
    assert!(requests[1].instructions.is_empty());
    assert!(requests[1].tools.is_empty());
    assert_eq!(requests[1].max_output_tokens, 1024);
    assert!(
        requests[2]
            .messages
            .iter()
            .any(|message| message.content().contains("oversized durable summary"))
    );
    assert!(
        !requests[2]
            .messages
            .iter()
            .any(|message| message.content().starts_with("seed-0-"))
    );
    let snapshot = fixture.history.compaction_history().await.unwrap().unwrap();
    assert!(
        snapshot
            .snapshot
            .summary
            .contains("oversized durable summary")
    );
    assert!(
        !runtime
            .output
            .text
            .lock()
            .unwrap()
            .contains("private summary")
    );
    assert!(!fixture.jsonl().contains("private summary"));
    let events = fixture.events().await;
    assert_eq!(
        events
            .iter()
            .filter(|event| event.has_compaction_started())
            .count(),
        1
    );
    assert_eq!(
        events
            .iter()
            .filter(|event| event.has_compaction_finished())
            .count(),
        1
    );
    assert!(!events.iter().any(|event| event.has_compaction_failed()));
    assert_eq!(
        events
            .iter()
            .filter(|event| event.has_request_usage_recorded())
            .count(),
        1
    );
    assert_eq!(provider.stream_closes.load(Ordering::SeqCst), 3);
    runtime.close().await;
}

#[tokio::test]
async fn oversized_recovery_summary_cancellation_records_failure_after_joining_the_stream() {
    let fixture = Fixture::new();
    fixture.seed().await;
    let mut oversized = Step::complete("rejected");
    oversized.completion = Err(Error::Http(
        parrot_provider::http_stream::HttpError::RequestTooLarge(1),
    ));
    let mut summary = Step::complete("must not persist");
    summary.completion = Err(Error::Cancellation("summary transport canceled".into()));
    let provider = Scripted::new(vec![oversized, summary]);
    let runtime = fixture
        .runtime(
            provider.clone(),
            Arc::new(Tools::default()),
            vec![],
            Arc::new(Signal::new()),
            0,
        )
        .await;
    let sent = runtime
        .send("question", protocol::Delivery::DELIVERY_STEER)
        .await;
    assert_eq!(
        bounded(sent.execution.join()).await.unwrap().status,
        ExecutionStatus::Canceled
    );
    assert_eq!(provider.stream_closes.load(Ordering::SeqCst), 2);
    assert_eq!(provider.requests.lock().unwrap().len(), 2);
    assert!(
        fixture
            .history
            .compaction_history()
            .await
            .unwrap()
            .is_none()
    );
    let events = fixture.events().await;
    assert_interrupted(&events);
    assert_eq!(
        events
            .iter()
            .filter(|event| event.has_compaction_started())
            .count(),
        1
    );
    let failed: Vec<_> = events
        .iter()
        .filter(|event| event.has_compaction_failed())
        .collect();
    assert_eq!(failed.len(), 1);
    assert_eq!(
        failed[0].compaction_failed().message,
        "summary transport canceled"
    );
    assert!(!events.iter().any(|event| event.has_compaction_finished()));
    runtime.close().await;
}
