use super::*;

#[tokio::test]
async fn claimed_terminal_survives_reopen_until_ack_and_saved_reference_outlives_successor() {
    let fixture = Fixture::new();
    let store = fixture.recovery_store().await;
    let claim = TaskResultClaim {
        owner_session_id: "owner".into(),
        claim_id: "task-prompt".into(),
    };
    let parent = Arc::new(RecoveryParent::default());
    let runtime = fixture
        .recovered_runtime(
            Scripted::new(vec![Step::complete("task answer")]),
            store.clone(),
            parent.clone(),
        )
        .await;
    let execution = runtime
        .session
        .enqueue_claimed_execution(
            vec![ConversationPart::text("task question")],
            "task-prompt".into(),
            claim.clone(),
            Arc::new(NeverCancelled),
        )
        .unwrap();
    assert_eq!(
        bounded(execution.join()).await.unwrap(),
        Execution::succeeded("task answer")
    );
    let reference = runtime
        .session
        .capture_claimed_result("task-prompt", &claim)
        .await
        .unwrap();
    assert_eq!(
        store
            .load()
            .await
            .unwrap()
            .unwrap()
            .execution
            .unwrap()
            .result_claim,
        Some(claim.clone())
    );
    runtime.close().await;
    let restarted = fixture.reopen().await;
    let provider = Scripted::new(vec![Step::complete("next answer")]);
    let resumed = restarted
        .recovered_runtime(provider.clone(), restarted.recovery_store().await, parent)
        .await;
    assert!(
        bounded(
            resumed
                .session
                .enqueue_execution(
                    vec![ConversationPart::text("ordinary overwrite")],
                    "ordinary".into(),
                    None,
                    None,
                    Arc::new(NeverCancelled)
                )
                .unwrap()
                .join()
        )
        .await
        .is_err()
    );
    assert!(bounded(resumed.session.recover().join()).await.is_err());
    let foreign = TaskResultClaim {
        owner_session_id: "other-owner".into(),
        claim_id: "foreign-prompt".into(),
    };
    assert!(
        bounded(
            resumed
                .session
                .enqueue_claimed_execution(
                    vec![ConversationPart::text("foreign overwrite")],
                    "foreign-prompt".into(),
                    foreign,
                    Arc::new(NeverCancelled)
                )
                .unwrap()
                .join()
        )
        .await
        .is_err()
    );
    assert_eq!(
        bounded(
            resumed
                .session
                .enqueue_claimed_execution(
                    vec![ConversationPart::text("must not be admitted again")],
                    "task-prompt".into(),
                    claim.clone(),
                    Arc::new(NeverCancelled)
                )
                .unwrap()
                .join()
        )
        .await
        .unwrap(),
        Execution::succeeded("task answer")
    );
    assert!(provider.requests.lock().unwrap().is_empty());
    assert_eq!(
        resumed
            .session
            .capture_claimed_result("task-prompt", &claim)
            .await
            .unwrap(),
        reference
    );
    resumed
        .session
        .acknowledge_claimed_result(&reference, &claim)
        .await
        .unwrap();
    resumed
        .session
        .acknowledge_claimed_result(&reference, &claim)
        .await
        .unwrap();
    let next = TaskResultClaim {
        owner_session_id: "owner".into(),
        claim_id: "next-prompt".into(),
    };
    assert_eq!(
        bounded(
            resumed
                .session
                .enqueue_claimed_execution(
                    vec![ConversationPart::text("next question")],
                    "next-prompt".into(),
                    next.clone(),
                    Arc::new(NeverCancelled)
                )
                .unwrap()
                .join()
        )
        .await
        .unwrap(),
        Execution::succeeded("next answer")
    );
    resumed
        .session
        .acknowledge_claimed_result(&reference, &claim)
        .await
        .unwrap();
    let latest = restarted
        .recovery_store()
        .await
        .load()
        .await
        .unwrap()
        .unwrap()
        .execution
        .unwrap();
    assert_eq!(latest.result_claim, Some(next.clone()));
    assert_eq!(
        resumed
            .session
            .restore_saved_response(&reference)
            .await
            .unwrap(),
        Execution::succeeded("task answer")
    );
    let current = resumed
        .session
        .capture_claimed_result("next-prompt", &next)
        .await
        .unwrap();
    let wrong = TaskResultClaim {
        owner_session_id: "intruder".into(),
        claim_id: "next-prompt".into(),
    };
    assert!(
        resumed
            .session
            .acknowledge_claimed_result(&current, &wrong)
            .await
            .is_err()
    );
    let mut changed = current.clone();
    changed.terminal.history_sequence += 1;
    assert!(
        resumed
            .session
            .acknowledge_claimed_result(&changed, &next)
            .await
            .is_err()
    );
    let mut foreign_reference = reference;
    foreign_reference.worker_session_id = "another-worker".into();
    assert!(
        resumed
            .session
            .restore_saved_response(&foreign_reference)
            .await
            .is_err()
    );
    assert_eq!(provider.requests.lock().unwrap().len(), 1);
    resumed.close().await;
}

