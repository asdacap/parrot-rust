use super::{harness::Harness, support::*};
use parrot_runtime::children::*;
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};
use tokio::sync::Barrier;

#[tokio::test]
async fn sixteen_get_or_spawn_callers_share_one_construction_and_reuse_an_exhausted_budget() {
    bounded(async {
        let h = Harness::with_capacity(1);
        let root = h.root("root").await;
        let gate = Gate::new();
        *h.factory.construction.lock().unwrap() = Some(gate.clone());
        let callbacks = Arc::new(AtomicUsize::new(0));
        let start = Arc::new(Barrier::new(17));
        let mut callers = Vec::new();
        for index in 0..16 {
            let request = h.request(&root, "helper");
            let spawner = root.spawner().clone();
            let start = start.clone();
            let callbacks = callbacks.clone();
            callers.push(tokio::spawn(async move {
                start.wait().await;
                spawner
                    .get_or_spawn(
                        if index % 2 == 0 { "HELPER!!" } else { "helper" }.into(),
                        move || {
                            callbacks.fetch_add(1, Ordering::SeqCst);
                            Ok(request)
                        },
                    )
                    .unwrap()
                    .join()
                    .await
                    .unwrap()
            }));
        }
        start.wait().await;
        gate.entered().await;
        assert_eq!(callbacks.load(Ordering::SeqCst), 1);
        assert_eq!(h.factory.build_calls.load(Ordering::SeqCst), 2);
        gate.open();
        let mut children = Vec::new();
        for caller in callers {
            children.push(caller.await.unwrap());
        }
        let child = &children[0];
        assert!(children.iter().all(|other| Arc::ptr_eq(child, other)));
        assert_eq!(callbacks.load(Ordering::SeqCst), 1);
        assert_eq!(h.factory.build_calls.load(Ordering::SeqCst), 2);
        let descendants = root.children().snapshot_descendants();
        assert_eq!(descendants.len(), 1);
        assert!(Arc::ptr_eq(&descendants[0], child.session()));
        let rejected = root
            .spawner()
            .spawn(h.request(&root, "another-child"))
            .unwrap()
            .join()
            .await;
        assert_eq!(
            rejected.err().unwrap().to_string(),
            "subagent retention limit reached"
        );
        let reused = root
            .spawner()
            .get_or_spawn("helper".into(), || {
                Err(ChildError::registry(
                    "an exhausted budget must not evaluate the launch callback",
                ))
            })
            .unwrap()
            .join()
            .await
            .unwrap();
        assert!(Arc::ptr_eq(child, &reused));
        assert_eq!(h.factory.build_calls.load(Ordering::SeqCst), 2);
        h.close().await;
    })
    .await;
}

#[tokio::test]
async fn eight_spawn_or_resume_callers_construct_one_actual_scope() {
    bounded(async {
        let h = Harness::with_capacity(4);
        let root = h.root("root").await;
        let gate = Gate::new();
        *h.factory.construction.lock().unwrap() = Some(gate.clone());
        let start = Arc::new(Barrier::new(9));
        let mut callers = Vec::new();
        for _ in 0..8 {
            let request = h.request(&root, "SHARED..Helper");
            let spawner = root.spawner().clone();
            let start = start.clone();
            callers.push(tokio::spawn(async move {
                start.wait().await;
                spawner
                    .spawn_or_resume(request)
                    .unwrap()
                    .join()
                    .await
                    .unwrap()
            }));
        }
        start.wait().await;
        gate.entered().await;
        assert_eq!(h.factory.build_calls.load(Ordering::SeqCst), 2);
        gate.open();
        let mut children = Vec::new();
        for caller in callers {
            children.push(caller.await.unwrap());
        }
        let child = &children[0];
        assert!(children.iter().all(|other| Arc::ptr_eq(child, other)));
        assert_eq!(child.identity().session.name, "shared-helper");
        let descendants = root.children().snapshot_descendants();
        assert_eq!(descendants.len(), 1);
        assert!(Arc::ptr_eq(&descendants[0], child.session()));
        assert_eq!(h.factory.build_calls.load(Ordering::SeqCst), 2);
        h.close().await;
    })
    .await;
}

#[tokio::test]
async fn punctuation_and_empty_get_or_spawn_names_keep_each_generated_id_fallback() {
    bounded(async {
        for requested_name in ["!!!", ""] {
            let h = Harness::with_capacity(2);
            let root = h.root("root").await;
            let mut children = Vec::new();
            for _ in 0..2 {
                let request = h.request(&root, requested_name);
                children.push(
                    root.spawner()
                        .get_or_spawn(requested_name.into(), move || Ok(request))
                        .unwrap()
                        .join()
                        .await
                        .unwrap(),
                );
            }
            for child in &children {
                let session = &child.identity().session;
                assert!(session.session_id.len() >= 6);
                assert_eq!(
                    session.name,
                    format!(
                        "agent-{}",
                        &session.session_id[session.session_id.len() - 6..]
                    )
                );
            }
            assert!(!Arc::ptr_eq(&children[0], &children[1]));
            assert_ne!(
                children[0].identity().session.session_id,
                children[1].identity().session.session_id
            );
            assert_ne!(
                children[0].identity().session.name,
                children[1].identity().session.name
            );
            let descendants = root.children().snapshot_descendants();
            assert_eq!(descendants.len(), 2);
            assert!(Arc::ptr_eq(&descendants[0], children[0].session()));
            assert!(Arc::ptr_eq(&descendants[1], children[1].session()));
            assert_eq!(h.factory.build_calls.load(Ordering::SeqCst), 3);
            h.close().await;
        }
    })
    .await;
}

