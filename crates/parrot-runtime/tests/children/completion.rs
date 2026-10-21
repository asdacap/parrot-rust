use super::{harness::Harness, support::*};
use parrot_runtime::{agent_turn::lifecycle::ExecutionStatus, children::*};
use parrot_storage::events::{ConversationPart, NeverCancelled};
use std::sync::{Arc, atomic::Ordering};

#[tokio::test]
async fn actual_foreground_child_completion_awaits_parent_admission_publication_and_retains_child()
{
    bounded(async {
        let h = Harness::new();
        let root = h.root("root").await;
        let mut request = h.request(&root, "automatic");
        request.delivery = CompletionDelivery::Automatic;
        let child = bounded(root.spawner().spawn(request).unwrap().join())
            .await
            .unwrap();
        let gate = Gate::new();
        *h.factory.get("root").publisher.gate.lock().unwrap() = Some(gate.clone());
        let done = h
            .factory
            .get("automatic")
            .adapter
            .session()
            .enqueue_execution(
                vec![ConversationPart::text("child question")],
                id(),
                None,
                None,
                Arc::new(NeverCancelled),
            )
            .unwrap();
        gate.entered().await;
        assert!(
            !done.is_finished(),
            "public child execution completed before awaited parent publication"
        );
        assert!(
            h.histories
                .repository
                .events()
                .replay()
                .await
                .unwrap()
                .iter()
                .any(
                    |event| event.agent_session_id == root.identity().session.session_id
                        && event.has_input_admitted()
                )
        );
        gate.open();
        assert_eq!(
            bounded(done.join()).await.unwrap().status,
            ExecutionStatus::Succeeded
        );
        assert!(Arc::ptr_eq(
            &root.children().resolve("automatic").unwrap(),
            &child
        ));
        let root_drain = h.factory.get("root").adapter.session().recover();
        bounded(root_drain.join()).await.unwrap();
        let root_history = root.history().conversation().await.unwrap();
        let message = root_history
            .iter()
            .flat_map(|item| item.message.parts.iter())
            .find_map(|part| match part {
                ConversationPart::Text(text) if text.contains("Agent task notification") => {
                    Some(text)
                }
                _ => None,
            })
            .unwrap();
        assert!(message.contains(&child.identity().session.session_id));
        assert!(message.contains("succeeded"));
        assert!(message.contains("answer"));
        h.close().await;
    })
    .await;
}

#[tokio::test]
async fn retained_only_skips_automatic_send_and_delivery_failure_does_not_change_child_success() {
    bounded(async {
        let h = Harness::new();
        let root = h.root("root").await;
        let retained = h.spawn(&root, "retained").await;
        let execution = h
            .factory
            .get("retained")
            .adapter
            .session()
            .enqueue_execution(
                vec![ConversationPart::text("run")],
                id(),
                None,
                None,
                Arc::new(NeverCancelled),
            )
            .unwrap();
        assert_eq!(
            bounded(execution.join()).await.unwrap().status,
            ExecutionStatus::Succeeded
        );
        assert!(root.history().conversation().await.unwrap().is_empty());
        assert!(Arc::ptr_eq(
            &retained,
            &root.children().resolve("retained").unwrap()
        ));
        let mut request = h.request(&root, "automatic");
        request.delivery = CompletionDelivery::Automatic;
        bounded(root.spawner().spawn(request).unwrap().join())
            .await
            .unwrap();
        h.factory
            .get("root")
            .publisher
            .reject
            .store(true, Ordering::SeqCst);
        let execution = h
            .factory
            .get("automatic")
            .adapter
            .session()
            .enqueue_execution(
                vec![ConversationPart::text("run")],
                id(),
                None,
                None,
                Arc::new(NeverCancelled),
            )
            .unwrap();
        assert_eq!(
            bounded(execution.join()).await.unwrap().status,
            ExecutionStatus::Succeeded
        );
        assert_eq!(
            h.factory
                .get("automatic")
                .output
                .closes
                .load(Ordering::SeqCst),
            0
        );
        h.close().await;
    })
    .await;
}

#[tokio::test]
async fn closing_only_root_session_keeps_registered_intermediate_spawn_and_swallows_parent_delivery_failure()
 {
    bounded(async {
        let h = Harness::new();
        let root = h.root("root").await;
        let mut request = h.request(&root, "middle");
        request.delivery = CompletionDelivery::Automatic;
        let middle = bounded(root.spawner().spawn(request).unwrap().join())
            .await
            .unwrap();
        bounded(root.session().close()).await.unwrap();
        assert!(h.registry.contains(&root));
        let grandchild = h.spawn(&middle, "grandchild").await;
        assert_eq!(grandchild.identity().session.depth, 2);
        let execution = h
            .factory
            .get("middle")
            .adapter
            .session()
            .enqueue_execution(
                vec![ConversationPart::text("completion to closed root")],
                id(),
                None,
                None,
                Arc::new(NeverCancelled),
            )
            .unwrap();
        assert_eq!(
            bounded(execution.join()).await.unwrap().status,
            ExecutionStatus::Succeeded
        );
        assert_eq!(
            h.factory.get("root").output.closes.load(Ordering::SeqCst),
            1
        );
        assert!(Arc::ptr_eq(
            &middle,
            &root.children().resolve("middle").unwrap()
        ));
        h.close().await;
    })
    .await;
}
