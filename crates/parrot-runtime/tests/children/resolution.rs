use super::{harness::Harness, support::*};
use parrot_runtime::children::*;
use std::sync::Arc;

#[tokio::test]
async fn security_refresh_is_conditional_and_preserves_new_model_and_profile() {
    use parrot_runtime::agent_turn::services::SelectionSource;

    let harness = Harness::new();
    let root = harness.root("root").await;
    let cell = root.selection();
    let original = cell.current().unwrap();
    let newer = selection(&harness.provider, "second", "new-mode", 0x03);
    let expected = newer.security.clone();
    cell.update(newer).unwrap();
    let denied: Result<(), &str> = cell.try_update_security(|current| {
        assert!(Arc::ptr_eq(current, &expected));
        Err("grant rejected")
    });
    assert_eq!(denied, Err("grant rejected"));
    let after_failure = cell.current().unwrap();
    assert!(Arc::ptr_eq(&after_failure.security, &expected));
    assert_eq!(after_failure.profile.id, "new-mode");
    assert!(after_failure.model.canonical_base().ends_with("/second"));

    let replacement = Security::new(0x07);
    cell.try_update_security::<()>(|current| {
        assert!(Arc::ptr_eq(current, &expected));
        Ok(replacement.clone())
    })
    .unwrap();
    let refreshed = cell.current().unwrap();
    assert!(Arc::ptr_eq(&refreshed.security, &replacement));
    assert_eq!(refreshed.profile.id, "new-mode");
    assert!(refreshed.model.canonical_base().ends_with("/second"));
    assert_eq!(original.security.permissions, 0xff);
    assert_eq!(expected.permissions, 0x03);
    harness.close().await;
}

#[tokio::test]
async fn explicit_paths_cross_branches_but_bare_names_ids_and_status_parent_alias_do_not() {
    bounded(async {
        let h = Harness::new();
        let root = h.root("root").await;
        let left = h.spawn(&root, "left").await;
        let right = h.spawn(&root, "right").await;
        let leaf = h.spawn(&left, "leaf").await;
        let named_root = h.spawn(&leaf, "left").await;
        let resolver = AgentResolver::new(&leaf, &h.registry);
        assert!(Arc::ptr_eq(
            &resolver.resolve_recipient("parent").unwrap(),
            left.session()
        ));
        assert!(Arc::ptr_eq(
            &resolver.resolve_recipient("left").unwrap(),
            left.session()
        ));
        assert!(Arc::ptr_eq(
            &resolver.resolve_status_scope("left").unwrap(),
            &named_root
        ));
        assert!(resolver.resolve_status_scope("parent").is_err());
        assert!(Arc::ptr_eq(
            &resolver.resolve_status_scope("..").unwrap(),
            &left
        ));
        assert!(Arc::ptr_eq(
            &resolver.resolve_status_scope("../../right").unwrap(),
            &right
        ));
        assert!(Arc::ptr_eq(
            &resolver
                .resolve_status_scope("parent/parent/right")
                .unwrap(),
            &right
        ));
        assert!(Arc::ptr_eq(
            &resolver.resolve_status_scope("/root/right").unwrap(),
            &right
        ));
        assert!(resolver.resolve_recipient("right").is_err());
        assert!(
            resolver
                .resolve_recipient(&right.identity().session.session_id)
                .is_err()
        );
        assert!(resolver.is_ancestor(root.session()));
        assert!(resolver.is_ancestor(left.session()));
        assert!(!resolver.is_ancestor(right.session()));
        assert!(!resolver.is_ancestor(leaf.session()));
        let (scope, name) = resolver.resolve_resource("../../right/inbox").unwrap();
        assert!(Arc::ptr_eq(&scope, &right));
        assert_eq!(name, "inbox");
        for invalid in [
            "",
            " ",
            "/",
            ".",
            "./left",
            "../",
            "//root",
            "/root/",
            "parent//right",
            "../../../..",
        ] {
            assert!(resolver.resolve_recipient(invalid).is_err(), "{invalid:?}");
        }
        for invalid in ["inbox", "/inbox", "left/"] {
            assert!(resolver.resolve_resource(invalid).is_err());
        }
        let duplicate = h.root("root").await;
        assert!(!Arc::ptr_eq(&root, &duplicate));
        assert!(resolver.resolve_recipient("/root/right").is_err());
        h.close().await;
    })
    .await;
}

