use super::{harness::Harness, lifetime::available, resources::*, support::*};
use std::sync::{Arc, atomic::AtomicUsize};

/// Approved correction to source's final sequential cleanup: attempt all final
/// resources, release retention, and join owned publication despite prior failure.
#[tokio::test]
async fn retained_final_cleanup_failures_release_budget_and_cannot_skip_publication_join() {
    bounded(async {
        let h = Harness::new();
        let root = h.root("root").await;
        let trace = h.factory.trace.clone();
        let gate = Gate::new();
        h.factory.plans.lock().unwrap().insert(
            "child".into(),
            ScopePlan {
                publishers: vec![Arc::new(Publisher {
                    name: "child-inventory",
                    trace: trace.clone(),
                    failures: AtomicUsize::new(0),
                    gate: Some(gate.clone()),
                    captures: AtomicUsize::new(0),
                })],
                final_resources: vec![
                    Resource::new("child-queues", trace.clone(), true),
                    Resource::new("child-questions", trace.clone(), true),
                ],
                ..Default::default()
            },
        );
        let child = h.spawn(&root, "child").await;
        gate.entered().await;
        assert_eq!(available(&h.budget), 31);
        let close = child.close();
        trace.wait_for("resource:child-questions").await;
        assert_eq!(available(&h.budget), 32);
        assert!(
            !close.is_finished(),
            "publication remains owned after both final-resource errors"
        );
        gate.open();
        let error = bounded(close.join()).await.err().unwrap();
        assert_eq!(error.to_string(), "child-queues");
        assert_eq!(error.code(), "unexpected");
        {
            let diagnostics = h.diagnostics.0.lock().unwrap();
            let cleanup = diagnostics
                .iter()
                .filter(|event| {
                    event.event == "cleanup_failed"
                        && event.session_id == child.identity().session.session_id
                })
                .collect::<Vec<_>>();
            assert_eq!(cleanup.len(), 2);
            assert!(
                cleanup
                    .iter()
                    .all(|event| event.error_code == Some("unexpected")
                        && event.outcome.is_none()
                        && event.correlation_id.is_empty())
            );
        }
        assert!(bounded(h.registry.shutdown().join()).await.is_err());
    })
    .await;
}
