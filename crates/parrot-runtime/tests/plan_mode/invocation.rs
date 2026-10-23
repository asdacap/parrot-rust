//! Real AgentSession admission/selection/drain consumer with an explicit test
//! backend. This is orchestration evidence, not retained WriteTool/P05 evidence.
use super::{
    modes::{Materializer, registry},
    support::*,
};
use parrot_domain::{
    contracts::{
        async_ops::{BoxFuture, Cancellation},
        security::AccessSnapshot,
    },
    protocol::messages::parrot::{Delivery, InputSource},
};
use parrot_runtime::{
    agent_turn::{lifecycle::*, services::TurnSelection},
    plan_mode::*,
};
use parrot_storage::events::{Admission, AdmittedInput, ConversationPart};
use std::{
    path::PathBuf,
    sync::{Arc, Mutex},
};

struct Consumer {
    selected: Mutex<Option<TurnSelection<Policy>>>,
    pending: Mutex<bool>,
    target: PathBuf,
    expected: Arc<Policy>,
    expected_id: &'static str,
    pass_entered: tokio::sync::Notify,
}
impl DrainBackend for Consumer {
    type Selection = TurnSelection<Policy>;
    fn admit(
        &self,
        parts: Vec<ConversationPart>,
        message_id: String,
        delivery: Delivery,
        _: InputSource,
    ) -> BoxFuture<'_, Result<Admission, LifecycleError>> {
        Box::pin(async move {
            self.expected.trace.lock().unwrap().push("admit".into());
            *self.pending.lock().unwrap() = true;
            Ok(Admission {
                input: AdmittedInput {
                    id: format!("input-{message_id}"),
                    message_id,
                    parts,
                    delivery,
                },
                published: None,
            })
        })
    }
    fn find_execution_input(
        &self,
        _: String,
    ) -> BoxFuture<'_, Result<Option<AdmittedInput>, LifecycleError>> {
        panic!("fixture does not use durable owned execution")
    }
    fn publish_admission<'a>(
        &'a self,
        _: &'a Admission,
        _: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<(), LifecycleError>> {
        Box::pin(async { Ok(()) })
    }
    fn has_recovery_work_after(&self, _: i64) -> BoxFuture<'_, Result<bool, LifecycleError>> {
        self.has_pending_inputs()
    }
    fn has_pending_inputs(&self) -> BoxFuture<'_, Result<bool, LifecycleError>> {
        Box::pin(async { Ok(*self.pending.lock().unwrap()) })
    }
    fn cancel_pending_input(&self, _: String) -> BoxFuture<'_, Result<(), LifecycleError>> {
        Box::pin(async {
            *self.pending.lock().unwrap() = false;
            Ok(())
        })
    }
    fn update_selection(&self, selection: Self::Selection) -> Result<(), LifecycleError> {
        assert!(Arc::ptr_eq(&selection.security, &self.expected));
        assert_eq!(selection.profile.id, self.expected_id);
        assert_eq!(
            selection.profile.maximum_requests,
            profiles()[self.expected_id].max_turns
        );
        assert_eq!(selection.profile.disabled_tools, ["disabled"]);
        assert_eq!(
            selection.model.canonical_model.selector(),
            "fixture/vendor/model"
        );
        self.expected
            .trace
            .lock()
            .unwrap()
            .push("consumer-selection".into());
        *self.selected.lock().unwrap() = Some(selection);
        Ok(())
    }
    fn drain_state_changed(&self, _: DrainState) {}
    fn pass<'a>(
        &'a self,
        _: &'a dyn Cancellation,
        work: &'a dyn DrainWork,
    ) -> BoxFuture<'a, Result<DrainPass, LifecycleError>> {
        Box::pin(async move {
            *self.pending.lock().unwrap() = false;
            let selected = self.selected.lock().unwrap().clone().unwrap();
            self.expected
                .trace
                .lock()
                .unwrap()
                .push("consumer-pass".into());
            // A selected fixture snapshot authorizes the actual target before
            // the fixture's mutation callback. No raw policy is substituted.
            if let Err(error) = selected.security.check_write(&self.target) {
                return Ok(DrainPass {
                    execution: Execution::failed(error),
                    recovery_watermark: 0,
                    result_reference: ExecutionResultReference::Missing,
                });
            }
            self.expected.trace.lock().unwrap().push("mutate".into());
            std::fs::create_dir_all(self.target.parent().unwrap()).unwrap();
            std::fs::write(&self.target, "fixture mutation").unwrap();
            self.pass_entered.notify_one();
            work.join_mutations().await?;
            self.expected.trace.lock().unwrap().push("joined".into());
            Ok(DrainPass {
                execution: Execution::succeeded("done"),
                recovery_watermark: 0,
                result_reference: ExecutionResultReference::Missing,
            })
        })
    }
    fn prepare_scheduled<'a>(
        &'a self,
        _: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<Self::Selection, LifecycleError>> {
        panic!("no scheduled work in this fixture")
    }
    fn execution_started(&self) -> BoxFuture<'_, Result<(), LifecycleError>> {
        Box::pin(async { Ok(()) })
    }
    fn execution_finished<'a>(
        &'a self,
        _: &'a Execution,
    ) -> BoxFuture<'a, Result<(), LifecycleError>> {
        Box::pin(async { Ok(()) })
    }
    fn execution_started_correlated(&self, _: &str) -> BoxFuture<'_, Result<(), LifecycleError>> {
        panic!("fixture does not use durable owned execution")
    }
    fn execution_finished_correlated<'a>(
        &'a self,
        _: &'a Execution,
        _: &'a str,
    ) -> BoxFuture<'a, Result<(), LifecycleError>> {
        panic!("fixture does not use durable owned execution")
    }
    fn cancel_execution_input(&self, _: &str) -> BoxFuture<'_, Result<i64, LifecycleError>> {
        panic!("fixture does not use durable owned execution")
    }
    fn execution_watermark(&self) -> BoxFuture<'_, Result<i64, LifecycleError>> {
        panic!("fixture does not use durable owned execution")
    }
    fn restore_execution<'a>(
        &'a self,
        _: &'a parrot_domain::values::agent_state::ExecutionTerminal,
    ) -> BoxFuture<'a, Result<Execution, LifecycleError>> {
        panic!("fixture does not use durable owned execution")
    }
    fn capture_execution<'a>(
        &'a self,
        _: &'a parrot_domain::values::agent_state::ExecutionCheckpoint,
        _: &'a mut Execution,
        _: ExecutionResultReference,
        _: i64,
    ) -> BoxFuture<'a, Result<parrot_domain::values::agent_state::ExecutionTerminal, LifecycleError>>
    {
        panic!("fixture does not use durable owned execution")
    }
    fn suspend_execution(&self) {
        panic!("fixture does not use durable owned execution")
    }
    fn close(&self) -> BoxFuture<'_, Result<(), LifecycleError>> {
        Box::pin(async { Ok(()) })
    }
}

