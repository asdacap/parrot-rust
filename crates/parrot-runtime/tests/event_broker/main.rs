mod replay;

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::task::{Context, Poll, Wake, Waker};

use parrot_domain::contracts::async_ops::{BoxFuture, Cancellation, OperationError};
use parrot_domain::contracts::events::{EventPublisher, EventSubscription, SubscriptionLimits};
use parrot_domain::contracts::storage::Committed;
use parrot_domain::protocol::messages::parrot::{AgentStarted, Event, ToolSnapshot, TurnEnded};
use parrot_runtime::event_broker::{BrokerError, EventBroker, Subscription};
use tokio::sync::Notify;

#[derive(Default)]
struct Token {
    cancelled: AtomicBool,
    changed: Notify,
}

impl Token {
    fn cancel(&self) {
        self.cancelled.store(true, Ordering::SeqCst);
        self.changed.notify_waiters();
    }
}

impl Cancellation for Token {
    fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::SeqCst)
    }

    fn cancelled(&self) -> BoxFuture<'_, ()> {
        Box::pin(async move {
            loop {
                let changed = self.changed.notified();
                tokio::pin!(changed);
                changed.as_mut().enable();
                if self.is_cancelled() {
                    return;
                }
                changed.await;
            }
        })
    }
}

struct NoWake;
impl Wake for NoWake {
    fn wake(self: Arc<Self>) {}
}

fn poll_once<F: Future + ?Sized>(future: Pin<&mut F>) -> Poll<F::Output> {
    let waker = Waker::from(Arc::new(NoWake));
    future.poll(&mut Context::from_waker(&waker))
}

fn event(id: impl Into<String>) -> Event {
    Event {
        id: id.into(),
        ..Event::new()
    }
}

fn inventory(kind: &str, owner: &str, revision: u64, chunks: u32) -> Vec<Event> {
    (0..chunks)
        .map(|index| {
            let mut event = event(format!("{kind}/{owner}/{revision}/{index}"));
            event.set_tool_snapshot(ToolSnapshot {
                kind: kind.into(),
                owner_agent_session_id: owner.into(),
                inventory_instance_id: "instance".into(),
                revision,
                chunk_index: index,
                final_chunk: index + 1 == chunks,
                ..ToolSnapshot::new()
            });
            event
        })
        .collect()
}

async fn retained(broker: &EventBroker, event: Event) {
    // Test fixture attestation only: this constructs no durable database proof.
    broker
        .publish_committed(&Committed::after_commit(event), &Token::default())
        .await
        .unwrap();
}

fn drain(subscription: &Subscription) -> Vec<Event> {
    std::iter::from_fn(|| subscription.try_next()).collect()
}

#[tokio::test]
async fn publication_without_subscribers_does_not_queue_or_block() {
    let broker = EventBroker::new();
    for index in 0..5000 {
        retained(&broker, event(index.to_string())).await;
    }
    let listener = broker.subscribe();
    assert!(listener.try_next().is_none());
    let token = Token::default();
    let mut pending = listener.next(&token);
    assert!(poll_once(pending.as_mut()).is_pending());
    retained(&broker, event("after")).await;
    assert_eq!(pending.await.unwrap().unwrap().id, "after");
}

#[tokio::test]
async fn eager_subscriber_receives_before_first_read() {
    let broker = EventBroker::new();
    let listener = broker.subscribe();
    retained(&broker, event("ready")).await;
    assert_eq!(listener.try_next().unwrap().id, "ready");
}

#[tokio::test]
async fn departed_listener_does_not_replay_into_a_new_listener() {
    let broker = EventBroker::new();
    let listener = broker.subscribe();
    retained(&broker, event("first")).await;
    assert_eq!(listener.try_next().unwrap().id, "first");
    drop(listener);
    retained(&broker, event("second")).await;
    let successor = broker.subscribe();
    retained(&broker, event("third")).await;
    assert_eq!(drain(&successor), vec![event("third")]);
}

