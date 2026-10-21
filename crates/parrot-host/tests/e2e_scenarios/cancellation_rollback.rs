use super::*;

#[tokio::test]
async fn dropped_listener_held_provider_interrupt_queued_restart_and_transaction_rollback() {
    let fixture = fixture().await;
    let hosted = fixture.open().await.unwrap();
    let root = fixture.root();
    let user = fixture.user();
    let barrier = Arc::new(TurnBarrier::default());
    fixture
        .turn_barriers
        .lock()
        .unwrap()
        .insert("t76-held-turn".into(), barrier.clone());
    let mut disconnected = hosted
        .listen(Some(String::new()), Arc::new(NeverCancelled))
        .unwrap();
    send(&hosted, "t76-held-turn", "t76-held-input").await;
    bounded(barrier.entered.notified()).await;
    let first = bounded(async {
        loop {
            let event = disconnected.next(&NeverCancelled).await.unwrap().unwrap();
            if event.has_turn_started() {
                break event;
            }
        }
    })
    .await;
    drop(disconnected);
    bounded(hosted.interrupt(Arc::new(NeverCancelled)))
        .await
        .unwrap();
    barrier.release.add_permits(1);
    idle(&root).await;
    let before = user.lease.events.replay_raw().await.unwrap();
    let mut attempted = Event::new();
    attempted.id = Identifier::event_id().unwrap();
    attempted.agent_session_id = root.identity().session.session_id.clone();
    let attempted_id = attempted.id.clone();
    let failed: Result<(), parrot_storage::events::EventError> = user
        .lease
        .events
        .transaction(move |tx| {
            parrot_storage::events::record(tx, &attempted, None, "2026-10-21T00:00:00Z")?;
            Err(parrot_storage::events::EventError::Invalid(
                "t76 deliberate rollback after insert".into(),
            ))
        })
        .await;
    let after = user.lease.events.replay_raw().await.unwrap();
    bounded(hosted.send(
        vec![ConversationPart::text("t76-queued-after-interrupt")],
        "t76-queued-restart".into(),
        Delivery::DELIVERY_QUEUE,
        Arc::new(NeverCancelled),
    ))
    .await
    .unwrap();
    idle(&root).await;
    let durable = user.lease.events.replay().await.unwrap();
    let history = root.history().conversation().await.unwrap();
    let suffix_start = durable
        .iter()
        .position(|event| event.id == first.id)
        .unwrap()
        + 1;
    let mut reconnected = hosted
        .listen(Some(first.id), Arc::new(NeverCancelled))
        .unwrap();
    let mut replayed = Vec::new();
    for _ in suffix_start..durable.len() {
        replayed.push(
            bounded(reconnected.next(&NeverCancelled))
                .await
                .unwrap()
                .unwrap(),
        );
    }
    drop(reconnected);
    bounded(hosted.close()).await.unwrap();
    assert_database_closed(&user);

    assert!(
        matches!(failed, Err(parrot_storage::events::EventError::Invalid(message)) if message == "t76 deliberate rollback after insert")
    );
    assert_eq!(
        before
            .iter()
            .map(|row| (row.sequence, &row.raw))
            .collect::<Vec<_>>(),
        after
            .iter()
            .map(|row| (row.sequence, &row.raw))
            .collect::<Vec<_>>()
    );
    assert!(!durable.iter().any(|event| event.id == attempted_id));
    assert_eq!(replayed, durable[suffix_start..]);
    let reasons: Vec<_> = durable
        .iter()
        .filter(|event| event.has_turn_ended())
        .map(|event| event.turn_ended().finish_reason.as_str())
        .collect();
    assert_eq!(reasons, vec!["interrupted", "stop"]);
    assert!(
        history
            .iter()
            .any(|item| parrot_storage::events::content(&item.message.parts)
                == "t76-queued-after-interrupt")
    );
    assert_eq!(fixture.requests.lock().unwrap().len(), 2);
}

#[tokio::test]
async fn pending_catalog_question_is_cancelled_and_joined_by_close_before_reactivation() {
    let fixture = fixture().await;
    let hosted = fixture.open().await.unwrap();
    let user = fixture.user();
    let root = fixture.root();
    let id = UserSessionId::parse(&user.id).unwrap();
    script(
        &fixture,
        "t76-question-input",
        tool_reply(
            "t76-pending-question",
            "question",
            json!({"questions":[{"prompt":"bounded pending question","custom":true}]}),
        ),
        None,
    );
    send(&hosted, "t76-question-input", "t76-question-message").await;
    let pending = bounded(async {
        loop {
            let pending = hosted.pending_questions().unwrap();
            if !pending.is_empty() {
                break pending;
            }
            tokio::task::yield_now().await;
        }
    })
    .await;
    let active_before_close = root.session().is_active();
    bounded(hosted.close()).await.unwrap();
    assert_database_closed(&user);
    let opened = bounded(fixture.store.resume(&id, true, false))
        .await
        .unwrap();
    let resumed = bounded(fixture.factory.bind_opened(opened)).await.unwrap();
    let resumed_user = fixture.user();
    let history = fixture.root().history().conversation().await.unwrap();
    let new_pending = resumed.pending_questions().unwrap();
    bounded(resumed.close()).await.unwrap();
    assert_database_closed(&resumed_user);

    assert!(active_before_close);
    assert_eq!(pending.len(), 1);
    assert!(!root.session().is_active());
    assert!(new_pending.is_empty());
    assert!(history.iter().any(|item| {
        item.message
            .tool_calls
            .iter()
            .any(|call| call.id == "t76-pending-question")
    }));
    assert!(
        history
            .iter()
            .any(|item| item.message.tool_call_id == "t76-pending-question")
    );
    assert_eq!(
        hosted.pending_questions().unwrap_err().code,
        RpcCode::Unavailable
    );
}
