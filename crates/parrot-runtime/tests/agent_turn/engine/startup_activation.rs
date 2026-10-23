use super::*;
use parrot_runtime::agent_turn::lifecycle::{DrainBackend, DrainState, StartupActivation};

impl Fixture {
    async fn held_runtime(
        &self,
        provider: Scripted,
        store: Arc<dyn AgentStateStore>,
        activation: Arc<StartupActivation>,
        lifetime: Arc<dyn Cancellation>,
    ) -> Runtime {
        self.runtime_with_overrides(
            provider,
            Arc::new(Tools::default()),
            vec![],
            lifetime,
            0,
            RuntimeOverrides {
                state: Some(store),
                activation: Some(activation),
                parent_completion: Some(Arc::new(RecoveryParent::default())),
                ..RuntimeOverrides::default()
            },
        )
        .await
    }

    async fn save_startup_intent(&self, store: &dyn AgentStateStore) -> ExecutionCheckpoint {
        let checkpoint = ExecutionCheckpoint {
            execution_id: "startup-execution".into(),
            initial_message_id: "startup-question".into(),
            initial_parts: vec![ExecutionInputPart::Text {
                text: "original question".into(),
            }],
            history_watermark: 0,
            phase: ExecutionPhase::Active,
            terminal: None,
            parent_notification: None,
            result_claim: None,
        };
        store
            .update(AgentStateUpdate::Execution(Some(checkpoint.clone())))
            .await
            .unwrap();
        checkpoint
    }
}

#[tokio::test]
async fn raw_startup_wake_stays_outside_owned_slot_and_provider_waits_for_activation() {
    let fixture = Fixture::new();
    let store = fixture.recovery_store().await;
    let original = fixture.save_startup_intent(store.as_ref()).await;
    let activation = Arc::new(StartupActivation::new());
    let provider = Scripted::new(vec![Step::complete("restored answer")]);
    let runtime = fixture
        .held_runtime(
            provider.clone(),
            store.clone(),
            activation.clone(),
            Arc::new(NeverCancelled),
        )
        .await;
    let raw = runtime.session.recover();
    assert!(runtime.session.is_active());
    assert!(!runtime.session.is_idle());
    assert_eq!(runtime.session.state(), DrainState::Idle);
    let owned = runtime.session.recover_owned_reserved().unwrap();
    bounded(owned.reservation.join()).await.unwrap();
    assert!(!owned.execution.is_finished());
    assert!(provider.requests.lock().unwrap().is_empty());
    assert_eq!(
        store
            .load()
            .await
            .unwrap()
            .unwrap()
            .execution
            .unwrap()
            .execution_id,
        original.execution_id
    );
    activation.activate();
    assert_eq!(
        bounded(owned.execution.join()).await.unwrap(),
        Execution::succeeded("restored answer")
    );
    bounded(raw.join()).await.unwrap();
    assert_eq!(provider.requests.lock().unwrap().len(), 1);
    assert!(!runtime.session.is_active());
    runtime.close().await;
}

#[tokio::test]
async fn durable_raw_admission_returns_while_closed_and_remains_visibly_pending() {
    let fixture = Fixture::new();
    let store = fixture.recovery_store().await;
    let activation = Arc::new(StartupActivation::new());
    let provider = Scripted::new(vec![Step::complete("raw answer")]);
    let runtime = fixture
        .held_runtime(
            provider.clone(),
            store,
            activation.clone(),
            Arc::new(NeverCancelled),
        )
        .await;
    let sent = bounded(runtime.session.send(
        vec![ConversationPart::text("raw question")],
        "raw-message".into(),
        protocol::Delivery::DELIVERY_QUEUE,
        IncomingActivity::default(),
        Arc::new(NeverCancelled),
    ))
    .await
    .unwrap();
    assert!(sent.admission.created());
    assert!(runtime.session.is_active());
    assert_eq!(runtime.session.state(), DrainState::Idle);
    assert!(provider.requests.lock().unwrap().is_empty());
    let absent = runtime.session.recover_owned_reserved().unwrap();
    bounded(absent.reservation.join()).await.unwrap();
    bounded(absent.execution.join()).await.unwrap();
    assert!(runtime.session.has_recovery_work().await.unwrap());
    activation.activate();
    assert_eq!(
        bounded(sent.execution.join()).await.unwrap(),
        Execution::succeeded("raw answer")
    );
    assert!(!runtime.session.is_active());
    assert!(!runtime.session.has_recovery_work().await.unwrap());
    runtime.close().await;
}