#[tokio::test]
async fn two_subscribers_both_receive_and_have_independent_queues() {
    let broker = EventBroker::new();
    let first = broker.subscribe();
    let second = broker.subscribe();
    retained(&broker, event("fanned")).await;
    assert_eq!(first.try_next(), Some(event("fanned")));
    assert_eq!(second.try_next(), Some(event("fanned")));
    retained(&broker, event("only-unread-in-second")).await;
    first.try_next();
    assert!(first.try_next().is_none());
    assert_eq!(second.try_next().unwrap().id, "only-unread-in-second");
}

#[tokio::test]
async fn default_transient_budget_only_evicts_its_oldest_class() {
    let broker = EventBroker::new();
    let listener = broker.subscribe();
    for index in 0..1024 {
        retained(&broker, event(format!("retained-{index}"))).await;
        broker
            .publish_transient(event(format!("transient-{index}")), &Token::default())
            .await
            .unwrap();
    }
    broker
        .publish_transient(event("transient-new"), &Token::default())
        .await
        .unwrap();
    let received = drain(&listener);
    assert_eq!(received.len(), 2048);
    assert_eq!(
        received
            .iter()
            .filter(|e| e.id.starts_with("retained-"))
            .count(),
        1024
    );
    assert!(received.contains(&event("retained-0")));
    assert!(received.contains(&event("retained-1023")));
    assert!(!received.contains(&event("transient-0")));
    assert!(received.contains(&event("transient-new")));
}

#[tokio::test]
async fn retained_runtime_publication_preserves_class_budget_and_raw_event_payload() {
    use protobuf::Message;

    let broker = EventBroker::with_limits(SubscriptionLimits {
        retained: 2,
        transient: 2,
    })
    .unwrap();
    let listener = broker.subscribe();
    let first = event("retained-first");
    let mut transient = event("transient");
    transient.set_turn_ended(TurnEnded::new());
    let last = event("retained-last");
    let mut newest = event("retained-new");
    newest.set_agent_started(AgentStarted::new());
    newest
        .special_fields
        .mut_unknown_fields()
        .add_varint(400, 17);
    let newest_bytes = newest.write_to_bytes().unwrap();
    broker.publish_retained(&first);
    broker
        .publish_transient(transient.clone(), &Token::default())
        .await
        .unwrap();
    broker.publish_retained(&last);
    broker.publish_retained(&newest);
    let actual = drain(&listener);
    assert_eq!(actual, vec![transient, last, newest]);
    assert_eq!(actual[2].write_to_bytes().unwrap(), newest_bytes);
}

#[tokio::test]
async fn retained_eviction_preserves_transient_and_survivor_order() {
    let broker = EventBroker::with_limits(SubscriptionLimits {
        retained: 2,
        transient: 1,
    })
    .unwrap();
    let listener = broker.subscribe();
    retained(&broker, event("r0")).await;
    broker
        .publish_transient(event("t0"), &Token::default())
        .await
        .unwrap();
    retained(&broker, event("r1")).await;
    retained(&broker, event("r2")).await;
    assert_eq!(
        drain(&listener),
        vec![event("t0"), event("r1"), event("r2")]
    );
    retained(&broker, event("r3")).await;
    retained(&broker, event("r4")).await;
    assert_eq!(drain(&listener), vec![event("r3"), event("r4")]);
}

#[test]
fn invalid_configured_capacity_is_explicit_not_an_unbounded_queue() {
    assert!(matches!(
        EventBroker::with_limits(SubscriptionLimits {
            retained: 0,
            transient: 1
        }),
        Err(BrokerError::InvalidCapacity)
    ));
    assert!(matches!(
        EventBroker::with_limits(SubscriptionLimits {
            retained: 1,
            transient: 0
        }),
        Err(BrokerError::InvalidCapacity)
    ));
}

