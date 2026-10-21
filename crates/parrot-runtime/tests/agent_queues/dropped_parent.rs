use super::{harness::*, support::*};
use parrot_domain::{
    contracts::events::EventSubscription, protocol::messages::queue::QueueSnapshot,
};
use parrot_storage::events::NeverCancelled;
use protobuf::Message;
use std::{
    sync::{Arc, mpsc},
    time::Duration,
};

fn release_factory_queue_refs(h: &Harness, name: &str) {
    h.factory.queues.lock().unwrap().remove(name);
    h.factory.publications.lock().unwrap().remove(name);
}

#[tokio::test]
async fn detached_child_scope_close_after_parent_scope_and_registry_drop_cleans_only_child_and_joins_terminal()
 {
    bounded(async {
        let h = Harness::new();
        let root = h.root().await;
        let child = h.spawn(&root, "detached").await;
        let (r, c) = (h.q(&root), h.q(&child));
        r.create("root-work", "").unwrap();
        c.create("child-work", "").unwrap();
        push(&c, "child-work", &["item"], false);
        let directory = c.local().directory().to_owned();
        let root_file = r.get("root-work").unwrap().path;
        let mut inventory = c.subscribe_inventory().unwrap();
        let _ = inventory.try_next();
        let events = h
            .factory
            .sessions
            .get("detached")
            .publisher
            .broker
            .subscribe();
        root.children().detach(&child).unwrap().unwrap();
        let weak_parent = Arc::downgrade(&root);
        let weak_registry = Arc::downgrade(root.children());
        let weak_parent_queues = Arc::downgrade(&r);
        let weak_child_queues = Arc::downgrade(&c);
        bounded(root.close().join()).await.unwrap();
        h.registry.unregister_root(&root).unwrap();
        drop(root);
        release_factory_queue_refs(&h, "root");
        drop(r);
        assert!(weak_parent.upgrade().is_none());
        assert!(weak_registry.upgrade().is_none());
        match c.create("after-parent-close", "") {
            Err(parrot_runtime::agent_queues::QueueError::Storage(error)) => {
                assert_eq!(error.kind, parrot_storage::queue_files::ErrorKind::Disposed);
            }
            _ => {
                panic!("child create must report disposed parent before accessing expired topology")
            }
        }
        bounded(child.close().join()).await.unwrap();
        assert!(!directory.exists());
        assert!(root_file.exists());
        let removed = bounded(inventory.next()).await.unwrap();
        assert!(removed.removed);
        assert!(removed.queues.is_empty());
        assert!(bounded(inventory.next()).await.is_none());
        let event = bounded(events.next(&NeverCancelled))
            .await
            .unwrap()
            .unwrap();
        assert!(event.tool_snapshot().removed);
        assert!(
            QueueSnapshot::parse_from_bytes(&event.tool_snapshot().data)
                .unwrap()
                .queues
                .is_empty()
        );
        release_factory_queue_refs(&h, "detached");
        drop(child);
        drop(c);
        assert!(weak_child_queues.upgrade().is_none());
        assert!(
            weak_parent_queues.upgrade().is_none(),
            "queue backlinks must not create a retention cycle"
        );
        h.close().await;
    })
    .await;
}

#[tokio::test]
async fn retained_actual_parent_registry_gate_still_serializes_dispose_after_parent_scope_is_gone()
{
    bounded(async {
        let h = Harness::new();
        let root = h.root().await;
        let child = h.spawn(&root, "detached").await;
        let c = h.q(&child);
        c.create("work", "").unwrap();
        let directory = c.local().directory().to_owned();
        root.children().detach(&child).unwrap().unwrap();
        let registry = root.children().clone();
        let weak_parent = Arc::downgrade(&root);
        bounded(root.close().join()).await.unwrap();
        h.registry.unregister_root(&root).unwrap();
        drop(root);
        assert!(weak_parent.upgrade().is_none());
        let (ready_tx, ready_rx) = mpsc::sync_channel(1);
        let (release_tx, release_rx) = mpsc::sync_channel(1);
        let holder = std::thread::spawn(move || {
            registry.with_children(|_| {
                ready_tx.send(()).unwrap();
                release_rx.recv_timeout(Duration::from_secs(5)).unwrap();
            })
        });
        ready_rx.recv_timeout(Duration::from_secs(5)).unwrap();
        let disposer = {
            let c = c.clone();
            std::thread::spawn(move || c.dispose())
        };
        bounded(async {
            while !c.is_disposed() {
                tokio::task::yield_now().await;
            }
        })
        .await;
        assert!(
            !disposer.is_finished(),
            "actual retained parent registry lock must not be skipped when only scope expired"
        );
        assert!(directory.exists());
        release_tx.send(()).unwrap();
        bounded(async {
            while !holder.is_finished() || !disposer.is_finished() {
                tokio::task::yield_now().await;
            }
        })
        .await;
        holder.join().unwrap();
        disposer.join().unwrap().unwrap();
        assert!(!directory.exists());
        bounded(child.close().join()).await.unwrap();
        h.close().await;
    })
    .await;
}
