use super::*;

impl Fixture {
    async fn save_claimed_intent(
        &self,
        store: &dyn AgentStateStore,
        claim: &TaskResultClaim,
    ) -> ExecutionCheckpoint {
        let checkpoint = ExecutionCheckpoint {
            execution_id: "task-execution".into(),
            initial_message_id: claim.claim_id.clone(),
            initial_parts: vec![ExecutionInputPart::Text {
                text: "original task question".into(),
            }],
            history_watermark: 0,
            phase: ExecutionPhase::Active,
            terminal: None,
            parent_notification: None,
            result_claim: Some(claim.clone()),
        };
        store
            .update(AgentStateUpdate::Execution(Some(checkpoint.clone())))
            .await
            .unwrap();
        checkpoint
    }
}

#[tokio::test]
async fn pre_canceled_matching_claim_does_not_restart_cold_provider_work() {
    let fixture = Fixture::new();
    let store = fixture.recovery_store().await;
    let claim = TaskResultClaim {
        owner_session_id: "owner".into(),
        claim_id: "task-prompt".into(),
    };
    let original = fixture.save_claimed_intent(store.as_ref(), &claim).await;
    let provider = Scripted::default();
    let runtime = fixture
        .recovered_runtime(
            provider.clone(),
            store.clone(),
            Arc::new(RecoveryParent::default()),
        )
        .await;
    let caller = Arc::new(Signal::new());
    caller.cancel();
    let task = runtime
        .session
        .enqueue_claimed_execution(
            vec![ConversationPart::text("do not resend")],
            claim.claim_id.clone(),
            claim,
            caller,
        )
        .unwrap();
    assert!(
        bounded(task.reservation.join())
            .await
            .unwrap_err()
            .is_cancelled()
    );
    assert!(
        bounded(task.execution.join())
            .await
            .unwrap_err()
            .is_cancelled()
    );
    assert_eq!(
        store.load().await.unwrap().unwrap().execution,
        Some(original)
    );
    assert!(provider.requests.lock().unwrap().is_empty());
    assert!(fixture.conversation().await.messages().is_empty());
    runtime.close().await;
}

#[tokio::test]
async fn matching_cold_claim_caller_cancellation_owns_and_joins_recovered_attempt() {
    let fixture = Fixture::new();
    let store = fixture.recovery_store().await;
    let claim = TaskResultClaim {
        owner_session_id: "owner".into(),
        claim_id: "task-prompt".into(),
    };
    fixture.save_claimed_intent(store.as_ref(), &claim).await;
    let gate = Gate::new();
    let mut step = Step::complete("not completed");
    step.gate = Some(gate.clone());
    let provider = Scripted::new(vec![step]);
    let runtime = fixture
        .recovered_runtime(
            provider.clone(),
            store.clone(),
            Arc::new(RecoveryParent::default()),
        )
        .await;
    let caller = Arc::new(Signal::new());
    let task = runtime
        .session
        .enqueue_claimed_execution(
            vec![ConversationPart::text("do not resend")],
            claim.claim_id.clone(),
            claim.clone(),
            caller.clone(),
        )
        .unwrap();
    bounded(task.reservation.join()).await.unwrap();
    gate.entered().await;
    caller.cancel();
    assert_eq!(
        bounded(task.execution.join()).await.unwrap().status,
        ExecutionStatus::Canceled
    );
    assert_eq!(provider.stream_closes.load(Ordering::SeqCst), 1);
    let saved = store.load().await.unwrap().unwrap().execution.unwrap();
    assert_eq!(saved.phase, ExecutionPhase::Canceled);
    assert_eq!(saved.terminal.unwrap().outcome, ExecutionOutcome::Canceled);
    assert_eq!(saved.result_claim, Some(claim.clone()));
    let reference = runtime
        .session
        .capture_claimed_result(&claim.claim_id, &claim)
        .await
        .unwrap();
    runtime
        .session
        .acknowledge_claimed_result(&reference, &claim)
        .await
        .unwrap();
    assert!(
        store
            .load()
            .await
            .unwrap()
            .unwrap()
            .execution
            .unwrap()
            .result_claim
            .is_none()
    );
    assert_eq!(provider.requests.lock().unwrap().len(), 1);
    runtime.close().await;
}

