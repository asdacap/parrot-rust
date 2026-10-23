use super::*;
use parrot_runtime::agent_turn::lifecycle::DrainBackend;

impl Fixture {
    async fn save_cancelable_execution(
        &self,
        store: &dyn AgentStateStore,
        phase: ExecutionPhase,
    ) -> ExecutionCheckpoint {
        let checkpoint = ExecutionCheckpoint {
            result_claim: None,
            execution_id: "saved-execution".into(),
            initial_message_id: "saved-message".into(),
            initial_parts: vec![ExecutionInputPart::Text {
                text: "saved question".into(),
            }],
            history_watermark: 0,
            phase,
            terminal: None,
            parent_notification: Some(NotificationCheckpoint {
                message_id: "saved-execution.parent".into(),
                admitted: false,
                delivery_error: None,
            }),
        };
        store
            .update(AgentStateUpdate::Execution(Some(checkpoint.clone())))
            .await
            .unwrap();
        checkpoint
    }
}

#[tokio::test]
async fn cold_cancellation_finishes_only_matching_saved_input_without_provider_work() {
    for phase in [
        ExecutionPhase::AdmissionIntent,
        ExecutionPhase::Active,
        ExecutionPhase::Suspended,
    ] {
        let fixture = Fixture::new();
        let store = fixture.recovery_store().await;
        let original = fixture
            .save_cancelable_execution(store.as_ref(), phase)
            .await;
        let provider = Scripted::default();
        let parent = Arc::new(RecoveryParent::default());
        let runtime = fixture
            .recovered_runtime(provider.clone(), store.clone(), parent.clone())
            .await;
        if phase != ExecutionPhase::AdmissionIntent {
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
        }
        let outcome = bounded(
            runtime
                .session
                .cancel_recovered_execution("saved-message")
                .unwrap()
                .join(),
        )
        .await
        .unwrap();
        assert_eq!(outcome.status, ExecutionStatus::Canceled);
        let saved = store.load().await.unwrap().unwrap().execution.unwrap();
        assert_eq!(saved.execution_id, original.execution_id);
        assert_eq!(saved.initial_message_id, original.initial_message_id);
        assert_eq!(saved.initial_parts, original.initial_parts);
        assert_eq!(saved.phase, ExecutionPhase::Canceled);
        assert_eq!(saved.terminal.unwrap().outcome, ExecutionOutcome::Canceled);
        assert!(saved.parent_notification.unwrap().admitted);
        assert!(provider.requests.lock().unwrap().is_empty());
        assert_eq!(parent.messages.lock().unwrap().len(), 1);
        assert!(
            !runtime
                .session
                .backend()
                .has_pending_inputs()
                .await
                .unwrap()
        );
        runtime.close().await;
    }
}

#[tokio::test]
async fn absent_cold_cancellation_is_idle_and_does_not_admit_the_task_prompt() {
    let fixture = Fixture::new();
    let store = fixture.recovery_store().await;
    let provider = Scripted::default();
    let runtime = fixture
        .recovered_runtime(
            provider.clone(),
            store.clone(),
            Arc::new(RecoveryParent::default()),
        )
        .await;
    assert_eq!(
        runtime
            .session
            .cancel_recovered_execution("never-admitted")
            .unwrap()
            .join()
            .await
            .unwrap(),
        Execution::succeeded("")
    );
    assert!(store.load().await.unwrap().unwrap().execution.is_none());
    assert!(runtime.session.wait(0, &NeverCancelled).await.is_err());
    assert!(fixture.conversation().await.messages().is_empty());
    assert!(provider.requests.lock().unwrap().is_empty());
    runtime.close().await;
}

#[tokio::test]
async fn cold_cancellation_preserves_matching_and_unrelated_terminal_results() {
    for expected in ["completed-message", "older-task-message"] {
        let fixture = Fixture::new();
        let store = fixture.recovery_store().await;
        let parent = Arc::new(RecoveryParent::default());
        let runtime = fixture
            .recovered_runtime(
                Scripted::new(vec![Step::complete("completed answer")]),
                store.clone(),
                parent.clone(),
            )
            .await;
        let outcome = runtime
            .session
            .enqueue_execution(
                vec![ConversationPart::text("completed question")],
                "completed-message".into(),
                None,
                None,
                Arc::new(NeverCancelled),
            )
            .unwrap();
        assert_eq!(
            bounded(outcome.join()).await.unwrap(),
            Execution::succeeded("completed answer")
        );
        let original = store.load().await.unwrap().unwrap().execution;
        runtime.close().await;
        let restarted = fixture.reopen().await;
        let provider = Scripted::default();
        let resumed = restarted
            .recovered_runtime(
                provider.clone(),
                restarted.recovery_store().await,
                parent.clone(),
            )
            .await;
        assert_eq!(
            bounded(
                resumed
                    .session
                    .cancel_recovered_execution(expected)
                    .unwrap()
                    .join()
            )
            .await
            .unwrap(),
            Execution::succeeded("completed answer")
        );
        assert_eq!(
            restarted
                .recovery_store()
                .await
                .load()
                .await
                .unwrap()
                .unwrap()
                .execution,
            original
        );
        assert_eq!(parent.messages.lock().unwrap().len(), 1);
        assert!(provider.requests.lock().unwrap().is_empty());
        resumed.close().await;
    }
}

