use super::{harness::*, support::*};
use parrot_domain::{
    contracts::events::EventSubscription, protocol::messages::queue::QueueSnapshot,
};
use parrot_runtime::{agent_queues::*, children::SnapshotPublisher};
use parrot_storage::{
    events::NeverCancelled,
    queue_files::{Direction, QueueStore},
};
use protobuf::Message;
use std::sync::Arc;

#[tokio::test]
async fn root_initialization_removes_stale_child_tree_but_keeps_persisted_root_queues() {
    bounded(async {
        let h = Harness::new();
        let paths = &h.factory.resources;
        let persisted = QueueStore::new(paths.queue_directory()).unwrap();
        persisted.create("root-work", "persistent").unwrap();
        persisted
            .push("root-work", &["retained".into()], Direction::Back, false)
            .unwrap();
        persisted.dispose().unwrap();
        let stale = paths.agent_queue_root_directory().join("stale/nested");
        std::fs::create_dir_all(&stale).unwrap();
        std::fs::write(stale.join("orphaned.jsonl"), "orphaned").unwrap();
        let root = h.root().await;
        let queues = h.q(&root);
        assert!(!paths.agent_queue_root_directory().exists());
        assert_eq!(queues.get("root-work").unwrap().size, 1);
        assert_eq!(queues.capture_inventory().queues.len(), 1);
        h.close().await;
        assert!(queues.local().directory().join("root-work.jsonl").exists());
        let reopened = QueueStore::new(paths.queue_directory()).unwrap();
        assert_eq!(reopened.get("root-work").unwrap().size, 1);
        reopened.dispose().unwrap();
    })
    .await;
}

#[tokio::test]
async fn root_queues_persist_across_distinct_actual_owner_scope_lifetimes() {
    bounded(async {
        let first = Harness::new();
        let root = first.root().await;
        let q = first.q(&root);
        q.create("persistent-work", "survives").unwrap();
        push(&q, "persistent-work", &["item"], false);
        let path = q.get("persistent-work").unwrap().path;
        first.close().await;
        let second = Harness::with_histories(first.histories.clone());
        let successor = second.root().await;
        let restored = second.q(&successor).get("persistent-work").unwrap();
        assert_ne!(
            root.identity().session.session_id,
            successor.identity().session.session_id
        );
        assert_eq!(restored.path, path);
        assert_eq!(restored.description, "survives");
        assert_eq!(restored.size, 1);
        second.close().await;
    })
    .await;
}

#[tokio::test]
async fn root_queue_survives_actual_foreground_turn_and_child_close_removes_only_child_owned_directory()
 {
    bounded(async {
        let h = Harness::new();
        let root = h.root().await;
        let child = h.spawn(&root, "child").await;
        let sibling = h.spawn(&root, "sibling").await;
        let (r, c, s) = (h.q(&root), h.q(&child), h.q(&sibling));
        r.create("root-work", "").unwrap();
        c.create("child-work", "").unwrap();
        s.create("sibling-work", "").unwrap();
        push(&r, "root-work", &["survives-turn"], false);
        push(&c, "child-work", &["temporary"], false);
        push(&s, "sibling-work", &["sibling"], false);
        let original = r.get("root-work").unwrap();
        let turn = send(root.session().as_ref(), "foreground work").await;
        bounded(turn.execution.join()).await.unwrap();
        assert_eq!(r.get("root-work").unwrap(), original);
        assert!(std::sync::Arc::ptr_eq(&r, &h.q(&root)));
        let child_dir = c.local().directory().to_owned();
        let sibling_file = s.get("sibling-work").unwrap().path;
        let broker = h.factory.sessions.get("child").publisher.broker.clone();
        let events = broker.subscribe();
        let close = child.close();
        drop(close);
        bounded(child.close().join()).await.unwrap();
        assert!(!child_dir.exists());
        assert!(sibling_file.exists());
        assert!(original.path.exists());
        assert_eq!(s.get("sibling-work").unwrap().size, 1);
        let mut terminal = None;
        while let Some(event) = events.try_next() {
            if event.has_tool_snapshot() {
                terminal = Some(event);
            }
        }
        let terminal = terminal.expect("scope close joins terminal queue publication");
        let envelope = terminal.tool_snapshot();
        assert!(envelope.removed);
        let body = QueueSnapshot::parse_from_bytes(&envelope.data).unwrap();
        assert!(body.queues.is_empty());
        assert!(c.snapshot().unwrap().queues.is_empty());
        assert!(c.create("late", "").is_err());
        h.close().await;
    })
    .await;
}