#[tokio::test]
async fn matching_live_claim_caller_cancels_only_the_captured_owned_attempt() {
    let fixture = Fixture::new();
    let store = fixture.recovery_store().await;
    let claim = TaskResultClaim {
        owner_session_id: "owner".into(),
        claim_id: "task-prompt".into(),
    };
    let gate = Gate::new();
    let mut step = Step::complete("not completed");
    step.gate = Some(gate.clone());
    let provider = Scripted::new(vec![step]);
    let runtime = fixture
        .recovered_runtime(
            provider.clone(),
            store.clone(),
            Arc::new(RecoveryParent::default()),
        )
        .await;
    let original = runtime
        .session
        .enqueue_claimed_execution(
            vec![ConversationPart::text("task question")],
            claim.claim_id.clone(),
            claim.clone(),
            Arc::new(NeverCancelled),
        )
        .unwrap();
    bounded(original.reservation.join()).await.unwrap();
    gate.entered().await;
    let caller = Arc::new(Signal::new());
    let matching = runtime
        .session
        .enqueue_claimed_execution(
            vec![ConversationPart::text("do not resend")],
            claim.claim_id.clone(),
            claim.clone(),
            caller.clone(),
        )
        .unwrap();
    bounded(matching.reservation.join()).await.unwrap();
    caller.cancel();
    assert_eq!(
        bounded(matching.execution.join()).await.unwrap().status,
        ExecutionStatus::Canceled
    );
    assert_eq!(
        bounded(original.execution.join()).await.unwrap().status,
        ExecutionStatus::Canceled
    );
    assert_eq!(provider.stream_closes.load(Ordering::SeqCst), 1);
    assert_eq!(
        store
            .load()
            .await
            .unwrap()
            .unwrap()
            .execution
            .unwrap()
            .result_claim,
        Some(claim)
    );
    assert_eq!(provider.requests.lock().unwrap().len(), 1);
    runtime.close().await;
}

struct PausedParentDelivery {
    inner: Arc<RecoveryParent>,
    gate: Arc<Gate>,
    first: AtomicBool,
}
impl ParentCompletion for PausedParentDelivery {
    fn deliver<'a>(&'a self, execution: &'a Execution) -> BoxFuture<'a, Result<(), TurnError>> {
        self.inner.deliver(execution)
    }
    fn deliver_correlated<'a>(
        &'a self,
        execution: &'a Execution,
        id: &'a str,
    ) -> BoxFuture<'a, Result<(), TurnError>> {
        Box::pin(async move {
            if self.first.swap(false, Ordering::SeqCst) {
                self.gate.wait(&NeverCancelled).await.unwrap();
            }
            self.inner.deliver_correlated(execution, id).await
        })
    }
}

#[tokio::test]
async fn terminal_parent_delivery_does_not_resurrect_an_acknowledged_claim() {
    let fixture = Fixture::new();
    let store = fixture.recovery_store().await;
    let claim = TaskResultClaim {
        owner_session_id: "owner".into(),
        claim_id: "task-prompt".into(),
    };
    let parent = Arc::new(RecoveryParent::default());
    parent.fail.store(true, Ordering::SeqCst);
    let runtime = fixture
        .recovered_runtime(
            Scripted::new(vec![Step::complete("saved answer")]),
            store.clone(),
            parent.clone(),
        )
        .await;
    let task = runtime
        .session
        .enqueue_claimed_execution(
            vec![ConversationPart::text("task question")],
            claim.claim_id.clone(),
            claim.clone(),
            Arc::new(NeverCancelled),
        )
        .unwrap();
    assert_eq!(
        bounded(task.execution.join()).await.unwrap(),
        Execution::succeeded("saved answer")
    );
    let reference = runtime
        .session
        .capture_claimed_result(&claim.claim_id, &claim)
        .await
        .unwrap();
    runtime.close().await;
    parent.fail.store(false, Ordering::SeqCst);
    let gate = Gate::new();
    let paused = Arc::new(PausedParentDelivery {
        inner: parent,
        gate: gate.clone(),
        first: AtomicBool::new(true),
    });
    let provider = Scripted::new(vec![Step::complete("ordinary next answer")]);
    let resumed = fixture
        .recovered_runtime(provider.clone(), store.clone(), paused)
        .await;
    let replay = resumed
        .session
        .enqueue_claimed_execution(
            vec![ConversationPart::text("do not resend")],
            claim.claim_id.clone(),
            claim.clone(),
            Arc::new(NeverCancelled),
        )
        .unwrap();
    bounded(replay.reservation.join()).await.unwrap();
    gate.entered().await;
    resumed
        .session
        .acknowledge_claimed_result(&reference, &claim)
        .await
        .unwrap();
    assert!(
        store
            .load()
            .await
            .unwrap()
            .unwrap()
            .execution
            .unwrap()
            .result_claim
            .is_none()
    );
    gate.open();
    assert_eq!(
        bounded(replay.execution.join()).await.unwrap(),
        Execution::succeeded("saved answer")
    );
    let after = store.load().await.unwrap().unwrap().execution.unwrap();
    assert!(after.result_claim.is_none());
    assert!(after.parent_notification.unwrap().admitted);
    let next = resumed
        .session
        .enqueue_execution(
            vec![ConversationPart::text("ordinary question")],
            "ordinary-prompt".into(),
            None,
            None,
            Arc::new(NeverCancelled),
        )
        .unwrap();
    assert_eq!(
        bounded(next.join()).await.unwrap(),
        Execution::succeeded("ordinary next answer")
    );
    assert_eq!(provider.requests.lock().unwrap().len(), 1);
    resumed.close().await;
}

