use super::{Token, event, inventory, poll_once};
use parrot_domain::contracts::async_ops::OperationError;
use parrot_domain::contracts::events::{EventPublisher, EventSubscription};
use parrot_domain::contracts::paths::{RootOrigin, SelectedSessionRoot};
use parrot_domain::contracts::storage::Committed;
use parrot_domain::protocol::messages::parrot::{Event, SessionUsageSnapshot, TurnEnded};
use parrot_runtime::event_broker::{EventBroker, ListenError};
use parrot_storage::database::SessionDatabase;
use parrot_storage::events::{EventError, EventRepository, NeverCancelled};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

static NEXT: AtomicU64 = AtomicU64::new(0);

struct Fixture {
    path: PathBuf,
    repository: EventRepository,
}

impl Fixture {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!(
            "parrot-t43-replay-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&path).unwrap();
        let root = SelectedSessionRoot::new(path.clone(), RootOrigin::Current).unwrap();
        Self {
            path,
            repository: EventRepository::new(SessionDatabase::open(root).unwrap()),
        }
    }

    async fn commit(&self, event: Event) -> Committed<Event> {
        // This token follows an actual acknowledged SQLite actor transaction,
        // unlike the ordinary queue-only fixtures in the parent test module.
        let receipt = self
            .repository
            .admit(event.id.clone(), &NeverCancelled, move |tx| {
                parrot_storage::events::record(tx, &event, None, "2026-01-02T03:04:05Z")?;
                Ok(event)
            })
            .unwrap()
            .into_completion()
            .await
            .unwrap();
        Committed::after_commit(receipt.into_value().into_output())
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

fn usage() -> Event {
    let mut published = event("usage");
    published.set_session_usage_snapshot(SessionUsageSnapshot::new());
    published
}

#[tokio::test]
async fn real_store_commit_during_replay_then_initial_capture_has_no_gap_or_double_delivery() {
    let fixture = Fixture::new();
    let broker = EventBroker::new();
    fixture.commit(event("before-cursor")).await;
    fixture.commit(event("cursor")).await;
    fixture.commit(event("after-cursor")).await;
    let snapshots = inventory("queue", "child", 7, 2);
    let expected_snapshots = snapshots.clone();
    let listener = broker
        .listen(
            Some("cursor"),
            || async {
                // listen must already own its live subscription when storage
                // is called. This event lands in both the real store and live.
                let overlap = fixture.commit(event("overlap")).await;
                broker
                    .publish_committed(&overlap, &NeverCancelled)
                    .await
                    .unwrap();
                fixture.repository.replay().await
            },
            || async {
                // A genuine update during initial snapshot capture must remain
                // queued after the replay and returned initial chunks/usage.
                broker
                    .publish_transient(event("during-capture"), &NeverCancelled)
                    .await
                    .unwrap();
                let mut initial = snapshots;
                initial.push(usage());
                Ok::<_, EventError>(initial)
            },
        )
        .await
        .unwrap();

    let mut expected = vec![event("after-cursor"), event("overlap")];
    expected.extend(expected_snapshots);
    expected.extend([usage(), event("during-capture")]);
    for expected in expected {
        assert_eq!(
            listener.next(&NeverCancelled).await.unwrap(),
            Some(expected)
        );
    }
    let mut waiting = listener.next(&NeverCancelled);
    assert!(poll_once(waiting.as_mut()).is_pending());
    let later = fixture.commit(event("later")).await;
    broker
        .publish_committed(&later, &NeverCancelled)
        .await
        .unwrap();
    assert_eq!(waiting.await.unwrap(), Some(event("later")));
    assert_eq!(
        fixture.repository.replay().await.unwrap(),
        [
            "before-cursor",
            "cursor",
            "after-cursor",
            "overlap",
            "later"
        ]
        .map(event)
    );
    listener.complete().unwrap();
    assert_eq!(listener.next(&NeverCancelled).await.unwrap(), None);
    fixture.repository.database().close().await.unwrap();
}

#[tokio::test]
async fn no_cursor_skips_storage_but_does_not_deduplicate_initial_against_live() {
    let broker = EventBroker::new();
    let listener = broker
        .listen(
            None,
            || async { panic!("None must not load persisted events") },
            || async {
                broker
                    .publish_transient(event("same-id"), &NeverCancelled)
                    .await
                    .unwrap();
                Ok::<_, ()>(vec![event("same-id"), usage()])
            },
        )
        .await
        .unwrap();
    for expected in [event("same-id"), usage(), event("same-id")] {
        assert_eq!(
            listener.next(&NeverCancelled).await.unwrap(),
            Some(expected)
        );
    }
    listener.complete().unwrap();
    assert_eq!(listener.next(&NeverCancelled).await.unwrap(), None);
}

#[tokio::test]
async fn empty_or_unknown_cursor_replays_actual_stored_duplicate_terminal_payloads_unchanged() {
    let fixture = Fixture::new();
    let mut first = event("end-first");
    first.set_turn_ended(TurnEnded::new());
    let mut second = event("end-second");
    second.set_turn_ended(TurnEnded::new());
    fixture.commit(first.clone()).await;
    fixture.commit(second.clone()).await;
    let original = fixture.repository.replay_raw().await.unwrap();
    let broker = EventBroker::new();
    for cursor in ["", "unknown"] {
        let listener = broker
            .listen(
                Some(cursor),
                || fixture.repository.replay(),
                || async { Ok::<_, EventError>(vec![usage()]) },
            )
            .await
            .unwrap();
        for expected in [first.clone(), second.clone(), usage()] {
            assert_eq!(
                listener.next(&NeverCancelled).await.unwrap(),
                Some(expected)
            );
        }
        listener.complete().unwrap();
        assert_eq!(listener.next(&NeverCancelled).await.unwrap(), None);
    }
    let after = fixture.repository.replay_raw().await.unwrap();
    assert_eq!(after.len(), original.len());
    for (before, after) in original.iter().zip(after) {
        assert_eq!(before.sequence, after.sequence);
        assert_eq!(before.raw, after.raw);
    }
    fixture.repository.database().close().await.unwrap();
}

#[tokio::test]
async fn replay_failure_is_returned_without_capturing_initial_state() {
    let broker = EventBroker::new();
    let result = broker
        .listen(
            Some("cursor"),
            || async { Err::<Vec<Event>, _>("read failed") },
            || async { panic!("failed replay must stop assembly") },
        )
        .await;
    assert_eq!(result.err(), Some("read failed"));
    let next = broker.subscribe();
    broker
        .publish_transient(event("after"), &NeverCancelled)
        .await
        .unwrap();
    assert_eq!(next.try_next(), Some(event("after")));
}

#[tokio::test]
async fn initial_capture_failure_is_visible_only_after_the_persisted_suffix() {
    let broker = EventBroker::new();
    let listener = broker
        .listen(
            Some("cursor"),
            || async { Ok(vec![event("persisted")]) },
            || async { Err::<Vec<Event>, _>("capture failed") },
        )
        .await
        .unwrap();
    assert_eq!(
        listener.next(&NeverCancelled).await.unwrap(),
        Some(event("persisted"))
    );
    assert_eq!(
        listener.next(&NeverCancelled).await,
        Err(OperationError::Failed(ListenError::Initial(
            "capture failed"
        )))
    );
    broker
        .publish_transient(event("after"), &NeverCancelled)
        .await
        .unwrap();
    assert_eq!(listener.next(&NeverCancelled).await.unwrap(), None);
    let successor = broker.subscribe();
    assert_eq!(successor.try_next(), None);
}

#[tokio::test]
async fn dropping_pending_assembly_does_not_close_the_broker_or_replay_old_live_events() {
    let broker = EventBroker::new();
    let token = Token::default();
    let mut pending = Box::pin(broker.listen(
        Some("cursor"),
        std::future::pending::<Result<Vec<Event>, ()>>,
        || async { Ok(vec![usage()]) },
    ));
    assert!(poll_once(pending.as_mut()).is_pending());
    broker
        .publish_transient(event("departed"), &token)
        .await
        .unwrap();
    drop(pending);
    let listener = broker
        .listen(
            None,
            || async { panic!("no replay") },
            || async { Ok::<_, ()>(vec![usage()]) },
        )
        .await
        .unwrap();
    assert_eq!(listener.next(&token).await.unwrap(), Some(usage()));
    let mut waiting = listener.next(&token);
    assert!(poll_once(waiting.as_mut()).is_pending());
    broker
        .publish_transient(event("current"), &token)
        .await
        .unwrap();
    assert_eq!(waiting.await.unwrap(), Some(event("current")));
}

#[tokio::test]
async fn slow_replay_consumer_captures_fresh_inventory_only_after_consuming_replay() {
    let fixture = Fixture::new();
    fixture.commit(event("first")).await;
    fixture.commit(event("second")).await;
    let broker = EventBroker::new();
    let revision = AtomicU64::new(1);
    let captures = AtomicU64::new(0);
    let listener = broker
        .listen(
            Some("unknown"),
            || fixture.repository.replay(),
            || async {
                captures.fetch_add(1, Ordering::SeqCst);
                let mut initial = inventory("queue", "child", revision.load(Ordering::SeqCst), 1);
                initial.push(usage());
                Ok::<_, EventError>(initial)
            },
        )
        .await
        .unwrap();
    assert_eq!(captures.load(Ordering::SeqCst), 0);
    assert_eq!(
        listener.next(&NeverCancelled).await.unwrap(),
        Some(event("first"))
    );
    revision.store(2, Ordering::SeqCst);
    let late = fixture.commit(event("during-replay-consumption")).await;
    broker
        .publish_committed(&late, &NeverCancelled)
        .await
        .unwrap();
    assert_eq!(
        listener.next(&NeverCancelled).await.unwrap(),
        Some(event("second"))
    );
    assert_eq!(captures.load(Ordering::SeqCst), 0);
    assert_eq!(
        listener.next(&NeverCancelled).await.unwrap(),
        inventory("queue", "child", 2, 1).pop()
    );
    assert_eq!(captures.load(Ordering::SeqCst), 1);
    assert_eq!(listener.next(&NeverCancelled).await.unwrap(), Some(usage()));
    assert_eq!(
        listener.next(&NeverCancelled).await.unwrap(),
        Some(event("during-replay-consumption"))
    );
    fixture.repository.database().close().await.unwrap();
}

#[tokio::test]
async fn dropping_an_initial_read_observation_does_not_skip_or_restart_capture() {
    let broker = EventBroker::new();
    let ready = tokio::sync::Notify::new();
    let captures = AtomicU64::new(0);
    let listener = broker
        .listen(
            Some("unknown"),
            || async { Ok::<_, ()>(vec![event("persisted")]) },
            || async {
                captures.fetch_add(1, Ordering::SeqCst);
                ready.notified().await;
                Ok(vec![usage()])
            },
        )
        .await
        .unwrap();
    assert_eq!(
        listener.next(&NeverCancelled).await.unwrap(),
        Some(event("persisted"))
    );
    let mut pending = listener.next(&NeverCancelled);
    assert!(poll_once(pending.as_mut()).is_pending());
    assert_eq!(captures.load(Ordering::SeqCst), 1);
    drop(pending);
    broker
        .publish_transient(event("live"), &NeverCancelled)
        .await
        .unwrap();
    ready.notify_one();
    assert_eq!(listener.next(&NeverCancelled).await.unwrap(), Some(usage()));
    assert_eq!(captures.load(Ordering::SeqCst), 1);
    assert_eq!(
        listener.next(&NeverCancelled).await.unwrap(),
        Some(event("live"))
    );
}

#[tokio::test]
async fn cancelling_initial_observation_preserves_owned_capture_for_a_later_read() {
    let broker = EventBroker::new();
    let ready = tokio::sync::Notify::new();
    let captures = AtomicU64::new(0);
    let listener = broker
        .listen(
            None,
            || async { panic!("no replay") },
            || async {
                captures.fetch_add(1, Ordering::SeqCst);
                ready.notified().await;
                Ok::<_, ()>(vec![usage()])
            },
        )
        .await
        .unwrap();
    let token = Token::default();
    let mut waiting = listener.next(&token);
    assert!(poll_once(waiting.as_mut()).is_pending());
    token.cancel();
    assert_eq!(waiting.await, Err(OperationError::Cancelled));
    assert_eq!(captures.load(Ordering::SeqCst), 1);
    ready.notify_one();
    assert_eq!(listener.next(&NeverCancelled).await.unwrap(), Some(usage()));
    assert_eq!(captures.load(Ordering::SeqCst), 1);
}
