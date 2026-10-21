use super::{harness::*, support::*};
use parrot_runtime::{
    agent_queues::QueueError, agent_turn::lifecycle::Signal, children::AgentResolver,
};
use parrot_storage::{
    events::NeverCancelled,
    queue_files::{Direction, ErrorKind},
};

#[tokio::test]
async fn actual_scope_queues_route_only_owner_and_direct_parent_and_preserve_sibling_isolation() {
    bounded(async {
        let h = Harness::new();
        let root = h.root().await;
        let left = h.spawn(&root, "left").await;
        let right = h.spawn(&root, "right").await;
        let grandchild = h.spawn(&left, "grandchild").await;
        let (r, l, s, g) = (h.q(&root), h.q(&left), h.q(&right), h.q(&grandchild));
        r.create("root-secret", "root owned").unwrap();
        l.create("left-secret", "left owned").unwrap();
        g.create("grandchild-secret", "nested").unwrap();
        push(&r, "root-secret", &["root-item"], false);
        push(&l, "root-secret", &["from-child"], false);
        assert_eq!(
            l.get("root-secret").unwrap().path,
            r.get("root-secret").unwrap().path
        );
        assert_eq!(l.get("root-secret").unwrap().size, 2);
        assert_eq!(names(&g), ["grandchild-secret", "left-secret"]);
        assert_eq!(names(&s), ["root-secret"]);
        for (queues, name) in [
            (&r, "left-secret"),
            (&s, "left-secret"),
            (&g, "root-secret"),
            (&r, "grandchild-secret"),
            (&s, "never-created"),
        ] {
            let error = queues.get(name).unwrap_err();
            assert_eq!(error.to_string(), format!("queue: '{name}' was not found"));
            assert!(matches!(error, QueueError::Storage(error) if error.kind == ErrorKind::NotFound));
        }
        let a = l.create("shared-name", "left").unwrap();
        let b = s.create("shared-name", "right").unwrap();
        assert_ne!(a.path, b.path);
        push(&l, "shared-name", &["left-item"], false);
        push(&s, "shared-name", &["right-item"], false);
        assert_eq!(
            l.try_take("shared-name", 1, Direction::Front)
                .unwrap()
                .items,
            ["left-item"]
        );
        assert_eq!(
            s.try_take("shared-name", 1, Direction::Front)
                .unwrap()
                .items,
            ["right-item"]
        );
        assert!(matches!(r.get("shared-name"), Err(QueueError::Storage(error)) if error.kind == ErrorKind::NotFound));
        assert!(matches!(r.create("shared-name", ""), Err(QueueError::Storage(error)) if error.kind == ErrorKind::AlreadyExists));
        h.close().await;
    })
    .await;
}

#[tokio::test]
async fn explicit_relative_and_absolute_queue_addresses_select_exact_local_store_without_fallback()
{
    bounded(async {
        let h = Harness::new();
        let root = h.root().await;
        let left = h.spawn(&root, "left").await;
        let right = h.spawn(&root, "right").await;
        let (r, l, s) = (h.q(&root), h.q(&left), h.q(&right));
        r.create("root-work", "").unwrap();
        s.create("right-work", "").unwrap();
        push(&s, "right-work", &["one"], false);
        let resolver = AgentResolver::new(&left, &h.registry);
        for address in [
            "parent/right/right-work",
            "../right/right-work",
            "/root/right/right-work",
        ] {
            let (store, name) = l.resolve_address(&resolver, address).unwrap();
            assert_eq!(
                store.get(&name).unwrap().path,
                s.local().get("right-work").unwrap().path
            );
            assert_eq!(store.get(&name).unwrap().size, 1);
        }
        let (store, name) = l
            .resolve_address(&resolver, "parent/right/root-work")
            .unwrap();
        assert_eq!(store.get(&name).unwrap_err().kind, ErrorKind::NotFound);
        assert_eq!(
            s.get("root-work").unwrap().path,
            r.get("root-work").unwrap().path,
            "bare resolution has parent fallback, explicit path must not"
        );
        for address in [
            "right-work",
            "right-agent-id",
            "/root//right-work",
            "/root/right/",
            "./right-work",
        ] {
            assert!(
                l.resolve_address(&resolver, address)
                    .and_then(|(store, name)| store.get(&name).map_err(Into::into))
                    .is_err(),
                "{address}"
            );
        }
        let (store, name) = l.resolve_address(&resolver, "parent/root-work").unwrap();
        store
            .push(
                &name,
                &["from-explicit-child".into()],
                Direction::Back,
                false,
            )
            .unwrap();
        assert_eq!(r.get("root-work").unwrap().size, 1);
        h.close().await;
    })
    .await;
}

