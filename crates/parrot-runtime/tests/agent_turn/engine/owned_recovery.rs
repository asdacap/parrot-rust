use super::*;
use parrot_domain::{contracts::agent_state::AgentStateStore, values::agent_state::*};
use parrot_foundation::paths::AgentScratchDirectory;
use parrot_storage::agent_state::FileAgentStateStore;

#[path = "owned_cancellation.rs"]
mod cancellation;
#[path = "owned_claims.rs"]
mod claims;

impl Fixture {
    async fn recovery_store(&self) -> Arc<dyn AgentStateStore> {
        let directory = self.path.join("agent-state");
        AgentScratchDirectory::new(&directory, &self.path).unwrap();
        let store = Arc::new(
            FileAgentStateStore::open(&directory, "agent".into())
                .await
                .unwrap(),
        );
        if store.load().await.unwrap().is_none() {
            store
                .initialize(AgentStateSnapshot::new(
                    "agent".into(),
                    AgentDescriptor {
                        parent_session_id: None,
                        name: "test".into(),
                        selection: SelectionSnapshot {
                            profile_id: "mode".into(),
                            model_selector: "scripted/model".into(),
                        },
                        local_scope: String::new(),
                        completion_delivery: CompletionDeliverySnapshot::Automatic,
                    },
                ))
                .await
                .unwrap();
        }
        store
    }
    async fn recovered_runtime(
        &self,
        provider: Scripted,
        store: Arc<dyn AgentStateStore>,
        parent: Arc<dyn ParentCompletion>,
    ) -> Runtime {
        self.runtime_with_overrides(
            provider,
            Arc::new(Tools::default()),
            vec![],
            Arc::new(Signal::new()),
            0,
            RuntimeOverrides {
                state: Some(store),
                parent_completion: Some(parent),
                ..RuntimeOverrides::default()
            },
        )
        .await
    }
    async fn reopen(&self) -> Fixture {
        self.repository.events().database().close().await.unwrap();
        let repository = Fixture::open(&self.path);
        let history = Fixture::history_for(&self.path, &repository);
        Fixture {
            path: self.path.clone(),
            repository,
            history,
        }
    }
}

#[derive(Default)]
struct RecoveryParent {
    fail: AtomicBool,
    messages: Mutex<Vec<(String, Execution)>>,
}
impl ParentCompletion for RecoveryParent {
    fn deliver<'a>(&'a self, _: &'a Execution) -> BoxFuture<'a, Result<(), TurnError>> {
        panic!("durable execution must use a correlated completion")
    }
    fn deliver_correlated<'a>(
        &'a self,
        execution: &'a Execution,
        message_id: &'a str,
    ) -> BoxFuture<'a, Result<(), TurnError>> {
        Box::pin(async move {
            if self.fail.load(Ordering::SeqCst) {
                return Err(TurnError::Service("parent temporarily unavailable".into()));
            }
            let mut messages = self.messages.lock().unwrap();
            if !messages.iter().any(|(id, _)| id == message_id) {
                messages.push((message_id.into(), execution.clone()));
            }
            Ok(())
        })
    }
}

#[tokio::test]
async fn suspension_reopens_original_admitted_prompt_and_restores_wait_and_parent() {
    let fixture = Fixture::new();
    let store = fixture.recovery_store().await;
    let gate = Gate::new();
    let mut step = Step::complete("discarded partial attempt");
    step.gate = Some(gate.clone());
    let parent = Arc::new(RecoveryParent::default());
    let runtime = fixture
        .recovered_runtime(Scripted::new(vec![step]), store.clone(), parent.clone())
        .await;
    let sent = runtime
        .session
        .send_and_select_execution(
            vec![ConversationPart::text("original question")],
            "original-message".into(),
            Arc::new(NeverCancelled),
        )
        .unwrap()
        .join()
        .await
        .unwrap();
    gate.entered().await;
    let before = store.load().await.unwrap().unwrap().execution.unwrap();
    bounded(runtime.session.suspend()).await.unwrap();
    assert_eq!(
        sent.execution.join().await.unwrap().status,
        ExecutionStatus::Canceled
    );
    let suspended = store.load().await.unwrap().unwrap().execution.unwrap();
    assert_eq!(suspended.phase, ExecutionPhase::Suspended);
    assert!(suspended.terminal.is_none());
    assert!(!fixture.jsonl().contains("interrupted"));
    assert!(parent.messages.lock().unwrap().is_empty());
    let restarted = fixture.reopen().await;
    let provider = Scripted::new(vec![Step::complete("resumed answer")]);
    let resumed = restarted
        .recovered_runtime(
            provider.clone(),
            restarted.recovery_store().await,
            parent.clone(),
        )
        .await;
    let result = bounded(resumed.session.recover_owned().unwrap().join())
        .await
        .unwrap();
    assert_eq!(result, Execution::succeeded("resumed answer"));
    assert_eq!(
        resumed
            .session
            .wait(0, &NeverCancelled)
            .await
            .unwrap()
            .execution,
        Some(result.clone())
    );
    let after = restarted
        .recovery_store()
        .await
        .load()
        .await
        .unwrap()
        .unwrap()
        .execution
        .unwrap();
    assert_eq!(after.execution_id, before.execution_id);
    assert_eq!(after.initial_message_id, "original-message");
    let conversation = restarted.conversation().await;
    let messages = conversation.messages();
    assert_eq!(
        messages
            .iter()
            .filter(|message| message.role == MessageRole::User)
            .count(),
        1
    );
    assert_eq!(provider.requests.lock().unwrap().len(), 1);
    assert_eq!(
        parent.messages.lock().unwrap().as_slice(),
        &[(format!("{}.parent", before.execution_id), result)]
    );
    assert_eq!(
        restarted
            .events()
            .await
            .iter()
            .filter(|event| event.has_agent_started())
            .count(),
        1
    );
    resumed.close().await;
}

