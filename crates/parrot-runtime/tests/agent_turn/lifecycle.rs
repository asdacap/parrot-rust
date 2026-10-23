//! Deterministic scheduler tests. Persistence and provider execution are tested
//! by the real backend's integration suite, not simulated here.

use parrot_runtime::agent_turn::lifecycle::{DrainPass, ExecutionResultReference};
use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use parrot_domain::contracts::async_ops::{BoxFuture, Cancellation};
use parrot_domain::protocol::messages::parrot::{Delivery, InputSource};
use parrot_runtime::agent_turn::lifecycle::*;
use parrot_storage::events::{Admission, AdmittedInput, ConversationPart};
use tokio::sync::Notify;

#[derive(Default)]
struct Gate {
    open: AtomicBool,
    changed: Notify,
}

impl Gate {
    fn release(&self) {
        self.open.store(true, Ordering::SeqCst);
        self.changed.notify_waiters();
    }

    async fn wait(&self) {
        loop {
            let notified = self.changed.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            if self.open.load(Ordering::SeqCst) {
                return;
            }
            notified.await;
        }
    }
}

#[derive(Default)]
struct TestCancellation(Gate);

impl TestCancellation {
    fn cancel(&self) {
        self.0.release();
    }
}

impl Cancellation for TestCancellation {
    fn is_cancelled(&self) -> bool {
        self.0.open.load(Ordering::SeqCst)
    }

    fn cancelled(&self) -> BoxFuture<'_, ()> {
        Box::pin(self.0.wait())
    }
}

async fn bounded<T>(future: impl std::future::Future<Output = T>) -> T {
    tokio::time::timeout(Duration::from_secs(5), future)
        .await
        .expect("deterministic lifecycle barrier did not complete")
}

struct PassControl {
    entered: Gate,
    release: Gate,
    cancellation_seen: Gate,
    cancellation_join: Gate,
    consume_inputs: bool,
    result: Execution,
    terminal_snapshot: bool,
    failure: Option<LifecycleError>,
    exited: Gate,
}

impl PassControl {
    fn blocked(consume_inputs: bool, result: Execution) -> Arc<Self> {
        Arc::new(Self {
            entered: Gate::default(),
            release: Gate::default(),
            cancellation_seen: Gate::default(),
            cancellation_join: Gate::default(),
            consume_inputs,
            result,
            terminal_snapshot: false,
            failure: None,
            exited: Gate::default(),
        })
    }

    fn terminal(result: Result<Execution, LifecycleError>) -> Arc<Self> {
        Arc::new(Self {
            entered: Gate::default(),
            release: Gate::default(),
            cancellation_seen: Gate::default(),
            cancellation_join: Gate::default(),
            consume_inputs: true,
            result: result.clone().unwrap_or_else(|_| Execution::canceled()),
            terminal_snapshot: true,
            failure: result.err(),
            exited: Gate::default(),
        })
    }
}

#[derive(Default)]
struct PendingControl {
    entered: Gate,
    release: Gate,
    exited: Gate,
    failure: Option<LifecycleError>,
}

#[derive(Default)]
struct FakeBackend {
    pending: Mutex<Vec<AdmittedInput>>,
    admitted: Mutex<Vec<String>>,
    promoted: Mutex<Vec<String>>,
    passes: Mutex<VecDeque<Arc<PassControl>>>,
    pending_control: Mutex<Option<Arc<PendingControl>>>,
    admit_gate: Mutex<Option<Arc<Gate>>>,
    admit_entered: Gate,
    active: AtomicUsize,
    max_active: AtomicUsize,
    pass_count: AtomicUsize,
    pass_entered: Gate,
    starts: AtomicUsize,
    finishes: Mutex<Vec<Execution>>,
    finish_gate: Mutex<Option<Arc<Gate>>>,
    finish_entered: Gate,
    selection: Mutex<String>,
    prepared: Mutex<Vec<String>>,
    canceled_inputs: Mutex<Vec<String>>,
    events: Mutex<Vec<&'static str>>,
    closed: AtomicBool,
    states: Mutex<Vec<DrainState>>,
}

impl FakeBackend {
    fn plan(&self, pass: Arc<PassControl>) {
        self.passes.lock().unwrap().push_back(pass);
    }

    fn consume(&self) {
        let inputs = std::mem::take(&mut *self.pending.lock().unwrap());
        self.promoted
            .lock()
            .unwrap()
            .extend(inputs.into_iter().map(|input| input.message_id));
    }
}

impl DrainBackend for FakeBackend {
    type Selection = String;

