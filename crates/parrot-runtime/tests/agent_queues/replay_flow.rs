use super::{harness::*, support::*};
use parrot_domain::{
    contracts::events::EventSubscription, protocol::messages::queue::QueueSnapshot,
};
use parrot_runtime::children::ChildError;
use parrot_storage::events::NeverCancelled;
use protobuf::Message;
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};

#[tokio::test]
async fn actual_stored_replay_then_registry_current_queue_capture_precedes_live_without_durable_queue_appends()
 {
    bounded(async {
        let h = Harness::new();
        let root = h.root().await;
        let child = h.spawn(&root, "child").await;
        let turn = send(root.session().as_ref(), "genuine persisted replay fixture").await;
        bounded(turn.execution.join()).await.unwrap();
        let repository = h.histories.repository.events();
        let persisted = repository.replay().await.unwrap();
        let stored_count = repository.replay_raw().await.unwrap().len();
        assert!(
            persisted.len() >= 2,
            "actual completed turn must supply durable replay events"
        );
        assert!(persisted.iter().all(|event| !event.has_tool_snapshot()));
        let cursor = persisted[0].id.clone();
        let broker = h.factory.sessions.get("root").publisher.broker.clone();
        let drain = broker.subscribe();
        let (r, c) = (h.q(&root), h.q(&child));
        r.create("root-work", "root current").unwrap();
        c.create("child-work", "child current").unwrap();
        push(&r, "root-work", &["root-before-capture"], false);
        push(&c, "child-work", &["child-before-capture"], false);
        let root_revision = r.capture_inventory().revision;
        // Consume the already-running actual publisher's current snapshot before listen
        // so the later live item is specifically the update made during initial capture.
        loop {
            let event = bounded(drain.next(&NeverCancelled)).await.unwrap().unwrap();
            if event.has_tool_snapshot() && event.tool_snapshot().revision == root_revision {
                break;
            }
        }
        assert_eq!(repository.replay().await.unwrap(), persisted);
        assert_eq!(repository.replay_raw().await.unwrap().len(), stored_count);
        let captured = Arc::new(AtomicUsize::new(0));
        let listener = broker
            .listen(
                Some(&cursor),
                || async { repository.replay().await.map_err(ChildError::from) },
                || async {
                    captured.fetch_add(1, Ordering::SeqCst);
                    let mut initial = Vec::new();
                    for scope in h.registry.snapshot_scopes() {
                        initial.extend(scope.capture_snapshot_events()?);
                    }
                    push(&r, "root-work", &["live-after-capture"], false);
                    Ok::<_, ChildError>(initial)
                },
            )
            .await
            .unwrap();
        assert_eq!(captured.load(Ordering::SeqCst), 0);
        for expected in &persisted[1..] {
            assert_eq!(
                bounded(listener.next(&NeverCancelled))
                    .await
                    .unwrap()
                    .as_ref(),
                Some(expected)
            );
            assert_eq!(
                captured.load(Ordering::SeqCst),
                0,
                "current queue capture must remain lazy until stored replay is exhausted"
            );
        }
        for (scope, name) in [(&root, "root-work"), (&child, "child-work")] {
            let event = bounded(listener.next(&NeverCancelled))
                .await
                .unwrap()
                .unwrap();
            let envelope = event.tool_snapshot();
            assert_eq!(envelope.kind, "queue");
            assert_eq!(
                envelope.owner_agent_session_id,
                scope.identity().session.session_id
            );
            assert!(envelope.final_chunk);
            let snapshot = QueueSnapshot::parse_from_bytes(&envelope.data).unwrap();
            assert_eq!(
                snapshot.root_agent_session_id,
                root.identity().session.session_id
            );
            assert_eq!(snapshot.queues.len(), 1);
            assert_eq!(snapshot.queues[0].name, name);
            assert_eq!(
                snapshot.queues[0].item_count, 1,
                "initial capture must precede the live update"
            );
        }
        assert_eq!(captured.load(Ordering::SeqCst), 1);
        let live = bounded(listener.next(&NeverCancelled))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            live.tool_snapshot().owner_agent_session_id,
            root.identity().session.session_id
        );
        assert_eq!(live.tool_snapshot().revision, root_revision + 1);
        let snapshot = QueueSnapshot::parse_from_bytes(&live.tool_snapshot().data).unwrap();
        assert_eq!(snapshot.queues[0].name, "root-work");
        assert_eq!(snapshot.queues[0].item_count, 2);
        assert_eq!(
            repository.replay().await.unwrap(),
            persisted,
            "queue inventory capture/publication must not append durable history"
        );
        assert_eq!(repository.replay_raw().await.unwrap().len(), stored_count);
        listener.complete().unwrap();
        h.close().await;
    })
    .await;
}