#[tokio::test]
async fn completed_and_never_started_recovery_stays_idle_and_retains_results() {
    let fixture = Fixture::new();
    let store = fixture.recovery_store().await;
    let provider = Scripted::default();
    let parent = Arc::new(RecoveryParent::default());
    let idle = fixture
        .recovered_runtime(provider.clone(), store.clone(), parent.clone())
        .await;
    assert_eq!(
        idle.session.recover_owned().unwrap().join().await.unwrap(),
        Execution::succeeded("")
    );
    assert!(idle.session.wait(0, &NeverCancelled).await.is_err());
    assert!(provider.requests.lock().unwrap().is_empty());
    idle.close().await;
    let active = fixture
        .recovered_runtime(
            Scripted::new(vec![Step::complete("saved answer")]),
            store,
            parent.clone(),
        )
        .await;
    let handle = active
        .session
        .enqueue_execution(
            vec![ConversationPart::text("question")],
            "completed-question".into(),
            None,
            None,
            Arc::new(NeverCancelled),
        )
        .unwrap();
    assert_eq!(
        bounded(handle.join()).await.unwrap(),
        Execution::succeeded("saved answer")
    );
    active.close().await;
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
        resumed
            .session
            .recover_owned()
            .unwrap()
            .join()
            .await
            .unwrap(),
        Execution::succeeded("saved answer")
    );
    assert_eq!(
        resumed
            .session
            .wait(0, &NeverCancelled)
            .await
            .unwrap()
            .execution,
        Some(Execution::succeeded("saved answer"))
    );
    assert!(provider.requests.lock().unwrap().is_empty());
    assert_eq!(parent.messages.lock().unwrap().len(), 1);
    assert_eq!(
        restarted
            .events()
            .await
            .iter()
            .filter(|event| event.has_agent_finished())
            .count(),
        1
    );
    resumed.close().await;
}

#[tokio::test]
async fn committed_assistant_before_execution_checkpoint_is_consumed_after_normal_readiness() {
    let fixture = Fixture::new();
    let store = fixture.recovery_store().await;
    let parent = Arc::new(RecoveryParent::default());
    let gate = Gate::new();
    let mut step = Step::complete("durable response");
    step.gate = Some(gate.clone());
    let runtime = fixture
        .recovered_runtime(Scripted::new(vec![step]), store.clone(), parent.clone())
        .await;
    let handle = runtime
        .session
        .enqueue_execution(
            vec![ConversationPart::text("question")],
            "crash-message".into(),
            None,
            None,
            Arc::new(NeverCancelled),
        )
        .unwrap();
    gate.entered().await;
    let active = store.load().await.unwrap().unwrap().execution.unwrap();
    gate.open();
    assert_eq!(
        bounded(handle.join()).await.unwrap(),
        Execution::succeeded("durable response")
    );
    runtime.close().await;
    // Simulate the older file visible after a crash between SQLite result commit
    // and execution-checkpoint replacement.
    store
        .update(AgentStateUpdate::Execution(Some(active)))
        .await
        .unwrap();
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
        bounded(resumed.session.recover_owned().unwrap().join())
            .await
            .unwrap(),
        Execution::succeeded("durable response")
    );
    assert!(provider.requests.lock().unwrap().is_empty());
    assert_eq!(parent.messages.lock().unwrap().len(), 1);
    assert_eq!(
        restarted
            .events()
            .await
            .iter()
            .filter(|event| event.has_agent_finished())
            .count(),
        1
    );
    resumed.close().await;
}

#[tokio::test]
async fn intent_before_admission_recovers_once_and_existing_admission_is_not_duplicated() {
    for already_admitted in [false, true] {
        let fixture = Fixture::new();
        let store = fixture.recovery_store().await;
        store
            .update(AgentStateUpdate::Execution(Some(ExecutionCheckpoint {
                result_claim: None,
                execution_id: "intent-execution".into(),
                initial_message_id: "intent-message".into(),
                initial_parts: vec![ExecutionInputPart::Text {
                    text: "intent question".into(),
                }],
                history_watermark: 0,
                phase: ExecutionPhase::AdmissionIntent,
                terminal: None,
                parent_notification: Some(NotificationCheckpoint {
                    message_id: "intent-execution.parent".into(),
                    admitted: false,
                    delivery_error: None,
                }),
            })))
            .await
            .unwrap();
        let parent = Arc::new(RecoveryParent::default());
        let provider = Scripted::new(vec![Step::complete("intent answer")]);
        let runtime = fixture
            .recovered_runtime(provider.clone(), store, parent)
            .await;
        if already_admitted {
            runtime
                .session
                .record(
                    vec![ConversationPart::text("intent question")],
                    "intent-message".into(),
                    protocol::Delivery::DELIVERY_STEER,
                    Arc::new(NeverCancelled),
                )
                .await
                .unwrap();
        }
        assert_eq!(
            bounded(runtime.session.recover_owned().unwrap().join())
                .await
                .unwrap(),
            Execution::succeeded("intent answer")
        );
        assert_eq!(provider.requests.lock().unwrap().len(), 1);
        assert_eq!(
            fixture
                .events()
                .await
                .iter()
                .filter(|event| event.has_input_admitted())
                .count(),
            1
        );
        assert_eq!(
            fixture
                .conversation()
                .await
                .messages()
                .iter()
                .filter(|message| message.role == MessageRole::User)
                .count(),
            1
        );
        runtime.close().await;
    }
}

