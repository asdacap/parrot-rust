//! Executable contract models, not production SQLite/provider/runtime acceptance.
use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::task::Poll;
use std::time::{Duration, SystemTime};

use parrot_contract_checks::*;
use parrot_domain::contracts::async_ops::OperationResult;
use parrot_domain::contracts::events::{
    EventPublisher, EventSubscription, RetentionClass, SubscriptionLimits,
};
use parrot_domain::contracts::paths::*;
use parrot_domain::contracts::security::{AccessSnapshot, InvocationContext, SecurityProvider};
use parrot_domain::contracts::storage::Committed;
use parrot_domain::contracts::tools::{ParallelSafety, Tool};

fn root(origin: RootOrigin) -> SelectedSessionRoot {
    SelectedSessionRoot::new(
        PathBuf::from(match origin {
            RootOrigin::Current => "/synthetic/current/s1",
            RootOrigin::Legacy => "/synthetic/legacy/s1",
        }),
        origin,
    )
    .unwrap()
}
fn event(key: u64) -> Event {
    Event {
        key,
        turn: 1,
        payload: vec![key as u8],
        delivery: Delivery::Foreground,
        terminal: false,
    }
}

#[test]
fn cancellation_and_fake_clock_require_explicit_owner_progress() {
    let cancel = Cancel::default();
    let mut wait = cancel.cancelled();
    assert!(step(wait.as_mut()).is_pending());
    assert!(cancel.check().is_ok());
    cancel.cancel();
    assert_eq!(step(wait.as_mut()), Poll::Ready(()));
    assert!(cancel.check().is_err());
    let clock: &dyn Clock = &FakeClock::default();
    assert_eq!(clock.utc_now(), SystemTime::UNIX_EPOCH);
    assert!(ready(clock.sleep_until(clock.now(), &cancel)).is_err());
    let cancel = Cancel::default();
    let clock = FakeClock::default();
    let before = clock.now();
    let mut sleep = clock.sleep_until(before + Duration::from_secs(4), &cancel);
    assert!(step(sleep.as_mut()).is_pending());
    clock.advance(Duration::from_secs(3));
    assert!(step(sleep.as_mut()).is_pending());
    clock.advance(Duration::from_secs(1));
    assert_eq!(step(sleep.as_mut()), Poll::Ready(Ok(())));
    assert_eq!(clock.now() - before, Duration::from_secs(4));
    assert_eq!(
        clock.utc_now(),
        SystemTime::UNIX_EPOCH + Duration::from_secs(4)
    );
}

#[test]
fn selected_root_is_immutable_internal_resources_never_mix_trees() {
    let selected = root(RootOrigin::Legacy);
    let artifact = selected.resolve_internal("agents/a/events").unwrap();
    assert_eq!(artifact.selected_root(), &selected);
    assert_eq!(
        artifact.path(),
        Path::new("/synthetic/legacy/s1/agents/a/events")
    );
    assert_eq!(artifact.relative_path(), Path::new("agents/a/events"));
    let current = root(RootOrigin::Current);
    let mut files = std::collections::BTreeMap::new();
    files.insert(
        current
            .resolve_internal("agents/a/events")
            .unwrap()
            .path()
            .to_owned(),
        "wrong tree",
    );
    // The fake resource consumer uses only the bound path. It cannot silently
    // discover a matching file from a different root when this path is absent.
    assert_eq!(files.get(artifact.path()), None);
    assert_eq!(selected.origin(), RootOrigin::Legacy);
    assert!(SelectedSessionRoot::new("relative/s1".into(), RootOrigin::Current).is_err());
    assert!(SelectedSessionRoot::new("/synthetic/../escape".into(), RootOrigin::Current).is_err());
    for invalid in ["", ".", "../secret", "a/../b", "/absolute", "./private"] {
        assert!(
            selected.resolve_internal(invalid).is_err(),
            "accepted {invalid}"
        );
    }
    // Backslashes / Windows drive letters are normal filename bytes on Unix;
    // platform prefixes are rejected by Component::Prefix on Windows. This is
    // lexical validation only, with no user-path or symlink security claim.
    assert_eq!(selected.path(), Path::new("/synthetic/legacy/s1"));
}

