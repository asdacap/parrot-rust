use super::*;
use parrot_runtime::agent_turn::lifecycle::DrainBackend;
use parrot_storage::conversation::{HistoryForkBoundary, HistoryForkSelection};

impl Fixture {
    async fn admit_consumed_input(&self, message_id: &str, text: &str) {
        self.history
            .admit_parts(
                message_id.into(),
                vec![ConversationPart::text(text)],
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
        self.history
            .promote_steers(|_| {
                Ok(Event {
                    id: id(),
                    agent_session_id: "agent".into(),
                    ..Default::default()
                })
            })
            .await
            .unwrap();
    }

    async fn save_consumed_intent(&self, store: &dyn AgentStateStore) {
        self.admit_consumed_input("consumed-input", "consumed question")
            .await;
        store
            .update(AgentStateUpdate::Execution(Some(ExecutionCheckpoint {
                execution_id: "consumed-execution".into(),
                initial_message_id: "consumed-input".into(),
                initial_parts: vec![ExecutionInputPart::Text {
                    text: "consumed question".into(),
                }],
                history_watermark: 0,
                phase: ExecutionPhase::Active,
                terminal: None,
                parent_notification: None,
                result_claim: None,
            })))
            .await
            .unwrap();
    }

    async fn append_recovery_metadata(&self) {
        self.history
            .append_system_prompt(
                Event {
                    id: id(),
                    agent_session_id: "agent".into(),
                    ..Default::default()
                },
                "status-only metadata".into(),
            )
            .await
            .unwrap();
    }
}

#[tokio::test]
async fn inherited_unfinished_fork_context_is_not_owned_recovery_work() {
    let fixture = Fixture::new();
    fixture
        .repository
        .append_conversation(
            Event {
                id: id(),
                agent_session_id: "parent".into(),
                ..Default::default()
            },
            ConversationMessage::text(MessageRole::User, "inherited parent question"),
        )
        .await
        .unwrap();
    fixture
        .history
        .initialize_fork(
            "parent".into(),
            HistoryForkBoundary::AfterCompletedHistory,
            HistoryForkSelection::parse("full"),
            None,
        )
        .await
        .unwrap();
    assert!(fixture.conversation().await.is_answerable());
    let provider = Scripted::default();
    let runtime = fixture
        .recovered_runtime(
            provider.clone(),
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
    assert!(!runtime.session.has_recovery_work().await.unwrap());
    assert!(provider.requests.lock().unwrap().is_empty());
    assert!(!runtime.session.is_active());
    runtime.close().await;
}

#[tokio::test]
async fn canceled_consumed_input_is_sealed_without_suppressing_later_real_input() {
    let fixture = Fixture::new();
    let store = fixture.recovery_store().await;
    fixture.save_consumed_intent(store.as_ref()).await;
    let provider = Scripted::default();
    let runtime = fixture
        .recovered_runtime(
            provider.clone(),
            store.clone(),
            Arc::new(RecoveryParent::default()),
        )
        .await;
    assert!(runtime.session.has_recovery_work().await.unwrap());
    assert_eq!(
        bounded(
            runtime
                .session
                .cancel_recovered_execution("consumed-input")
                .unwrap()
                .join()
        )
        .await
        .unwrap()
        .status,
        ExecutionStatus::Canceled
    );
    assert!(fixture.conversation().await.is_answerable());
    let terminal = store
        .load()
        .await
        .unwrap()
        .unwrap()
        .execution
        .unwrap()
        .terminal
        .unwrap();
    assert_eq!(terminal.outcome, ExecutionOutcome::Canceled);
    assert!(terminal.recovery_watermark > 0);
    assert_eq!(terminal.history_sequence, 0);
    assert!(!runtime.session.has_recovery_work().await.unwrap());
    fixture.append_recovery_metadata().await;
    assert!(!runtime.session.has_recovery_work().await.unwrap());
    runtime.close().await;
    let reopened = fixture
        .recovered_runtime(
            provider.clone(),
            store.clone(),
            Arc::new(RecoveryParent::default()),
        )
        .await;
    bounded(reopened.session.recover_owned().unwrap().join())
        .await
        .unwrap();
    assert!(!reopened.session.has_recovery_work().await.unwrap());
    fixture
        .admit_consumed_input("later-input", "later real question")
        .await;
    assert!(reopened.session.has_recovery_work().await.unwrap());
    assert_eq!(
        store
            .load()
            .await
            .unwrap()
            .unwrap()
            .execution
            .unwrap()
            .terminal,
        Some(terminal)
    );
    assert!(provider.requests.lock().unwrap().is_empty());
    reopened.close().await;
}

#[tokio::test]
async fn succeeded_and_failed_terminal_seals_ignore_metadata_but_preserve_new_queued_and_promoted_input()
 {
    for failed in [false, true] {
        let fixture = Fixture::new();
        let store = fixture.recovery_store().await;
        let mut step = Step::complete("terminal answer");
        if failed {
            step.completion = Err(Error::Io("terminal provider failure".into()));
        }
        let provider = Scripted::new(vec![step]);
        let runtime = fixture
            .recovered_runtime(
                provider.clone(),
                store.clone(),
                Arc::new(RecoveryParent::default()),
            )
            .await;
        let execution = runtime
            .session
            .enqueue_execution(
                vec![ConversationPart::text("original question")],
                "original-input".into(),
                None,
                None,
                Arc::new(NeverCancelled),
            )
            .unwrap();
        assert_eq!(
            bounded(execution.join()).await.unwrap().status,
            if failed {
                ExecutionStatus::Failed
            } else {
                ExecutionStatus::Succeeded
            }
        );
        let terminal = store
            .load()
            .await
            .unwrap()
            .unwrap()
            .execution
            .unwrap()
            .terminal
            .unwrap();
        assert!(terminal.recovery_watermark > 0);
        assert!(!runtime.session.has_recovery_work().await.unwrap());
        fixture.append_recovery_metadata().await;
        assert!(!runtime.session.has_recovery_work().await.unwrap());
        runtime
            .session
            .record(
                vec![ConversationPart::text("queued question")],
                "queued-input".into(),
                protocol::Delivery::DELIVERY_QUEUE,
                Arc::new(NeverCancelled),
            )
            .await
            .unwrap();
        assert!(runtime.session.has_recovery_work().await.unwrap());
        fixture
            .history
            .promote_next_queue(|_| {
                Ok(Event {
                    id: id(),
                    agent_session_id: "agent".into(),
                    ..Default::default()
                })
            })
            .await
            .unwrap();
        assert!(
            !runtime
                .session
                .backend()
                .has_pending_inputs()
                .await
                .unwrap()
        );
        assert!(runtime.session.has_recovery_work().await.unwrap());
        assert_eq!(
            store
                .load()
                .await
                .unwrap()
                .unwrap()
                .execution
                .unwrap()
                .terminal,
            Some(terminal)
        );
        fixture
            .history
            .append_conversation(
                Event {
                    id: id(),
                    agent_session_id: "agent".into(),
                    ..Default::default()
                },
                ConversationMessage::text(MessageRole::Assistant, "already answered"),
            )
            .await
            .unwrap();
        fixture.append_recovery_metadata().await;
        assert!(!runtime.session.has_recovery_work().await.unwrap());
        assert_eq!(provider.requests.lock().unwrap().len(), 1);
        runtime.close().await;
    }
}

#[tokio::test]
async fn later_promoted_input_after_pass_capture_is_not_swallowed_by_terminal_persistence() {
    for failed in [false, true] {
        let fixture = Fixture::new();
        let store = fixture.recovery_store().await;
        let gate = Gate::new();
        let paused = Arc::new(CompletionPauseState {
            inner: store.clone(),
            gate: gate.clone(),
            first: AtomicBool::new(true),
            fail_terminal: AtomicBool::new(false),
        });
        let mut step = Step::complete("original answer");
        if failed {
            step.completion = Err(Error::Io("original failure".into()));
        }
        let provider = Scripted::new(vec![step, Step::complete("later answer")]);
        let runtime = fixture
            .recovered_runtime(
                provider.clone(),
                paused,
                Arc::new(RecoveryParent::default()),
            )
            .await;
        let original = runtime
            .session
            .enqueue_execution(
                vec![ConversationPart::text("original question")],
                "original-input".into(),
                None,
                None,
                Arc::new(NeverCancelled),
            )
            .unwrap();
        gate.entered().await;
        let captured_boundary = fixture
            .repository
            .execution_watermark("agent".into())
            .await
            .unwrap();
        fixture
            .admit_consumed_input("later-input", "later question")
            .await;
        let later_boundary = fixture
            .repository
            .execution_watermark("agent".into())
            .await
            .unwrap();
        assert!(later_boundary > captured_boundary);
        gate.open();
        assert_eq!(
            bounded(original.join()).await.unwrap().status,
            if failed {
                ExecutionStatus::Failed
            } else {
                ExecutionStatus::Succeeded
            }
        );
        let terminal = store
            .load()
            .await
            .unwrap()
            .unwrap()
            .execution
            .unwrap()
            .terminal
            .unwrap();
        assert_eq!(terminal.recovery_watermark, captured_boundary);
        assert!(terminal.recovery_watermark < later_boundary);
        assert!(runtime.session.has_recovery_work().await.unwrap());
        runtime.close().await;
        let resumed = fixture
            .recovered_runtime(
                provider.clone(),
                store.clone(),
                Arc::new(RecoveryParent::default()),
            )
            .await;
        let restored = bounded(resumed.session.recover_owned().unwrap().join())
            .await
            .unwrap();
        assert_eq!(
            restored.status,
            if failed {
                ExecutionStatus::Failed
            } else {
                ExecutionStatus::Succeeded
            }
        );
        assert!(resumed.session.has_recovery_work().await.unwrap());
        assert_eq!(
            bounded(resumed.session.recover().join()).await.unwrap(),
            Execution::succeeded("later answer")
        );
        assert_eq!(provider.requests.lock().unwrap().len(), 2);
        assert!(!resumed.session.has_recovery_work().await.unwrap());
        assert_eq!(
            store
                .load()
                .await
                .unwrap()
                .unwrap()
                .execution
                .unwrap()
                .terminal,
            Some(terminal)
        );
        resumed.close().await;
    }
}