#[tokio::test]
async fn real_store_inventories_are_owner_isolated_and_child_removal_does_not_change_sibling_or_root()
 {
    bounded(async {
        let h = Harness::new();
        let root = h.root().await;
        let left = h.spawn(&root, "left").await;
        let right = h.spawn(&root, "right").await;
        let (r, l, s) = (h.q(&root), h.q(&left), h.q(&right));
        r.create("root-work", "").unwrap();
        l.create("shared-name", "left").unwrap();
        s.create("shared-name", "right").unwrap();
        let mut subscription = l.subscribe_inventory().unwrap();
        let initial = bounded(subscription.next()).await.unwrap();
        push(&r, "root-work", &["root-item"], false);
        push(&s, "shared-name", &["right-item"], false);
        assert!(subscription.try_next().is_none());
        push(&l, "shared-name", &["left-item"], false);
        let changed = bounded(subscription.next()).await.unwrap();
        assert_eq!(changed.queues.len(), 1);
        assert_eq!(
            changed.owner_agent_session_id,
            left.identity().session.session_id
        );
        assert_eq!(
            changed.queues[0].owner.session_id,
            left.identity().session.session_id
        );
        assert_eq!(changed.queues[0].name, "shared-name");
        assert!(changed.revision > initial.revision);
        assert_eq!(changed.inventory_instance_id, initial.inventory_instance_id);
        assert_ne!(
            changed.inventory_instance_id,
            s.capture_inventory().inventory_instance_id
        );
        let (before_root, before_right) = (r.capture_inventory(), s.capture_inventory());
        bounded(left.close().join()).await.unwrap();
        let removed = bounded(subscription.next()).await.unwrap();
        assert!(removed.removed);
        assert!(removed.queues.is_empty());
        assert!(removed.revision > changed.revision);
        assert_eq!(removed.inventory_instance_id, changed.inventory_instance_id);
        assert!(bounded(subscription.next()).await.is_none());
        assert_eq!(r.capture_inventory(), before_root);
        assert_eq!(s.capture_inventory(), before_right);
        assert_eq!(r.get("root-work").unwrap().size, 1);
        assert_eq!(s.get("shared-name").unwrap().size, 1);
        h.close().await;
    })
    .await;
}

#[tokio::test]
async fn actual_publisher_subscribes_before_first_poll_and_joins_terminal_removed_event_after_immediate_dispose()
 {
    bounded(async {
        let h = Harness::new();
        let root = h.root().await;
        let q = h.q(&root);
        let broker = Arc::new(parrot_runtime::event_broker::EventBroker::new());
        let events = broker.subscribe();
        let publisher = QueueSnapshotPublisher::new(
            q.clone(),
            broker,
            root.identity().session.session_id.clone(),
        );
        let publication = publisher.start().unwrap();
        q.dispose().unwrap(); // no await between start and dispose: subscriber must already exist
        bounded(publication.join()).await.unwrap();
        let event = bounded(events.next(&NeverCancelled))
            .await
            .unwrap()
            .unwrap();
        assert!(event.tool_snapshot().removed);
        assert!(event.tool_snapshot().final_chunk);
        assert_eq!(event.tool_snapshot().chunk_index, 0);
        assert_eq!(
            event.tool_snapshot().owner_agent_session_id,
            root.identity().session.session_id
        );
        assert!(events.try_next().is_none());
        h.close().await;
    })
    .await;
}

#[tokio::test]
async fn scope_joins_real_publisher_encoding_failure_after_dropped_close_observer_and_still_disposes_queues()
 {
    use parrot_storage::queue_files::Inventory;
    bounded(async {
        let h = Harness::new();
        let root = h.root().await;
        let q = h.q(&root);
        q.create("work", "").unwrap();
        let inventory = h
            .factory
            .inventories
            .lock()
            .unwrap()
            .get("root")
            .unwrap()
            .clone();
        let mut state = super::inventory::state("work", 1);
        state.owner = inventory.owner().clone();
        state.description = "private".repeat((MAXIMUM_EVENT_BYTES as usize / 7) + 1);
        inventory.update(state).unwrap();
        let publication = h
            .factory
            .publications
            .lock()
            .unwrap()
            .get("root")
            .unwrap()
            .operation();
        let failure = bounded(publication.join()).await.unwrap_err();
        assert_eq!(failure.code(), "unexpected");
        assert_eq!(
            failure.to_string(),
            format!("queue: inventory record exceeds {MAXIMUM_EVENT_BYTES} bytes")
        );
        let close = root.close();
        drop(close);
        assert_eq!(bounded(root.close().join()).await.unwrap_err(), failure);
        assert!(q.is_disposed());
        assert!(q.capture_inventory().removed);
        assert!(q.capture_inventory().queues.is_empty());
        assert_eq!(
            bounded(h.registry.shutdown().join()).await.unwrap_err(),
            failure
        );
    })
    .await;
}

#[tokio::test]
async fn live_actual_publisher_tracks_queue_data_then_empty_then_terminal_and_scope_close_joins_it()
{
    bounded(async {
        let h = Harness::new();
        let root = h.root().await;
        let q = h.q(&root);
        let events = h.factory.sessions.get("root").publisher.broker.subscribe();
        q.create("work", "metadata").unwrap();
        push(&q, "work", &["one", "two"], false);
        let event = bounded(events.next(&NeverCancelled))
            .await
            .unwrap()
            .unwrap();
        let snapshot = QueueSnapshot::parse_from_bytes(&event.tool_snapshot().data).unwrap();
        assert_eq!(snapshot.queues.len(), 1);
        assert_eq!(snapshot.queues[0].name, "work");
        assert_eq!(snapshot.queues[0].item_count, 2);
        q.try_take("work", 2, Direction::Front).unwrap();
        let empty = bounded(events.next(&NeverCancelled))
            .await
            .unwrap()
            .unwrap();
        assert!(
            QueueSnapshot::parse_from_bytes(&empty.tool_snapshot().data)
                .unwrap()
                .queues
                .is_empty()
        );
        assert!(!empty.tool_snapshot().removed);
        bounded(root.close().join()).await.unwrap();
        let terminal = bounded(events.next(&NeverCancelled))
            .await
            .unwrap()
            .unwrap();
        assert!(terminal.tool_snapshot().removed);
        assert!(terminal.tool_snapshot().revision > empty.tool_snapshot().revision);
        h.close().await;
    })
    .await;
}
