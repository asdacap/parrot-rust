use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use parrot_domain::contracts::async_ops::{BoxFuture, Cancellation};
use parrot_domain::contracts::events::{EventSubscription, SubscriptionLimits};
use parrot_domain::protocol::messages::parrot::{Delivery, Event, InputSource};
use parrot_domain::values::agent_state::{ExecutionCheckpoint, ExecutionTerminal};
use parrot_runtime::agent_turn::lifecycle::{
    AgentSession, DrainBackend, DrainPass, DrainState, DrainWork, Execution,
    ExecutionResultReference, ExecutionStatus, LifecycleError, Signal,
};
use parrot_runtime::event_broker::EventBroker;
use parrot_storage::events::{Admission, AdmittedInput, ConversationPart, NeverCancelled};
use tokio::sync::Semaphore;

use crate::support::{Case, Generator, cases, seeds};

async fn bounded<T>(future: impl std::future::Future<Output = T>) -> T {
    tokio::time::timeout(Duration::from_secs(5), future)
        .await
        .expect("T77 controlled scheduler barrier exceeded its committed five-second budget")
}

async fn pending(future: impl std::future::Future) {
    tokio::pin!(future);
    std::future::poll_fn(|context| {
        assert!(future.as_mut().poll(context).is_pending());
        std::task::Poll::Ready(())
    })
    .await;
}

struct Backend {
    entered: Semaphore,
    release: Semaphore,
    passes: AtomicUsize,
    active: AtomicUsize,
    maximum_active: AtomicUsize,
    starts: AtomicUsize,
    finishes: AtomicUsize,
    closes: AtomicUsize,
}

impl Backend {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            entered: Semaphore::new(0),
            release: Semaphore::new(0),
            passes: AtomicUsize::new(0),
            active: AtomicUsize::new(0),
            maximum_active: AtomicUsize::new(0),
            starts: AtomicUsize::new(0),
            finishes: AtomicUsize::new(0),
            closes: AtomicUsize::new(0),
        })
    }
}

impl DrainBackend for Backend {
    type Selection = ();

    fn admit(
        &self,
        _: Vec<ConversationPart>,
        _: String,
        _: Delivery,
        _: InputSource,
    ) -> BoxFuture<'_, Result<Admission, LifecycleError>> {
        Box::pin(async {
            panic!("wake/mutation stress must not fabricate durable input admission")
        })
    }

    fn find_execution_input(
        &self,
        _: String,
    ) -> BoxFuture<'_, Result<Option<AdmittedInput>, LifecycleError>> {
        Box::pin(async { panic!("wake/mutation stress has no durable execution inputs") })
    }

    fn publish_admission<'a>(
        &'a self,
        _: &'a Admission,
        _: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<(), LifecycleError>> {
        Box::pin(async { panic!("wake/mutation stress must not fabricate publication") })
    }

    fn has_recovery_work_after(&self, _: i64) -> BoxFuture<'_, Result<bool, LifecycleError>> {
        Box::pin(async { panic!("wake/mutation stress has no persistent recovery work") })
    }

    fn has_pending_inputs(&self) -> BoxFuture<'_, Result<bool, LifecycleError>> {
        Box::pin(async { Ok(false) })
    }

    fn cancel_pending_input(&self, _: String) -> BoxFuture<'_, Result<(), LifecycleError>> {
        Box::pin(async { panic!("wake/mutation stress has no admitted inputs") })
    }

    fn update_selection(&self, _: ()) -> Result<(), LifecycleError> {
        Ok(())
    }

    fn drain_state_changed(&self, _: DrainState) {}

    fn pass<'a>(
        &'a self,
        cancellation: &'a dyn Cancellation,
        work: &'a dyn DrainWork,
    ) -> BoxFuture<'a, Result<DrainPass, LifecycleError>> {
        Box::pin(async move {
            let active = self.active.fetch_add(1, Ordering::SeqCst) + 1;
            self.maximum_active.fetch_max(active, Ordering::SeqCst);
            let pass = self.passes.fetch_add(1, Ordering::SeqCst);
            if pass == 0 {
                self.entered.add_permits(1);
                tokio::select! {
                    permit = self.release.acquire() => permit.unwrap().forget(),
                    _ = cancellation.cancelled() => {
                        self.active.fetch_sub(1, Ordering::SeqCst);
                        return Ok(DrainPass {
                            execution: Execution::canceled(),
                            result_reference: ExecutionResultReference::Missing,
                            recovery_watermark: 0,
                        });
                    }
                }
            }
            work.join_mutations().await?;
            self.active.fetch_sub(1, Ordering::SeqCst);
            Ok(DrainPass {
                execution: Execution::succeeded("t77-controlled-pass"),
                result_reference: ExecutionResultReference::Missing,
                recovery_watermark: 0,
            })
        })
    }

    fn prepare_scheduled<'a>(
        &'a self,
        _: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<(), LifecycleError>> {
        Box::pin(async { Ok(()) })
    }

    fn execution_started(&self) -> BoxFuture<'_, Result<(), LifecycleError>> {
        Box::pin(async move {
            self.starts.fetch_add(1, Ordering::SeqCst);
            Ok(())
        })
    }

    fn execution_finished<'a>(
        &'a self,
        _: &'a Execution,
    ) -> BoxFuture<'a, Result<(), LifecycleError>> {
        Box::pin(async move {
            self.finishes.fetch_add(1, Ordering::SeqCst);
            Ok(())
        })
    }

    fn execution_started_correlated(&self, _: &str) -> BoxFuture<'_, Result<(), LifecycleError>> {
        Box::pin(async { panic!("wake/mutation stress has no durable execution starts") })
    }

    fn execution_finished_correlated<'a>(
        &'a self,
        _: &'a Execution,
        _: &'a str,
    ) -> BoxFuture<'a, Result<(), LifecycleError>> {
        Box::pin(async { panic!("wake/mutation stress has no durable execution finishes") })
    }

    fn cancel_execution_input(&self, _: &str) -> BoxFuture<'_, Result<i64, LifecycleError>> {
        Box::pin(async { panic!("wake/mutation stress has no durable execution inputs") })
    }

    fn execution_watermark(&self) -> BoxFuture<'_, Result<i64, LifecycleError>> {
        Box::pin(async { panic!("wake/mutation stress has no durable conversation") })
    }

    fn restore_execution<'a>(
        &'a self,
        _: &'a ExecutionTerminal,
    ) -> BoxFuture<'a, Result<Execution, LifecycleError>> {
        Box::pin(async { panic!("wake/mutation stress has no saved execution") })
    }

    fn capture_execution<'a>(
        &'a self,
        _: &'a ExecutionCheckpoint,
        _: &'a mut Execution,
        _: ExecutionResultReference,
        _: i64,
    ) -> BoxFuture<'a, Result<ExecutionTerminal, LifecycleError>> {
        Box::pin(async { panic!("wake/mutation stress does not persist executions") })
    }

    fn suspend_execution(&self) {
        panic!("wake/mutation stress does not suspend durable executions")
    }

    fn close(&self) -> BoxFuture<'_, Result<(), LifecycleError>> {
        Box::pin(async move {
            assert_eq!(self.active.load(Ordering::SeqCst), 0);
            self.closes.fetch_add(1, Ordering::SeqCst);
            Ok(())
        })
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn seeded_concurrent_passive_wakes_share_one_drain_without_execution_callbacks() {
    // Fixed one-wake and maximum-participant regressions always precede seeded schedules.
    for participants in [1, 8] {
        exercise_wakes(participants, "T77 fixed wake regression").await;
    }
    for seed in seeds() {
        let mut generator = Generator::new(seed);
        for index in 0..cases("wake_race") {
            let case = Case {
                domain: "wake_race",
                seed,
                index,
            };
            exercise_wakes(1 + generator.pick(8), &case.to_string()).await;
        }
        println!(
            "T77_END domain=wake_race seed=0x{seed:016x} completed={}",
            cases("wake_race")
        );
    }
}