#[tokio::test]
async fn awaited_plan_completion_observes_joined_real_t52_reminder_mutation_after_observer_cancellation()
 {
    use parrot_domain::contracts::{
        async_ops::OperationError, events::EventPublisher, storage::Committed,
    };
    use parrot_domain::protocol::messages::parrot::Event;
    use parrot_runtime::active_work::exit_reminders::ExitReminders;
    use parrot_storage::{
        conversation::ConversationRepository, database::SessionDatabase, events::EventRepository,
        history_files::AgentHistoryFile,
    };
    use std::sync::atomic::{AtomicBool, Ordering};
    struct Publisher {
        first: AtomicBool,
        entered: tokio::sync::Notify,
        release: tokio::sync::Notify,
    }
    impl EventPublisher for Publisher {
        type Event = Event;
        type Error = String;
        fn publish_committed<'a>(
            &'a self,
            _: &'a Committed<Event>,
            cancellation: &'a dyn Cancellation,
        ) -> BoxFuture<'a, Result<(), OperationError<String>>> {
            Box::pin(async move {
                if self.first.swap(false, Ordering::SeqCst) {
                    self.entered.notify_one();
                    self.release.notified().await;
                }
                cancellation.check().map_err(|_| OperationError::Cancelled)
            })
        }
        fn publish_transient<'a>(
            &'a self,
            _: Event,
            _: &'a dyn Cancellation,
        ) -> BoxFuture<'a, Result<(), OperationError<String>>> {
            panic!("no transient event")
        }
    }
    let fixture = Fixture::new();
    let scratch = fixture.scratch("main");
    let root = fixture.resources.selected_root().clone();
    let repository = ConversationRepository::new(EventRepository::new(
        SessionDatabase::open(root.clone()).unwrap(),
    ));
    let history = repository
        .bind_agent_history(
            "main".into(),
            Arc::new(
                AgentHistoryFile::new(root, scratch.history_path(), vec!["main".into()]).unwrap(),
            ),
        )
        .unwrap();
    let publisher = Arc::new(Publisher {
        first: AtomicBool::new(true),
        entered: tokio::sync::Notify::new(),
        release: tokio::sync::Notify::new(),
    });
    let reminders = Arc::new(
        ExitReminders::new(history, publisher.clone(), templates())
            .await
            .unwrap(),
    );
    let (workflow, parser) = fixture.workflow();
    workflow.prepare().unwrap();
    write_valid(&workflow, &parser);
    let paths = workflow.artifacts().paths().unwrap();
    let raw = policy("raw");
    let output = Arc::new(Policy {
        name: "materialized".into(),
        writes: vec![scratch.root().into()],
        trace: Arc::new(Mutex::new(vec![])),
    });
    let modes = UserSessionModes::new(registry(raw.clone()), vec![]).unwrap();
    modes.attach(scratch.clone());
    let materializer = Materializer {
        raw,
        output: output.clone(),
        scratch,
        calls: Mutex::new(0),
        failure: None,
    };
    let selection = modes
        .capture_turn_selection(PLAN, model(), &materializer)
        .unwrap();
    let backend = Arc::new(Consumer {
        selected: Mutex::new(None),
        pending: Mutex::new(false),
        target: paths.plan().into(),
        expected: output,
        expected_id: PLAN,
        pass_entered: tokio::sync::Notify::new(),
    });
    let lifetime = Arc::new(Signal::new());
    let session = AgentSession::new(backend.clone(), lifetime.clone());
    let observer = Arc::new(Signal::new());
    let owned_reminders = reminders.clone();
    let publication = observer.clone();
    let mutation = session
        .owned_mutation(
            move |_, _| {
                Box::pin(async move {
                    owned_reminders
                        .set(
                            "finish plan",
                            "join accepted publication",
                            publication.as_ref(),
                        )
                        .await
                        .map_err(|error| LifecycleError::Failed(error.to_string()))
                })
            },
            observer.clone(),
        )
        .unwrap();
    publisher.entered.notified().await;
    observer.cancel();
    let sent = session
        .enqueue_execution(
            vec![ConversationPart::Text("finish".into())],
            "joined-plan".into(),
            Some(selection),
            None,
            lifetime.clone(),
        )
        .unwrap();
    backend.pass_entered.notified().await;
    assert!(
        !backend
            .expected
            .trace
            .lock()
            .unwrap()
            .iter()
            .any(|event| event == "joined")
    );
    assert_eq!(reminders.titles().await, ["finish plan"]);
    publisher.release.notify_one();
    assert!(
        mutation.join().await.is_err(),
        "publication cancellation is the mutation observer's result"
    );
    assert_eq!(
        sent.join().await.unwrap().status,
        ExecutionStatus::Succeeded
    );
    assert!(
        backend
            .expected
            .trace
            .lock()
            .unwrap()
            .iter()
            .any(|event| event == "joined")
    );
    // The host's source ordering is await session -> complete. The workflow
    // itself does not publish terminal turns or bypass active-work blockers.
    assert!(matches!(
        workflow.complete().unwrap(),
        ModeCompletionOutcome::Completed(_)
    ));
    assert!(
        reminders
            .clear("finish plan", lifetime.as_ref())
            .await
            .unwrap()
    );
    assert!(reminders.titles().await.is_empty());
    session.close().await.unwrap();
}