#[tokio::test]
async fn explicit_interruption_remains_terminal_on_reopen() {
    let fixture = Fixture::new();
    let store = fixture.recovery_store().await;
    let gate = Gate::new();
    let mut step = Step::complete("not completed");
    step.gate = Some(gate.clone());
    let parent = Arc::new(RecoveryParent::default());
    let runtime = fixture
        .recovered_runtime(Scripted::new(vec![step]), store.clone(), parent.clone())
        .await;
    let sent = runtime
        .session
        .send_and_select_execution(
            vec![ConversationPart::text("question")],
            "cancel-message".into(),
            Arc::new(NeverCancelled),
        )
        .unwrap()
        .join()
        .await
        .unwrap();
    gate.entered().await;
    bounded(runtime.session.interrupt(&NeverCancelled))
        .await
        .unwrap();
    assert_eq!(
        bounded(sent.execution.join()).await.unwrap().status,
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
            .phase,
        ExecutionPhase::Canceled
    );
    runtime.close().await;
    let restarted = fixture.reopen().await;
    let provider = Scripted::default();
    let resumed = restarted
        .recovered_runtime(provider.clone(), restarted.recovery_store().await, parent)
        .await;
    assert_eq!(
        resumed
            .session
            .recover_owned()
            .unwrap()
            .join()
            .await
            .unwrap()
            .status,
        ExecutionStatus::Canceled
    );
    assert!(provider.requests.lock().unwrap().is_empty());
    resumed.close().await;
}

#[tokio::test]
async fn pending_parent_admission_retries_without_changing_execution_outcome() {
    let fixture = Fixture::new();
    let store = fixture.recovery_store().await;
    let parent = Arc::new(RecoveryParent::default());
    parent.fail.store(true, Ordering::SeqCst);
    let runtime = fixture
        .recovered_runtime(
            Scripted::new(vec![Step::complete("actual answer")]),
            store.clone(),
            parent.clone(),
        )
        .await;
    let handle = runtime
        .session
        .enqueue_execution(
            vec![ConversationPart::text("question")],
            "notification-message".into(),
            None,
            None,
            Arc::new(NeverCancelled),
        )
        .unwrap();
    assert_eq!(
        bounded(handle.join()).await.unwrap(),
        Execution::succeeded("actual answer")
    );
    let checkpoint = store.load().await.unwrap().unwrap().execution.unwrap();
    assert_eq!(
        checkpoint.terminal.unwrap().outcome,
        ExecutionOutcome::Succeeded
    );
    let notification = checkpoint.parent_notification.unwrap();
    assert!(!notification.admitted);
    assert_eq!(
        notification.delivery_error.as_deref(),
        Some("parent temporarily unavailable")
    );
    runtime.close().await;
    let restarted = fixture.reopen().await;
    parent.fail.store(false, Ordering::SeqCst);
    let provider = Scripted::default();
    let resumed = restarted
        .recovered_runtime(
            provider.clone(),
            restarted.recovery_store().await,
            parent.clone(),
        )
        .await;
    assert_eq!(
        resumed
            .session
            .recover_owned()
            .unwrap()
            .join()
            .await
            .unwrap(),
        Execution::succeeded("actual answer")
    );
    assert_eq!(parent.messages.lock().unwrap().len(), 1);
    assert!(provider.requests.lock().unwrap().is_empty());
    assert!(
        restarted
            .recovery_store()
            .await
            .load()
            .await
            .unwrap()
            .unwrap()
            .execution
            .unwrap()
            .parent_notification
            .unwrap()
            .admitted
    );
    assert_eq!(
        restarted
            .events()
            .await
            .iter()
            .filter(|event| event.has_agent_finished())
            .count(),
        1
    );
    resumed.close().await;
}

#[tokio::test]
async fn successor_does_not_replace_a_pending_parent_completion() {
    let fixture = Fixture::new();
    let store = fixture.recovery_store().await;
    let parent = Arc::new(RecoveryParent::default());
    parent.fail.store(true, Ordering::SeqCst);
    let provider = Scripted::new(vec![
        Step::complete("first answer"),
        Step::complete("second answer"),
    ]);
    let runtime = fixture
        .recovered_runtime(provider.clone(), store.clone(), parent.clone())
        .await;
    let first = runtime
        .session
        .send_and_select_execution(
            vec![ConversationPart::text("first question")],
            "first-question".into(),
            Arc::new(NeverCancelled),
        )
        .unwrap()
        .join()
        .await
        .unwrap();
    assert_eq!(
        bounded(first.execution.join()).await.unwrap(),
        Execution::succeeded("first answer")
    );
    let previous = store.load().await.unwrap().unwrap().execution.unwrap();
    let denied = runtime
        .session
        .send_and_select_execution(
            vec![ConversationPart::text("second question")],
            "second-question".into(),
            Arc::new(NeverCancelled),
        )
        .unwrap()
        .join()
        .await;
    assert!(denied.is_err());
    assert_eq!(
        store.load().await.unwrap().unwrap().execution,
        Some(previous.clone())
    );
    assert_eq!(provider.requests.lock().unwrap().len(), 1);
    parent.fail.store(false, Ordering::SeqCst);
    let second = runtime
        .session
        .send_and_select_execution(
            vec![ConversationPart::text("second question")],
            "second-question".into(),
            Arc::new(NeverCancelled),
        )
        .unwrap()
        .join()
        .await
        .unwrap();
    assert_eq!(
        bounded(second.execution.join()).await.unwrap(),
        Execution::succeeded("second answer")
    );
    let delivered = parent.messages.lock().unwrap().clone();
    assert_eq!(delivered.len(), 2);
    assert_eq!(
        delivered[0],
        (
            previous.parent_notification.unwrap().message_id,
            Execution::succeeded("first answer")
        )
    );
    runtime.close().await;
}

