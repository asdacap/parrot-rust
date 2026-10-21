use super::{harness::Harness, support::*};
use parrot_runtime::{agent_turn::lifecycle::ExecutionStatus, children::*};
use parrot_storage::events::{ConversationPart, NeverCancelled};
use std::sync::{Arc, atomic::Ordering};

#[tokio::test]
async fn parent_scope_shutdown_cancels_and_joins_actual_automatic_child_without_parent_followup() {
    bounded(async {
        let h = Harness::new();
        let root = h.root("root").await;
        let mut request = h.request(&root, "automatic");
        request.delivery = CompletionDelivery::Automatic;
        let child = bounded(root.spawner().spawn(request).unwrap().join())
            .await
            .unwrap();
        let gate = Gate::new();
        h.provider.gates.lock().unwrap().push_back(gate.clone());
        let built = h.factory.get("automatic");
        let execution = built
            .adapter
            .session()
            .enqueue_execution(
                vec![ConversationPart::text("held child")],
                id(),
                None,
                None,
                Arc::new(NeverCancelled),
            )
            .unwrap();
        gate.entered().await;
        bounded(root.close().join()).await.unwrap();
        assert!(!root.children().is_accepting());
        let terminal = bounded(execution.join()).await.unwrap();
        assert_eq!(terminal.status, ExecutionStatus::Canceled);
        assert_eq!(terminal.error, "interrupted");
        let retained = bounded(built.adapter.session().wait(0, &NeverCancelled))
            .await
            .unwrap()
            .execution
            .unwrap();
        assert_eq!(retained, terminal);
        assert_eq!(h.provider.requests.lock().unwrap().len(), 1);
        assert!(root.history().conversation().await.unwrap().is_empty());
        assert_eq!(built.output.closes.load(Ordering::SeqCst), 1);
        let events = h.histories.repository.events().replay().await.unwrap();
        let failed = events
            .iter()
            .filter(|event| {
                event.agent_session_id == child.identity().session.session_id
                    && event.has_agent_failed()
            })
            .collect::<Vec<_>>();
        assert_eq!(failed.len(), 1);
        assert_eq!(failed[0].agent_failed().message, "interrupted");
        assert_eq!(
            failed[0].agent_failed().parent_agent_session_id,
            root.identity().session.session_id
        );
        assert_eq!(failed[0].agent_failed().name, "automatic");
        h.close().await;
    })
    .await;
}

#[tokio::test]
async fn registry_shutdown_of_reused_child_retains_cancelled_followup_and_exact_durable_terminal_events()
 {
    bounded(async {
        let h = Harness::new();
        let root = h.root("root").await;
        let child = h.spawn(&root, "worker").await;
        let built = h.factory.get("worker");
        let first = built
            .adapter
            .session()
            .enqueue_execution(
                vec![ConversationPart::text("first")],
                id(),
                None,
                None,
                Arc::new(NeverCancelled),
            )
            .unwrap();
        assert_eq!(
            bounded(first.join()).await.unwrap().status,
            ExecutionStatus::Succeeded
        );
        let resumed = bounded(
            root.spawner()
                .spawn_or_resume(h.request(&root, "worker"))
                .unwrap()
                .join(),
        )
        .await
        .unwrap();
        assert!(Arc::ptr_eq(&resumed, &child));
        let gate = Gate::new();
        h.provider.gates.lock().unwrap().push_back(gate.clone());
        let followup = built
            .adapter
            .session()
            .enqueue_execution(
                vec![ConversationPart::text("wait forever")],
                id(),
                None,
                None,
                Arc::new(NeverCancelled),
            )
            .unwrap();
        gate.entered().await;
        bounded(h.registry.shutdown().join()).await.unwrap();
        let terminal = bounded(followup.join()).await.unwrap();
        assert_eq!(terminal.status, ExecutionStatus::Canceled);
        assert_eq!(terminal.error, "interrupted");
        for _ in 0..2 {
            assert_eq!(
                bounded(built.adapter.session().wait(0, &NeverCancelled))
                    .await
                    .unwrap()
                    .execution
                    .unwrap(),
                terminal
            );
        }
        let events = h.histories.repository.events().replay().await.unwrap();
        let child_events = events
            .iter()
            .filter(|event| event.agent_session_id == child.identity().session.session_id)
            .collect::<Vec<_>>();
        assert_eq!(
            child_events
                .iter()
                .filter(|event| event.has_agent_started())
                .count(),
            2
        );
        assert_eq!(
            child_events
                .iter()
                .filter(|event| event.has_agent_finished())
                .count(),
            1
        );
        let failed = child_events
            .into_iter()
            .filter(|event| event.has_agent_failed())
            .collect::<Vec<_>>();
        assert_eq!(failed.len(), 1);
        assert_eq!(failed[0].agent_failed().message, "interrupted");
        assert_eq!(
            failed[0].agent_failed().parent_agent_session_id,
            root.identity().session.session_id
        );
        assert_eq!(failed[0].agent_failed().name, "worker");
        assert_eq!(
            AgentResolver::new(&root, &h.registry)
                .resolve_recipient("worker")
                .err()
                .unwrap()
                .to_string(),
            "the user session is shutting down"
        );
        if let Ok(operation) = root.spawner().spawn(h.request(&root, "new")) {
            assert_eq!(
                bounded(operation.join()).await.err().unwrap().to_string(),
                "the user session is shutting down"
            );
        }
    })
    .await;
}
