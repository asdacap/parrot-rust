use super::{harness::Harness, support::*};
use parrot_provider::sessions::Error;
use parrot_runtime::{agent_turn::lifecycle::ExecutionStatus, children::*};
use parrot_storage::events::{ConversationPart, NeverCancelled};
use std::sync::Arc;

#[tokio::test]
async fn actual_automatic_completion_bounds_oversized_multibyte_terminal_error_to_one_mib() {
    bounded(async {
        let h = Harness::new();
        let root = h.root("root").await;
        let mut request = h.request(&root, "child");
        request.delivery = CompletionDelivery::Automatic;
        let child = bounded(root.spawner().spawn(request).unwrap().join())
            .await
            .unwrap();
        let multibyte_error = "界".repeat(400_000);
        assert!(multibyte_error.len() > 1024 * 1024);
        h.provider
            .failures
            .lock()
            .unwrap()
            .push_back(Error::Provider(multibyte_error));
        let publication = Gate::new();
        *h.factory.get("root").publisher.gate.lock().unwrap() = Some(publication.clone());
        let execution = h
            .factory
            .get("child")
            .adapter
            .session()
            .enqueue_execution(
                vec![ConversationPart::text("fail with multibyte error")],
                id(),
                None,
                None,
                Arc::new(NeverCancelled),
            )
            .unwrap();
        publication.entered().await;
        let events = h.histories.repository.events();
        let session = root.identity().session.session_id.clone();
        assert_eq!(
            events
                .capture_pending_input_ids(session.clone())
                .await
                .unwrap()
                .len(),
            1
        );
        let pending = events.peek_pending_input(session).await.unwrap().unwrap();
        assert_eq!(pending.parts.len(), 1);
        let ConversationPart::Text(notification) = &pending.parts[0] else {
            panic!("completion must be one text part")
        };
        assert!(notification.contains("Status: failed"));
        assert!(notification.contains("Runtime: 1.23 seconds."));
        assert!(notification.contains("Error:"));
        assert!(notification.contains('界'));
        assert!(notification.contains(&child.identity().session.session_id));
        assert!(notification.len() <= 1024 * 1024);
        assert_eq!(
            std::str::from_utf8(notification.as_bytes()).unwrap(),
            notification
        );
        assert!(!execution.is_finished());
        let shutdown = h.registry.shutdown();
        publication.open();
        let terminal = bounded(execution.join()).await.unwrap();
        assert_eq!(terminal.status, ExecutionStatus::Failed);
        assert!(terminal.error.len() <= 1024 * 1024);
        assert!(terminal.error.contains('界'));
        bounded(shutdown.join()).await.unwrap();
        assert_eq!(h.provider.requests.lock().unwrap().len(), 1);
    })
    .await;
}
