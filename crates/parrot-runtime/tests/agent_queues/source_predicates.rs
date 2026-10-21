use super::{harness::*, support::*};
use parrot_runtime::children::{ChildError, ChildRegistry};
use parrot_storage::queue_files::Direction;
use std::{
    sync::{
        Arc, Mutex, Weak,
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
    time::Duration,
};

/// AgentQueueTests.cs:26-51, including the original own-path, exact-list and drain predicates.
#[tokio::test]
async fn direct_access_preserves_child_own_path_exact_list_and_parent_drain_items() {
    bounded(async {
        let h = Harness::new();
        let root = h.root().await;
        let child = h.spawn(&root, "child").await;
        let (r, c) = (h.q(&root), h.q(&child));
        let parent_queue = r.create("parent-work", "parent owned").unwrap();
        let child_queue = c.create("child-work", "child owned").unwrap();
        push(&r, "parent-work", &["parent-item"], false);
        push(&c, "child-work", &["child-item"], false);
        push(&c, "parent-work", &["from-child"], false);
        assert_eq!(c.get("child-work").unwrap().path, child_queue.path);
        assert_eq!(c.get("parent-work").unwrap().path, parent_queue.path);
        assert_eq!(c.get("parent-work").unwrap().size, 2);
        assert_eq!(names(&c), ["child-work", "parent-work"]);
        let own = c.try_take("child-work", 1, Direction::Front).unwrap();
        let inherited = c.try_take("parent-work", 2, Direction::Front).unwrap();
        assert_eq!(own.items, ["child-item"]);
        assert_eq!(inherited.items, ["parent-item", "from-child"]);
        assert_eq!(r.get("parent-work").unwrap().size, 0);
        h.close().await;
    })
    .await;
}

/// AgentQueueTests.cs:205-226: replacement uses the SAME child session ID and directory.
#[tokio::test]
async fn closed_child_replacement_with_same_id_lists_only_root_and_recreates_fresh_child_queue() {
    bounded(async {
        let h = Harness::new();
        let root = h.root().await;
        let child = h.spawn(&root, "child").await;
        let (r, c) = (h.q(&root), h.q(&child));
        let directory = h
            .factory
            .resources
            .agent_queue_directory(&child.identity().session.session_id)
            .unwrap();
        c.create("temporary-work", "").unwrap();
        r.create("root-work", "").unwrap();
        bounded(child.close().join()).await.unwrap();
        assert!(!directory.exists());
        assert!(r.get("root-work").unwrap().path.exists());
        root.children().detach(&child).unwrap().unwrap();
        let replacement = h.replacement_child(&root, &child).await;
        assert_eq!(
            replacement.identity().session.session_id,
            child.identity().session.session_id
        );
        assert!(!Arc::ptr_eq(replacement.identity(), child.identity()));
        let fresh = h.q(&replacement);
        assert!(!Arc::ptr_eq(&fresh, &c));
        assert_eq!(fresh.local().directory(), directory);
        assert_eq!(names(&fresh), ["root-work"]);
        fresh.create("fresh-work", "").unwrap();
        assert!(directory.exists());
        assert!(fresh.local().get("fresh-work").unwrap().path.exists());
        assert!(fresh.local().get("temporary-work").is_err());
        h.close().await;
    })
    .await;
}

/// QueueChildAdmissionTests.cs:196-233: behavioral gate observation replaces the source-only
/// IsHeldByCurrentThread introspection; a contender cannot enter the SAME actual registry gate
/// until the rejecting validator returns, then it acquires and is joined.
#[tokio::test]
async fn admission_validator_holds_actual_registry_gate_and_exact_rejection_never_inserts_child() {
    bounded(async {
        let h = Harness::new();
        let root = h.root().await;
        let child = h.spawn(&root, "worker").await;
        let target_slot = Arc::new(Mutex::new(None::<Weak<ChildRegistry<Security>>>));
        let called = Arc::new(AtomicBool::new(false));
        let held = Arc::new(AtomicBool::new(false));
        let order = Arc::new(Mutex::new(Vec::new()));
        let contender = Arc::new(Mutex::new(None));
        let completion = Arc::new(Mutex::new(None));
        let target = ChildRegistry::new(root.identity().clone(), {
            let (target_slot, called, held, order, contender, completion) = (
                target_slot.clone(),
                called.clone(),
                held.clone(),
                order.clone(),
                contender.clone(),
                completion.clone(),
            );
            Arc::new(move |_| {
                called.store(true, Ordering::SeqCst);
                order.lock().unwrap().push("validator-entered");
                let target = target_slot
                    .lock()
                    .unwrap()
                    .as_ref()
                    .unwrap()
                    .upgrade()
                    .unwrap();
                let (ready_tx, ready_rx) = mpsc::sync_channel(1);
                let (acquired_tx, acquired_rx) = mpsc::sync_channel(1);
                let acquisition_order = order.clone();
                *contender.lock().unwrap() = Some(std::thread::spawn(move || {
                    ready_tx.send(()).unwrap();
                    target.with_children(|_| {
                        acquisition_order.lock().unwrap().push("contender-acquired");
                        acquired_tx.send(()).unwrap();
                    });
                }));
                ready_rx.recv_timeout(Duration::from_secs(2)).unwrap();
                let blocked = matches!(
                    acquired_rx.recv_timeout(Duration::from_millis(50)),
                    Err(mpsc::RecvTimeoutError::Timeout)
                );
                held.store(blocked, Ordering::SeqCst);
                *completion.lock().unwrap() = Some(acquired_rx);
                order.lock().unwrap().push("validator-rejected");
                Err(ChildError::registry("rejected"))
            })
        });
        *target_slot.lock().unwrap() = Some(Arc::downgrade(&target));
        let error = target.try_add(child).unwrap_err();
        assert_eq!(error, ChildError::registry("rejected"));
        assert_eq!(error.to_string(), "rejected");
        assert!(called.load(Ordering::SeqCst));
        assert!(
            held.load(Ordering::SeqCst),
            "contender acquired actual registry gate during validator"
        );
        completion
            .lock()
            .unwrap()
            .take()
            .unwrap()
            .recv_timeout(Duration::from_secs(2))
            .unwrap();
        let worker = contender.lock().unwrap().take().unwrap();
        bounded(async {
            while !worker.is_finished() {
                tokio::task::yield_now().await;
            }
        })
        .await;
        worker.join().unwrap();
        assert_eq!(
            *order.lock().unwrap(),
            [
                "validator-entered",
                "validator-rejected",
                "contender-acquired"
            ]
        );
        assert!(target.snapshot_children().is_empty());
        assert!(target.find("worker").is_none());
        bounded(target.close().join()).await.unwrap();
        h.close().await;
    })
    .await;
}