    fn admit(
        &self,
        parts: Vec<ConversationPart>,
        message_id: String,
        delivery: Delivery,
        _source: InputSource,
    ) -> BoxFuture<'_, Result<Admission, LifecycleError>> {
        Box::pin(async move {
            self.admit_entered.release();
            let blocked = self.admit_gate.lock().unwrap().clone();
            if let Some(gate) = blocked {
                gate.wait().await;
            }
            assert!(!self.closed.load(Ordering::SeqCst));
            let input = AdmittedInput {
                id: format!("input-{message_id}"),
                message_id: message_id.clone(),
                parts,
                delivery,
            };
            self.admitted.lock().unwrap().push(message_id);
            self.pending.lock().unwrap().push(input.clone());
            self.events.lock().unwrap().push("admit");
            Ok(Admission {
                input,
                published: Some(parrot_domain::protocol::messages::parrot::Event::new()),
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
        _admission: &'a Admission,
        cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<(), LifecycleError>> {
        Box::pin(async move {
            self.events.lock().unwrap().push("publish");
            if cancellation.is_cancelled() {
                Err(LifecycleError::Cancelled)
            } else {
                Ok(())
            }
        })
    }

    fn has_recovery_work_after(&self, _: i64) -> BoxFuture<'_, Result<bool, LifecycleError>> {
        self.has_pending_inputs()
    }
    fn has_pending_inputs(&self) -> BoxFuture<'_, Result<bool, LifecycleError>> {
        Box::pin(async move {
            let control = self.pending_control.lock().unwrap().take();
            if let Some(control) = control {
                control.entered.release();
                control.release.wait().await;
                control.exited.release();
                if let Some(error) = &control.failure {
                    return Err(error.clone());
                }
            }
            Ok(!self.pending.lock().unwrap().is_empty())
        })
    }

    fn cancel_pending_input(&self, input_id: String) -> BoxFuture<'_, Result<(), LifecycleError>> {
        Box::pin(async move {
            self.pending
                .lock()
                .unwrap()
                .retain(|input| input.id != input_id);
            self.canceled_inputs.lock().unwrap().push(input_id);
            Ok(())
        })
    }

    fn update_selection(&self, selection: String) -> Result<(), LifecycleError> {
        *self.selection.lock().unwrap() = selection;
        Ok(())
    }

    fn drain_state_changed(&self, state: DrainState) {
        self.states.lock().unwrap().push(state);
    }

    fn pass<'a>(
        &'a self,
        cancellation: &'a dyn Cancellation,
        work: &'a dyn DrainWork,
    ) -> BoxFuture<'a, Result<DrainPass, LifecycleError>> {
        Box::pin(async move {
            let active = self.active.fetch_add(1, Ordering::SeqCst) + 1;
            self.max_active.fetch_max(active, Ordering::SeqCst);
            self.pass_count.fetch_add(1, Ordering::SeqCst);
            self.pass_entered.release();
            self.events.lock().unwrap().push("pass");
            let plan = self.passes.lock().unwrap().pop_front();
            if let Some(plan) = &plan {
                if plan.consume_inputs {
                    self.consume();
                }
                if plan.terminal_snapshot {
                    work.join_mutations().await?;
                }
                plan.entered.release();
                tokio::select! {
                    _ = plan.release.wait() => {},
                    _ = cancellation.cancelled() => {
                        plan.cancellation_seen.release();
                        plan.cancellation_join.wait().await;
                        self.active.fetch_sub(1, Ordering::SeqCst);
                        plan.exited.release();
                        return Ok(DrainPass { execution: Execution::canceled(), recovery_watermark: 0, result_reference: ExecutionResultReference::Missing });
                    }
                }
            } else {
                self.consume();
            }
            self.active.fetch_sub(1, Ordering::SeqCst);
            if let Some(plan) = &plan {
                plan.exited.release();
                if let Some(error) = &plan.failure {
                    return Err(error.clone());
                }
            }
            Ok(DrainPass {
                execution: plan.map_or_else(
                    || Execution::succeeded("answer"),
                    |plan| plan.result.clone(),
                ),
                recovery_watermark: 0,
                result_reference: ExecutionResultReference::Missing,
            })
        })
    }

    fn prepare_scheduled<'a>(
        &'a self,
        cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<String, LifecycleError>> {
        Box::pin(async move {
            assert!(!cancellation.is_cancelled());
            let selection = self.selection.lock().unwrap().clone();
            self.prepared.lock().unwrap().push(selection.clone());
            Ok(selection)
        })
    }

    fn execution_started(&self) -> BoxFuture<'_, Result<(), LifecycleError>> {
        Box::pin(async move {
            self.starts.fetch_add(1, Ordering::SeqCst);
            self.events.lock().unwrap().push("started");
            Ok(())
        })
    }

    fn execution_finished<'a>(
        &'a self,
        execution: &'a Execution,
    ) -> BoxFuture<'a, Result<(), LifecycleError>> {
        Box::pin(async move {
            self.finish_entered.release();
            let blocked = self.finish_gate.lock().unwrap().clone();
            if let Some(gate) = blocked {
                gate.wait().await;
            }
            self.finishes.lock().unwrap().push(execution.clone());
            self.events.lock().unwrap().push("finished");
            Ok(())
        })
    }

    fn execution_started_correlated(&self, _: &str) -> BoxFuture<'_, Result<(), LifecycleError>> {
        Box::pin(async { panic!("not a persistent backend") })
    }
    fn execution_finished_correlated<'a>(
        &'a self,
        _: &'a Execution,
        _: &'a str,
    ) -> BoxFuture<'a, Result<(), LifecycleError>> {
        Box::pin(async { panic!("not a persistent backend") })
    }
    fn cancel_execution_input(&self, _: &str) -> BoxFuture<'_, Result<i64, LifecycleError>> {
        Box::pin(async { panic!("not a persistent backend") })
    }
    fn execution_watermark(&self) -> BoxFuture<'_, Result<i64, LifecycleError>> {
        Box::pin(async { panic!("not a persistent backend") })
    }
    fn restore_execution<'a>(
        &'a self,
        _: &'a parrot_domain::values::agent_state::ExecutionTerminal,
    ) -> BoxFuture<'a, Result<Execution, LifecycleError>> {
        Box::pin(async { panic!("not a persistent backend") })
    }
    fn capture_execution<'a>(
        &'a self,
        _: &'a parrot_domain::values::agent_state::ExecutionCheckpoint,
        _: &'a mut Execution,
        _: ExecutionResultReference,
        _: i64,
    ) -> BoxFuture<'a, Result<parrot_domain::values::agent_state::ExecutionTerminal, LifecycleError>>
    {
        Box::pin(async { panic!("not a persistent backend") })
    }
    fn suspend_execution(&self) {
        panic!("not a persistent backend")
    }
    fn close(&self) -> BoxFuture<'_, Result<(), LifecycleError>> {
        Box::pin(async move {
            assert_eq!(self.active.load(Ordering::SeqCst), 0);
            self.closed.store(true, Ordering::SeqCst);
            self.events.lock().unwrap().push("close");
            Ok(())
        })
    }
}