async fn exercise_wakes(participants: usize, context: &str) {
    let backend = Backend::new();
    let session = AgentSession::new(backend.clone(), Arc::new(NeverCancelled));
    let (started, first) = session.wake(None);
    bounded(backend.entered.acquire()).await.unwrap().forget();
    let mut joins = Vec::new();
    for _ in 0..participants {
        let session = session.clone();
        joins.push(tokio::spawn(async move { session.wake(None) }));
    }
    let mut results = Vec::new();
    for join in joins {
        results.push(bounded(join).await.unwrap());
    }
    let unfinished = results
        .iter()
        .all(|(started, handle)| !started && !handle.is_finished());
    backend.release.add_permits(1);
    let result = bounded(first.join()).await.unwrap();
    bounded(session.close()).await.unwrap();
    assert!(started && unfinished, "{context}");
    assert_eq!(result.status, ExecutionStatus::Succeeded, "{context}");
    assert_eq!(
        backend.maximum_active.load(Ordering::SeqCst),
        1,
        "{context}"
    );
    assert_eq!(backend.passes.load(Ordering::SeqCst), 2, "{context}");
    // Passive wake(None) reserves no input execution; callbacks must not be fabricated.
    assert_eq!(backend.starts.load(Ordering::SeqCst), 0, "{context}");
    assert_eq!(backend.finishes.load(Ordering::SeqCst), 0, "{context}");
    assert_eq!(backend.closes.load(Ordering::SeqCst), 1, "{context}");
    assert!(session.is_idle(), "{context}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn seeded_cancelled_or_dropped_observer_cannot_abandon_admitted_mutation_on_close() {
    for cancelled_observer in [false, true] {
        exercise_mutation(cancelled_observer, 0, "T77 fixed mutation regression").await;
    }
    for seed in seeds() {
        let mut generator = Generator::new(seed);
        for index in 0..cases("mutation_close_race") {
            let case = Case {
                domain: "mutation_close_race",
                seed,
                index,
            };
            exercise_mutation(generator.pick(2) == 0, generator.next(), &case.to_string()).await;
        }
        println!(
            "T77_END domain=mutation_close_race seed=0x{seed:016x} completed={}",
            cases("mutation_close_race")
        );
    }
}

async fn exercise_mutation(cancelled_observer: bool, value: u64, context: &str) {
    let backend = Backend::new();
    let session = AgentSession::new(backend.clone(), Arc::new(NeverCancelled));
    let entered = Arc::new(Semaphore::new(0));
    let release = Arc::new(Semaphore::new(0));
    let caller = Arc::new(Signal::new());
    let mutation = session
        .owned_mutation(
            {
                let entered = entered.clone();
                let release = release.clone();
                move |_, lifetime| {
                    Box::pin(async move {
                        entered.add_permits(1);
                        release.acquire().await.unwrap().forget();
                        assert!(!lifetime.is_cancelled());
                        Ok(value)
                    })
                }
            },
            caller.clone(),
        )
        .unwrap();
    bounded(entered.acquire()).await.unwrap().forget();
    if cancelled_observer {
        caller.cancel();
        assert_eq!(
            mutation.wait(caller.as_ref()).await,
            Err(LifecycleError::Cancelled),
            "{context}"
        );
    } else {
        pending(mutation.wait(&NeverCancelled)).await;
    }
    let close = session.close_handle();
    pending(close.join()).await;
    let blocked = !mutation.is_finished()
        && !close.is_finished()
        && backend.closes.load(Ordering::SeqCst) == 0;
    release.add_permits(1);
    let completed = bounded(mutation.join()).await.unwrap();
    bounded(close.join()).await.unwrap();
    assert!(blocked, "{context}");
    assert_eq!(completed, value, "{context}");
    assert_eq!(backend.closes.load(Ordering::SeqCst), 1, "{context}");
    assert_eq!(backend.passes.load(Ordering::SeqCst), 0, "{context}");
    assert!(
        matches!(
            session.owned_mutation(|_, _| Box::pin(async { Ok(()) }), Arc::new(NeverCancelled)),
            Err(LifecycleError::ShuttingDown)
        ),
        "{context}"
    );
}

fn event(id: &str) -> Event {
    let mut event = Event::new();
    event.id = id.into();
    event
}

#[tokio::test]
async fn seeded_subscribe_before_replay_preserves_duplicates_and_defers_snapshot_capture() {
    exercise_replay(2, 0, "T77 fixed replay/live duplicate regression").await;
    for seed in seeds() {
        let mut generator = Generator::new(seed);
        for index in 0..cases("replay_live_race") {
            let case = Case {
                domain: "replay_live_race",
                seed,
                index,
            };
            let count = 2 + generator.pick(7);
            exercise_replay(count, generator.pick(count), &case.to_string()).await;
        }
        println!(
            "T77_END domain=replay_live_race seed=0x{seed:016x} completed={}",
            cases("replay_live_race")
        );
    }
}

async fn exercise_replay(count: usize, duplicate: usize, context: &str) {
    let broker = EventBroker::with_limits(SubscriptionLimits {
        retained: 32,
        transient: 32,
    })
    .unwrap();
    let release = Arc::new(Semaphore::new(0));
    let captures = Arc::new(AtomicUsize::new(0));
    let mut persisted: Vec<_> = (0..count)
        .map(|index| event(&format!("e{index}")))
        .collect();
    persisted.insert(duplicate, persisted[duplicate].clone());
    let listen = broker.listen(
        Some(""),
        {
            let release = release.clone();
            let persisted = persisted.clone();
            move || async move {
                release.acquire().await.unwrap().forget();
                Ok::<_, ()>(persisted)
            }
        },
        {
            let captures = captures.clone();
            move || async move {
                captures.fetch_add(1, Ordering::SeqCst);
                Ok::<_, ()>(vec![event("initial-snapshot")])
            }
        },
    );
    tokio::pin!(listen);
    std::future::poll_fn(|context| {
        assert!(listen.as_mut().poll(context).is_pending());
        std::task::Poll::Ready(())
    })
    .await;
    for index in 0..count {
        broker.publish_retained(&event(&format!("e{index}")));
    }
    broker.publish_retained(&event(&format!("e{duplicate}")));
    broker.publish_retained(&event("live-only"));
    release.add_permits(1);
    let stream = bounded(listen).await.unwrap();
    let mut replayed = Vec::new();
    for _ in 0..persisted.len() {
        replayed.push(
            bounded(stream.next(&NeverCancelled))
                .await
                .unwrap()
                .unwrap(),
        );
    }
    let captured_before_tail = captures.load(Ordering::SeqCst);
    broker.complete();
    let mut tail = Vec::new();
    while let Some(next) = bounded(stream.next(&NeverCancelled)).await.unwrap() {
        tail.push(next.id);
    }
    assert_eq!(replayed, persisted, "{context}");
    assert_eq!(captured_before_tail, 0, "{context}");
    assert_eq!(captures.load(Ordering::SeqCst), 1, "{context}");
    assert_eq!(
        tail,
        vec![
            "initial-snapshot".to_owned(),
            format!("e{duplicate}"),
            "live-only".to_owned()
        ],
        "{context}"
    );
}