#[tokio::test]
async fn failed_initial_intent_write_does_not_finalize_previous_execution() {
    let fixture = Fixture::new();
    let store = fixture.recovery_store().await;
    let provider = Scripted::new(vec![Step::complete("previous answer")]);
    let runtime = fixture
        .recovered_runtime(
            provider.clone(),
            store.clone(),
            Arc::new(RecoveryParent::default()),
        )
        .await;
    let first = runtime
        .session
        .send_and_select_execution(
            vec![ConversationPart::text("first")],
            "first".into(),
            Arc::new(NeverCancelled),
        )
        .unwrap()
        .join()
        .await
        .unwrap();
    assert_eq!(
        bounded(first.execution.join()).await.unwrap(),
        Execution::succeeded("previous answer")
    );
    let expected = store.load().await.unwrap().unwrap();
    let obstacle = fixture
        .path
        .join("agent-state")
        .join(AGENT_STATE_STAGING_FILE_NAME);
    std::fs::create_dir(&obstacle).unwrap();
    let failed = runtime
        .session
        .send_and_select_execution(
            vec![ConversationPart::text("must not be admitted")],
            "failed-intent".into(),
            Arc::new(NeverCancelled),
        )
        .unwrap()
        .join()
        .await;
    assert!(failed.is_err());
    assert_eq!(store.load().await.unwrap(), Some(expected));
    std::fs::remove_dir(obstacle).unwrap();
    assert_eq!(provider.requests.lock().unwrap().len(), 1);
    assert_eq!(
        fixture
            .conversation()
            .await
            .messages()
            .iter()
            .filter(|message| message.role == MessageRole::User)
            .count(),
        1
    );
    runtime.close().await;
}

#[tokio::test]
async fn restored_output_and_error_keep_the_original_bounds_and_notification_identity() {
    use parrot_runtime::agent_turn::lifecycle::MAX_AGENT_RESULT_BYTES;
    for fail in [false, true] {
        let fixture = Fixture::new();
        let store = fixture.recovery_store().await;
        let parent = Arc::new(RecoveryParent::default());
        parent.fail.store(true, Ordering::SeqCst);
        let text = "é".repeat(MAX_AGENT_RESULT_BYTES);
        let mut step = Step::complete(&text);
        if fail {
            step.completion = Err(Error::InvalidOperation(text));
        }
        let runtime = fixture
            .recovered_runtime(Scripted::new(vec![step]), store.clone(), parent.clone())
            .await;
        let handle = runtime
            .session
            .enqueue_execution(
                vec![ConversationPart::text("question")],
                "bounded-message".into(),
                None,
                None,
                Arc::new(NeverCancelled),
            )
            .unwrap();
        let expected = bounded(handle.join()).await.unwrap();
        assert!(expected.output.len() <= MAX_AGENT_RESULT_BYTES);
        assert!(expected.error.len() <= MAX_AGENT_RESULT_BYTES);
        let checkpoint = store.load().await.unwrap().unwrap().execution.unwrap();
        let notification = checkpoint.parent_notification.unwrap().message_id;
        runtime.close().await;
        let restarted = fixture.reopen().await;
        parent.fail.store(false, Ordering::SeqCst);
        let provider = Scripted::default();
        let resumed = restarted
            .recovered_runtime(
                provider.clone(),
                restarted.recovery_store().await,
                parent.clone(),
            )
            .await;
        assert_eq!(
            bounded(resumed.session.recover_owned().unwrap().join())
                .await
                .unwrap(),
            expected
        );
        assert_eq!(
            parent.messages.lock().unwrap().as_slice(),
            &[(notification, expected)]
        );
        assert!(provider.requests.lock().unwrap().is_empty());
        resumed.close().await;
    }
}

struct PausedState {
    inner: Arc<dyn AgentStateStore>,
    gate: Arc<Gate>,
    first: AtomicBool,
}
impl AgentStateStore for PausedState {
    fn load(
        &self,
    ) -> BoxFuture<
        '_,
        Result<Option<AgentStateSnapshot>, parrot_domain::contracts::agent_state::AgentStateError>,
    > {
        Box::pin(async move {
            let snapshot = self.inner.load().await?;
            if self.first.swap(false, Ordering::SeqCst) {
                self.gate.wait(&NeverCancelled).await.map_err(|error| {
                    parrot_domain::contracts::agent_state::AgentStateError::new(
                        parrot_domain::contracts::agent_state::AgentStateErrorKind::Io,
                        error.to_string(),
                    )
                })?;
            }
            Ok(snapshot)
        })
    }
    fn initialize(
        &self,
        snapshot: AgentStateSnapshot,
    ) -> BoxFuture<'_, Result<(), parrot_domain::contracts::agent_state::AgentStateError>> {
        self.inner.initialize(snapshot)
    }
    fn update(
        &self,
        update: AgentStateUpdate,
    ) -> BoxFuture<
        '_,
        Result<AgentStateSnapshot, parrot_domain::contracts::agent_state::AgentStateError>,
    > {
        self.inner.update(update)
    }
    fn retire(
        &self,
    ) -> BoxFuture<'_, Result<(), parrot_domain::contracts::agent_state::AgentStateError>> {
        self.inner.retire()
    }
}

