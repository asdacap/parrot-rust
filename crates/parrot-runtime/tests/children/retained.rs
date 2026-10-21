use super::{harness::Harness, support::*};
use parrot_runtime::{
    agent_turn::{
        lifecycle::{ExecutionStatus, Signal},
        services::SelectionSource,
    },
    children::*,
};
use parrot_storage::{conversation::HistoryForkSelection, events::ConversationPart};
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};

#[tokio::test]
async fn retained_resume_preserves_actual_runtime_history_resources_and_updates_model_profile() {
    bounded(async {
        let h = Harness::new();
        let root = h.root("root").await;
        let child = h.spawn(&root, "Research NOTE").await;
        assert_eq!(child.identity().session.name, "research-note");
        let built = h.factory.get("research-note");
        assert!(Arc::ptr_eq(child.session().selection(), &built.selection));
        let original_service = child.get_service::<Marker>().unwrap();
        let original_scope = child.identity().prompt_scope.format(1).unwrap();
        assert!(original_scope.contains("test scope"));
        assert!(original_scope.contains("Self"));
        let first = send(child.session().as_ref(), "first question").await;
        assert_eq!(
            bounded(first.execution.join()).await.unwrap().status,
            ExecutionStatus::Succeeded
        );
        let original_history = child.history().conversation().await.unwrap();
        assert!(original_history.iter().any(|item| item.message.role
            == parrot_storage::conversation::MessageRole::User
            && item.message.parts
                == [parrot_storage::events::ConversationPart::text(
                    "first question"
                )]));
        assert!(original_history.iter().any(|item| item.message.role
            == parrot_storage::conversation::MessageRole::Assistant
            && item.message.parts == [parrot_storage::events::ConversationPart::text("answer")]));
        let history_id = child.history().agent_session_id().to_owned();
        let mut request = h.request(&root, "RESEARCH note");
        request.model = h.provider.selection("second");
        request.requested_profile = "revised".into();
        request.fork = HistoryForkSelection::parse("full");
        request.requested_scope = "replacement scope must be ignored".into();
        request.delivery = CompletionDelivery::Automatic;
        let resumed = bounded(root.spawner().spawn_or_resume(request).unwrap().join())
            .await
            .unwrap();
        assert!(Arc::ptr_eq(&child, &resumed));
        assert!(Arc::ptr_eq(
            &original_service,
            &resumed.get_service::<Marker>().unwrap()
        ));
        assert_eq!(resumed.history().agent_session_id(), history_id);
        assert_eq!(h.factory.built.lock().unwrap().len(), 2);
        assert_eq!(resumed.selection().current().unwrap().profile.id, "revised");
        assert_eq!(
            resumed
                .selection()
                .current()
                .unwrap()
                .model
                .canonical_base(),
            "scripted/second"
        );
        let second = send(resumed.session().as_ref(), "second question").await;
        bounded(second.execution.join()).await.unwrap();
        let resumed_history = resumed.history().conversation().await.unwrap();
        assert_eq!(
            &resumed_history[..original_history.len()],
            original_history.as_slice()
        );
        assert!(resumed_history.len() > original_history.len());
        assert_eq!(h.provider.requests.lock().unwrap().len(), 2);
        assert_eq!(h.provider.requests.lock().unwrap()[1].model, "second");
        assert_eq!(
            resumed.identity().prompt_scope.format(1).unwrap(),
            original_scope
        );
        assert_eq!(
            resumed.parent_scope().delivery(),
            CompletionDelivery::RetainedOnly
        );
        let mut nested = h.request(&resumed, "nested");
        nested.requested_scope = "narrow child scope".into();
        let nested = bounded(resumed.spawner().spawn(nested).unwrap().join())
            .await
            .unwrap();
        let inherited = nested.identity().prompt_scope.format(2).unwrap();
        assert!(inherited.contains("test scope"));
        assert!(inherited.contains("narrow child scope"));
        assert!(inherited.contains("1st Ancestor (research-note)"));
        assert_eq!(built.output.closes.load(Ordering::SeqCst), 0);
        h.close().await;
        assert_eq!(built.output.closes.load(Ordering::SeqCst), 1);
        assert!(Arc::ptr_eq(
            &original_service,
            &child.get_service::<Marker>().unwrap()
        ));
    })
    .await;
}

#[tokio::test]
async fn active_resume_is_rejected_but_get_or_spawn_never_evaluates_callback() {
    bounded(async {
        let h = Harness::new();
        let root = h.root("root").await;
        let child = h.spawn(&root, "worker").await;
        let gate = Gate::new();
        h.provider.gates.lock().unwrap().push_back(gate.clone());
        let sent = send(child.session().as_ref(), "hold provider").await;
        gate.entered().await;
        assert_eq!(h.registry.active_snapshot().len(), 1);
        let result = bounded(
            root.spawner()
                .spawn_or_resume(h.request(&root, "worker"))
                .unwrap()
                .join(),
        )
        .await;
        assert!(result.err().unwrap().to_string().contains("busy"));
        let evaluated = Arc::new(AtomicUsize::new(0));
        let observed = evaluated.clone();
        let found = bounded(
            root.spawner()
                .get_or_spawn("WORKER".into(), move || {
                    observed.fetch_add(1, Ordering::SeqCst);
                    Err(ChildError::registry("must not evaluate"))
                })
                .unwrap()
                .join(),
        )
        .await
        .unwrap();
        assert!(Arc::ptr_eq(&found, &child));
        assert_eq!(evaluated.load(Ordering::SeqCst), 0);
        gate.open();
        bounded(sent.execution.join()).await.unwrap();
        h.close().await;
    })
    .await;
}