fn fixture() -> (
    Arc<FakeBackend>,
    AgentSession<FakeBackend>,
    Arc<TestCancellation>,
) {
    let backend = Arc::new(FakeBackend::default());
    let lifetime = Arc::new(TestCancellation::default());
    let session = AgentSession::new(backend.clone(), lifetime.clone());
    (backend, session, lifetime)
}

fn parts(text: &str) -> Vec<ConversationPart> {
    vec![ConversationPart::text(text)]
}

fn caller() -> Arc<TestCancellation> {
    Arc::new(TestCancellation::default())
}

fn enqueue(session: &AgentSession<FakeBackend>, id: &str) -> ExecutionHandle {
    session
        .enqueue_execution(parts(id), id.into(), None, None, caller())
        .unwrap()
}

async fn poll_pending<F: std::future::Future>(mut future: std::pin::Pin<&mut F>) {
    std::future::poll_fn(|context| {
        assert!(future.as_mut().poll(context).is_pending());
        std::task::Poll::Ready(())
    })
    .await;
}

#[tokio::test]
async fn concurrent_wakes_share_one_drain_and_one_last_look_pass() {
    let (backend, session, _) = fixture();
    let pass = PassControl::blocked(true, Execution::succeeded("first"));
    backend.plan(pass.clone());
    let (started, first) = session.wake(None);
    assert!(started);
    bounded(pass.entered.wait()).await;
    let mut wakes = Vec::new();
    for _ in 0..32 {
        let session = session.clone();
        wakes.push(tokio::spawn(async move { session.wake(None) }));
    }
    for wake in wakes {
        let (started, selected) = bounded(wake).await.unwrap();
        assert!(!started);
        assert!(!selected.is_finished());
    }
    pass.release.release();
    assert_eq!(
        bounded(first.join()).await.unwrap().status,
        ExecutionStatus::Succeeded
    );
    assert_eq!(backend.max_active.load(Ordering::SeqCst), 1);
    assert_eq!(backend.pass_count.load(Ordering::SeqCst), 2);
    assert!(session.is_idle());
    bounded(session.close()).await.unwrap();
}

#[tokio::test]
async fn unary_observer_cancellation_never_owns_the_selected_drain() {
    let (backend, session, _) = fixture();
    let pass = PassControl::blocked(true, Execution::succeeded("kept running"));
    backend.plan(pass.clone());
    let token = caller();
    let sent = bounded(
        session
            .send_and_select_execution(parts("first"), "first".into(), token.clone())
            .unwrap()
            .join(),
    )
    .await
    .unwrap();
    bounded(pass.entered.wait()).await;
    token.cancel();
    assert!(sent.execution.wait(token.as_ref()).await.is_err());
    assert!(!pass.cancellation_seen.open.load(Ordering::SeqCst));
    pass.release.release();
    assert_eq!(
        bounded(sent.execution.join()).await.unwrap().output,
        "kept running"
    );
    let terminal = bounded(session.wait(0, caller().as_ref())).await.unwrap();
    assert_eq!(terminal.execution.unwrap().output, "kept running");
    assert_eq!(backend.starts.load(Ordering::SeqCst), 1);
    assert_eq!(backend.finishes.lock().unwrap().len(), 1);
    bounded(session.close()).await.unwrap();
}

