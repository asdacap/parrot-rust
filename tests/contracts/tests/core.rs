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
use parrot_domain::contracts::providers::ProviderSession;
use parrot_domain::contracts::security::{AccessSnapshot, InvocationContext, SecurityProvider};
use parrot_domain::contracts::storage::{AdmissionError, Committed, SessionTransactions};
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
fn command(key: u64, turn: u64, terminal: bool) -> Command {
    Command {
        key,
        turn,
        payload: vec![0, 255, key as u8],
        delivery: Delivery::Foreground,
        usage: 17,
        terminal,
        fail_projection: false,
    }
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
fn fallback_matrix_is_only_enabled_absent_application_state_or_data() {
    // T11 owns the actual default-on configuration; this contract consumer's
    // default-on invocation is explicit rather than a fake configuration port.
    let default_enabled = true;
    let purposes = [
        ReadPurpose::ApplicationState,
        ReadPurpose::ApplicationData,
        ReadPurpose::SessionInternal,
        ReadPurpose::Configuration,
        ReadPurpose::Credentials,
        ReadPurpose::UserFile,
    ];
    let failures = [
        PrimaryReadFailure::Absent,
        PrimaryReadFailure::PermissionDenied,
        PrimaryReadFailure::Corrupt,
        PrimaryReadFailure::Other,
    ];
    for enabled in [false, default_enabled] {
        for purpose in purposes {
            for failure in failures {
                assert_eq!(
                    allows_legacy_fallback(enabled, purpose, failure),
                    enabled
                        && matches!(
                            purpose,
                            ReadPurpose::ApplicationState | ReadPurpose::ApplicationData
                        )
                        && failure == PrimaryReadFailure::Absent
                );
            }
        }
    }
}

#[test]
fn actual_resolved_target_and_per_invocation_owned_snapshots_control_tools() {
    let policy = Policy::default();
    *policy.0.lock().unwrap() = vec!["/allowed".into()];
    let first = policy.capture();
    *policy.0.lock().unwrap() = vec!["/legacy".into()];
    let second = policy.capture();
    let fallback = ResolvedRead::new(
        "/allowed/missing".into(),
        "/legacy/actual".into(),
        ReadOrigin::LegacyFallback,
    );
    assert_eq!(first.check_read(&fallback), Err(Fault::Denied));
    assert_eq!(second.check_read(&fallback), Ok(()));
    assert_eq!(fallback.requested_path(), Path::new("/allowed/missing"));
    assert_eq!(fallback.origin(), ReadOrigin::LegacyFallback);
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

#[test]
fn storage_admission_cancellation_observer_drop_and_atomic_rollback() {
    let store = Store::new(root(RootOrigin::Legacy));
    let port: &dyn SessionTransactions<Command = Command, Error = Fault> = &store;
    assert_eq!(port.selected_root(), &root(RootOrigin::Legacy));
    let cancelled = Cancel::default();
    cancelled.cancel();
    assert!(matches!(
        ready(port.admit(command(1, 1, false), &cancelled)),
        Err(AdmissionError::Cancelled)
    ));
    assert_eq!(store.pending(), 0);
    assert!(store.durable().events.is_empty());
    let cancel = Cancel::default();
    let admitted = ready(port.admit(command(1, 1, false), &cancel)).unwrap();
    assert_eq!(admitted.key(), &1);
    let mut observation = admitted.into_completion();
    assert!(step(observation.as_mut()).is_pending());
    cancel.cancel();
    drop(observation);
    assert_eq!(store.pending(), 1);
    assert!(store.durable().events.is_empty());
    // Actor execution occurs on a different worker after the observer is gone.
    std::thread::scope(|scope| scope.spawn(|| store.drive_one().unwrap()).join().unwrap());
    assert_eq!(store.durable().events.len(), 1);
    assert_eq!(store.durable().projection.get(&1), Some(&17));
    let retry_cancel = Cancel::default();
    let retry = ready(port.admit(command(1, 1, false), &retry_cancel)).unwrap();
    let receipt = ready(retry.into_completion()).unwrap();
    assert_eq!(receipt.value().key(), &1);
    assert_eq!(receipt.value().output(), &store.durable().events[0]);
    assert_eq!(store.pending(), 0);
    let mut conflict = command(1, 1, false);
    conflict.payload.push(99);
    assert!(matches!(
        ready(port.admit(conflict, &retry_cancel)),
        Err(AdmissionError::Rejected(Fault::Conflict))
    ));
    let mut failed = command(2, 2, false);
    failed.fail_projection = true;
    let ticket = ready(port.admit(failed, &retry_cancel)).unwrap();
    assert_eq!(store.drive_one(), Err(Fault::Rollback));
    assert_eq!(ready(ticket.into_completion()), Err(Fault::Rollback));
    assert_eq!(store.durable().events.len(), 1);
    assert_eq!(store.durable().projection.get(&2), None);
    assert_eq!(store.durable().closed.get(&2), None);
}

#[test]
fn exact_retries_during_admission_share_one_whole_command_and_conflicts_fail() {
    let store = Store::new(root(RootOrigin::Current));
    let cancel = Cancel::default();
    let a = ready(store.admit(command(9, 9, false), &cancel)).unwrap();
    let b = ready(store.admit(command(9, 9, false), &cancel)).unwrap();
    let mut changed = command(9, 9, false);
    changed.usage += 1;
    assert!(matches!(
        ready(store.admit(changed, &cancel)),
        Err(AdmissionError::Rejected(Fault::Conflict))
    ));
    assert_eq!(store.pending(), 1);
    store.drive_one().unwrap();
    assert_eq!(ready(a.into_completion()), ready(b.into_completion()));
    assert_eq!(store.durable().events.len(), 1);
}

#[test]
fn exact_delivery_conflicts_are_rejected_queued_committed_and_after_restart() {
    let store = Store::new(root(RootOrigin::Current));
    let cancel = Cancel::default();
    let original = command(44, 44, false);
    let mut changed = original.clone();
    changed.delivery = Delivery::Background;
    let ticket = ready(store.admit(original.clone(), &cancel)).unwrap();
    assert!(matches!(
        ready(store.admit(changed.clone(), &cancel)),
        Err(AdmissionError::Rejected(Fault::Conflict))
    ));
    assert_eq!(store.pending(), 1);
    store.drive_one().unwrap();
    let receipt = ready(ticket.into_completion()).unwrap();
    assert_eq!(receipt.value().output().delivery, Delivery::Foreground);
    assert_eq!(store.durable().events[0].delivery, Delivery::Foreground);
    assert!(matches!(
        ready(store.admit(changed.clone(), &cancel)),
        Err(AdmissionError::Rejected(Fault::Conflict))
    ));
    let restarted = Store::restart(root(RootOrigin::Current), store.durable());
    assert!(matches!(
        ready(restarted.admit(changed, &cancel)),
        Err(AdmissionError::Rejected(Fault::Conflict))
    ));
    let retry = ready(restarted.admit(original, &cancel)).unwrap();
    assert_eq!(ready(retry.into_completion()), Ok(receipt));
    assert_eq!(restarted.pending(), 0);
    assert_eq!(restarted.durable().events.len(), 1);
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
fn a003_model_terminal_commit_closes_turn_before_cancelled_publication_and_replay() {
    // A003 CONTRACT MODEL ONLY. This is not SQLite/AgentSession integration and
    // does not claim T17/T46/T54's real terminal-race correction is implemented.
    let store = Store::new(root(RootOrigin::Current));
    let cancel = Cancel::default();
    let first = ready(store.admit(command(10, 7, true), &cancel)).unwrap();
    store.drive_one().unwrap();
    // Simulate losing the original terminal receipt after commit, before poll.
    drop(first);
    let recovered = ready(store.admit(command(10, 7, true), &cancel)).unwrap();
    assert_eq!(store.pending(), 0);
    let committed = ready(recovered.into_completion()).unwrap();
    let durable = store.durable();
    assert_eq!(durable.events.len(), 1); // Actual list, never deduped/normalized.
    assert_eq!(durable.closed.get(&7), Some(&true));
    assert_eq!(durable.projection.get(&7), Some(&17));
    let published = Committed::after_commit(committed.value().output().clone());
    let broker = Broker::new(SubscriptionLimits::default());
    cancel.cancel();
    assert_eq!(
        ready(broker.publish_committed(&published, &cancel)),
        Err(OperationError::Cancelled)
    );
    assert_eq!(store.durable().events.len(), 1);
    let live = Cancel::default();
    // Lost receipt and retries reuse the committed command, without admission.
    let lost = ready(store.admit(command(10, 7, true), &live)).unwrap();
    drop(lost);
    let retried = ready(store.admit(command(10, 7, true), &live)).unwrap();
    assert_eq!(ready(retried.into_completion()), Ok(committed.clone()));
    assert_eq!(store.pending(), 0);
    // A distinct attempted second terminal fails at the actor's completion.
    let duplicate = ready(store.admit(command(11, 7, true), &live)).unwrap();
    assert_eq!(store.drive_one(), Err(Fault::TurnClosed));
    assert_eq!(ready(duplicate.into_completion()), Err(Fault::TurnClosed));
    assert_eq!(store.durable().events.len(), 1);
    // Simulated crash loses volatile queues/publication, keeps committed image.
    let restarted = Store::restart(root(RootOrigin::Current), store.durable());
    let retry = ready(restarted.admit(command(10, 7, true), &live)).unwrap();
    assert_eq!(ready(retry.into_completion()), Ok(committed));
    assert_eq!(restarted.pending(), 0);
    assert_eq!(restarted.durable().events.len(), 1);
    let duplicate_after_restart = ready(restarted.admit(command(12, 7, true), &live)).unwrap();
    assert_eq!(restarted.drive_one(), Err(Fault::TurnClosed));
    assert_eq!(
        ready(duplicate_after_restart.into_completion()),
        Err(Fault::TurnClosed)
    );
    assert_eq!(restarted.durable().events.len(), 1);
    for replayed in restarted.durable().events {
        ready(broker.publish_committed(&Committed::after_commit(replayed), &live)).unwrap();
    }
    broker.complete().unwrap();
    assert_eq!(ready(broker.next(&live)), Ok(Some(published.into_value())));
    assert_eq!(ready(broker.next(&live)), Ok(None));
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

fn content(known: &str, unknown: &[u8]) -> Content {
    Content {
        known: known.into(),
        unknown: unknown.into(),
    }
}
#[test]
fn provider_owns_complete_content_and_idle_deadline_starts_before_first_read() {
    let clock = FakeClock::default();
    let provider = Provider::new(clock.clone(), Duration::from_secs(10));
    let request = content("request", &[255, 0]);
    let expected = content("answer", &[128, 17, 42]);
    provider.configure(request.clone(), vec![(Duration::from_secs(11), expected)]);
    let port: &dyn ProviderSession<Request = Content, Event = Content, Error = Fault> = &provider;
    let cancel = Cancel::default();
    assert!(matches!(
        ready(port.send(content("request", &[]), &cancel)),
        Err(OperationError::Failed(Fault::Unconfigured))
    ));
    let stream = ready(port.send(request, &cancel)).unwrap();
    clock.advance(Duration::from_secs(10)); // No first read has happened yet.
    assert_eq!(
        ready(stream.next(&cancel)),
        Err(OperationError::Failed(Fault::IdleTimeout))
    );
    ready(port.close()).unwrap();
    assert!(matches!(
        ready(port.send(content("request", &[255, 0]), &cancel)),
        Err(OperationError::Failed(Fault::Closed))
    ));
}

#[test]
fn provider_read_activity_resets_idle_not_total_request_deadline() {
    let clock = FakeClock::default();
    let provider = Provider::new(clock.clone(), Duration::from_secs(10));
    let request = content("request", &[0, 250]);
    let first = content("first", &[17, 0, 255]);
    let second = content("second", &[23, 11, 128]);
    provider.configure(
        request.clone(),
        vec![
            (Duration::from_secs(7), first.clone()),
            (Duration::from_secs(14), second.clone()),
        ],
    );
    let cancel = Cancel::default();
    let stream = ready(provider.send(request, &cancel)).unwrap();
    let mut next = stream.next(&cancel);
    assert!(step(next.as_mut()).is_pending());
    clock.advance(Duration::from_secs(7));
    assert_eq!(step(next.as_mut()), Poll::Ready(Ok(Some(first))));
    drop(next);
    clock.advance(Duration::from_secs(7));
    assert_eq!(ready(stream.next(&cancel)), Ok(Some(second)));
    assert_eq!(ready(stream.next(&cancel)), Ok(None));
    cancel.cancel();
    assert_eq!(ready(stream.next(&cancel)), Err(OperationError::Cancelled));
}

#[test]
fn typed_bindings_preserve_exact_instances_and_isolate_scopes() {
    let (storage, provider, tool, clock) = components();
    let first = Bindings {
        storage,
        provider,
        tool,
        clock,
    };
    let (storage, provider, tool, clock) = components();
    let second = Bindings {
        storage,
        provider,
        tool,
        clock,
    };
    assert!(std::ptr::eq(first.storage(), &first.storage));
    assert!(!std::ptr::eq(&first.storage, &second.storage));
    assert!(!std::ptr::eq(&first.provider, &second.provider));
    let cancel = Cancel::default();
    let ticket = ready(first.storage().admit(command(1, 1, false), &cancel)).unwrap();
    first.storage.drive_one().unwrap();
    assert_eq!(ready(ticket.into_completion()).unwrap().value().key(), &1);
    assert_eq!(first.storage.durable().events.len(), 1);
    assert!(second.storage.durable().events.is_empty());
    first.clock.advance(Duration::from_secs(8));
    assert_eq!(
        first.clock.utc_now(),
        SystemTime::UNIX_EPOCH + Duration::from_secs(8)
    );
    assert_eq!(second.clock.utc_now(), SystemTime::UNIX_EPOCH);
}