#[tokio::test]
async fn recovery_reservation_preserves_interrupt_or_suspension_during_snapshot_io() {
    for suspend in [false, true] {
        let fixture = Fixture::new();
        let store = fixture.recovery_store().await;
        store
            .update(AgentStateUpdate::Execution(Some(ExecutionCheckpoint {
                result_claim: None,
                execution_id: "reserved-execution".into(),
                initial_message_id: "reserved-message".into(),
                initial_parts: vec![ExecutionInputPart::Text {
                    text: "reserved question".into(),
                }],
                history_watermark: 0,
                phase: ExecutionPhase::Active,
                terminal: None,
                parent_notification: Some(NotificationCheckpoint {
                    message_id: "reserved-execution.parent".into(),
                    admitted: false,
                    delivery_error: None,
                }),
            })))
            .await
            .unwrap();
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
        let recovered = runtime.session.recover_owned().unwrap();
        gate.entered().await;
        let stopping = if suspend {
            runtime.session.suspend_handle()
        } else {
            runtime.session.interrupt_handle().unwrap()
        };
        gate.open();
        bounded(stopping.join()).await.unwrap();
        assert_eq!(
            bounded(recovered.join()).await.unwrap().status,
            ExecutionStatus::Canceled
        );
        let saved = store.load().await.unwrap().unwrap().execution.unwrap();
        assert_eq!(
            saved.phase,
            if suspend {
                ExecutionPhase::Suspended
            } else {
                ExecutionPhase::Canceled
            }
        );
        assert_eq!(saved.terminal.is_none(), suspend);
        assert!(provider.requests.lock().unwrap().is_empty());
        runtime.close().await;
    }
}

#[tokio::test]
async fn correlated_event_reconciliation_rejects_conflicting_records() {
    let fixture = Fixture::new();
    let mut event = Event {
        id: "stable-execution.finished".into(),
        agent_session_id: "agent".into(),
        ..Event::new()
    };
    event.set_agent_finished(protocol::AgentFinished {
        name: "test".into(),
        ..Default::default()
    });
    assert!(
        fixture
            .repository
            .events()
            .append_execution_event(event.clone())
            .await
            .unwrap()
            .is_some()
    );
    assert!(
        fixture
            .repository
            .events()
            .append_execution_event(event.clone())
            .await
            .unwrap()
            .is_none()
    );
    let mut conflict = event.clone();
    conflict.agent_session_id = "other".into();
    assert!(
        fixture
            .repository
            .events()
            .append_execution_event(conflict)
            .await
            .is_err()
    );
    assert!(
        fixture
            .repository
            .events()
            .append(event, None)
            .await
            .is_err()
    );
}

#[tokio::test]
async fn fresh_durable_start_reserves_the_drain_before_snapshot_io() {
    for suspend in [false, true] {
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
        let sent = runtime
            .session
            .send_and_select_execution(
                vec![ConversationPart::text("fresh question")],
                "fresh-message".into(),
                Arc::new(NeverCancelled),
            )
            .unwrap();
        gate.entered().await;
        let stopping = if suspend {
            runtime.session.suspend_handle()
        } else {
            runtime.session.interrupt_handle().unwrap()
        };
        gate.open();
        let sent = bounded(sent.join()).await.unwrap();
        bounded(stopping.join()).await.unwrap();
        assert_eq!(
            bounded(sent.execution.join()).await.unwrap().status,
            ExecutionStatus::Canceled
        );
        let checkpoint = store.load().await.unwrap().unwrap().execution.unwrap();
        assert_eq!(
            checkpoint.phase,
            if suspend {
                ExecutionPhase::Suspended
            } else {
                ExecutionPhase::Canceled
            }
        );
        assert_eq!(checkpoint.terminal.is_none(), suspend);
        assert!(provider.requests.lock().unwrap().is_empty());
        runtime.close().await;
    }
}

#[tokio::test]
async fn canceled_follow_up_observer_does_not_own_a_durable_drain() {
    let fixture = Fixture::new();
    let store = fixture.recovery_store().await;
    let gate = Gate::new();
    let paused = Arc::new(PausedState {
        inner: store,
        gate: gate.clone(),
        first: AtomicBool::new(false),
    });
    let provider = Scripted::new(vec![
        Step::complete("first answer"),
        Step::complete("follow-up answer"),
    ]);
    let runtime = fixture
        .recovered_runtime(
            provider,
            paused.clone(),
            Arc::new(RecoveryParent::default()),
        )
        .await;
    let first = runtime
        .session
        .send_and_select_execution(
            vec![ConversationPart::text("first")],
            "observer-first".into(),
            Arc::new(NeverCancelled),
        )
        .unwrap()
        .join()
        .await
        .unwrap();
    assert_eq!(
        bounded(first.execution.join()).await.unwrap(),
        Execution::succeeded("first answer")
    );
    paused.first.store(true, Ordering::SeqCst);
    let caller = Arc::new(Signal::new());
    let follow_up = runtime
        .session
        .send_and_select_execution(
            vec![ConversationPart::text("follow-up")],
            "observer-follow-up".into(),
            caller.clone(),
        )
        .unwrap();
    gate.entered().await;
    caller.cancel();
    gate.open();
    let accepted = bounded(follow_up.join()).await.unwrap();
    assert!(accepted.follow_up);
    assert_eq!(
        bounded(accepted.execution.join()).await.unwrap(),
        Execution::succeeded("follow-up answer")
    );
    runtime.close().await;
}