#[tokio::test]
async fn dropped_send_observer_during_admission_does_not_drop_commit_or_wake() {
    let (backend, session, _) = fixture();
    let commit = Arc::new(Gate::default());
    *backend.admit_gate.lock().unwrap() = Some(commit.clone());
    let token = caller();
    let mut observer = Box::pin(session.send(
        parts("durable"),
        "durable".into(),
        Delivery::DELIVERY_STEER,
        IncomingActivity::default(),
        token.clone(),
    ));
    poll_pending(observer.as_mut()).await;
    bounded(backend.admit_entered.wait()).await;
    drop(observer);
    token.cancel();
    commit.release();
    bounded(backend.pass_entered.wait()).await;
    bounded(session.close()).await.unwrap();
    assert_eq!(*backend.admitted.lock().unwrap(), ["durable"]);
    assert_eq!(*backend.promoted.lock().unwrap(), ["durable"]);
    assert_eq!(backend.max_active.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn interrupt_joins_canceled_drain_and_resumes_unpromoted_durable_inputs() {
    let (backend, session, _) = fixture();
    let pass = PassControl::blocked(true, Execution::succeeded("unused"));
    backend.plan(pass.clone());
    let sent = bounded(session.send(
        parts("active"),
        "active".into(),
        Delivery::DELIVERY_STEER,
        IncomingActivity::default(),
        caller(),
    ))
    .await
    .unwrap();
    bounded(pass.entered.wait()).await;
    bounded(session.record(
        parts("pending"),
        "pending".into(),
        Delivery::DELIVERY_QUEUE,
        caller(),
    ))
    .await
    .unwrap();
    let interrupt = session.interrupt_handle().unwrap();
    bounded(pass.cancellation_seen.wait()).await;
    assert_eq!(session.state(), DrainState::Interrupting);
    assert!(!interrupt.is_finished());
    let canceled_observer = caller();
    canceled_observer.cancel();
    assert!(interrupt.wait(canceled_observer.as_ref()).await.is_err());
    pass.cancellation_join.release();
    bounded(interrupt.join()).await.unwrap();
    assert_eq!(
        bounded(sent.execution.join()).await.unwrap().status,
        ExecutionStatus::Canceled
    );
    bounded(session.wake(None).1.join()).await.unwrap();
    assert_eq!(*backend.promoted.lock().unwrap(), ["active", "pending"]);
    assert_eq!(backend.max_active.load(Ordering::SeqCst), 1);
    bounded(session.close()).await.unwrap();
}

#[tokio::test]
async fn close_joins_admission_and_accepted_mutation_before_backend_close() {
    let (backend, session, _) = fixture();
    let commit = Arc::new(Gate::default());
    *backend.admit_gate.lock().unwrap() = Some(commit.clone());
    let admission = session
        .record_handle(
            parts("record"),
            "record".into(),
            Delivery::DELIVERY_QUEUE,
            caller(),
        )
        .unwrap();
    bounded(backend.admit_entered.wait()).await;
    let entered = Arc::new(Gate::default());
    let finish = Arc::new(Gate::default());
    let token = caller();
    let mutation = session
        .owned_mutation(
            {
                let entered = entered.clone();
                let finish = finish.clone();
                move |backend, _lifetime| {
                    Box::pin(async move {
                        entered.release();
                        finish.wait().await;
                        assert!(!backend.closed.load(Ordering::SeqCst));
                        backend.events.lock().unwrap().push("mutation");
                        Ok("mutated".to_owned())
                    })
                }
            },
            token.clone(),
        )
        .unwrap();
    bounded(entered.wait()).await;
    let close = session.close_handle();
    token.cancel();
    assert!(mutation.wait(token.as_ref()).await.is_err());
    assert!(!close.is_finished());
    assert!(!backend.closed.load(Ordering::SeqCst));
    assert!(
        session
            .record_handle(
                parts("late"),
                "late".into(),
                Delivery::DELIVERY_QUEUE,
                caller()
            )
            .is_err()
    );
    commit.release();
    bounded(admission.join()).await.unwrap();
    assert!(!close.is_finished());
    finish.release();
    assert_eq!(bounded(mutation.join()).await.unwrap(), "mutated");
    bounded(close.join()).await.unwrap();
    bounded(session.close()).await.unwrap();
    assert_eq!(backend.events.lock().unwrap().last(), Some(&"close"));
    assert_eq!(*backend.admitted.lock().unwrap(), ["record"]);
}

#[tokio::test]
async fn canceled_fifo_reservation_keeps_its_predecessor_position() {
    let (backend, session, _) = fixture();
    let pass = PassControl::blocked(true, Execution::succeeded("first"));
    backend.plan(pass.clone());
    let first = enqueue(&session, "first");
    bounded(pass.entered.wait()).await;
    let token = caller();
    let canceled = session
        .enqueue_execution(
            parts("canceled"),
            "canceled".into(),
            None,
            None,
            token.clone(),
        )
        .unwrap();
    token.cancel();
    assert!(bounded(canceled.join()).await.is_err());
    let third = session
        .enqueue_execution(
            parts("third"),
            "third".into(),
            Some("third-selection".into()),
            None,
            caller(),
        )
        .unwrap();
    tokio::task::yield_now().await;
    assert!(!third.is_finished());
    assert_eq!(*backend.admitted.lock().unwrap(), ["first"]);
    pass.release.release();
    bounded(first.join()).await.unwrap();
    bounded(third.join()).await.unwrap();
    assert_eq!(*backend.admitted.lock().unwrap(), ["first", "third"]);
    assert_eq!(*backend.promoted.lock().unwrap(), ["first", "third"]);
    assert_eq!(*backend.selection.lock().unwrap(), "third-selection");
    assert_eq!(backend.starts.load(Ordering::SeqCst), 2);
    bounded(session.close()).await.unwrap();
}

#[tokio::test]
async fn owned_prompt_steers_into_incoming_wait_and_duplicate_wait_is_rejected() {
    let (backend, session, _) = fixture();
    let pass = PassControl::blocked(true, Execution::succeeded("old"));
    backend.plan(pass.clone());
    let drain = session.wake(None).1;
    bounded(pass.entered.wait()).await;
    let token = caller();
    let mut incoming =
        Box::pin(session.wait_for_incoming_input(Duration::from_secs(60), token.as_ref()));
    poll_pending(incoming.as_mut()).await;
    let duplicate = session
        .wait_for_incoming_input(Duration::from_secs(60), token.as_ref())
        .await;
    assert_eq!(
        duplicate.unwrap_err().to_string(),
        "An incoming-input wait is already active."
    );
    let execution = enqueue(&session, "steered");
    assert!(bounded(incoming).await.unwrap().is_some());
    assert_eq!(*backend.admitted.lock().unwrap(), ["steered"]);
    assert!(!execution.is_finished());
    assert_eq!(
        backend.starts.load(Ordering::SeqCst),
        0,
        "steering is not a new public execution"
    );
    pass.release.release();
    bounded(execution.join()).await.unwrap();
    bounded(drain.join()).await.unwrap();
    assert_eq!(*backend.promoted.lock().unwrap(), ["steered"]);
    bounded(session.close()).await.unwrap();
}

#[tokio::test]
async fn incoming_wait_drop_and_cancellation_release_the_single_reader_registration() {
    let (_, session, _) = fixture();
    let token = caller();
    let mut incoming =
        Box::pin(session.wait_for_incoming_input(Duration::from_secs(60), token.as_ref()));
    poll_pending(incoming.as_mut()).await;
    drop(incoming);
    let token = caller();
    let mut incoming =
        Box::pin(session.wait_for_incoming_input(Duration::from_secs(60), token.as_ref()));
    poll_pending(incoming.as_mut()).await;
    token.cancel();
    assert!(bounded(incoming).await.is_err());
    assert!(
        bounded(session.wait_for_incoming_input(Duration::ZERO, caller().as_ref()))
            .await
            .unwrap()
            .is_none()
    );
    bounded(session.close()).await.unwrap();
}

#[tokio::test]
async fn scheduled_work_prepares_current_selection_then_executes_and_failed_drain_cancels_pending()
{
    let (backend, session, _) = fixture();
    *backend.selection.lock().unwrap() = "current".into();
    let observed = Arc::new(Mutex::new(Vec::new()));
    let request = session
        .schedule_work(
            {
                let observed = observed.clone();
                move |selection, cancellation| {
                    Box::pin(async move {
                        assert!(!cancellation.is_cancelled());
                        observed.lock().unwrap().push(selection);
                        Ok(())
                    })
                }
            },
            caller(),
        )
        .unwrap();
    bounded(request.join()).await.unwrap();
    bounded(session.wake(None).1.join()).await.unwrap();
    assert_eq!(*backend.prepared.lock().unwrap(), ["current"]);
    assert_eq!(*observed.lock().unwrap(), ["current"]);
    let pass = PassControl::blocked(true, Execution::failed("failed pass"));
    backend.plan(pass.clone());
    let drain = session.wake(None).1;
    bounded(pass.entered.wait()).await;
    let ran = Arc::new(AtomicBool::new(false));
    let canceled = session
        .schedule_work(
            {
                let ran = ran.clone();
                move |_, _| {
                    Box::pin(async move {
                        ran.store(true, Ordering::SeqCst);
                        Ok(())
                    })
                }
            },
            caller(),
        )
        .unwrap();
    pass.release.release();
    assert_eq!(
        bounded(drain.join()).await.unwrap().status,
        ExecutionStatus::Failed
    );
    assert!(bounded(canceled.join()).await.is_err());
    assert!(!ran.load(Ordering::SeqCst));
    bounded(session.close()).await.unwrap();
}

#[tokio::test]
async fn wait_yields_and_public_execution_bounds_utf8_results_before_finished_callback() {
    let (backend, session, _) = fixture();
    assert!(session.wait(0, caller().as_ref()).await.is_err());
    for fail in [false, true] {
        let oversized = "€".repeat(MAX_AGENT_RESULT_BYTES / 3 + 2);
        let result = if fail {
            Execution::failed(oversized)
        } else {
            Execution::succeeded(oversized)
        };
        let pass = PassControl::blocked(true, result);
        backend.plan(pass.clone());
        let execution = enqueue(&session, if fail { "failure" } else { "success" });
        bounded(pass.entered.wait()).await;
        let yielded = bounded(session.wait(1, caller().as_ref())).await.unwrap();
        assert!(yielded.yielded);
        assert!(yielded.execution.is_none());
        pass.release.release();
        let result = bounded(execution.join()).await.unwrap();
        let bounded_text = if fail { &result.error } else { &result.output };
        assert_eq!(bounded_text.len(), MAX_AGENT_RESULT_BYTES - 1);
        assert!(bounded_text.chars().all(|character| character == '€'));
        let terminal = bounded(session.wait(0, caller().as_ref())).await.unwrap();
        assert!(!terminal.yielded);
        assert_eq!(terminal.execution, Some(result.clone()));
        assert_eq!(backend.finishes.lock().unwrap().last(), Some(&result));
    }
    assert_eq!(backend.starts.load(Ordering::SeqCst), 2);
    bounded(session.close()).await.unwrap();
}

#[tokio::test]
async fn owned_execution_cancellation_joins_and_cancels_its_unpromoted_admission() {
    let (backend, session, _) = fixture();
    let pass = PassControl::blocked(false, Execution::succeeded("unused"));
    backend.plan(pass.clone());
    let token = caller();
    let execution = session
        .enqueue_execution(parts("owned"), "owned".into(), None, None, token.clone())
        .unwrap();
    bounded(pass.entered.wait()).await;
    token.cancel();
    bounded(pass.cancellation_seen.wait()).await;
    assert!(!execution.is_finished());
    pass.cancellation_join.release();
    let completed = bounded(execution.join()).await.unwrap();
    assert_eq!(completed.status, ExecutionStatus::Canceled);
    assert_eq!(completed.error, "interrupted");
    assert_eq!(*backend.canceled_inputs.lock().unwrap(), ["input-owned"]);
    assert!(backend.pending.lock().unwrap().is_empty());
    bounded(session.close()).await.unwrap();
}

#[tokio::test]
async fn session_lifetime_cancels_a_drain_without_transferring_ownership_to_observers() {
    let (backend, session, lifetime) = fixture();
    let pass = PassControl::blocked(true, Execution::succeeded("unused"));
    backend.plan(pass.clone());
    let execution = session.wake(None).1;
    bounded(pass.entered.wait()).await;
    lifetime.cancel();
    bounded(pass.cancellation_seen.wait()).await;
    assert!(!execution.is_finished());
    pass.cancellation_join.release();
    assert_eq!(
        bounded(execution.join()).await.unwrap().status,
        ExecutionStatus::Canceled
    );
    assert!(
        session
            .enqueue_execution(parts("late"), "late".into(), None, None, caller())
            .is_err()
    );
    bounded(session.close()).await.unwrap();
}

#[tokio::test]
async fn record_does_not_wake_and_execution_message_limits_count_utf8_bytes() {
    let (backend, session, _) = fixture();
    bounded(session.record(
        parts("recorded"),
        "recorded".into(),
        Delivery::DELIVERY_QUEUE,
        caller(),
    ))
    .await
    .unwrap();
    assert!(session.is_idle());
    assert_eq!(backend.pass_count.load(Ordering::SeqCst), 0);
    assert_eq!(backend.pending.lock().unwrap().len(), 1);
    assert!(
        session
            .enqueue_execution(parts(" \t\n"), "blank".into(), None, None, caller())
            .is_err()
    );
    assert!(
        session
            .enqueue_execution(
                parts(&"€".repeat(MAX_AGENT_MESSAGE_BYTES / 3 + 1)),
                "large".into(),
                None,
                None,
                caller()
            )
            .is_err()
    );
    bounded(session.recover().join()).await.unwrap();
    assert_eq!(*backend.promoted.lock().unwrap(), ["recorded"]);
    let image_only = session
        .enqueue_execution(
            vec![ConversationPart::image("artifact", "image/png", "image")],
            "image".into(),
            None,
            None,
            caller(),
        )
        .unwrap();
    bounded(image_only.join()).await.unwrap();
    bounded(session.close()).await.unwrap();
}

#[tokio::test]
async fn owned_admission_reserves_drain_before_async_commit_and_concurrent_wake() {
    for cancel_owner in [false, true] {
        let (backend, session, _) = fixture();
        let commit = Arc::new(Gate::default());
        *backend.admit_gate.lock().unwrap() = Some(commit.clone());
        let token = caller();
        let execution = session
            .enqueue_execution(
                parts("reserved"),
                "reserved".into(),
                None,
                None,
                token.clone(),
            )
            .unwrap();
        bounded(backend.admit_entered.wait()).await;
        let (started, selected) = session.wake(None);
        assert!(
            !started,
            "owned admission must already hold the drain reservation"
        );
        tokio::task::yield_now().await;
        assert_eq!(backend.pass_count.load(Ordering::SeqCst), 0);
        assert_eq!(backend.starts.load(Ordering::SeqCst), 0);
        assert!(!selected.is_finished());
        if cancel_owner {
            token.cancel();
        }
        commit.release();
        let result = bounded(execution.join()).await.unwrap();
        assert_eq!(
            result.status,
            if cancel_owner {
                ExecutionStatus::Canceled
            } else {
                ExecutionStatus::Succeeded
            }
        );
        bounded(selected.join()).await.unwrap();
        assert_eq!(*backend.admitted.lock().unwrap(), ["reserved"]);
        if cancel_owner {
            assert!(backend.promoted.lock().unwrap().is_empty());
            assert_eq!(*backend.canceled_inputs.lock().unwrap(), ["input-reserved"]);
        } else {
            assert_eq!(*backend.promoted.lock().unwrap(), ["reserved"]);
        }
        let events = backend.events.lock().unwrap().clone();
        let started_at = events.iter().position(|event| *event == "started").unwrap();
        assert!(
            events
                .iter()
                .position(|event| *event == "pass")
                .is_none_or(|pass_at| started_at < pass_at)
        );
        bounded(session.close()).await.unwrap();
    }
}

#[tokio::test]
async fn fresh_public_send_returns_execution_completion_including_finished_callback() {
    let (backend, session, _) = fixture();
    let finish = Arc::new(Gate::default());
    *backend.finish_gate.lock().unwrap() = Some(finish.clone());
    let sent = bounded(
        session
            .send_and_select_execution(parts("public"), "public".into(), caller())
            .unwrap()
            .join(),
    )
    .await
    .unwrap();
    bounded(backend.finish_entered.wait()).await;
    assert!(
        !sent.execution.is_finished(),
        "public execution cannot complete before its finished callback"
    );
    assert!(backend.finishes.lock().unwrap().is_empty());
    let mut result = Box::pin(sent.execution.join());
    poll_pending(result.as_mut()).await;
    finish.release();
    assert_eq!(
        bounded(result).await.unwrap().status,
        ExecutionStatus::Succeeded
    );
    assert_eq!(backend.finishes.lock().unwrap().len(), 1);
    bounded(session.close()).await.unwrap();
}

fn gated_mutation(
    session: &AgentSession<FakeBackend>,
    release: Arc<Gate>,
    failure: Option<&'static str>,
) -> OperationHandle<()> {
    session
        .owned_mutation(
            move |_, _| {
                Box::pin(async move {
                    release.wait().await;
                    failure.map_or(Ok(()), |message| {
                        Err(LifecycleError::Failed(message.into()))
                    })
                })
            },
            caller(),
        )
        .unwrap()
}

#[tokio::test]
async fn successful_settlement_rechecks_late_mutations_and_inputs_after_each_snapshot() {
    let (backend, session, _) = fixture();
    let pass = PassControl::terminal(Ok(Execution::succeeded("first")));
    let follow_up = PassControl::blocked(true, Execution::succeeded("late input"));
    backend.plan(pass.clone());
    backend.plan(follow_up.clone());
    let execution = enqueue(&session, "initial");
    bounded(pass.entered.wait()).await;

    // The backend already completed its terminal mutation snapshot.
    let first_release = Arc::new(Gate::default());
    let first = gated_mutation(&session, first_release.clone(), None);
    let pending = Arc::new(PendingControl::default());
    *backend.pending_control.lock().unwrap() = Some(pending.clone());
    pass.release.release();
    bounded(pass.exited.wait()).await;
    assert!(!execution.is_finished());
    assert!(!pending.entered.open.load(Ordering::SeqCst));

    first_release.release();
    bounded(pending.entered.wait()).await;
    bounded(first.join()).await.unwrap();
    // A second mutation arrives after the first final-barrier snapshot, while
    // the durable last-look read is suspended. The same admission gate must
    // notice it before settling, without manufacturing another backend pass.
    let second_release = Arc::new(Gate::default());
    let second = gated_mutation(&session, second_release.clone(), None);
    let sent = bounded(session.send(
        parts("late"),
        "late".into(),
        Delivery::DELIVERY_STEER,
        IncomingActivity::default(),
        caller(),
    ))
    .await
    .unwrap();
    pending.release.release();
    bounded(pending.exited.wait()).await;
    assert!(!execution.is_finished());
    assert!(!sent.execution.is_finished());
    assert_eq!(backend.pass_count.load(Ordering::SeqCst), 1);

    second_release.release();
    bounded(second.join()).await.unwrap();
    bounded(follow_up.entered.wait()).await;
    follow_up.release.release();
    assert_eq!(
        bounded(execution.join()).await.unwrap().output,
        "late input"
    );
    assert_eq!(*backend.promoted.lock().unwrap(), ["initial", "late"]);
    bounded(session.close()).await.unwrap();
}

#[tokio::test]
async fn terminal_exits_join_mutation_lifetimes_without_rethrowing_invocation_errors() {
    for mode in ["failed", "canceled", "backend error", "invocation failure"] {
        let (backend, session, _) = fixture();
        let result = match mode {
            "failed" => Ok(Execution::failed("primary failure")),
            "canceled" | "invocation failure" => Ok(Execution::succeeded("original output")),
            _ => Err(LifecycleError::Failed("primary backend error".into())),
        };
        let pass = PassControl::terminal(result);
        backend.plan(pass.clone());
        if mode == "invocation failure" {
            let idle = PassControl::terminal(Ok(Execution::succeeded("")));
            idle.release.release();
            backend.plan(idle);
        }
        let execution = session.wake(None).1;
        bounded(pass.entered.wait()).await;
        let release = Arc::new(Gate::default());
        let mutation = gated_mutation(&session, release.clone(), Some("mutation failure"));
        let interrupt = if mode == "canceled" {
            let interrupt = session.interrupt_handle().unwrap();
            bounded(pass.cancellation_seen.wait()).await;
            pass.cancellation_join.release();
            Some(interrupt)
        } else {
            pass.release.release();
            None
        };
        bounded(pass.exited.wait()).await;
        assert!(!execution.is_finished(), "{mode}");
        assert!(session.is_active(), "{mode}");
        if let Some(interrupt) = &interrupt {
            assert!(!interrupt.is_finished());
        }
        release.release();
        assert_eq!(
            bounded(mutation.join()).await,
            Err(LifecycleError::Failed("mutation failure".into()))
        );
        match mode {
            "failed" => assert_eq!(
                bounded(execution.join()).await.unwrap(),
                Execution::failed("primary failure")
            ),
            "canceled" => {
                assert_eq!(
                    bounded(execution.join()).await.unwrap(),
                    Execution::canceled()
                );
                bounded(interrupt.unwrap().join()).await.unwrap();
            }
            "invocation failure" => assert_eq!(
                bounded(execution.join()).await.unwrap(),
                Execution::succeeded("original output")
            ),
            _ => assert_eq!(
                bounded(execution.join()).await,
                Err(LifecycleError::Failed("primary backend error".into()))
            ),
        }
        assert!(!session.is_active());
        bounded(session.close()).await.unwrap();
    }
}

#[tokio::test]
async fn failed_and_cancellation_returned_mutations_preserve_errors_without_failing_next_pass() {
    for expected in [
        LifecycleError::Failed("reminder mutation failed after commit".into()),
        LifecycleError::Failed("reminder publication cancelled after commit".into()),
        LifecycleError::Failed("operation cancelled".into()),
        LifecycleError::Cancelled,
    ] {
        let (backend, session, _) = fixture();
        let returned = expected.clone();
        let mutation = session
            .owned_mutation(
                move |_, _| Box::pin(async move { Err::<String, _>(returned) }),
                caller(),
            )
            .unwrap();
        assert_eq!(bounded(mutation.join()).await, Err(expected));

        // The next actual backend terminal snapshot must join the completed
        // task without interpreting its already-observed invocation error.
        let pass = PassControl::terminal(Ok(Execution::succeeded("successful answer")));
        backend.plan(pass.clone());
        let execution = enqueue(&session, "after mutation");
        bounded(pass.entered.wait()).await;
        pass.release.release();
        assert_eq!(
            bounded(execution.join()).await.unwrap(),
            Execution::succeeded("successful answer")
        );
        assert_eq!(backend.pass_count.load(Ordering::SeqCst), 1);
        bounded(session.close()).await.unwrap();
        assert!(backend.closed.load(Ordering::SeqCst));
    }
}

#[tokio::test]
async fn mutation_panics_settle_observer_and_lifetime_barrier_and_remain_close_failures() {
    for panic_before_future in [true, false] {
        let (backend, session, _) = fixture();
        let mutation = session
            .owned_mutation::<(), _>(
                move |_, _| {
                    assert!(!panic_before_future, "mutation closure panic");
                    Box::pin(async { panic!("mutation future panic") })
                },
                caller(),
            )
            .unwrap();
        let cleanup =
            LifecycleError::Failed("owned agent mutation task ended before completion".into());
        assert_eq!(bounded(mutation.join()).await, Err(cleanup.clone()));
        assert_eq!(bounded(session.join_mutations()).await, Err(cleanup));
        assert!(!session.is_active());
        let error = bounded(session.close()).await.unwrap_err();
        assert!(error.to_string().contains(if panic_before_future {
            "mutation closure panic"
        } else {
            "mutation future panic"
        }));
        assert!(backend.closed.load(Ordering::SeqCst));
    }
}

#[tokio::test]
async fn persistence_error_cleanup_rechecks_mutations_accepted_while_joining() {
    let (backend, session, _) = fixture();
    let pass = PassControl::terminal(Ok(Execution::succeeded("unused")));
    backend.plan(pass.clone());
    let execution = session.wake(None).1;
    bounded(pass.entered.wait()).await;
    let pending = Arc::new(PendingControl {
        failure: Some(LifecycleError::Failed("durable read failed".into())),
        ..PendingControl::default()
    });
    *backend.pending_control.lock().unwrap() = Some(pending.clone());
    pass.release.release();
    bounded(pending.entered.wait()).await;
    let first_release = Arc::new(Gate::default());
    let first = gated_mutation(&session, first_release.clone(), None);
    pending.release.release();
    bounded(pending.exited.wait()).await;
    assert!(!execution.is_finished());

    // The error-cleanup snapshot is now awaiting first. A second accepted
    // mutation must keep that same drain owned after first completes.
    let second_release = Arc::new(Gate::default());
    let second = gated_mutation(&session, second_release.clone(), Some("secondary failure"));
    first_release.release();
    bounded(first.join()).await.unwrap();
    assert!(!execution.is_finished());
    assert!(session.is_active());
    second_release.release();
    assert!(bounded(second.join()).await.is_err());
    assert_eq!(
        bounded(execution.join()).await,
        Err(LifecycleError::Failed("durable read failed".into()))
    );
    assert!(!session.is_active());
    bounded(session.close()).await.unwrap();
}

#[tokio::test]
async fn idle_session_is_active_only_while_its_owned_mutation_is_unfinished() {
    let (_, session, _) = fixture();
    let release = Arc::new(Gate::default());
    assert!(!session.is_active());
    let mutation = gated_mutation(&session, release.clone(), None);
    assert!(session.is_idle());
    assert!(session.is_active());
    release.release();
    bounded(mutation.join()).await.unwrap();
    assert!(session.is_idle());
    assert!(!session.is_active());
    bounded(session.close()).await.unwrap();
}

#[test]
fn lifecycle_error_kind_is_independent_of_its_display_message() {
    assert!(LifecycleError::Cancelled.is_cancelled());
    assert_eq!(LifecycleError::Cancelled.to_string(), "operation cancelled");
    let failure = LifecycleError::Failed("operation cancelled".into());
    assert!(!failure.is_cancelled());
    assert_eq!(failure.to_string(), "operation cancelled");
    assert!(!LifecycleError::ShuttingDown.is_cancelled());
    assert_eq!(
        LifecycleError::ShuttingDown.to_string(),
        "the user session is shutting down"
    );
}

#[tokio::test]
async fn public_execution_classifies_only_typed_cancellation_as_canceled() {
    for owned in [false, true] {
        for error in [
            LifecycleError::Failed("operation cancelled".into()),
            LifecycleError::Cancelled,
        ] {
            let (backend, session, _) = fixture();
            let expected = if error.is_cancelled() {
                Execution::canceled()
            } else {
                Execution::failed("operation cancelled")
            };
            let pass = PassControl::terminal(Err(error));
            backend.plan(pass.clone());
            let execution = if owned {
                enqueue(&session, "typed error")
            } else {
                bounded(
                    session
                        .send_and_select_execution(
                            parts("typed error"),
                            "typed error".into(),
                            caller(),
                        )
                        .unwrap()
                        .join(),
                )
                .await
                .unwrap()
                .execution
            };
            bounded(pass.entered.wait()).await;
            pass.release.release();
            assert_eq!(bounded(execution.join()).await.unwrap(), expected);
            assert_eq!(backend.finishes.lock().unwrap().last(), Some(&expected));
            bounded(session.close()).await.unwrap();
        }
    }
}

#[tokio::test]
async fn accepted_queue_cancellation_is_distinct_from_new_admission_during_shutdown() {
    for close_session in [false, true] {
        let (backend, session, lifetime) = fixture();
        let pass = PassControl::blocked(true, Execution::succeeded("unused"));
        backend.plan(pass.clone());
        let first = enqueue(&session, "active");
        bounded(pass.entered.wait()).await;
        let queued = enqueue(&session, "queued");
        let close = if close_session {
            Some(session.close_handle())
        } else {
            lifetime.cancel();
            None
        };
        bounded(pass.cancellation_seen.wait()).await;
        assert_eq!(bounded(queued.join()).await, Err(LifecycleError::Cancelled));
        assert_eq!(
            session
                .enqueue_execution(parts("new"), "new".into(), None, None, caller())
                .err()
                .unwrap(),
            LifecycleError::ShuttingDown
        );
        assert_eq!(*backend.admitted.lock().unwrap(), ["active"]);
        assert!(!first.is_finished());
        pass.cancellation_join.release();
        assert_eq!(bounded(first.join()).await.unwrap(), Execution::canceled());
        if let Some(close) = close {
            bounded(close.join()).await.unwrap();
        }
        bounded(session.close()).await.unwrap();
    }
}

#[tokio::test]
async fn drain_state_callbacks_follow_actual_interrupt_and_close_transitions() {
    let (backend, session, _) = fixture();
    let pass = PassControl::blocked(false, Execution::succeeded("unused"));
    backend.plan(pass.clone());
    session.wake(None);
    bounded(pass.entered.wait()).await;
    let first = session.interrupt_handle().unwrap();
    let second = session.interrupt_handle().unwrap();
    bounded(pass.cancellation_seen.wait()).await;
    assert_eq!(
        *backend.states.lock().unwrap(),
        [DrainState::Running, DrainState::Interrupting]
    );
    pass.cancellation_join.release();
    bounded(first.join()).await.unwrap();
    bounded(second.join()).await.unwrap();
    assert_eq!(
        *backend.states.lock().unwrap(),
        [
            DrainState::Running,
            DrainState::Interrupting,
            DrainState::Idle
        ]
    );

    let pass = PassControl::blocked(false, Execution::succeeded("unused"));
    backend.plan(pass.clone());
    session.wake(None);
    bounded(pass.entered.wait()).await;
    let close = session.close_handle();
    bounded(pass.cancellation_seen.wait()).await;
    assert_eq!(
        backend.states.lock().unwrap().last(),
        Some(&DrainState::Interrupting)
    );
    pass.cancellation_join.release();
    bounded(close.join()).await.unwrap();
    bounded(session.close()).await.unwrap();
    assert_eq!(
        *backend.states.lock().unwrap(),
        [
            DrainState::Running,
            DrainState::Interrupting,
            DrainState::Idle,
            DrainState::Running,
            DrainState::Interrupting,
            DrainState::Idle,
        ]
    );
}
