use super::{harness::*, support::*};
use parrot_runtime::{agent_queues::QueueError, children::*};
use parrot_storage::queue_files::ErrorKind;
use std::sync::{
    Arc, Barrier,
    atomic::{AtomicUsize, Ordering},
};

async fn join_racers<T: Send + 'static>(
    a: std::thread::JoinHandle<T>,
    b: std::thread::JoinHandle<T>,
) -> (T, T) {
    bounded(async {
        while !a.is_finished() || !b.is_finished() {
            tokio::task::yield_now().await;
        }
    })
    .await;
    (
        a.join()
            .unwrap_or_else(|_| panic!("first race worker panicked")),
        b.join()
            .unwrap_or_else(|_| panic!("second race worker panicked")),
    )
}

#[tokio::test]
async fn parent_child_queue_collisions_are_rejected_in_both_creation_orders() {
    bounded(async {
        let h = Harness::new();
        let root = h.root().await;
        let child = h.spawn(&root, "child").await;
        let (r, c) = (h.q(&root), h.q(&child));
        r.create("parent-first", "").unwrap();
        assert!(matches!(c.create("parent-first", ""), Err(QueueError::Storage(error)) if error.kind == ErrorKind::AlreadyExists));
        c.create("child-first", "").unwrap();
        assert!(matches!(r.create("child-first", ""), Err(QueueError::Storage(error)) if error.kind == ErrorKind::AlreadyExists));
        assert_eq!(
            r.get("parent-first").unwrap().path,
            c.get("parent-first").unwrap().path
        );
        assert!(matches!(r.get("child-first"), Err(QueueError::Storage(error)) if error.kind == ErrorKind::NotFound));
        h.close().await;
    })
    .await;
}

#[tokio::test]
async fn concurrent_parent_child_create_uses_the_actual_registry_gates_and_has_one_physical_winner()
{
    bounded(async {
        let h = Harness::new();
        let root = h.root().await;
        let child = h.spawn(&root, "child").await;
        let (r, c) = (h.q(&root), h.q(&child));
        for i in 0..12 {
            let start = Arc::new(Barrier::new(3));
            let name = format!("racing-{i}");
            let a = {
                let (q, start, name) = (r.clone(), start.clone(), name.clone());
                std::thread::spawn(move || {
                    start.wait();
                    q.create(&name, "").is_ok()
                })
            };
            let b = {
                let (q, start, name) = (c.clone(), start.clone(), name.clone());
                std::thread::spawn(move || {
                    start.wait();
                    q.create(&name, "").is_ok()
                })
            };
            start.wait();
            let (a, b) = join_racers(a, b).await;
            let outcomes = [a, b];
            assert_eq!(outcomes.into_iter().filter(|x| *x).count(), 1);
            let physical = [r.local().get(&name).ok(), c.local().get(&name).ok()]
                .into_iter()
                .flatten()
                .collect::<Vec<_>>();
            assert_eq!(physical.len(), 1);
            assert_eq!(c.get(&name).unwrap().path, physical[0].path);
        }
        h.close().await;
    })
    .await;
}

#[tokio::test]
async fn existing_detached_child_overlap_is_rejected_before_admission_and_both_local_queues_remain()
{
    bounded(async {
        let h = Harness::new();
        let root = h.root().await;
        let child = h.spawn(&root, "child").await;
        let (r, c) = (h.q(&root), h.q(&child));
        root.children().detach(&child).unwrap().unwrap();
        c.local().create("overlap", "child-owned").unwrap();
        r.create("overlap", "parent-owned").unwrap();
        assert!(matches!(c.validate_parent(), Err(QueueError::Storage(error)) if error.kind == ErrorKind::AlreadyExists));
        let rejection = root.children().try_add(child.clone()).unwrap_err();
        assert_eq!(rejection.code(), "unexpected");
        assert_eq!(rejection.to_string(), "queue: 'overlap' already exists");
        assert!(root.children().snapshot_children().is_empty());
        assert_eq!(r.get("overlap").unwrap().description, "parent-owned");
        assert_eq!(c.get("overlap").unwrap().description, "child-owned");
        bounded(child.close().join()).await.unwrap();
        h.close().await;
    })
    .await;
}

#[tokio::test]
async fn parent_create_racing_preloaded_child_admission_preserves_exactly_one_authoritative_owner()
{
    bounded(async {
        let h = Harness::new();
        let root = h.root().await;
        for i in 0..12 {
            let child = h.spawn(&root, &format!("child-{i}")).await;
            let (r, c) = (h.q(&root), h.q(&child));
            root.children().detach(&child).unwrap().unwrap();
            let name = format!("racing-{i}");
            c.create(&name, "child-owned").unwrap();
            let start = Arc::new(Barrier::new(3));
            let a = {
                let (children, child, start) =
                    (root.children().clone(), child.clone(), start.clone());
                std::thread::spawn(move || {
                    start.wait();
                    children.try_add(child).unwrap_or(false)
                })
            };
            let b = {
                let (r, name, start) = (r.clone(), name.clone(), start.clone());
                std::thread::spawn(move || {
                    start.wait();
                    r.create(&name, "parent-owned").is_ok()
                })
            };
            start.wait();
            let (admitted, created) = join_racers(a, b).await;
            assert_ne!(admitted, created);
            assert!(c.local().get(&name).is_ok());
            assert_eq!(r.local().get(&name).is_ok(), !admitted);
            assert_eq!(
                root.children().find(&format!("child-{i}")).is_some(),
                admitted
            );
            if admitted {
                root.children().detach(&child).unwrap();
            }
            bounded(child.close().join()).await.unwrap();
        }
        h.close().await;
    })
    .await;
}

#[tokio::test]
async fn malformed_parent_and_shutdown_precede_queue_admission_callback() {
    bounded(async {
        let h = Harness::new();
        let root = h.root().await;
        let child = h.spawn(&root, "child").await;
        let calls = Arc::new(AtomicUsize::new(0));
        let wrong_owner =
            AgentIdentity::root("different-parent".into(), "other".into(), templates());
        let target = ChildRegistry::new(wrong_owner, {
            let calls = calls.clone();
            Arc::new(move |_| {
                calls.fetch_add(1, Ordering::SeqCst);
                Ok(())
            })
        });
        assert!(matches!(
            target.try_add(child.clone()),
            Err(ChildError::Registry(_))
        ));
        assert_eq!(calls.load(Ordering::SeqCst), 0);
        let stopped = ChildRegistry::new(root.identity().clone(), {
            let calls = calls.clone();
            Arc::new(move |_| {
                calls.fetch_add(1, Ordering::SeqCst);
                Ok(())
            })
        });
        bounded(stopped.close().join()).await.unwrap();
        assert!(!stopped.try_add(child).unwrap());
        assert_eq!(calls.load(Ordering::SeqCst), 0);
        assert!(target.snapshot_children().is_empty());
        h.close().await;
    })
    .await;
}