#[tokio::test]
async fn held_owned_and_raw_work_settle_on_lifetime_cancellation_without_durable_terminal() {
    let fixture = Fixture::new();
    let store = fixture.recovery_store().await;
    let original = fixture.save_startup_intent(store.as_ref()).await;
    let provider = Scripted::default();
    let lifetime = Arc::new(Signal::new());
    let runtime = fixture
        .held_runtime(
            provider.clone(),
            store.clone(),
            Arc::new(StartupActivation::new()),
            lifetime.clone(),
        )
        .await;
    let raw = runtime.session.recover();
    let owned = runtime.session.recover_owned_reserved().unwrap();
    bounded(owned.reservation.join()).await.unwrap();
    lifetime.cancel();
    assert_eq!(
        bounded(owned.execution.join()).await.unwrap().status,
        ExecutionStatus::Canceled
    );
    bounded(raw.join()).await.unwrap();
    let saved = store.load().await.unwrap().unwrap().execution.unwrap();
    assert_eq!(saved.execution_id, original.execution_id);
    assert!(saved.terminal.is_none());
    assert!(provider.requests.lock().unwrap().is_empty());
    assert!(
        !fixture
            .events()
            .await
            .iter()
            .any(|event| event.has_turn_ended())
    );
    runtime.close().await;
}

#[tokio::test]
async fn held_owned_and_raw_work_suspend_without_activation() {
    let fixture = Fixture::new();
    let store = fixture.recovery_store().await;
    fixture.save_startup_intent(store.as_ref()).await;
    let provider = Scripted::default();
    let runtime = fixture
        .held_runtime(
            provider.clone(),
            store.clone(),
            Arc::new(StartupActivation::new()),
            Arc::new(NeverCancelled),
        )
        .await;
    let raw = runtime.session.recover();
    let owned = runtime.session.recover_owned_reserved().unwrap();
    bounded(owned.reservation.join()).await.unwrap();
    bounded(runtime.session.suspend()).await.unwrap();
    bounded(raw.join()).await.unwrap();
    assert_eq!(
        bounded(owned.execution.join()).await.unwrap().status,
        ExecutionStatus::Canceled
    );
    let saved = store.load().await.unwrap().unwrap().execution.unwrap();
    assert_eq!(saved.phase, ExecutionPhase::Suspended);
    assert!(saved.terminal.is_none());
    assert!(provider.requests.lock().unwrap().is_empty());
}

#[tokio::test]
async fn consumed_queue_retry_restores_exact_result_without_new_intent_or_terminal() {
    let fixture = Fixture::new();
    let store = fixture.recovery_store().await;
    let provider = Scripted::new(vec![Step::complete("original result")]);
    let runtime = fixture
        .recovered_runtime(
            provider.clone(),
            store.clone(),
            Arc::new(RecoveryParent::default()),
        )
        .await;
    let execute = || {
        runtime
            .session
            .enqueue_execution(
                vec![ConversationPart::text("same question")],
                "queue-retry".into(),
                None,
                None,
                Arc::new(NeverCancelled),
            )
            .unwrap()
    };
    assert_eq!(
        bounded(execute().join()).await.unwrap(),
        Execution::succeeded("original result")
    );
    let saved = store.load().await.unwrap().unwrap().execution;
    let terminal_count = fixture
        .events()
        .await
        .iter()
        .filter(|event| event.has_turn_ended())
        .count();
    assert_eq!(
        bounded(execute().join()).await.unwrap(),
        Execution::succeeded("original result")
    );
    assert_eq!(store.load().await.unwrap().unwrap().execution, saved);
    assert_eq!(
        fixture
            .events()
            .await
            .iter()
            .filter(|event| event.has_turn_ended())
            .count(),
        terminal_count
    );
    assert_eq!(provider.requests.lock().unwrap().len(), 1);
    assert!(!runtime.session.is_active());
    runtime.close().await;
}

#[tokio::test]
async fn older_duplicate_joins_newer_execution_without_replacing_its_checkpoint() {
    let fixture = Fixture::new();
    let store = fixture.recovery_store().await;
    let gate = Gate::new();
    let mut newer = Step::complete("newer result");
    newer.gate = Some(gate.clone());
    let provider = Scripted::new(vec![Step::complete("old result"), newer]);
    let runtime = fixture
        .recovered_runtime(
            provider.clone(),
            store.clone(),
            Arc::new(RecoveryParent::default()),
        )
        .await;
    let enqueue = |message: &str| {
        runtime
            .session
            .enqueue_execution(
                vec![ConversationPart::text(message)],
                message.into(),
                None,
                None,
                Arc::new(NeverCancelled),
            )
            .unwrap()
    };
    bounded(enqueue("old").join()).await.unwrap();
    let newer = enqueue("new");
    gate.entered().await;
    let saved = store.load().await.unwrap().unwrap().execution.unwrap();
    let duplicate = enqueue("old");
    gate.open();
    assert_eq!(
        bounded(newer.join()).await.unwrap(),
        Execution::succeeded("newer result")
    );
    bounded(duplicate.join()).await.unwrap();
    let completed = store.load().await.unwrap().unwrap().execution.unwrap();
    assert_eq!(completed.execution_id, saved.execution_id);
    assert_eq!(completed.initial_message_id, "new");
    assert!(completed.terminal.is_some());
    assert_eq!(provider.requests.lock().unwrap().len(), 2);
    assert_eq!(
        fixture
            .events()
            .await
            .iter()
            .filter(|event| event.has_turn_ended())
            .count(),
        2
    );
    runtime.close().await;
}