#[tokio::test]
async fn scheduled_work_rejected_by_held_claim_is_settled_and_not_run_after_ack() {
    let fixture = Fixture::new();
    let store = fixture.recovery_store().await;
    let claim = TaskResultClaim {
        owner_session_id: "owner".into(),
        claim_id: "task-prompt".into(),
    };
    let runtime = fixture
        .recovered_runtime(
            Scripted::new(vec![Step::complete("task answer")]),
            store,
            Arc::new(RecoveryParent::default()),
        )
        .await;
    let task = runtime
        .session
        .enqueue_claimed_execution(
            vec![ConversationPart::text("task question")],
            claim.claim_id.clone(),
            claim.clone(),
            Arc::new(NeverCancelled),
        )
        .unwrap();
    bounded(task.execution.join()).await.unwrap();
    let calls = Arc::new(AtomicUsize::new(0));
    let observed = calls.clone();
    let scheduled = runtime
        .session
        .schedule_work(
            move |_, _| {
                Box::pin(async move {
                    observed.fetch_add(1, Ordering::SeqCst);
                    Ok(())
                })
            },
            Arc::new(NeverCancelled),
        )
        .unwrap();
    assert!(
        bounded(scheduled.join())
            .await
            .unwrap_err()
            .to_string()
            .contains("claimed")
    );
    let reference = runtime
        .session
        .capture_claimed_result(&claim.claim_id, &claim)
        .await
        .unwrap();
    runtime
        .session
        .acknowledge_claimed_result(&reference, &claim)
        .await
        .unwrap();
    bounded(runtime.session.recover().join()).await.unwrap();
    assert_eq!(calls.load(Ordering::SeqCst), 0);
    runtime.close().await;
}

#[tokio::test]
async fn scheduled_work_preflight_load_error_settles_only_its_exact_request() {
    let fixture = Fixture::new();
    let store = fixture.recovery_store().await;
    let runtime = fixture
        .recovered_runtime(
            Scripted::default(),
            store,
            Arc::new(RecoveryParent::default()),
        )
        .await;
    let path = fixture.path.join("agent-state").join(AGENT_STATE_FILE_NAME);
    let bytes = std::fs::read(&path).unwrap();
    std::fs::write(&path, "not a snapshot").unwrap();
    let calls = Arc::new(AtomicUsize::new(0));
    let rejected = calls.clone();
    let scheduled = runtime
        .session
        .schedule_work(
            move |_, _| {
                Box::pin(async move {
                    rejected.fetch_add(100, Ordering::SeqCst);
                    Ok(())
                })
            },
            Arc::new(NeverCancelled),
        )
        .unwrap();
    assert!(bounded(scheduled.join()).await.is_err());
    std::fs::write(path, bytes).unwrap();
    let accepted = calls.clone();
    let scheduled = runtime
        .session
        .schedule_work(
            move |_, _| {
                Box::pin(async move {
                    accepted.fetch_add(1, Ordering::SeqCst);
                    Ok(())
                })
            },
            Arc::new(NeverCancelled),
        )
        .unwrap();
    bounded(scheduled.join()).await.unwrap();
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    runtime.close().await;
}
