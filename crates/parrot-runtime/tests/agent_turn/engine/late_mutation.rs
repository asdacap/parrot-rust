use super::*;
use parrot_runtime::agent_turn::lifecycle::LifecycleError;

struct AdmissionWatch {
    armed: AtomicBool,
    reached: Semaphore,
}
impl AdmissionWatch {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            armed: AtomicBool::new(false),
            reached: Semaphore::new(0),
        })
    }
}
impl Cancellation for AdmissionWatch {
    fn is_cancelled(&self) -> bool {
        if self.armed.swap(false, Ordering::SeqCst) {
            self.reached.add_permits(1);
        }
        false
    }
    fn cancelled(&self) -> BoxFuture<'_, ()> {
        Box::pin(std::future::pending())
    }
}
struct ArmAfterCallback {
    gate: Arc<Gate>,
    watch: Arc<AdmissionWatch>,
}
impl TurnCompletion<Security> for ArmAfterCallback {
    fn complete<'a>(
        &'a self,
        _: &'a CompletionCandidate<Security>,
        cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<CompletionDecision, TurnError>> {
        Box::pin(async move {
            self.gate
                .wait(cancellation)
                .await
                .map_err(TurnError::Provider)?;
            self.watch.armed.store(true, Ordering::SeqCst);
            Ok(CompletionDecision::Complete)
        })
    }
}
#[tokio::test]
async fn mutation_admitted_after_last_terminal_snapshot_is_joined_before_drain_settles() {
    use std::{future::Future, task::Poll};
    let fixture = Fixture::new();
    let callback = Gate::new();
    let watch = AdmissionWatch::new();
    let runtime = fixture
        .runtime(
            Scripted::new(vec![Step::complete("normal answer")]),
            Arc::new(Tools::default()),
            vec![Arc::new(ArmAfterCallback {
                gate: callback.clone(),
                watch: watch.clone(),
            })],
            watch.clone(),
            0,
        )
        .await;
    let reminder_publication = Gate::new();
    *runtime.publisher.reminder_gate.lock().unwrap() = Some(reminder_publication.clone());
    let sent = runtime
        .send("question", protocol::Delivery::DELIVERY_STEER)
        .await;
    callback.entered().await;
    let (release, blocked) = std::sync::mpsc::channel();
    let barrier = fixture
        .repository
        .events()
        .database()
        .admit("hold-terminal-commit", &NeverCancelled, move |_| {
            blocked.recv_timeout(Duration::from_secs(10)).unwrap();
            Ok(())
        })
        .unwrap();
    callback.open();
    bounded(watch.reached.acquire()).await.unwrap().forget();
    // This cancellation check is the real terminal transaction admission after
    // the callback and second mutation snapshot. The actor is still blocked.
    let admitted = Arc::new(Semaphore::new(0));
    let worker_admitted = admitted.clone();
    let observer = runtime
        .session
        .owned_mutation(
            move |backend, _| {
                Box::pin(async move {
                    let mut mutation = Box::pin(backend.reminders().set(
                        "late",
                        "after last snapshot",
                        &NeverCancelled,
                    ));
                    std::future::poll_fn(|cx| {
                        assert!(matches!(mutation.as_mut().poll(cx), Poll::Pending));
                        Poll::Ready(())
                    })
                    .await;
                    worker_admitted.add_permits(1);
                    mutation
                        .await
                        .map_err(|error| LifecycleError::Failed(error.to_string()))
                })
            },
            Arc::new(NeverCancelled),
        )
        .unwrap();
    bounded(admitted.acquire()).await.unwrap().forget();
    drop(observer);
    release.send(()).unwrap();
    barrier.into_completion().await.unwrap();
    reminder_publication.entered().await;
    let events = fixture.events().await;
    assert_eq!(
        events.iter().filter(|event| event.has_turn_ended()).count(),
        1
    );
    assert!(!sent.execution.is_finished());
    assert!(runtime.session.is_active());
    reminder_publication.open();
    assert_eq!(
        bounded(sent.execution.join()).await.unwrap().status,
        ExecutionStatus::Succeeded
    );
    assert_eq!(
        runtime.session.backend().reminders().titles().await,
        ["late"]
    );
    let restored = ExitReminders::new(
        fixture.history.clone(),
        runtime.publisher.clone(),
        templates(),
    )
    .await
    .unwrap();
    assert_eq!(restored.titles().await, ["late"]);
    runtime.close().await;
}
#[tokio::test]
async fn handled_cancelled_reminder_publication_does_not_fail_the_provider_turn_or_drop_committed_state()
 {
    let fixture = Fixture::new();
    let provider_gate = Gate::new();
    let mut step = Step::complete("normal completion after reminder publication cancellation");
    step.gate = Some(provider_gate.clone());
    let runtime = fixture
        .runtime(
            Scripted::new(vec![step]),
            Arc::new(Tools::default()),
            vec![],
            Arc::new(Signal::new()),
            0,
        )
        .await;
    let publication = Gate::new();
    *runtime.publisher.reminder_gate.lock().unwrap() = Some(publication.clone());
    let sent = runtime
        .send("question", protocol::Delivery::DELIVERY_STEER)
        .await;
    provider_gate.entered().await;
    let caller = Arc::new(Signal::new());
    let mutation = runtime
        .session
        .set_exit_reminder(
            "committed".into(),
            "publication canceled".into(),
            caller.clone(),
        )
        .unwrap();
    publication.entered().await;
    caller.cancel();
    assert_eq!(
        bounded(mutation.join()).await.unwrap_err(),
        LifecycleError::Cancelled
    );
    assert_eq!(
        runtime.session.backend().reminders().titles().await,
        ["committed"]
    );
    assert_eq!(
        fixture
            .history
            .notices("exit_reminder_changed".into())
            .await
            .unwrap()
            .len(),
        1
    );
    provider_gate.open();
    assert_eq!(
        bounded(sent.execution.join()).await.unwrap().status,
        ExecutionStatus::Succeeded
    );
    assert!(
        !fixture
            .events()
            .await
            .iter()
            .any(|event| event.has_turn_failed())
    );
    runtime.close().await;
}