#[tokio::test]
async fn canceled_intent_clears_only_its_exact_admitted_input() {
    let fixture = Fixture::new();
    let store = fixture.recovery_store().await;
    store
        .update(AgentStateUpdate::Execution(Some(ExecutionCheckpoint {
            result_claim: None,
            execution_id: "cancel-intent".into(),
            initial_message_id: "cancel-original".into(),
            initial_parts: vec![ExecutionInputPart::Text {
                text: "original".into(),
            }],
            history_watermark: 0,
            phase: ExecutionPhase::Canceled,
            terminal: None,
            parent_notification: Some(NotificationCheckpoint {
                message_id: "cancel-intent.parent".into(),
                admitted: false,
                delivery_error: None,
            }),
        })))
        .await
        .unwrap();
    let provider = Scripted::default();
    let runtime = fixture
        .recovered_runtime(provider.clone(), store, Arc::new(RecoveryParent::default()))
        .await;
    let original = runtime
        .session
        .record(
            vec![ConversationPart::text("original")],
            "cancel-original".into(),
            protocol::Delivery::DELIVERY_STEER,
            Arc::new(NeverCancelled),
        )
        .await
        .unwrap();
    let peer = runtime
        .session
        .record(
            vec![ConversationPart::text("peer")],
            "cancel-peer".into(),
            protocol::Delivery::DELIVERY_QUEUE,
            Arc::new(NeverCancelled),
        )
        .await
        .unwrap();
    assert_eq!(
        bounded(runtime.session.recover_owned().unwrap().join())
            .await
            .unwrap()
            .status,
        ExecutionStatus::Canceled
    );
    assert!(fixture.history.has_pending_inputs().await.unwrap());
    let events = fixture.events().await;
    let canceled = events
        .iter()
        .filter(|event| event.has_input_canceled())
        .map(|event| event.input_canceled().input_id.as_str())
        .collect::<Vec<_>>();
    assert_eq!(canceled, [original.input.id.as_str()]);
    assert_ne!(original.input.id, peer.input.id);
    assert!(provider.requests.lock().unwrap().is_empty());
    runtime.close().await;
}

#[tokio::test]
async fn corrupted_recovery_state_reports_error_without_running_or_replacing_it() {
    let fixture = Fixture::new();
    let store = fixture.recovery_store().await;
    let path = fixture.path.join("agent-state").join(AGENT_STATE_FILE_NAME);
    std::fs::write(&path, "not a snapshot").unwrap();
    let provider = Scripted::default();
    let runtime = fixture
        .recovered_runtime(provider.clone(), store, Arc::new(RecoveryParent::default()))
        .await;
    assert!(
        bounded(runtime.session.recover_owned().unwrap().join())
            .await
            .is_err()
    );
    assert!(runtime.session.wait(0, &NeverCancelled).await.is_err());
    assert!(runtime.session.is_idle());
    assert!(provider.requests.lock().unwrap().is_empty());
    assert_eq!(std::fs::read_to_string(path).unwrap(), "not a snapshot");
    runtime.close().await;
}

#[tokio::test]
async fn separate_pending_input_recovery_preserves_retained_wait_result_or_unstarted_state() {
    for previously_started in [false, true] {
        let fixture = Fixture::new();
        let store = fixture.recovery_store().await;
        let parent = Arc::new(RecoveryParent::default());
        let first_provider = if previously_started {
            Scripted::new(vec![Step::complete("retained answer")])
        } else {
            Scripted::default()
        };
        let original = fixture
            .recovered_runtime(first_provider, store.clone(), parent.clone())
            .await;
        if previously_started {
            let first = original
                .session
                .enqueue_execution(
                    vec![ConversationPart::text("first question")],
                    "retained-question".into(),
                    None,
                    None,
                    Arc::new(NeverCancelled),
                )
                .unwrap();
            assert_eq!(
                bounded(first.join()).await.unwrap(),
                Execution::succeeded("retained answer")
            );
        }
        original
            .session
            .record(
                vec![ConversationPart::text("queued question")],
                "queued-after-terminal".into(),
                protocol::Delivery::DELIVERY_QUEUE,
                Arc::new(NeverCancelled),
            )
            .await
            .unwrap();
        let expected_checkpoint = store.load().await.unwrap().unwrap().execution;
        original.session.suspend().await.unwrap();
        let restarted = fixture.reopen().await;
        let provider = Scripted::new(vec![Step::complete("queued answer")]);
        let resumed = restarted
            .recovered_runtime(provider.clone(), restarted.recovery_store().await, parent)
            .await;
        let restored = bounded(resumed.session.recover_owned().unwrap().join())
            .await
            .unwrap();
        assert_eq!(
            restored,
            Execution::succeeded(if previously_started {
                "retained answer"
            } else {
                ""
            })
        );
        assert!(provider.requests.lock().unwrap().is_empty());
        assert!(restarted.history.has_pending_inputs().await.unwrap());
        assert_eq!(
            bounded(resumed.session.recover().join()).await.unwrap(),
            Execution::succeeded("queued answer")
        );
        assert!(!restarted.history.has_pending_inputs().await.unwrap());
        assert_eq!(provider.requests.lock().unwrap().len(), 1);
        if previously_started {
            assert_eq!(
                resumed
                    .session
                    .wait(0, &NeverCancelled)
                    .await
                    .unwrap()
                    .execution,
                Some(Execution::succeeded("retained answer"))
            );
        } else {
            assert!(resumed.session.wait(0, &NeverCancelled).await.is_err());
        }
        assert_eq!(
            restarted
                .recovery_store()
                .await
                .load()
                .await
                .unwrap()
                .unwrap()
                .execution,
            expected_checkpoint
        );
        resumed.close().await;
    }
}