#[tokio::test]
async fn actual_next_child_turn_recaptures_dynamic_ancestor_policy_without_mutating_old_capture() {
    bounded(async {
        let h = Harness::new();
        let root = h.root("root").await;
        root.session()
            .update_selection(selection(&h.provider, "first", "root-mode", 0b1111))
            .unwrap();
        let child = h.spawn(&root, "child").await;
        let source: Arc<dyn SelectionSource<Security>> = child.selection().clone();
        assert!(Arc::ptr_eq(&source, child.session().selection()));
        let first = send(child.session().as_ref(), "before tightening").await;
        bounded(first.execution.join()).await.unwrap();
        let built = h.factory.get("child");
        let before = built.tools.captures.lock().unwrap().last().unwrap().clone();
        assert_eq!(before.security.permissions, 0b1111);
        root.session()
            .update_selection(selection(&h.provider, "second", "tightened", 0b0011))
            .unwrap();
        assert_eq!(
            child.selection().current().unwrap().security.permissions,
            0b1111
        );
        let next = send(child.session().as_ref(), "after tightening").await;
        bounded(next.execution.join()).await.unwrap();
        {
            let captures = built.tools.captures.lock().unwrap();
            assert_eq!(captures.last().unwrap().security.permissions, 0b0011);
            assert_eq!(before.security.permissions, 0b1111);
            assert_eq!(
                child
                    .selection()
                    .lineage()
                    .count_profile("root-mode")
                    .unwrap(),
                0
            );
            assert_eq!(
                child
                    .selection()
                    .lineage()
                    .count_profile("tightened")
                    .unwrap(),
                1
            );
            assert!(child.selection().lineage().count_profile("  ").is_err());
        }
        h.close().await;
    })
    .await;
}

#[tokio::test]
async fn names_use_ascii_collision_suffixes_reserved_rejection_and_source_depth_profile_limits() {
    bounded(async {
        let h = Harness::new();
        let root = h.root("root").await;
        let a = h.spawn(&root, "  ÉMY __ Bird!?  ").await;
        assert_eq!(a.identity().session.name, "my-bird");
        let b = h.spawn(&root, "MY Bird").await;
        assert_eq!(b.identity().session.name, "my-bird-2");
        let anonymous = h.spawn(&root, "鸟").await;
        assert!(anonymous.identity().session.name.starts_with("agent-"));
        assert!(
            bounded(
                root.spawner()
                    .spawn_or_resume(h.request(&root, "鸟"))
                    .unwrap()
                    .join()
            )
            .await
            .is_err()
        );
        for name in ["scratch", "blobs", "plan"] {
            assert!(
                bounded(root.spawner().spawn(h.request(&root, name)).unwrap().join())
                    .await
                    .err()
                    .unwrap()
                    .to_string()
                    .contains("reserved")
            );
        }
        let mut once = h.request(&root, "once-parent");
        once.requested_profile = "once".into();
        let once = bounded(root.spawner().spawn(once).unwrap().join())
            .await
            .unwrap();
        let mut recur = h.request(&once, "again");
        recur.requested_profile = "once".into();
        assert!(
            bounded(once.spawner().spawn(recur).unwrap().join())
                .await
                .err()
                .unwrap()
                .to_string()
                .contains("recursion")
        );
        once.session()
            .update_selection(selection(&h.provider, "first", "different", 0xff))
            .unwrap();
        let mut recur = h.request(&once, "again");
        recur.requested_profile = "once".into();
        bounded(once.spawner().spawn(recur).unwrap().join())
            .await
            .unwrap();
        let mut deepest = a;
        for name in ["level2", "level3", "level4"] {
            deepest = h.spawn(&deepest, name).await;
        }
        assert_eq!(deepest.identity().session.depth, 4);
        assert!(
            bounded(
                deepest
                    .spawner()
                    .spawn(h.request(&deepest, "too-deep"))
                    .unwrap()
                    .join()
            )
            .await
            .err()
            .unwrap()
            .to_string()
            .contains("depth")
        );
        h.close().await;
    })
    .await;
}

#[tokio::test]
async fn canceling_one_foreground_turn_does_not_cancel_retained_sibling_or_spawner_lifetime() {
    bounded(async {
        let h = Harness::new();
        let root = h.root("root").await;
        let a = h.spawn(&root, "a").await;
        let b = h.spawn(&root, "b").await;
        let gate = Gate::new();
        h.provider.gates.lock().unwrap().push_back(gate.clone());
        let foreground = Arc::new(Signal::new());
        let sent = h
            .factory
            .get("a")
            .adapter
            .session()
            .enqueue_execution(
                vec![ConversationPart::text("cancel me")],
                id(),
                None,
                None,
                foreground.clone(),
            )
            .unwrap();
        gate.entered().await;
        foreground.cancel();
        assert_eq!(
            bounded(sent.join()).await.unwrap().status,
            ExecutionStatus::Canceled
        );
        assert!(!h.registry.lifetime().is_cancelled());
        let next = send(b.session().as_ref(), "unaffected sibling").await;
        assert_eq!(
            bounded(next.execution.join()).await.unwrap().status,
            ExecutionStatus::Succeeded
        );
        h.spawn(&a, "still-accepting").await;
        let inherited = Gate::new();
        h.provider
            .gates
            .lock()
            .unwrap()
            .push_back(inherited.clone());
        let running = send(b.session().as_ref(), "parent lifetime cancellation").await;
        inherited.entered().await;
        h.lifetime.cancel();
        assert!(h.registry.lifetime().is_cancelled());
        assert_eq!(
            bounded(running.execution.join()).await.unwrap().status,
            ExecutionStatus::Canceled
        );
        h.close().await;
    })
    .await;
}
