use super::{harness::*, support::*};
use parrot_runtime::{agent_queues::*, children::AgentIdentity};
use parrot_storage::queue_files::{ErrorKind, Owner};
use std::sync::Arc;

fn inventory_for(identity: &AgentIdentity) -> Arc<QueueInventory> {
    QueueInventory::new(
        Owner {
            session_id: identity.session.session_id.clone(),
            name: identity.session.name.clone(),
            parent_session_id: identity.session.parent_session_id.clone(),
            parent_name: identity.parent_name.clone(),
        },
        format!("queue-inventory-{}", id()),
    )
    .unwrap()
}

#[tokio::test]
async fn binding_requires_exact_registered_queue_instance_and_identity_not_just_matching_ids() {
    bounded(async {
        let h = Harness::new();
        let root = h.root().await;
        let q = h.q(&root);
        assert!(Arc::ptr_eq(&q, &h.factory.queues("root")));
        let alternate = AgentQueues::<Security>::new(
            root.identity().clone(),
            h.factory.resources.as_ref(),
            inventory_for(root.identity()),
            h.factory.diagnostics.clone(),
        )
        .unwrap();
        assert!(
            alternate.bind_scope(&root).is_err(),
            "same identity but unregistered service instance must reject"
        );
        let spoof = AgentIdentity::root(
            root.identity().session.session_id.clone(),
            "root".into(),
            templates(),
        );
        let other = AgentQueues::<Security>::new(
            spoof.clone(),
            h.factory.resources.as_ref(),
            inventory_for(&spoof),
            h.factory.diagnostics.clone(),
        )
        .unwrap();
        assert!(
            other.bind_scope(&root).is_err(),
            "matching ID text does not replace identity Arc"
        );
        alternate
            .local()
            .create("before-bind", "local constructor staging")
            .unwrap();
        assert_eq!(alternate.local().get("before-bind").unwrap().size, 0);
        assert!(alternate.create("runtime-before-bind", "").is_err());
        alternate.dispose().unwrap();
        other.dispose().unwrap();
        h.close().await;
    })
    .await;
}

#[tokio::test]
async fn actual_selected_resource_factory_keeps_child_directories_canonical_and_rejects_escape_ids()
{
    bounded(async {
        let h = Harness::new();
        let root = h.root().await;
        let child = h.spawn(&root, "child").await;
        let resources = &h.factory.resources;
        let expected = resources
            .agent_queue_root_directory()
            .join(&child.identity().session.session_id);
        assert_eq!(h.q(&child).local().directory(), expected);
        assert!(
            resources
                .owns(&expected, &std::env::current_dir().unwrap())
                .unwrap()
        );
        for id in ["../escaped", "..", "/"] {
            assert_eq!(
                resources.agent_queue_directory(id).unwrap_err().kind(),
                std::io::ErrorKind::InvalidInput
            );
            let identity = Arc::new(AgentIdentity {
                session: parrot_runtime::agent_turn::services::SessionIdentity {
                    session_id: id.into(),
                    name: "invalid".into(),
                    parent_session_id: root.identity().session.session_id.clone(),
                    depth: 1,
                },
                parent_name: "root".into(),
                name_path: vec!["root".into(), "invalid".into()],
                prompt_scope: root
                    .identity()
                    .prompt_scope
                    .derive_child("invalid", 1, "test"),
                templates: templates(),
            });
            assert!(matches!(
                AgentQueues::<Security>::new(
                    identity.clone(),
                    resources.as_ref(),
                    inventory_for(&identity),
                    h.factory.diagnostics.clone()
                ),
                Err(QueueError::Storage(error)) if error.kind == ErrorKind::Argument
            ));
        }
        h.close().await;
    })
    .await;
}

#[tokio::test]
async fn reattaching_same_scope_preserves_queue_inventory_and_closing_keeps_borrowed_service() {
    bounded(async {
        let h = Harness::new();
        let root = h.root().await;
        let q = h.q(&root);
        q.create("work", "before reattachment").unwrap();
        push(&q, "work", &["item"], false);
        let mut subscription = q.subscribe_inventory().unwrap();
        let before = bounded(subscription.next()).await.unwrap();
        q.attach(root.session()).unwrap();
        q.bind_scope(&root).unwrap();
        assert_eq!(q.capture_inventory(), before);
        assert!(subscription.try_next().is_none());
        assert_eq!(q.get("work").unwrap().size, 1);
        let snapshot = q.snapshot().unwrap();
        assert!(Arc::ptr_eq(&snapshot.identity, root.identity()));
        assert_eq!(snapshot.queues.len(), 1);
        bounded(root.close().join()).await.unwrap();
        assert!(Arc::ptr_eq(&q, &h.q(&root)));
        assert!(q.bind_scope(&root).is_err());
        assert!(matches!(q.attach(root.session()), Err(QueueError::Storage(error)) if error.kind == ErrorKind::Disposed));
        assert!(q.snapshot().unwrap().queues.is_empty());
        h.close().await;
    })
    .await;
}

#[tokio::test]
async fn queue_diagnostics_record_only_source_creation_and_explicit_close_events() {
    bounded(async {
        let h = Harness::new();
        let root = h.root().await;
        let q = h.q(&root);
        q.create("work", "private description").unwrap();
        push(&q, "work", &["private data"], false);
        push(&q, "work", &[], true);
        let diagnostics = h.factory.diagnostics.0.lock().unwrap().clone();
        assert_eq!(diagnostics.len(), 2);
        assert_eq!(
            diagnostics
                .iter()
                .map(|event| (event.event, event.outcome))
                .collect::<Vec<_>>(),
            [("created", "created"), ("closed", "closed")]
        );
        assert!(
            diagnostics
                .iter()
                .all(|event| event.agent_session_id == root.identity().session.session_id)
        );
        h.close().await;
    })
    .await;
}