#[tokio::test]
async fn cold_cancellation_rejects_another_unfinished_input_without_changing_it() {
    let fixture = Fixture::new();
    let store = fixture.recovery_store().await;
    let original = fixture
        .save_cancelable_execution(store.as_ref(), ExecutionPhase::Active)
        .await;
    let provider = Scripted::new(vec![Step::complete("original answer")]);
    let runtime = fixture
        .recovered_runtime(
            provider.clone(),
            store.clone(),
            Arc::new(RecoveryParent::default()),
        )
        .await;
    let denied = bounded(
        runtime
            .session
            .cancel_recovered_execution("different-input")
            .unwrap()
            .join(),
    )
    .await;
    assert!(denied.unwrap_err().to_string().contains("does not match"));
    assert_eq!(
        store.load().await.unwrap().unwrap().execution,
        Some(original)
    );
    assert!(provider.requests.lock().unwrap().is_empty());
    assert_eq!(
        bounded(runtime.session.recover_owned().unwrap().join())
            .await
            .unwrap(),
        Execution::succeeded("original answer")
    );
    assert_eq!(provider.requests.lock().unwrap().len(), 1);
    runtime.close().await;
}

#[tokio::test]
async fn cold_cancellation_reserves_startup_before_snapshot_io_and_blocks_competing_work() {
    let fixture = Fixture::new();
    let store = fixture.recovery_store().await;
    fixture
        .save_cancelable_execution(store.as_ref(), ExecutionPhase::Active)
        .await;
    let gate = Gate::new();
    let paused = Arc::new(PausedState {
        inner: store.clone(),
        gate: gate.clone(),
        first: AtomicBool::new(true),
    });
    let provider = Scripted::new(vec![Step::complete("later answer")]);
    let runtime = fixture
        .recovered_runtime(
            provider.clone(),
            paused,
            Arc::new(RecoveryParent::default()),
        )
        .await;
    runtime
        .session
        .record(
            vec![ConversationPart::text("saved question")],
            "saved-message".into(),
            protocol::Delivery::DELIVERY_STEER,
            Arc::new(NeverCancelled),
        )
        .await
        .unwrap();
    let canceled = runtime
        .session
        .cancel_recovered_execution("saved-message")
        .unwrap();
    gate.entered().await;
    assert!(
        runtime
            .session
            .cancel_recovered_execution("saved-message")
            .is_err()
    );
    runtime
        .session
        .record(
            vec![ConversationPart::text("later question")],
            "later-message".into(),
            protocol::Delivery::DELIVERY_STEER,
            Arc::new(NeverCancelled),
        )
        .await
        .unwrap();
    let competing = runtime.session.recover();
    assert!(!competing.is_finished());
    assert!(provider.requests.lock().unwrap().is_empty());
    gate.open();
    assert_eq!(
        bounded(canceled.join()).await.unwrap().status,
        ExecutionStatus::Canceled
    );
    bounded(competing.join()).await.unwrap();
    assert!(provider.requests.lock().unwrap().is_empty());
    assert!(
        runtime
            .session
            .backend()
            .has_pending_inputs()
            .await
            .unwrap()
    );
    assert_eq!(
        bounded(runtime.session.recover().join()).await.unwrap(),
        Execution::succeeded("later answer")
    );
    assert_eq!(provider.requests.lock().unwrap().len(), 1);
    runtime.close().await;
}

#[tokio::test]
async fn cold_cancellation_rejects_live_owned_or_raw_work_without_interrupting_it() {
    for owned in [false, true] {
        let fixture = Fixture::new();
        let store = fixture.recovery_store().await;
        let gate = Gate::new();
        let mut step = Step::complete("live answer");
        step.gate = Some(gate.clone());
        let runtime = fixture
            .recovered_runtime(
                Scripted::new(vec![step]),
                store.clone(),
                Arc::new(RecoveryParent::default()),
            )
            .await;
        let active = if owned {
            runtime
                .session
                .enqueue_execution(
                    vec![ConversationPart::text("live question")],
                    "live-message".into(),
                    None,
                    None,
                    Arc::new(NeverCancelled),
                )
                .unwrap()
        } else {
            runtime
                .session
                .record(
                    vec![ConversationPart::text("live question")],
                    "live-message".into(),
                    protocol::Delivery::DELIVERY_STEER,
                    Arc::new(NeverCancelled),
                )
                .await
                .unwrap();
            runtime.session.recover()
        };
        gate.entered().await;
        let original = store.load().await.unwrap().unwrap().execution;
        assert!(
            runtime
                .session
                .cancel_recovered_execution("live-message")
                .is_err()
        );
        assert_eq!(store.load().await.unwrap().unwrap().execution, original);
        gate.open();
        assert_eq!(
            bounded(active.join()).await.unwrap(),
            Execution::succeeded("live answer")
        );
        runtime.close().await;
    }
}

#[tokio::test]
async fn failed_cold_cancellation_write_never_activates_saved_work() {
    let fixture = Fixture::new();
    let store = fixture.recovery_store().await;
    let original = fixture
        .save_cancelable_execution(store.as_ref(), ExecutionPhase::Active)
        .await;
    let provider = Scripted::default();
    let runtime = fixture
        .recovered_runtime(
            provider.clone(),
            store.clone(),
            Arc::new(RecoveryParent::default()),
        )
        .await;
    let obstacle = fixture
        .path
        .join("agent-state")
        .join(AGENT_STATE_STAGING_FILE_NAME);
    std::fs::create_dir(&obstacle).unwrap();
    assert!(
        bounded(
            runtime
                .session
                .cancel_recovered_execution("saved-message")
                .unwrap()
                .join()
        )
        .await
        .is_err()
    );
    assert_eq!(
        store.load().await.unwrap().unwrap().execution,
        Some(original)
    );
    assert!(provider.requests.lock().unwrap().is_empty());
    std::fs::remove_dir(obstacle).unwrap();
    runtime.close().await;
}