struct CompletionPauseState {
    inner: Arc<dyn AgentStateStore>,
    gate: Arc<Gate>,
    first: AtomicBool,
    fail_terminal: AtomicBool,
}
impl AgentStateStore for CompletionPauseState {
    fn load(
        &self,
    ) -> BoxFuture<
        '_,
        Result<Option<AgentStateSnapshot>, parrot_domain::contracts::agent_state::AgentStateError>,
    > {
        Box::pin(async move {
            let snapshot = self.inner.load().await?;
            if snapshot
                .as_ref()
                .and_then(|snapshot| snapshot.execution.as_ref())
                .is_some_and(|execution| execution.phase == ExecutionPhase::Active)
                && self.first.swap(false, Ordering::SeqCst)
            {
                self.gate.wait(&NeverCancelled).await.map_err(|error| {
                    parrot_domain::contracts::agent_state::AgentStateError::new(
                        parrot_domain::contracts::agent_state::AgentStateErrorKind::Io,
                        error.to_string(),
                    )
                })?;
            }
            Ok(snapshot)
        })
    }
    fn initialize(
        &self,
        snapshot: AgentStateSnapshot,
    ) -> BoxFuture<'_, Result<(), parrot_domain::contracts::agent_state::AgentStateError>> {
        self.inner.initialize(snapshot)
    }
    fn update(
        &self,
        update: AgentStateUpdate,
    ) -> BoxFuture<
        '_,
        Result<AgentStateSnapshot, parrot_domain::contracts::agent_state::AgentStateError>,
    > {
        Box::pin(async move {
            if matches!(&update, AgentStateUpdate::Execution(Some(checkpoint)) if checkpoint.terminal.is_some())
                && self.fail_terminal.swap(false, Ordering::SeqCst)
            {
                return Err(parrot_domain::contracts::agent_state::AgentStateError::new(
                    parrot_domain::contracts::agent_state::AgentStateErrorKind::Io,
                    "terminal snapshot write failed",
                ));
            }
            self.inner.update(update).await
        })
    }
    fn retire(
        &self,
    ) -> BoxFuture<'_, Result<(), parrot_domain::contracts::agent_state::AgentStateError>> {
        self.inner.retire()
    }
}

#[tokio::test]
async fn owned_result_reference_is_not_replaced_by_later_input_or_raw_response() {
    for later_response in [false, true] {
        let fixture = Fixture::new();
        let store = fixture.recovery_store().await;
        let gate = Gate::new();
        let paused = Arc::new(CompletionPauseState {
            inner: store.clone(),
            gate: gate.clone(),
            first: AtomicBool::new(true),
            fail_terminal: AtomicBool::new(false),
        });
        let mut steps = vec![Step::complete("original owned answer")];
        if later_response {
            steps.push(Step::complete("later raw answer"));
        }
        let provider = Scripted::new(steps);
        let runtime = fixture
            .recovered_runtime(
                provider.clone(),
                paused,
                Arc::new(RecoveryParent::default()),
            )
            .await;
        let original = runtime
            .session
            .send_and_select_execution(
                vec![ConversationPart::text("original question")],
                "original-race-message".into(),
                Arc::new(NeverCancelled),
            )
            .unwrap()
            .join()
            .await
            .unwrap();
        gate.entered().await;
        let original_sequence = fixture
            .history
            .conversation()
            .await
            .unwrap()
            .into_iter()
            .find(|item| {
                item.message.role == MessageRole::Assistant
                    && parrot_storage::events::content(&item.message.parts)
                        == "original owned answer"
            })
            .unwrap()
            .sequence;
        let later = if later_response {
            let admitted = runtime
                .session
                .send(
                    vec![ConversationPart::text("later question")],
                    "later-raw-message".into(),
                    protocol::Delivery::DELIVERY_STEER,
                    IncomingActivity::default(),
                    Arc::new(NeverCancelled),
                )
                .await
                .unwrap();
            assert!(!admitted.execution.is_finished());
            assert_eq!(provider.requests.lock().unwrap().len(), 1);
            assert_eq!(
                fixture
                    .history
                    .conversation()
                    .await
                    .unwrap()
                    .iter()
                    .filter(|item| item.message.role == MessageRole::Assistant)
                    .count(),
                1
            );
            Some(admitted.execution)
        } else {
            runtime
                .session
                .record(
                    vec![ConversationPart::text("later queued question")],
                    "later-queued-message".into(),
                    protocol::Delivery::DELIVERY_QUEUE,
                    Arc::new(NeverCancelled),
                )
                .await
                .unwrap();
            None
        };
        gate.open();
        assert_eq!(
            bounded(original.execution.join()).await.unwrap(),
            Execution::succeeded("original owned answer")
        );
        if let Some(later) = later {
            assert_eq!(
                bounded(later.join()).await.unwrap(),
                Execution::succeeded("later raw answer")
            );
        }
        let saved = store.load().await.unwrap().unwrap().execution.unwrap();
        assert_eq!(saved.terminal.unwrap().history_sequence, original_sequence);
        runtime.session.suspend().await.unwrap();
        let restarted = fixture.reopen().await;
        let fresh_provider = Scripted::default();
        let resumed = restarted
            .recovered_runtime(
                fresh_provider.clone(),
                restarted.recovery_store().await,
                Arc::new(RecoveryParent::default()),
            )
            .await;
        assert_eq!(
            bounded(resumed.session.recover_owned().unwrap().join())
                .await
                .unwrap(),
            Execution::succeeded("original owned answer")
        );
        assert!(fresh_provider.requests.lock().unwrap().is_empty());
        resumed.close().await;
    }
}