#[tokio::test]
async fn captured_materialized_selection_reaches_actual_session_consumer_before_check_and_mutation()
{
    let fixture = Fixture::new();
    let scratch = fixture.scratch("actual-attached-main");
    let explicit = fixture.root.join("explicit-exception");
    let cache = fixture.root.join("configured-cache");
    let trace = Arc::new(Mutex::new(Vec::new()));
    let raw = Arc::new(Policy {
        name: "raw-plan".into(),
        writes: vec![explicit.clone(), cache.clone()],
        trace: Arc::new(Mutex::new(Vec::new())),
    });
    let output = Arc::new(Policy {
        name: "materialized-plan".into(),
        writes: vec![scratch.root().into(), explicit.clone(), cache.clone()],
        trace: trace.clone(),
    });
    let modes = UserSessionModes::new(registry(raw.clone()), vec![]).unwrap();
    modes.attach(scratch.clone());
    let materializer = Materializer {
        raw: raw.clone(),
        output: output.clone(),
        scratch: scratch.clone(),
        calls: Mutex::new(0),
        failure: None,
    };
    let code = fixture.root.join("workspace/code.rs");
    std::fs::write(&code, "original code").unwrap();
    let cases = [
        (PLAN, scratch.plan_directory().join("plan.md"), true),
        (PLAN, explicit.join("allowed.txt"), true),
        (PLAN, cache.join("allowed.txt"), true),
        (PLAN, code, false),
        (
            PLAN,
            fixture.root.join("another-session/scratch/denied.txt"),
            false,
        ),
        (BUILD, cache.join("build.txt"), true),
        (QUERY, cache.join("query.txt"), true),
    ];
    for (index, (id, target, allowed)) in cases.into_iter().enumerate() {
        let before = std::fs::read(&target).ok();
        trace.lock().unwrap().clear();
        trace.lock().unwrap().push("capture".into());
        let selection = modes
            .capture_turn_selection(id, model(), &materializer)
            .unwrap();
        assert!(Arc::ptr_eq(&selection.security, &output));
        assert!(Arc::ptr_eq(
            modes.resolve(PLAN).unwrap().profile().security(),
            &raw
        ));
        let backend = Arc::new(Consumer {
            selected: Mutex::new(None),
            pending: Mutex::new(false),
            target: target.clone(),
            expected: output.clone(),
            expected_id: id,
            pass_entered: tokio::sync::Notify::new(),
        });
        let lifetime = Arc::new(Signal::new());
        let session = AgentSession::new(backend, lifetime.clone());
        let result = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            session.send_and_wait(
                vec![ConversationPart::Text("fixture".into())],
                format!("message-{index}"),
                Some(selection),
                None,
                lifetime,
            ),
        )
        .await
        .unwrap();
        assert_eq!(result.is_ok(), allowed, "{target:?}: {result:?}");
        if allowed {
            assert_eq!(
                std::fs::read_to_string(&target).unwrap(),
                "fixture mutation"
            );
        } else {
            assert_eq!(
                result,
                Err(LifecycleError::Failed(format!(
                    "materialized-plan denied {}",
                    target.display()
                )))
            );
            assert_eq!(std::fs::read(&target).ok(), before);
        }
        let observed = trace.lock().unwrap().clone();
        let position = |name: &str| observed.iter().position(|event| event == name).unwrap();
        assert!(position("capture") < position("materialize"));
        assert!(position("materialize") < position("consumer-selection"));
        assert!(position("consumer-selection") < position("consumer-pass"));
        let check = observed
            .iter()
            .position(|event| event.starts_with("check:"))
            .unwrap();
        assert!(position("consumer-pass") < check);
        if allowed {
            assert!(check < position("mutate"));
            assert!(position("mutate") < position("joined"));
        } else {
            assert!(!observed.iter().any(|event| event == "mutate"));
        }
        session.close().await.unwrap();
    }
    assert_eq!(*materializer.calls.lock().unwrap(), 7);
    assert!(
        raw.trace.lock().unwrap().is_empty(),
        "consumer must not check the raw profile policy"
    );
}