#[test]
fn actual_resolved_target_and_per_invocation_owned_snapshots_control_tools() {
    let policy = Policy::default();
    *policy.0.lock().unwrap() = vec!["/allowed".into()];
    let first = policy.capture();
    *policy.0.lock().unwrap() = vec!["/legacy".into()];
    let second = policy.capture();
    let fallback = ResolvedRead::new("/allowed/missing".into(), "/legacy/actual".into());
    assert_eq!(first.check_read(&fallback), Err(Fault::Denied));
    assert_eq!(second.check_read(&fallback), Ok(()));
    assert_eq!(fallback.requested_path(), Path::new("/allowed/missing"));
    let cancel = Cancel::default();
    let tool = FakeTool::default();
    let port: &dyn Tool<
        Definition = String,
        Invocation = Invocation,
        Selection = u64,
        Security = Snapshot,
        Output = PathBuf,
        Error = Fault,
    > = &tool;
    assert_eq!(port.definition(), "checked-file-model");
    assert!(!port.enabled_after_interruption());
    assert_eq!(
        port.parallel_safety(&Invocation::Read(fallback.clone())),
        ParallelSafety::ParallelSafe
    );
    assert_eq!(
        port.parallel_safety(&Invocation::Write("/legacy/actual".into())),
        ParallelSafety::Sequential
    );
    assert_eq!(
        ready(port.invoke(
            Invocation::Read(fallback.clone()),
            InvocationContext::new(first.clone(), 11, &cancel)
        )),
        Err(OperationError::Failed(Fault::Denied))
    );
    assert_eq!(
        ready(port.invoke(
            Invocation::Read(fallback),
            InvocationContext::new(second, 12, &cancel)
        )),
        Ok(PathBuf::from("/legacy/actual"))
    );
    // A previously captured snapshot retains its original policy after mutation.
    assert_eq!(
        ready(port.invoke(
            Invocation::Write("/allowed/output".into()),
            InvocationContext::new(first, 13, &cancel)
        )),
        Ok(PathBuf::from("/allowed/output"))
    );
    assert_eq!(
        tool.invocations.lock().unwrap().as_slice(),
        &[
            (12, PathBuf::from("/legacy/actual")),
            (13, PathBuf::from("/allowed/output"))
        ]
    );
    cancel.cancel();
    assert_eq!(
        ready(port.invoke(
            Invocation::Write("/allowed/output".into()),
            InvocationContext::new(policy.capture(), 14, &cancel)
        )),
        Err(OperationError::Cancelled)
    );
}

#[derive(Clone)]
struct Batch {
    class: RetentionClass,
    inventory: Option<(u64, u64)>,
    started: bool,
    events: VecDeque<Event>,
}
struct BrokerState {
    batches: VecDeque<Batch>,
    completed: bool,
}
struct Broker {
    limits: SubscriptionLimits,
    state: Mutex<BrokerState>,
}
impl Broker {
    fn new(limits: SubscriptionLimits) -> Self {
        Self {
            limits,
            state: Mutex::new(BrokerState {
                batches: VecDeque::new(),
                completed: false,
            }),
        }
    }
    fn publish_batch(
        &self,
        class: RetentionClass,
        inventory: Option<(u64, u64)>,
        events: Vec<Event>,
    ) -> Result<(), Fault> {
        let mut state = self.state.lock().unwrap();
        if state.completed {
            return Err(Fault::Closed);
        }
        if let Some(identity) = inventory {
            state
                .batches
                .retain(|batch| batch.started || batch.inventory != Some(identity));
        }
        let capacity = match class {
            RetentionClass::Retained => self.limits.retained,
            RetentionClass::Transient => self.limits.transient,
        };
        // Whole-batch eviction models contiguous delivery: no interleaving of
        // inventory chunks. Over-capacity batches are dropped whole, never wait.
        if events.len() > capacity {
            return Ok(());
        }
        while state
            .batches
            .iter()
            .filter(|batch| batch.class == class)
            .map(|batch| batch.events.len())
            .sum::<usize>()
            + events.len()
            > capacity
        {
            let index = state
                .batches
                .iter()
                .position(|batch| batch.class == class)
                .unwrap();
            state.batches.remove(index);
        }
        if !events.is_empty() {
            state.batches.push_back(Batch {
                class,
                inventory,
                started: false,
                events: events.into(),
            });
        }
        Ok(())
    }
}
impl EventPublisher for Broker {
    type Event = Event;
    type Error = Fault;
    fn publish_committed<'a>(
        &'a self,
        event: &'a Committed<Event>,
        cancel: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<(), OperationError<Fault>>> {
        Box::pin(async move {
            cancel.check().map_err(|_| OperationError::Cancelled)?;
            self.publish_batch(RetentionClass::Retained, None, vec![event.value().clone()])
                .map_err(OperationError::Failed)
        })
    }
    fn publish_transient<'a>(
        &'a self,
        event: Event,
        cancel: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<(), OperationError<Fault>>> {
        Box::pin(async move {
            cancel.check().map_err(|_| OperationError::Cancelled)?;
            self.publish_batch(RetentionClass::Transient, None, vec![event])
                .map_err(OperationError::Failed)
        })
    }
}
impl EventSubscription for Broker {
    type Event = Event;
    type Error = Fault;
    fn next<'a>(
        &'a self,
        cancel: &'a dyn Cancellation,
    ) -> BoxFuture<'a, OperationResult<Option<Event>, Fault>> {
        Box::pin(std::future::poll_fn(move |_| {
            if cancel.is_cancelled() {
                return Poll::Ready(Err(OperationError::Cancelled));
            }
            let mut state = self.state.lock().unwrap();
            if let Some(batch) = state.batches.front_mut() {
                batch.started = true;
                let event = batch.events.pop_front().unwrap();
                if batch.events.is_empty() {
                    state.batches.pop_front();
                }
                Poll::Ready(Ok(Some(event)))
            } else if state.completed {
                Poll::Ready(Ok(None))
            } else {
                Poll::Pending
            }
        }))
    }
    fn complete(&self) -> Result<(), Fault> {
        self.state.lock().unwrap().completed = true;
        Ok(())
    }
}