#[tokio::test]
async fn cold_claimed_execution_is_not_blocked_by_foreign_waiters_or_raw_successor_slots() {
    let fixture = Fixture::new();
    let store = fixture.recovery_store().await;
    let claim = TaskResultClaim {
        owner_session_id: "owner".into(),
        claim_id: "original-prompt".into(),
    };
    store
        .update(AgentStateUpdate::Execution(Some(ExecutionCheckpoint {
            execution_id: "claimed-execution".into(),
            initial_message_id: "original-prompt".into(),
            initial_parts: vec![ExecutionInputPart::Text {
                text: "original task question".into(),
            }],
            history_watermark: 0,
            phase: ExecutionPhase::Active,
            terminal: None,
            parent_notification: None,
            result_claim: Some(claim.clone()),
        })))
        .await
        .unwrap();
    let provider = Scripted::new(vec![Step::complete("resumed task answer")]);
    let runtime = fixture
        .recovered_runtime(
            provider.clone(),
            store.clone(),
            Arc::new(RecoveryParent::default()),
        )
        .await;
    let foreign = TaskResultClaim {
        owner_session_id: "intruder".into(),
        claim_id: "other-prompt".into(),
    };
    let foreign_waiter = runtime
        .session
        .enqueue_claimed_execution(
            vec![ConversationPart::text("foreign prompt")],
            "other-prompt".into(),
            foreign,
            Arc::new(NeverCancelled),
        )
        .unwrap();
    let ordinary_waiter = runtime
        .session
        .enqueue_execution(
            vec![ConversationPart::text("ordinary prompt")],
            "ordinary".into(),
            None,
            None,
            Arc::new(NeverCancelled),
        )
        .unwrap();
    assert!(bounded(foreign_waiter.join()).await.is_err());
    assert!(bounded(ordinary_waiter.join()).await.is_err());
    assert!(bounded(runtime.session.recover().join()).await.is_err());
    assert_eq!(
        bounded(
            runtime
                .session
                .enqueue_claimed_execution(
                    vec![ConversationPart::text("not another admission")],
                    "original-prompt".into(),
                    claim.clone(),
                    Arc::new(NeverCancelled)
                )
                .unwrap()
                .join()
        )
        .await
        .unwrap(),
        Execution::succeeded("resumed task answer")
    );
    assert_eq!(provider.requests.lock().unwrap().len(), 1);
    let conversation = fixture.conversation().await;
    assert_eq!(
        conversation
            .messages()
            .iter()
            .filter(|message| message.role == MessageRole::User)
            .count(),
        1
    );
    assert!(fixture.jsonl().contains("original task question"));
    assert!(!fixture.jsonl().contains("not another admission"));
    assert_eq!(
        store
            .load()
            .await
            .unwrap()
            .unwrap()
            .execution
            .unwrap()
            .execution_id,
        "claimed-execution"
    );
    runtime.close().await;
}

#[tokio::test]
async fn failed_claim_ack_preserves_terminal_until_exact_owner_can_retry() {
    let fixture = Fixture::new();
    let store = fixture.recovery_store().await;
    let claim = TaskResultClaim {
        owner_session_id: "owner".into(),
        claim_id: "task-prompt".into(),
    };
    let runtime = fixture
        .recovered_runtime(
            Scripted::new(vec![Step::complete("task answer")]),
            store.clone(),
            Arc::new(RecoveryParent::default()),
        )
        .await;
    bounded(
        runtime
            .session
            .enqueue_claimed_execution(
                vec![ConversationPart::text("task question")],
                "task-prompt".into(),
                claim.clone(),
                Arc::new(NeverCancelled),
            )
            .unwrap()
            .join(),
    )
    .await
    .unwrap();
    let reference = runtime
        .session
        .capture_claimed_result("task-prompt", &claim)
        .await
        .unwrap();
    let before = store.load().await.unwrap().unwrap().execution;
    let obstacle = fixture
        .path
        .join("agent-state")
        .join(AGENT_STATE_STAGING_FILE_NAME);
    std::fs::create_dir(&obstacle).unwrap();
    assert!(
        runtime
            .session
            .acknowledge_claimed_result(&reference, &claim)
            .await
            .is_err()
    );
    assert_eq!(store.load().await.unwrap().unwrap().execution, before);
    std::fs::remove_dir(obstacle).unwrap();
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
    assert_eq!(
        runtime
            .session
            .restore_saved_response(&reference)
            .await
            .unwrap(),
        Execution::succeeded("task answer")
    );
    runtime.close().await;
}