#[tokio::test]
async fn inventories_coalesce_by_owner_kind_without_chunk_or_event_loss() {
    let broker = EventBroker::new();
    let listener = broker.subscribe();
    for index in 0..1024 {
        retained(&broker, event(format!("ordinary-{index}"))).await;
    }
    let obsolete = inventory("queue", "first", 1, 1);
    broker.publish_inventory(&obsolete).unwrap();
    let sibling = inventory("queue", "second", 1, 1);
    broker.publish_inventory(&sibling).unwrap();
    let processes = inventory("shell_process", "first", 1, 1);
    broker.publish_inventory(&processes).unwrap();
    let replacement = inventory("queue", "first", 2, 1100);
    broker.publish_inventory(&replacement).unwrap();
    let received = drain(&listener);
    assert_eq!(received.len(), 2126);
    assert!(!received.contains(&obsolete[0]));
    assert!(received.contains(&sibling[0]));
    assert!(received.contains(&processes[0]));
    let first_queue: Vec<_> = received
        .iter()
        .filter(|e| {
            e.has_tool_snapshot()
                && e.tool_snapshot().kind == "queue"
                && e.tool_snapshot().owner_agent_session_id == "first"
        })
        .cloned()
        .collect();
    assert_eq!(first_queue, replacement);
    let ordinary: Vec<_> = received
        .iter()
        .filter(|e| !e.has_tool_snapshot())
        .cloned()
        .collect();
    assert_eq!(
        ordinary,
        (0..1024)
            .map(|i| event(format!("ordinary-{i}")))
            .collect::<Vec<_>>()
    );
}

#[tokio::test]
async fn active_inventory_finishes_and_replenishment_cannot_starve_ordinary_events() {
    let broker = EventBroker::new();
    let listener = broker.subscribe();
    let original = inventory("queue", "owner", 1, 3);
    broker.publish_inventory(&original).unwrap();
    assert_eq!(listener.try_next(), Some(original[0].clone()));
    retained(&broker, event("ordinary-first")).await;
    retained(&broker, event("ordinary-second")).await;
    for (index, expected) in original.iter().enumerate().skip(1) {
        broker
            .publish_inventory(&inventory("queue", "owner", (index + 1) as u64, 1))
            .unwrap();
        assert_eq!(listener.try_next(), Some(expected.clone()));
    }
    let mut ordinary = Vec::new();
    for index in 0..4 {
        broker
            .publish_inventory(&inventory("queue", "owner", index + 10, 1))
            .unwrap();
        let published = listener.try_next().unwrap();
        if !published.has_tool_snapshot() {
            ordinary.push(published.id);
        }
    }
    assert_eq!(ordinary, ["ordinary-first", "ordinary-second"]);
}

#[test]
fn invalid_inventory_is_rejected_before_any_partial_enqueue() {
    for queue_first in [true, false] {
        for direct in [true, false] {
            let broker = EventBroker::new();
            let listener = broker.subscribe();
            let queue = inventory("queue", "owner", 1, 1).remove(0);
            let process = inventory("shell_process", "owner", 1, 1).remove(0);
            let mixed = if queue_first {
                vec![queue, process]
            } else {
                vec![process, queue]
            };
            let result = if direct {
                listener.publish_inventory(&mixed)
            } else {
                broker.publish_inventory(&mixed)
            };
            assert_eq!(result, Err(BrokerError::InvalidInventory));
            assert!(listener.try_next().is_none());
            broker.publish_inventory(&mixed[..1]).unwrap();
            assert_eq!(listener.try_next(), Some(mixed[0].clone()));
        }
    }
}

#[test]
fn invalid_inventory_is_validated_even_without_listeners_or_after_completion() {
    let broker = EventBroker::new();
    for completed in [false, true] {
        if completed {
            broker.complete();
        }
        assert_eq!(
            broker.publish_inventory(&[event("not snapshot")]),
            Err(BrokerError::InvalidInventory)
        );
        let mut mixed = inventory("queue", "first", 1, 1);
        mixed.extend(inventory("queue", "second", 1, 1));
        assert_eq!(
            broker.publish_inventory(&mixed),
            Err(BrokerError::InvalidInventory)
        );
    }
}