#[tokio::test]
async fn raw_preflight_is_immediately_active_after_each_completed_send() {
    let fixture = Fixture::new();
    let store = fixture.recovery_store().await;
    let runtime = fixture
        .recovered_runtime(
            Scripted::new(vec![Step::complete("first"), Step::complete("second")]),
            store,
            Arc::new(RecoveryParent::default()),
        )
        .await;
    for message in ["one", "two"] {
        let sent = runtime
            .send(message, protocol::Delivery::DELIVERY_STEER)
            .await;
        assert!(runtime.session.is_active());
        assert!(!runtime.session.is_idle());
        bounded(sent.execution.join()).await.unwrap();
        assert!(!runtime.session.is_active());
    }
    assert_eq!(
        fixture
            .events()
            .await
            .iter()
            .filter(|event| event.has_turn_ended())
            .count(),
        2
    );
    runtime.close().await;
}

#[tokio::test]
async fn recovery_work_detects_consumed_unanswered_conversation_without_pending_rows() {
    let fixture = Fixture::new();
    fixture
        .history
        .admit_parts(
            "legacy-user".into(),
            vec![ConversationPart::text("unfinished")],
            protocol::Delivery::DELIVERY_STEER,
            |_| {
                Ok(Event {
                    id: id(),
                    agent_session_id: "agent".into(),
                    ..Default::default()
                })
            },
        )
        .await
        .unwrap();
    fixture
        .history
        .promote_steers(|_| {
            Ok(Event {
                id: id(),
                agent_session_id: "agent".into(),
                ..Default::default()
            })
        })
        .await
        .unwrap();
    let runtime = fixture
        .recovered_runtime(
            Scripted::default(),
            fixture.recovery_store().await,
            Arc::new(RecoveryParent::default()),
        )
        .await;
    assert!(
        !runtime
            .session
            .backend()
            .has_pending_inputs()
            .await
            .unwrap()
    );
    assert!(runtime.session.has_recovery_work().await.unwrap());
    runtime.close().await;
}

#[tokio::test]
async fn matching_terminal_ack_defers_pending_input_until_activation_and_preserves_task_result() {
    let fixture = Fixture::new();
    let store = fixture.recovery_store().await;
    let claim = TaskResultClaim {
        owner_session_id: "owner".into(),
        claim_id: "task-prompt".into(),
    };
    let original = fixture
        .recovered_runtime(
            Scripted::new(vec![Step::complete("task answer")]),
            store.clone(),
            Arc::new(RecoveryParent::default()),
        )
        .await;
    let task = original
        .session
        .enqueue_claimed_execution(
            vec![ConversationPart::text("task question")],
            claim.claim_id.clone(),
            claim.clone(),
            Arc::new(NeverCancelled),
        )
        .unwrap();
    bounded(task.execution.join()).await.unwrap();
    let reference = original
        .session
        .capture_claimed_result(&claim.claim_id, &claim)
        .await
        .unwrap();
    original
        .session
        .record(
            vec![ConversationPart::text("queued question")],
            "queued-question".into(),
            protocol::Delivery::DELIVERY_QUEUE,
            Arc::new(NeverCancelled),
        )
        .await
        .unwrap();
    original.close().await;

    let activation = Arc::new(StartupActivation::new());
    let provider = Scripted::new(vec![Step::complete("queued answer")]);
    let resumed = fixture
        .held_runtime(
            provider.clone(),
            store.clone(),
            activation.clone(),
            Arc::new(NeverCancelled),
        )
        .await;
    let recovered = resumed
        .session
        .enqueue_claimed_execution(
            vec![ConversationPart::text("do not resend")],
            claim.claim_id.clone(),
            claim.clone(),
            Arc::new(NeverCancelled),
        )
        .unwrap();
    bounded(recovered.reservation.join()).await.unwrap();
    bounded(recovered.execution.join()).await.unwrap();
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
    assert!(resumed.session.is_active());
    assert!(provider.requests.lock().unwrap().is_empty());
    activation.activate();
    bounded(async {
        while resumed.session.is_active() {
            tokio::task::yield_now().await;
        }
    })
    .await;
    assert_eq!(provider.requests.lock().unwrap().len(), 1);
    assert_eq!(
        resumed
            .session
            .wait(0, &NeverCancelled)
            .await
            .unwrap()
            .execution,
        Some(Execution::succeeded("task answer"))
    );
    let after = store.load().await.unwrap().unwrap().execution.unwrap();
    assert_eq!(after.execution_id, reference.execution_id);
    assert_eq!(after.terminal, Some(reference.terminal.clone()));
    assert!(after.result_claim.is_none());
    resumed
        .session
        .acknowledge_claimed_result(&reference, &claim)
        .await
        .unwrap();
    assert!(!resumed.session.is_active());
    assert_eq!(provider.requests.lock().unwrap().len(), 1);
    resumed.close().await;
}