#[tokio::test]
async fn final_items_close_is_idempotent_and_front_back_draining_preserves_source_order() {
    bounded(async {
        let h = Harness::new();
        let root = h.root().await;
        let child = h.spawn(&root, "child").await;
        let q = h.q(&child);
        q.create("work", "").unwrap();
        push(&q, "work", &["middle"], false);
        q.push(
            "work",
            &["first".into(), "second".into()],
            Direction::Front,
            false,
            &NeverCancelled,
        )
        .unwrap();
        let closed = q
            .push(
                "work",
                &["last".into()],
                Direction::Back,
                true,
                &NeverCancelled,
            )
            .unwrap();
        assert!(closed.closed);
        assert_eq!(closed.size, 4);
        let repeat = q
            .push("work", &[], Direction::Back, true, &NeverCancelled)
            .unwrap();
        assert!(repeat.closed);
        assert_eq!(repeat.size, 4);
        assert_eq!(
            q.try_take("work", 2, Direction::Back).unwrap().items,
            ["last", "middle"]
        );
        assert_eq!(
            q.try_take("work", 2, Direction::Front).unwrap().items,
            ["second", "first"],
            "source QueueStore.Push reverses a front-pushed batch"
        );
        let empty = q.try_take("work", 4, Direction::Front).unwrap();
        assert!(empty.acquired);
        assert!(empty.items.is_empty());
        assert!(empty.info.unwrap().closed);
        h.close().await;
    })
    .await;
}

#[tokio::test]
async fn direct_child_can_close_and_drain_parent_queue_and_pre_cancelled_push_is_not_applied() {
    bounded(async {
        let h = Harness::new();
        let root = h.root().await;
        let child = h.spawn(&root, "child").await;
        let (r, c) = (h.q(&root), h.q(&child));
        r.create("parent-work", "shared").unwrap();
        push(&r, "parent-work", &["last"], false);
        let cancel = Signal::new();
        cancel.cancel();
        assert!(matches!(
            c.push("parent-work", &["not-applied".into()], Direction::Front, false, &cancel),
            Err(QueueError::Storage(error)) if error.kind == ErrorKind::Cancelled
        ));
        assert_eq!(r.get("parent-work").unwrap().size, 1);
        let closed = c
            .push("parent-work", &[], Direction::Back, true, &NeverCancelled)
            .unwrap();
        assert!(closed.closed);
        assert!(r.get("parent-work").unwrap().closed);
        let late = r
            .push(
                "parent-work",
                &["late".into()],
                Direction::Back,
                false,
                &NeverCancelled,
            )
            .unwrap_err();
        assert_eq!(late.to_string(), "queue: 'parent-work' is closed");
        assert!(matches!(late, QueueError::Storage(error) if error.kind == ErrorKind::Closed));
        let first = c.try_take("parent-work", 1, Direction::Front).unwrap();
        assert!(first.acquired);
        assert_eq!(first.items, ["last"]);
        assert!(first.info.unwrap().closed);
        let drained = c.try_take("parent-work", 1, Direction::Front).unwrap();
        assert!(drained.acquired);
        assert!(drained.items.is_empty());
        h.close().await;
    })
    .await;
}
