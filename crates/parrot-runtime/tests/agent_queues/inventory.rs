use super::support::bounded;
use parrot_runtime::agent_queues::*;
use parrot_storage::queue_files::{ErrorKind, Inventory, Owner, QueueState};

pub fn owner(id: &str) -> Owner {
    Owner {
        session_id: id.into(),
        name: format!("{id}-name"),
        parent_session_id: "parent-id".into(),
        parent_name: "parent-name".into(),
    }
}
pub fn state(name: &str, count: usize) -> QueueState {
    QueueState {
        owner: owner("child"),
        name: name.into(),
        description: format!("{name} description"),
        item_count: count,
    }
}
pub fn inventory() -> std::sync::Arc<QueueInventory> {
    QueueInventory::new(owner("child"), "queue-inventory-test".into()).unwrap()
}

#[tokio::test]
async fn inventory_revisions_change_only_on_real_nonempty_updates_and_are_ordinal() {
    let inventory = inventory();
    let mut sub = inventory.subscribe().unwrap();
    let initial = bounded(sub.next()).await.unwrap();
    assert_eq!(initial.revision, 0);
    assert!(!initial.removed);
    assert!(initial.queues.is_empty());
    inventory.update(state("ignored", 3)).unwrap();
    assert!(sub.try_next().is_none());
    inventory.register_owner(&[]).unwrap();
    assert!(sub.try_next().is_none());
    inventory.update(state("zebra", 2)).unwrap();
    inventory.update(state("alpha", 1)).unwrap();
    let newest = bounded(sub.next()).await.unwrap();
    assert_eq!(newest.revision, 2);
    assert_eq!(
        newest
            .queues
            .iter()
            .map(|q| q.name.as_str())
            .collect::<Vec<_>>(),
        ["alpha", "zebra"]
    );
    inventory.update(state("alpha", 1)).unwrap();
    inventory.update(state("absent", 0)).unwrap();
    assert!(sub.try_next().is_none());
    assert_eq!(inventory.capture().revision, 2);
    inventory.update(state("alpha", 0)).unwrap();
    let removed = bounded(sub.next()).await.unwrap();
    assert_eq!(removed.revision, 3);
    assert_eq!(removed.queues.len(), 1);
    inventory.unregister_owner().unwrap();
    let unregistered = bounded(sub.next()).await.unwrap();
    assert_eq!(unregistered.revision, 4);
    assert!(unregistered.queues.is_empty());
    assert!(!unregistered.removed);
    inventory.unregister_owner().unwrap();
    inventory.update(state("ignored", 9)).unwrap();
    assert!(sub.try_next().is_none());
    inventory.dispose();
    let terminal = bounded(sub.next()).await.unwrap();
    assert_eq!(terminal.revision, 5);
    assert!(terminal.removed);
    assert!(terminal.queues.is_empty());
    assert!(bounded(sub.next()).await.is_none());
    inventory.dispose();
    assert_eq!(inventory.capture(), terminal);
    assert_eq!(
        inventory.subscribe().err().unwrap().kind,
        ErrorKind::Disposed
    );
}

#[tokio::test]
async fn inventory_registration_preserves_metadata_and_rejects_wrong_owner_even_after_close() {
    let inventory = inventory();
    let mut entry = state("initial", 0);
    entry.owner.session_id = "different".into();
    inventory.register_owner(&[entry.clone()]).unwrap();
    let snapshot = inventory.capture();
    assert_eq!(snapshot.revision, 1);
    assert_eq!(snapshot.queues[0].owner.session_id, "child");
    assert_eq!(snapshot.queues[0].owner.name, entry.owner.name);
    assert_eq!(snapshot.queues[0].item_count, 0);
    assert_eq!(
        inventory.register_owner(&[]).unwrap_err().kind,
        ErrorKind::Inventory
    );
    assert_eq!(
        inventory.update(entry.clone()).unwrap_err().kind,
        ErrorKind::Inventory
    );
    entry.owner.session_id.clear();
    assert_eq!(
        inventory.update(entry.clone()).unwrap_err().kind,
        ErrorKind::Argument
    );
    inventory.dispose();
    assert_eq!(
        inventory.update(entry).unwrap_err().kind,
        ErrorKind::Argument
    );
}

#[tokio::test]
async fn capacity_one_feed_coalesces_slow_readers_and_drains_terminal_before_eof() {
    let inventory = inventory();
    inventory.register_owner(&[]).unwrap();
    let mut slow = inventory.subscribe().unwrap();
    let mut fast = inventory.subscribe().unwrap();
    assert_eq!(bounded(fast.next()).await.unwrap().revision, 0);
    for count in 1..=32 {
        inventory.update(state("work", count)).unwrap();
        assert_eq!(bounded(fast.next()).await.unwrap().revision, count as u64);
    }
    assert_eq!(bounded(slow.next()).await.unwrap().revision, 32);
    assert!(slow.try_next().is_none());
    inventory.update(state("work", 33)).unwrap();
    inventory.dispose();
    for sub in [&mut slow, &mut fast] {
        let terminal = bounded(sub.next()).await.unwrap();
        assert!(terminal.removed);
        assert_eq!(terminal.revision, 34);
        assert!(bounded(sub.next()).await.is_none());
    }
    let feed = QueueInventoryFeed::new();
    feed.dispose();
    let mut closed = feed.subscribe(inventory.capture());
    assert!(bounded(closed.next()).await.is_none());
}

#[tokio::test]
async fn disposing_subscription_keeps_its_pending_value_then_eof_and_does_not_close_other_reader() {
    let inventory = inventory();
    inventory.register_owner(&[]).unwrap();
    let mut closed = inventory.subscribe().unwrap();
    let mut live = inventory.subscribe().unwrap();
    closed.dispose();
    assert_eq!(bounded(closed.next()).await.unwrap().revision, 0);
    assert!(bounded(closed.next()).await.is_none());
    inventory.update(state("work", 1)).unwrap();
    assert_eq!(bounded(live.next()).await.unwrap().revision, 1);
    inventory.dispose();
    assert!(bounded(live.next()).await.unwrap().removed);
    assert!(bounded(live.next()).await.is_none());
}