#[tokio::test]
async fn older_terminal_ack_does_not_wake_pending_input_under_a_newer_execution() {
    let fixture = Fixture::new();
    let store = fixture.recovery_store().await;
    let claim = TaskResultClaim {
        owner_session_id: "owner".into(),
        claim_id: "old-task".into(),
    };
    let original = fixture
        .recovered_runtime(
            Scripted::new(vec![
                Step::complete("old task answer"),
                Step::complete("new result"),
            ]),
            store.clone(),
            Arc::new(RecoveryParent::default()),
        )
        .await;
    let task = original
        .session
        .enqueue_claimed_execution(
            vec![ConversationPart::text("old question")],
            claim.claim_id.clone(),
            claim.clone(),
            Arc::new(NeverCancelled),
        )
        .unwrap();
    bounded(task.execution.join()).await.unwrap();
    let reference = original
        .session
        .capture_claimed_result(&claim.claim_id, &claim)
        .await
        .unwrap();
    original
        .session
        .acknowledge_claimed_result(&reference, &claim)
        .await
        .unwrap();
    let newer = original
        .session
        .enqueue_execution(
            vec![ConversationPart::text("new question")],
            "new-question".into(),
            None,
            None,
            Arc::new(NeverCancelled),
        )
        .unwrap();
    bounded(newer.join()).await.unwrap();
    original
        .session
        .record(
            vec![ConversationPart::text("pending question")],
            "pending-question".into(),
            protocol::Delivery::DELIVERY_QUEUE,
            Arc::new(NeverCancelled),
        )
        .await
        .unwrap();
    original.close().await;
    let provider = Scripted::default();
    let resumed = fixture
        .held_runtime(
            provider.clone(),
            store.clone(),
            Arc::new(StartupActivation::new()),
            Arc::new(NeverCancelled),
        )
        .await;
    let saved = store.load().await.unwrap().unwrap().execution;
    resumed
        .session
        .acknowledge_claimed_result(&reference, &claim)
        .await
        .unwrap();
    assert!(!resumed.session.is_active());
    assert_eq!(resumed.session.state(), DrainState::Idle);
    assert!(resumed.session.has_recovery_work().await.unwrap());
    assert_eq!(store.load().await.unwrap().unwrap().execution, saved);
    assert!(provider.requests.lock().unwrap().is_empty());
    resumed.close().await;
}

#[tokio::test]
async fn cold_duplicate_validates_saved_parts_before_admission_and_resumes_same_intent() {
    let fixture = Fixture::new();
    let store = fixture.recovery_store().await;
    let original = fixture.save_startup_intent(store.as_ref()).await;
    let activation = Arc::new(StartupActivation::new());
    let provider = Scripted::new(vec![Step::complete("recovered answer")]);
    let runtime = fixture
        .held_runtime(
            provider.clone(),
            store.clone(),
            activation.clone(),
            Arc::new(NeverCancelled),
        )
        .await;
    let altered = runtime
        .session
        .enqueue_execution(
            vec![ConversationPart::text("altered content")],
            original.initial_message_id.clone(),
            None,
            None,
            Arc::new(NeverCancelled),
        )
        .unwrap();
    assert!(
        bounded(altered.join())
            .await
            .unwrap_err()
            .to_string()
            .contains("different content")
    );
    assert_eq!(
        store.load().await.unwrap().unwrap().execution,
        Some(original.clone())
    );
    assert!(fixture.conversation().await.messages().is_empty());
    assert!(!runtime.session.is_active());
    let matching = bounded(
        runtime
            .session
            .send_and_select_execution(
                vec![ConversationPart::text("original question")],
                original.initial_message_id.clone(),
                Arc::new(NeverCancelled),
            )
            .unwrap()
            .join(),
    )
    .await
    .unwrap();
    assert!(matching.admission.created());
    assert!(provider.requests.lock().unwrap().is_empty());
    activation.activate();
    assert_eq!(
        bounded(matching.execution.join()).await.unwrap(),
        Execution::succeeded("recovered answer")
    );
    assert_eq!(
        store
            .load()
            .await
            .unwrap()
            .unwrap()
            .execution
            .unwrap()
            .execution_id,
        original.execution_id
    );
    assert_eq!(provider.requests.lock().unwrap().len(), 1);
    runtime.close().await;
}