#[tokio::test]
async fn cold_cancel_preserves_claim_until_canceled_result_is_acknowledged() {
    let fixture = Fixture::new();
    let store = fixture.recovery_store().await;
    let claim = TaskResultClaim {
        owner_session_id: "owner".into(),
        claim_id: "canceled-prompt".into(),
    };
    store
        .update(AgentStateUpdate::Execution(Some(ExecutionCheckpoint {
            execution_id: "canceled-execution".into(),
            initial_message_id: "canceled-prompt".into(),
            initial_parts: vec![ExecutionInputPart::Text {
                text: "must never execute".into(),
            }],
            history_watermark: 0,
            phase: ExecutionPhase::Active,
            terminal: None,
            parent_notification: None,
            result_claim: Some(claim.clone()),
        })))
        .await
        .unwrap();
    let provider = Scripted::default();
    let runtime = fixture
        .recovered_runtime(
            provider.clone(),
            store.clone(),
            Arc::new(RecoveryParent::default()),
        )
        .await;
    assert_eq!(
        bounded(
            runtime
                .session
                .cancel_recovered_execution("canceled-prompt")
                .unwrap()
                .join()
        )
        .await
        .unwrap()
        .status,
        ExecutionStatus::Canceled
    );
    let reference = runtime
        .session
        .capture_claimed_result("canceled-prompt", &claim)
        .await
        .unwrap();
    assert_eq!(reference.terminal.history_sequence, 0);
    assert_eq!(
        runtime
            .session
            .restore_saved_response(&reference)
            .await
            .unwrap()
            .status,
        ExecutionStatus::Canceled
    );
    assert_eq!(
        store
            .load()
            .await
            .unwrap()
            .unwrap()
            .execution
            .unwrap()
            .result_claim,
        Some(claim.clone())
    );
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
    assert!(provider.requests.lock().unwrap().is_empty());
    runtime.close().await;
}

#[tokio::test]
async fn claimed_readiness_precedes_provider_completion_and_matching_owner_reuses_it() {
    let fixture = Fixture::new();
    let store = fixture.recovery_store().await;
    let gate = Gate::new();
    let mut step = Step::complete("claimed answer");
    step.gate = Some(gate.clone());
    let provider = Scripted::new(vec![step]);
    let runtime = fixture
        .recovered_runtime(provider.clone(), store, Arc::new(RecoveryParent::default()))
        .await;
    let claim = TaskResultClaim {
        owner_session_id: "owner".into(),
        claim_id: "claimed-prompt".into(),
    };
    let first = runtime
        .session
        .enqueue_claimed_execution(
            vec![ConversationPart::text("claimed question")],
            "claimed-prompt".into(),
            claim.clone(),
            Arc::new(NeverCancelled),
        )
        .unwrap();
    bounded(first.reservation.join()).await.unwrap();
    gate.entered().await;
    assert!(!first.execution.is_finished());
    let matching = runtime
        .session
        .enqueue_claimed_execution(
            vec![ConversationPart::text("must not resend")],
            "claimed-prompt".into(),
            claim,
            Arc::new(NeverCancelled),
        )
        .unwrap();
    bounded(matching.reservation.join()).await.unwrap();
    assert!(!matching.execution.is_finished());
    let foreign = runtime
        .session
        .enqueue_claimed_execution(
            vec![ConversationPart::text("foreign question")],
            "foreign-prompt".into(),
            TaskResultClaim {
                owner_session_id: "other".into(),
                claim_id: "foreign-prompt".into(),
            },
            Arc::new(NeverCancelled),
        )
        .unwrap();
    assert!(bounded(foreign.reservation.join()).await.is_err());
    assert!(bounded(foreign.execution.join()).await.is_err());
    gate.open();
    assert_eq!(
        bounded(first.execution.join()).await.unwrap(),
        Execution::succeeded("claimed answer")
    );
    assert_eq!(
        bounded(matching.execution.join()).await.unwrap(),
        Execution::succeeded("claimed answer")
    );
    assert_eq!(provider.requests.lock().unwrap().len(), 1);
    runtime.close().await;
}

#[tokio::test]
async fn suspended_claim_preflight_settles_readiness_without_reserving_a_drain() {
    let fixture = Fixture::new();
    let store = fixture.recovery_store().await;
    let gate = Gate::new();
    let paused = Arc::new(PausedState {
        inner: store.clone(),
        gate: gate.clone(),
        first: AtomicBool::new(true),
    });
    let provider = Scripted::default();
    let runtime = fixture
        .recovered_runtime(
            provider.clone(),
            paused,
            Arc::new(RecoveryParent::default()),
        )
        .await;
    let claimed = runtime
        .session
        .enqueue_claimed_execution(
            vec![ConversationPart::text("not yet admitted")],
            "claimed-prompt".into(),
            TaskResultClaim {
                owner_session_id: "owner".into(),
                claim_id: "claimed-prompt".into(),
            },
            Arc::new(NeverCancelled),
        )
        .unwrap();
    gate.entered().await;
    assert!(!claimed.reservation.is_finished());
    let stopping = runtime.session.suspend_handle();
    gate.open();
    assert!(bounded(claimed.reservation.join()).await.is_err());
    assert!(bounded(claimed.execution.join()).await.is_err());
    bounded(stopping.join()).await.unwrap();
    assert!(store.load().await.unwrap().unwrap().execution.is_none());
    assert!(provider.requests.lock().unwrap().is_empty());
    runtime.close().await;
}