#[tokio::test]
async fn success_without_a_captured_assistant_reference_fails_before_snapshot_commit() {
    use parrot_runtime::agent_turn::lifecycle::{DrainBackend, ExecutionResultReference};
    let fixture = Fixture::new();
    let store = fixture.recovery_store().await;
    let runtime = fixture
        .recovered_runtime(
            Scripted::default(),
            store.clone(),
            Arc::new(RecoveryParent::default()),
        )
        .await;
    let checkpoint = ExecutionCheckpoint {
        result_claim: None,
        execution_id: "missing-result".into(),
        initial_message_id: "missing-result-input".into(),
        initial_parts: vec![ExecutionInputPart::Text {
            text: "question".into(),
        }],
        history_watermark: 0,
        phase: ExecutionPhase::Active,
        terminal: None,
        parent_notification: None,
    };
    let mut execution = Execution::succeeded("");
    assert!(
        runtime
            .session
            .backend()
            .capture_execution(
                &checkpoint,
                &mut execution,
                ExecutionResultReference::Missing
            )
            .await
            .is_err()
    );
    assert!(store.load().await.unwrap().unwrap().execution.is_none());
    runtime.close().await;
}

#[tokio::test]
async fn failed_terminal_persistence_blocks_deferred_raw_work_and_successor_intent() {
    let fixture = Fixture::new();
    let store = fixture.recovery_store().await;
    let gate = Gate::new();
    let paused = Arc::new(CompletionPauseState {
        inner: store.clone(),
        gate: gate.clone(),
        first: AtomicBool::new(true),
        fail_terminal: AtomicBool::new(true),
    });
    let provider = Scripted::new(vec![Step::complete("original answer")]);
    let runtime = fixture
        .recovered_runtime(
            provider.clone(),
            paused,
            Arc::new(RecoveryParent::default()),
        )
        .await;
    let original = runtime
        .session
        .send_and_select_execution(
            vec![ConversationPart::text("original")],
            "failed-terminal-original".into(),
            Arc::new(NeverCancelled),
        )
        .unwrap()
        .join()
        .await
        .unwrap();
    gate.entered().await;
    let deferred = runtime
        .session
        .send(
            vec![ConversationPart::text("deferred")],
            "failed-terminal-deferred".into(),
            protocol::Delivery::DELIVERY_STEER,
            IncomingActivity::default(),
            Arc::new(NeverCancelled),
        )
        .await
        .unwrap();
    assert!(!deferred.execution.is_finished());
    assert_eq!(provider.requests.lock().unwrap().len(), 1);
    gate.open();
    assert_eq!(
        bounded(original.execution.join()).await.unwrap().status,
        ExecutionStatus::Failed
    );
    assert!(bounded(deferred.execution.join()).await.is_err());
    let expected = store.load().await.unwrap().unwrap().execution.unwrap();
    assert!(expected.terminal.is_none());
    let another = runtime
        .session
        .send(
            vec![ConversationPart::text("after failed completion")],
            "after-failed-terminal".into(),
            protocol::Delivery::DELIVERY_STEER,
            IncomingActivity::default(),
            Arc::new(NeverCancelled),
        )
        .await
        .unwrap();
    assert!(bounded(another.execution.join()).await.is_err());
    let successor = runtime
        .session
        .send_and_select_execution(
            vec![ConversationPart::text("successor")],
            "must-not-replace".into(),
            Arc::new(NeverCancelled),
        )
        .unwrap()
        .join()
        .await;
    assert!(successor.is_err());
    assert_eq!(
        store.load().await.unwrap().unwrap().execution,
        Some(expected)
    );
    assert_eq!(provider.requests.lock().unwrap().len(), 1);
    assert!(fixture.history.has_pending_inputs().await.unwrap());
    runtime.close().await;
}

#[tokio::test]
async fn suspension_stops_deferred_raw_work_while_preserving_late_committed_result() {
    let fixture = Fixture::new();
    let store = fixture.recovery_store().await;
    let gate = Gate::new();
    let paused = Arc::new(CompletionPauseState {
        inner: store.clone(),
        gate: gate.clone(),
        first: AtomicBool::new(true),
        fail_terminal: AtomicBool::new(false),
    });
    let provider = Scripted::new(vec![Step::complete("committed before suspension")]);
    let runtime = fixture
        .recovered_runtime(
            provider.clone(),
            paused,
            Arc::new(RecoveryParent::default()),
        )
        .await;
    let original = runtime
        .session
        .send_and_select_execution(
            vec![ConversationPart::text("original")],
            "suspend-finalization-original".into(),
            Arc::new(NeverCancelled),
        )
        .unwrap()
        .join()
        .await
        .unwrap();
    gate.entered().await;
    let deferred = runtime
        .session
        .send(
            vec![ConversationPart::text("later")],
            "suspend-finalization-later".into(),
            protocol::Delivery::DELIVERY_STEER,
            IncomingActivity::default(),
            Arc::new(NeverCancelled),
        )
        .await
        .unwrap();
    let stopping = runtime.session.suspend_handle();
    assert_eq!(
        bounded(deferred.execution.join()).await.unwrap().status,
        ExecutionStatus::Canceled
    );
    assert_eq!(provider.requests.lock().unwrap().len(), 1);
    gate.open();
    bounded(stopping.join()).await.unwrap();
    assert_eq!(
        bounded(original.execution.join()).await.unwrap(),
        Execution::succeeded("committed before suspension")
    );
    assert!(
        store
            .load()
            .await
            .unwrap()
            .unwrap()
            .execution
            .unwrap()
            .terminal
            .is_some()
    );
    assert!(fixture.history.has_pending_inputs().await.unwrap());
    runtime.close().await;
}