#[tokio::test]
async fn session_capacity_counts_direct_children_and_grandchildren_together() {
    bounded(async {
        let h = Harness::with_capacity(2);
        let root = h.root("root").await;
        let child = h.spawn(&root, "first").await;
        let grandchild = h.spawn(&child, "descendant").await;
        assert_eq!(
            grandchild.identity().session.parent_session_id,
            child.identity().session.session_id
        );
        let descendants = root.children().snapshot_descendants();
        assert_eq!(descendants.len(), 2);
        assert!(Arc::ptr_eq(&descendants[0], child.session()));
        assert!(Arc::ptr_eq(&descendants[1], grandchild.session()));
        let rejected = root
            .spawner()
            .spawn(h.request(&root, "third"))
            .unwrap()
            .join()
            .await;
        assert_eq!(
            rejected.err().unwrap().to_string(),
            "subagent retention limit reached"
        );
        assert_eq!(h.factory.build_calls.load(Ordering::SeqCst), 3);
        assert_eq!(root.children().snapshot_descendants().len(), 2);
        h.close().await;
    })
    .await;
}

#[tokio::test]
async fn detaching_and_closing_the_exact_scope_releases_name_and_single_retained_slot() {
    bounded(async {
        let h = Harness::with_capacity(1);
        let root = h.root("root").await;
        let original = h.spawn(&root, "worker").await;
        let original_built = h.factory.get("worker");
        let detached = root.children().detach(&original).unwrap().unwrap();
        assert!(Arc::ptr_eq(&detached, &original));
        assert!(root.children().snapshot_descendants().is_empty());
        assert!(h.budget.reserve().is_err());
        detached.close().join().await.unwrap();
        assert_eq!(original_built.output.closes.load(Ordering::SeqCst), 1);
        let replacement = h.spawn(&root, "worker").await;
        assert_eq!(replacement.identity().session.name, "worker");
        assert!(!Arc::ptr_eq(&replacement, &original));
        assert_ne!(
            replacement.identity().session.session_id,
            original.identity().session.session_id
        );
        let descendants = root.children().snapshot_descendants();
        assert_eq!(descendants.len(), 1);
        assert!(Arc::ptr_eq(&descendants[0], replacement.session()));
        assert_eq!(h.factory.build_calls.load(Ordering::SeqCst), 3);
        h.close().await;
        assert_eq!(original_built.output.closes.load(Ordering::SeqCst), 1);
    })
    .await;
}

#[tokio::test]
async fn pending_factory_construction_reserves_capacity_before_the_gate_releases() {
    bounded(async {
        let h = Harness::with_capacity(1);
        let root = h.root("root").await;
        let gate = Gate::new();
        *h.factory.construction.lock().unwrap() = Some(gate.clone());
        let pending = root.spawner().spawn(h.request(&root, "pending")).unwrap();
        gate.entered().await;
        assert!(!pending.is_finished());
        assert!(root.children().snapshot_descendants().is_empty());
        let rejected = root
            .spawner()
            .spawn(h.request(&root, "rejected"))
            .unwrap()
            .join()
            .await;
        assert_eq!(
            rejected.err().unwrap().to_string(),
            "subagent retention limit reached"
        );
        assert!(!pending.is_finished());
        assert_eq!(h.factory.build_calls.load(Ordering::SeqCst), 2);
        gate.open();
        let child = pending.join().await.unwrap();
        let descendants = root.children().snapshot_descendants();
        assert_eq!(descendants.len(), 1);
        assert!(Arc::ptr_eq(&descendants[0], child.session()));
        assert_eq!(h.factory.build_calls.load(Ordering::SeqCst), 2);
        h.close().await;
    })
    .await;
}

#[tokio::test]
async fn duplicate_registration_and_wrong_scope_detach_preserve_the_original_actual_scope() {
    bounded(async {
        let h = Harness::new();
        let root = h.root("parent").await;
        let original = h.unregistered_child(&root, "duplicate").await;
        let duplicate = h.unregistered_child(&root, "duplicate").await;
        assert!(!Arc::ptr_eq(&original, &duplicate));
        assert!(!Arc::ptr_eq(original.session(), duplicate.session()));
        assert_ne!(
            original.identity().session.session_id,
            duplicate.identity().session.session_id
        );
        assert!(root.children().try_add(original.clone()).unwrap());
        let conflict = root.children().try_add(duplicate.clone()).err().unwrap();
        assert!(matches!(conflict, ChildError::NameConflict(_)));
        assert_eq!(
            conflict.to_string(),
            "child agent name is already registered: duplicate"
        );
        assert!(Arc::ptr_eq(
            &root.children().resolve("duplicate").unwrap(),
            &original
        ));
        let wrong_detach = root.children().detach(&duplicate).err().unwrap();
        assert_eq!(
            wrong_detach.to_string(),
            format!(
                "child agent scope not found: {}",
                duplicate.identity().session.session_id
            )
        );
        assert!(Arc::ptr_eq(
            &root.children().resolve("duplicate").unwrap(),
            &original
        ));
        let descendants = root.children().snapshot_descendants();
        assert_eq!(descendants.len(), 1);
        assert!(Arc::ptr_eq(&descendants[0], original.session()));
        duplicate.close().join().await.unwrap();
        assert!(Arc::ptr_eq(
            &root.children().resolve("duplicate").unwrap(),
            &original
        ));
        assert_eq!(root.children().snapshot_descendants().len(), 1);
        h.close().await;
    })
    .await;
}
