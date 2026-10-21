use super::*;

#[tokio::test]
async fn external_compact_waits_for_active_provider_before_starting_real_compaction() {
    let fixture = Fixture::new();
    fixture.seed().await;
    let active = Gate::new();
    let summary = Gate::new();
    let mut first = Step::complete("provider answer");
    first.gate = Some(active.clone());
    let mut second = Step::complete("compact summary");
    second.gate = Some(summary.clone());
    let provider = Scripted::new(vec![first, second]);
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
    active.entered().await;
    let session = runtime.session.clone();
    let compact = tokio::spawn(async move {
        session
            .compact_context(None, Arc::new(NeverCancelled))
            .await
    });
    tokio::task::yield_now().await;
    assert!(!compact.is_finished());
    assert_eq!(provider.requests.lock().unwrap().len(), 1);
    assert!(
        !fixture
            .events()
            .await
            .iter()
            .any(|event| event.has_compaction_started())
    );
    active.open();
    summary.entered().await;
    assert_eq!(provider.requests.lock().unwrap().len(), 2);
    let events = fixture.events().await;
    let ended = events
        .iter()
        .position(|event| event.has_turn_ended())
        .unwrap();
    let started = events
        .iter()
        .position(|event| event.has_compaction_started())
        .unwrap();
    assert!(ended < started);
    summary.open();
    assert!(bounded(compact).await.unwrap().unwrap().reduced);
    assert_eq!(
        bounded(sent.execution.join()).await.unwrap().status,
        ExecutionStatus::Succeeded
    );
    assert_eq!(
        fixture
            .events()
            .await
            .iter()
            .filter(|event| event.has_compaction_finished())
            .count(),
        1
    );
    runtime.close().await;
}
#[tokio::test]
async fn canceling_queued_external_compaction_does_not_cancel_active_provider() {
    let fixture = Fixture::new();
    fixture.seed().await;
    let active = Gate::new();
    let mut first = Step::complete("provider survives queued cancel");
    first.gate = Some(active.clone());
    let provider = Scripted::new(vec![first]);
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
    active.entered().await;
    let caller = Arc::new(Signal::new());
    let request = caller.clone();
    let session = runtime.session.clone();
    let compact = tokio::spawn(async move { session.compact_context(None, request).await });
    tokio::task::yield_now().await;
    caller.cancel();
    assert!(bounded(compact).await.unwrap().unwrap_err().is_cancelled());
    assert!(!sent.execution.is_finished());
    assert_eq!(provider.stream_closes.load(Ordering::SeqCst), 0);
    active.open();
    assert_eq!(
        bounded(sent.execution.join()).await.unwrap().status,
        ExecutionStatus::Succeeded
    );
    assert!(
        !fixture
            .events()
            .await
            .iter()
            .any(|event| event.has_compaction_started())
    );
    assert_eq!(provider.requests.lock().unwrap().len(), 1);
    runtime.close().await;
}
#[tokio::test]
async fn shutdown_cancels_queued_external_compaction_without_starting_it() {
    let fixture = Fixture::new();
    fixture.seed().await;
    let active = Gate::new();
    let mut first = Step::complete("never complete");
    first.gate = Some(active.clone());
    let provider = Scripted::new(vec![first]);
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
    active.entered().await;
    let session = runtime.session.clone();
    let compact = tokio::spawn(async move {
        session
            .compact_context(None, Arc::new(NeverCancelled))
            .await
    });
    tokio::task::yield_now().await;
    runtime.close().await;
    assert!(bounded(compact).await.unwrap().unwrap_err().is_cancelled());
    assert_eq!(
        bounded(sent.execution.join()).await.unwrap().status,
        ExecutionStatus::Canceled
    );
    assert!(
        !fixture
            .events()
            .await
            .iter()
            .any(|event| event.has_compaction_started())
    );
    assert_eq!(provider.requests.lock().unwrap().len(), 1);
    assert_eq!(provider.stream_closes.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn external_compaction_requested_after_shutdown_is_rejected_without_starting() {
    let fixture = Fixture::new();
    fixture.seed().await;
    let provider = Scripted::new(vec![]);
    let runtime = fixture
        .runtime(
            provider.clone(),
            Arc::new(Tools::default()),
            vec![],
            Arc::new(Signal::new()),
            0,
        )
        .await;
    runtime.close().await;
    let after_close = fixture.events().await;
    let rejected = bounded(
        runtime
            .session
            .compact_context(None, Arc::new(NeverCancelled)),
    )
    .await;
    assert!(matches!(rejected, Err(TurnError::Service(_))));
    assert_eq!(fixture.events().await, after_close);
    assert!(
        !after_close
            .iter()
            .any(|event| event.has_compaction_started())
    );
    assert!(provider.requests.lock().unwrap().is_empty());
    assert_eq!(provider.stream_closes.load(Ordering::SeqCst), 0);
    assert!(
        fixture
            .history
            .compaction_history()
            .await
            .unwrap()
            .is_none()
    );
    let raw: Vec<serde_json::Value> = fixture
        .jsonl()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    assert!(!raw.iter().any(|entry| entry["type"] == "compaction"));
}

#[tokio::test]
async fn cancellation_stops_only_running_external_compaction_and_subsequent_provider_still_runs() {
    let fixture = Fixture::new();
    fixture.seed().await;
    let summary = Gate::new();
    let mut first = Step::complete("cancelled summary");
    first.gate = Some(summary.clone());
    let provider = Scripted::new(vec![first, Step::complete("next provider succeeds")]);
    let runtime = fixture
        .runtime(
            provider.clone(),
            Arc::new(Tools::default()),
            vec![],
            Arc::new(Signal::new()),
            0,
        )
        .await;
    let caller = Arc::new(Signal::new());
    let request = caller.clone();
    let session = runtime.session.clone();
    let compact = tokio::spawn(async move { session.compact_context(None, request).await });
    summary.entered().await;
    caller.cancel();
    assert!(bounded(compact).await.unwrap().unwrap_err().is_cancelled());
    // The wrapper's caller can observe cancellation before owned cleanup settles;
    // the next real turn is serialized after that cleanup by the same drain.
    let sent = runtime
        .send("next question", protocol::Delivery::DELIVERY_STEER)
        .await;
    assert_eq!(
        bounded(sent.execution.join()).await.unwrap().status,
        ExecutionStatus::Succeeded
    );
    assert_eq!(provider.requests.lock().unwrap().len(), 2);
    assert_eq!(provider.stream_closes.load(Ordering::SeqCst), 2);
    assert_eq!(provider.session_closes.load(Ordering::SeqCst), 0);
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
            .filter(|event| event.has_compaction_failed())
            .count(),
        1
    );
    assert_eq!(
        events.iter().filter(|event| event.has_turn_ended()).count(),
        1
    );
    runtime.close().await;
}

#[tokio::test]
async fn canceling_running_external_compaction_preserves_already_queued_next_compaction() {
    use std::{future::Future, task::Poll};

    let fixture = Fixture::new();
    fixture.seed().await;
    let first_summary = Gate::new();
    let next_summary = Gate::new();
    let mut first = Step::complete("canceled summary must not commit");
    first.gate = Some(first_summary.clone());
    let mut second = Step::complete("queued compaction durable summary");
    second.gate = Some(next_summary.clone());
    let provider = Scripted::new(vec![first, second]);
    let runtime = fixture
        .runtime(
            provider.clone(),
            Arc::new(Tools::default()),
            vec![],
            Arc::new(Signal::new()),
            0,
        )
        .await;
    let first_caller = Arc::new(Signal::new());
    let request = first_caller.clone();
    let session = runtime.session.clone();
    let first_compact = tokio::spawn(async move { session.compact_context(None, request).await });
    first_summary.entered().await;

    let mut next_compact = Box::pin(
        runtime
            .session
            .compact_context(None, Arc::new(NeverCancelled)),
    );
    // The wrapper schedules its work on its first poll. Keep that same future
    // alive so the second compaction is already queued before canceling the first.
    std::future::poll_fn(|cx| {
        assert!(matches!(next_compact.as_mut().poll(cx), Poll::Pending));
        Poll::Ready(())
    })
    .await;
    assert_eq!(provider.requests.lock().unwrap().len(), 1);
    first_caller.cancel();
    assert!(
        bounded(first_compact)
            .await
            .unwrap()
            .unwrap_err()
            .is_cancelled()
    );

    next_summary.entered().await;
    assert_eq!(provider.requests.lock().unwrap().len(), 2);
    assert_eq!(provider.stream_closes.load(Ordering::SeqCst), 1);
    assert_eq!(provider.session_closes.load(Ordering::SeqCst), 0);
    let running_next = fixture.events().await;
    assert_eq!(
        running_next
            .iter()
            .filter(|event| event.has_compaction_started())
            .count(),
        2
    );
    assert_eq!(
        running_next
            .iter()
            .filter(|event| event.has_compaction_failed())
            .count(),
        1
    );
    assert!(
        !running_next
            .iter()
            .any(|event| event.has_compaction_finished())
    );
    assert!(
        fixture
            .history
            .compaction_history()
            .await
            .unwrap()
            .is_none()
    );

    next_summary.open();
    assert!(bounded(next_compact).await.unwrap().reduced);
    assert_eq!(provider.stream_closes.load(Ordering::SeqCst), 2);
    let events = fixture.events().await;
    assert_eq!(
        events
            .iter()
            .filter(|event| event.has_compaction_started())
            .count(),
        2
    );
    assert_eq!(
        events
            .iter()
            .filter(|event| event.has_compaction_failed())
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
    assert!(
        !events
            .iter()
            .any(|event| event.has_turn_started() || event.has_turn_failed())
    );
    assert!(
        fixture
            .history
            .compaction_history()
            .await
            .unwrap()
            .is_some()
    );
    let raw: Vec<serde_json::Value> = fixture
        .jsonl()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    let compacted: Vec<_> = raw
        .iter()
        .filter(|entry| entry["type"] == "compaction")
        .collect();
    assert_eq!(compacted.len(), 1);
    assert!(
        compacted[0]["summary"]
            .as_str()
            .unwrap()
            .contains("queued compaction durable summary")
    );
    assert!(!fixture.jsonl().contains("canceled summary must not commit"));
    runtime.close().await;
}