#[test]
fn retention_classes_are_independent_nonblocking_and_complete_drains() {
    assert_eq!(
        SubscriptionLimits::default(),
        SubscriptionLimits {
            retained: 1024,
            transient: 1024
        }
    );
    let broker = Broker::new(SubscriptionLimits {
        retained: 2,
        transient: 1,
    });
    let cancel = Cancel::default();
    for key in [1, 2, 3] {
        ready(broker.publish_committed(&Committed::after_commit(event(key)), &cancel)).unwrap();
    }
    for key in [4, 5] {
        ready(broker.publish_transient(event(key), &cancel)).unwrap();
    }
    // Immediate ready publication even while full: bounded lossy, not waiting
    // for consumer progress. Transient pressure did not evict retained events.
    assert_eq!(ready(broker.next(&cancel)).unwrap().unwrap().key, 2);
    assert_eq!(ready(broker.next(&cancel)).unwrap().unwrap().key, 3);
    assert_eq!(ready(broker.next(&cancel)).unwrap().unwrap().key, 5);
    let mut terminal = event(6);
    terminal.terminal = true;
    ready(broker.publish_committed(&Committed::after_commit(terminal), &cancel)).unwrap();
    assert!(ready(broker.next(&cancel)).unwrap().unwrap().terminal);
    let mut waiting = broker.next(&cancel);
    assert!(step(waiting.as_mut()).is_pending()); // Foreground end != stream end.
    drop(waiting);
    ready(broker.publish_transient(event(7), &cancel)).unwrap();
    broker.complete().unwrap();
    assert_eq!(ready(broker.next(&cancel)).unwrap().unwrap().key, 7);
    assert_eq!(ready(broker.next(&cancel)), Ok(None));
    assert_eq!(
        ready(broker.publish_transient(event(8), &cancel)),
        Err(OperationError::Failed(Fault::Closed))
    );
}

#[test]
fn inventory_replacement_is_by_kind_owner_and_batches_stay_contiguous() {
    let broker = Broker::new(SubscriptionLimits {
        retained: 8,
        transient: 8,
    });
    broker
        .publish_batch(
            RetentionClass::Retained,
            Some((1, 10)),
            vec![event(1), event(2)],
        )
        .unwrap();
    broker
        .publish_batch(
            RetentionClass::Retained,
            Some((1, 20)),
            vec![event(3), event(4)],
        )
        .unwrap();
    broker
        .publish_batch(RetentionClass::Retained, Some((2, 10)), vec![event(5)])
        .unwrap();
    broker
        .publish_batch(
            RetentionClass::Retained,
            Some((1, 10)),
            vec![event(6), event(7)],
        )
        .unwrap();
    broker.complete().unwrap();
    let cancel = Cancel::default();
    let mut keys = Vec::new();
    while let Some(event) = ready(broker.next(&cancel)).unwrap() {
        keys.push(event.key);
    }
    assert_eq!(keys, vec![3, 4, 5, 6, 7]);

    let broker = Broker::new(SubscriptionLimits {
        retained: 8,
        transient: 8,
    });
    broker
        .publish_batch(
            RetentionClass::Retained,
            Some((1, 10)),
            vec![event(1), event(2)],
        )
        .unwrap();
    assert_eq!(ready(broker.next(&cancel)).unwrap().unwrap().key, 1);
    broker
        .publish_batch(
            RetentionClass::Retained,
            Some((1, 10)),
            vec![event(6), event(7)],
        )
        .unwrap();
    // An active inventory finishes; only pending inventory is replaceable.
    assert_eq!(ready(broker.next(&cancel)).unwrap().unwrap().key, 2);
    assert_eq!(ready(broker.next(&cancel)).unwrap().unwrap().key, 6);
    assert_eq!(ready(broker.next(&cancel)).unwrap().unwrap().key, 7);
}

#[test]
fn typed_bindings_preserve_exact_instances_and_isolate_scopes() {
    let (tool, clock) = components();
    let first = Bindings { tool, clock };
    let (tool, clock) = components();
    let second = Bindings { tool, clock };
    assert!(!std::ptr::eq(&first.tool, &second.tool));
    first.clock.advance(Duration::from_secs(8));
    assert_eq!(
        first.clock.utc_now(),
        SystemTime::UNIX_EPOCH + Duration::from_secs(8)
    );
    assert_eq!(second.clock.utc_now(), SystemTime::UNIX_EPOCH);
}