#[tokio::test]
async fn registry_and_resolver_require_exact_registered_scope_not_matching_identity_fields() {
    bounded(async {
        let h = Harness::new();
        let root = h.root("root").await;
        let duplicate = bounded(
            Scope::create(ScopeConfig {
                authority: h.registry.clone(),
                identity: root.identity().clone(),
                history: root.history().clone(),
                selection: root.selection().clone(),
                parent: None,
                delivery: CompletionDelivery::RetainedOnly,
                retention: None,
                parts: ScopeParts {
                    session: root.session().clone(),
                    services: vec![],
                    publishers: vec![],
                    work: vec![],
                    resources: vec![],
                    final_resources: vec![],
                },
            })
            .join(),
        )
        .await
        .unwrap();
        assert!(!h.registry.contains(&duplicate));
        assert!(h.registry.register_root(duplicate.clone()).is_err());
        assert!(h.registry.unregister_root(&duplicate).is_err());
        assert!(
            AgentResolver::new(&duplicate, &h.registry)
                .resolve_recipient("/root")
                .is_err()
        );
        root.parent_scope().validate_owner(&root).unwrap();
        assert_eq!(root.parent_scope().validate_owner(&duplicate).err().unwrap().to_string(), format!("agent scope does not match child registry owner: {}", root.identity().session.session_id));
        let resolver = AgentResolver::new(&root, &h.registry);
        assert_eq!(resolver.resolve_recipient("  ").err().unwrap().code(), "argument");
        assert_eq!(resolver.resolve_status_scope("  ").err().unwrap().code(), "argument");
        assert_eq!(resolver.resolve_resource("  ").err().unwrap().code(), "argument");
        assert_eq!(root.selection().lineage().count_profile("  ").err().unwrap().code(), "argument");
        let mut invalid_session = root.identity().session.clone();
        invalid_session.depth = 1; invalid_session.parent_session_id = "expected-parent-id".into();
        let invalid_identity = Arc::new(AgentIdentity { session: invalid_session, parent_name: "root".into(), name_path: vec!["root".into(), "invalid".into()], prompt_scope: root.identity().prompt_scope.clone(), templates: root.identity().templates.clone() });
        let parent = ParentScope::completion(invalid_identity.clone(), Some(&root), CompletionDelivery::RetainedOnly, Arc::new(Ids));
        assert_eq!(parent.validate(&invalid_identity).err().unwrap().to_string(), format!("agent parent scope does not match parent session: expected expected-parent-id, actual {}", root.identity().session.session_id));
        let other = h.root("other").await;
        assert!(
            bounded(
                root.spawner()
                    .spawn(h.request(&other, "wrong-owner"))
                    .unwrap()
                    .join()
            )
            .await
            .err()
            .unwrap()
            .to_string()
            .contains("launch parent does not match")
        );
        h.registry.unregister_root(&root).unwrap();
        assert!(
            AgentResolver::new(&root, &h.registry)
                .resolve_recipient("/root")
                .is_err()
        );
        bounded(root.close().join()).await.unwrap();
        h.close().await;
    })
    .await;
}

#[tokio::test]
async fn scoped_typeid_services_are_borrowed_exact_arcs_and_remain_available_after_close() {
    bounded(async {
        let h = Harness::new();
        let root = h.root("root").await;
        let child = h.spawn(&root, "child").await;
        let concrete = child.get_service::<Marker>().unwrap();
        let erased = child.get_service::<dyn BorrowedService>().unwrap();
        let expected: Arc<dyn BorrowedService> = concrete;
        assert!(Arc::ptr_eq(&erased, &expected));
        assert_eq!(erased.name(), "child");
        assert!(child.get_service::<String>().is_err());
        assert!(!Arc::ptr_eq(
            &erased,
            &root.get_service::<dyn BorrowedService>().unwrap()
        ));
        bounded(child.close().join()).await.unwrap();
        assert!(Arc::ptr_eq(
            &erased,
            &child.get_service::<dyn BorrowedService>().unwrap()
        ));
        assert_eq!(erased.name(), "child");
        h.close().await;
    })
    .await;
}
