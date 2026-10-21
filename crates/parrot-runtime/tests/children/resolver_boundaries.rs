use super::{harness::Harness, support::*};
use parrot_runtime::children::*;
use std::sync::Arc;

#[tokio::test]
async fn literal_root_child_parent_names_and_branch_local_names_follow_exact_source_lookup_rules() {
    bounded(async {
        let h = Harness::new();
        let root = h.root("main").await;
        let literal_root = h.spawn(&root, "root").await;
        let literal_child = h.spawn(&root, "child").await;
        let literal_parent = h.spawn(&root, "parent").await;
        let root_resolver = AgentResolver::new(&root, &h.registry);
        for (name, scope) in [
            ("root", &literal_root),
            ("child", &literal_child),
            ("parent", &literal_parent),
        ] {
            assert!(Arc::ptr_eq(
                &root_resolver.resolve_status_scope(name).unwrap(),
                scope
            ));
            assert!(Arc::ptr_eq(
                &root_resolver.resolve_recipient(name).unwrap(),
                scope.session()
            ));
        }
        let left = h.spawn(&root, "left").await;
        let right = h.spawn(&root, "right").await;
        let left_worker = h.spawn(&left, "worker").await;
        let right_worker = h.spawn(&right, "worker").await;
        assert_eq!(
            left_worker.identity().session.name,
            right_worker.identity().session.name
        );
        assert_ne!(
            left_worker.identity().session.session_id,
            right_worker.identity().session.session_id
        );
        assert_eq!(left_worker.identity().name_path, ["main", "left", "worker"]);
        assert_eq!(
            right_worker.identity().name_path,
            ["main", "right", "worker"]
        );
        let local_parent = h.spawn(&left, "parent").await;
        let parent_named_child = h.spawn(&left, "main").await;
        let left_resolver = AgentResolver::new(&left, &h.registry);
        assert!(Arc::ptr_eq(
            &left_resolver.resolve_status_scope("parent").unwrap(),
            &local_parent
        ));
        assert!(Arc::ptr_eq(
            &left_resolver.resolve_recipient("parent").unwrap(),
            root.session()
        ));
        assert!(Arc::ptr_eq(
            &left_resolver.resolve_status_scope("main").unwrap(),
            &parent_named_child
        ));
        assert!(Arc::ptr_eq(
            &left_resolver.resolve_recipient("main").unwrap(),
            root.session()
        ));
        assert!(Arc::ptr_eq(
            &left_resolver.resolve_status_scope("worker").unwrap(),
            &left_worker
        ));
        assert!(Arc::ptr_eq(
            &left_resolver
                .resolve_status_scope("../right/worker")
                .unwrap(),
            &right_worker
        ));
        assert!(root_resolver.resolve_recipient("worker").is_err());
        h.close().await;
    })
    .await;
}

#[tokio::test]
async fn each_lookup_entrypoint_rejects_an_unregistered_real_child_with_exact_owner_error() {
    bounded(async {
        let h = Harness::new();
        let root = h.root("root").await;
        let orphan = h.unregistered_child(&root, "orphan").await;
        let resolver = AgentResolver::new(&orphan, &h.registry);
        let expected = format!(
            "parent agent scope not found: {}",
            orphan.identity().session.session_id
        );
        let errors = [
            resolver.resolve_recipient("parent").err().unwrap(),
            resolver.resolve_recipient("/root").err().unwrap(),
            resolver.resolve_status_scope("child").err().unwrap(),
            resolver.resolve_status_scope("/root").err().unwrap(),
            resolver.resolve_status("..").err().unwrap(),
            resolver.resolve_resource("/root/inbox").err().unwrap(),
        ];
        for error in errors {
            assert_eq!(error.code(), "invalid_operation");
            assert_eq!(error.to_string(), expected);
        }
        assert!(
            orphan
                .spawner()
                .spawn(h.request(&orphan, "cannot-spawn"))
                .unwrap()
                .join()
                .await
                .err()
                .unwrap()
                .to_string()
                .contains(&expected)
        );
        bounded(orphan.close().join()).await.unwrap();
        h.close().await;
    })
    .await;
}
