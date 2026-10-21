use super::{harness::Harness, support::*};
use parrot_runtime::children::ChildRegistry;
use std::{
    sync::{Arc, mpsc},
    thread,
    time::Duration,
};

const DEADLINE: Duration = Duration::from_secs(5);
const EXCLUSION_WINDOW: Duration = Duration::from_millis(100);

#[tokio::test]
async fn children_callback_excludes_real_admission_and_matches_snapshots_after_shutdown() {
    bounded(async {
        let h = Harness::new();
        let root = h.root("root").await;
        let child = h.unregistered_child(&root, "helper").await;
        let registry = root.children();
        assert!(registry.with_children(|children| children.is_empty()));
        thread::scope(|threads| {
            let (entered_tx, entered_rx) = mpsc::channel();
            let (release_tx, release_rx) = mpsc::channel();
            let callback = threads.spawn(move || {
                registry.with_children(|children| {
                    assert!(children.is_empty());
                    entered_tx.send(()).unwrap();
                    release_rx.recv_timeout(DEADLINE).unwrap();
                    children.len()
                })
            });
            entered_rx.recv_timeout(DEADLINE).unwrap();
            let (attempt_tx, attempt_rx) = mpsc::channel();
            let (admitted_tx, admitted_rx) = mpsc::channel();
            let candidate = child.clone();
            let admission = threads.spawn(move || {
                attempt_tx.send(()).unwrap();
                let result = registry.try_add(candidate);
                admitted_tx.send(result).unwrap();
            });
            attempt_rx.recv_timeout(DEADLINE).unwrap();
            let blocked = admitted_rx.recv_timeout(EXCLUSION_WINDOW);
            release_tx.send(()).unwrap();
            assert_eq!(callback.join().unwrap(), 0);
            admission.join().unwrap();
            assert!(matches!(blocked, Err(mpsc::RecvTimeoutError::Timeout)));
            assert!(admitted_rx.recv_timeout(DEADLINE).unwrap().unwrap());
        });
        let snapshot = registry.snapshot_children();
        let borrowed = registry.with_children(|children| {
            assert_eq!(children.len(), 1);
            assert!(Arc::ptr_eq(&children[0], &child));
            children.to_vec()
        });
        assert!(Arc::ptr_eq(&snapshot[0], &borrowed[0]));
        registry.close().join().await.unwrap();
        assert!(registry.snapshot_children().is_empty());
        assert!(registry.with_children(|children| children.is_empty()));
        assert!(!registry.try_add(child).unwrap());
        h.close().await;
    })
    .await;
}

#[tokio::test]
async fn admission_validator_excludes_children_callback_under_the_same_registry_gate() {
    bounded(async {
        let h = Harness::new();
        let root = h.root("root").await;
        let child = h.unregistered_child(&root, "helper").await;
        let (entered_tx, entered_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();
        let release_rx = std::sync::Mutex::new(release_rx);
        let expected = child.clone();
        let registry = ChildRegistry::new(
            root.identity().clone(),
            Arc::new(move |scope| {
                assert!(Arc::ptr_eq(scope, &expected));
                entered_tx.send(()).unwrap();
                release_rx.lock().unwrap().recv_timeout(DEADLINE).unwrap();
                Ok(())
            }),
        );
        thread::scope(|threads| {
            let registry = &registry;
            let candidate = child.clone();
            let admission = threads.spawn(move || registry.try_add(candidate));
            entered_rx.recv_timeout(DEADLINE).unwrap();
            let (attempt_tx, attempt_rx) = mpsc::channel();
            let (observed_tx, observed_rx) = mpsc::channel();
            let callback = threads.spawn(move || {
                attempt_tx.send(()).unwrap();
                registry.with_children(|children| {
                    observed_tx.send(children.to_vec()).unwrap();
                });
            });
            attempt_rx.recv_timeout(DEADLINE).unwrap();
            let blocked = observed_rx.recv_timeout(EXCLUSION_WINDOW);
            release_tx.send(()).unwrap();
            assert!(admission.join().unwrap().unwrap());
            callback.join().unwrap();
            assert!(matches!(blocked, Err(mpsc::RecvTimeoutError::Timeout)));
            let observed = observed_rx.recv_timeout(DEADLINE).unwrap();
            assert_eq!(observed.len(), 1);
            assert!(Arc::ptr_eq(&observed[0], &child));
        });
        registry.close().join().await.unwrap();
        assert!(registry.with_children(|children| children.is_empty()));
        h.close().await;
    })
    .await;
}