#[test]
fn empty_inventory_is_a_no_op_and_revision_metadata_is_not_rewritten() {
    let broker = EventBroker::new();
    let listener = broker.subscribe();
    broker.publish_inventory(&[]).unwrap();
    listener.publish_inventory(&[]).unwrap();
    assert!(listener.try_next().is_none());
    let mut chunks = inventory("queue", "", 0, 2);
    chunks[0].mut_tool_snapshot().chunk_index = 47;
    chunks[1].mut_tool_snapshot().final_chunk = false;
    broker.publish_inventory(&chunks).unwrap();
    assert_eq!(drain(&listener), chunks);
}

#[tokio::test]
async fn completion_is_idempotent_stops_admission_but_drains_all_classes_and_chunks() {
    let broker = EventBroker::new();
    let listener = broker.subscribe();
    retained(&broker, event("retained")).await;
    broker
        .publish_transient(event("transient"), &Token::default())
        .await
        .unwrap();
    let chunks = inventory("queue", "owner", 1, 3);
    broker.publish_inventory(&chunks).unwrap();
    listener.complete();
    listener.complete();
    retained(&broker, event("too-late")).await;
    listener
        .publish_inventory(&inventory("queue", "owner", 2, 1))
        .unwrap();
    let mut expected = chunks;
    expected.extend([event("retained"), event("transient")]);
    let token = Token::default();
    for value in expected {
        assert_eq!(listener.next(&token).await.unwrap(), Some(value));
    }
    assert_eq!(listener.next(&token).await.unwrap(), None);
}

#[tokio::test]
async fn broker_drop_completes_waiter_and_future_subscription_after_complete_is_empty() {
    let broker = EventBroker::new();
    let listener = broker.subscribe();
    let token = Token::default();
    let mut pending = listener.next(&token);
    assert!(poll_once(pending.as_mut()).is_pending());
    drop(broker);
    assert_eq!(pending.await.unwrap(), None);
    let broker = EventBroker::new();
    broker.complete();
    broker.complete();
    retained(&broker, event("ignored")).await;
    assert_eq!(broker.subscribe().next(&token).await.unwrap(), None);
}

#[tokio::test]
async fn cancellation_is_observable_and_does_not_remove_or_invalidate_committed_value() {
    let broker = EventBroker::new();
    let listener = broker.subscribe();
    let token = Token::default();
    let mut waiting = listener.next(&token);
    assert!(poll_once(waiting.as_mut()).is_pending());
    token.cancel();
    assert_eq!(waiting.await, Err(OperationError::Cancelled));
    let committed = Committed::after_commit(event("committed"));
    assert_eq!(
        broker.publish_committed(&committed, &token).await,
        Err(OperationError::Cancelled)
    );
    assert_eq!(committed.value(), &event("committed"));
    assert!(listener.try_next().is_none());
    broker
        .publish_committed(&committed, &Token::default())
        .await
        .unwrap();
    // Source ChannelReader first drains ready data even for a cancelled waiter.
    assert_eq!(
        listener.next(&token).await.unwrap(),
        Some(event("committed"))
    );
    assert_eq!(
        broker.publish_transient(event("cancelled"), &token).await,
        Err(OperationError::Cancelled)
    );
    assert!(listener.try_next().is_none());
}

#[tokio::test]
async fn dropped_observation_does_not_complete_the_subscription() {
    let broker = EventBroker::new();
    let listener = broker.subscribe();
    let token = Token::default();
    let mut waiting = listener.next(&token);
    assert!(poll_once(waiting.as_mut()).is_pending());
    drop(waiting);
    retained(&broker, event("after-observer-drop")).await;
    assert_eq!(
        listener.next(&token).await.unwrap().unwrap().id,
        "after-observer-drop"
    );
}

#[tokio::test]
async fn turn_end_is_not_stream_completion_and_child_events_remain_deliverable() {
    let broker = EventBroker::new();
    let listener = broker.subscribe();
    let mut ended = event("ended");
    ended.agent_session_id = "main".into();
    ended.set_turn_ended(TurnEnded::new());
    retained(&broker, ended.clone()).await;
    assert_eq!(listener.try_next(), Some(ended));
    let token = Token::default();
    let mut waiting = listener.next(&token);
    assert!(poll_once(waiting.as_mut()).is_pending());
    let mut child = event("child-after-turn");
    child.agent_session_id = "child".into();
    child.set_agent_started(AgentStarted::new());
    retained(&broker, child.clone()).await;
    assert_eq!(waiting.await.unwrap(), Some(child));
}

#[tokio::test]
async fn subscribe_before_replay_drops_exactly_one_live_overlap_without_gaps() {
    let broker = EventBroker::new();
    let live = broker.subscribe();
    // The durable-store fixture commits after subscribe but before its snapshot.
    // Production storage loading/writer integration remains the T46 owner's job.
    retained(&broker, event("during-replay")).await;
    let snapshot = vec![event("before"), event("cursor"), event("during-replay")];
    let replay = live.replay_after("cursor", snapshot);
    retained(&broker, event("after-replay")).await;
    let token = Token::default();
    assert_eq!(
        replay.next(&token).await.unwrap().unwrap().id,
        "during-replay"
    );
    assert_eq!(
        replay.next(&token).await.unwrap().unwrap().id,
        "after-replay"
    );
    retained(&broker, event("during-replay")).await;
    assert_eq!(
        replay.next(&token).await.unwrap().unwrap().id,
        "during-replay"
    );
}

#[tokio::test]
async fn unknown_cursor_replays_all_and_raw_duplicate_endings_are_not_normalized() {
    let broker = EventBroker::new();
    let live = broker.subscribe();
    let mut first = event("duplicate");
    first.set_turn_ended(TurnEnded {
        input_tokens: 17,
        output_tokens: 9,
        ..TurnEnded::new()
    });
    let mut second = event("duplicate");
    second.set_turn_ended(TurnEnded {
        input_tokens: 23,
        output_tokens: 11,
        ..TurnEnded::new()
    });
    let snapshot = vec![first, second, event("last")];
    let replay = live.replay_after("missing", snapshot.clone());
    replay.complete().unwrap();
    for expected in snapshot {
        assert_eq!(
            replay.next(&Token::default()).await.unwrap(),
            Some(expected)
        );
    }
    assert_eq!(replay.next(&Token::default()).await.unwrap(), None);
}

#[tokio::test]
async fn cursor_after_first_occurrence_and_reconnect_create_new_live_boundaries() {
    let broker = EventBroker::new();
    let snapshot = vec![event("cursor"), event("cursor"), event("tail")];
    let replay = broker.subscribe().replay_after("cursor", snapshot);
    assert_eq!(
        replay.next(&Token::default()).await.unwrap(),
        Some(event("cursor"))
    );
    assert_eq!(
        replay.next(&Token::default()).await.unwrap(),
        Some(event("tail"))
    );
    drop(replay);
    retained(&broker, event("between-connections")).await;
    let second = broker
        .subscribe()
        .replay_after("tail", vec![event("tail"), event("between-connections")]);
    assert_eq!(
        second.next(&Token::default()).await.unwrap(),
        Some(event("between-connections"))
    );
    retained(&broker, event("new-live")).await;
    assert_eq!(
        second.next(&Token::default()).await.unwrap(),
        Some(event("new-live"))
    );
}

#[tokio::test]
async fn fanout_preserves_payload_envelope_and_unknown_fields() {
    let broker = EventBroker::new();
    let listener = broker.subscribe();
    let mut original = inventory("queue", "owner", 2, 1).remove(0);
    original.agent_session_id = "child".into();
    original
        .special_fields
        .mut_unknown_fields()
        .add_varint(400, 17);
    original
        .mut_tool_snapshot()
        .special_fields
        .mut_unknown_fields()
        .add_length_delimited(401, vec![0, 255, 10]);
    retained(&broker, original.clone()).await;
    assert_eq!(listener.try_next(), Some(original.clone()));
    broker.publish_inventory(&[original.clone()]).unwrap();
    assert_eq!(listener.try_next(), Some(original));
}
