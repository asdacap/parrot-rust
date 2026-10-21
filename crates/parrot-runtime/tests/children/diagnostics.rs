use super::{harness::Harness, support::*};
use parrot_runtime::children::*;
use std::sync::atomic::Ordering;

#[tokio::test]
async fn actual_scope_construction_diagnostics_correlate_success_failure_cancellation_and_validation_without_raw_body()
 {
    bounded(async {
        let h = Harness::new();
        let root = h.root("root").await;
        let successful = h.spawn(&root, "successful").await;
        for (name, plan) in [
            (
                "failed",
                ScopePlan {
                    failure: Some(ChildError::failure(
                        "PRIVATE-secret-code",
                        "PRIVATE response body and prompt",
                    )),
                    ..Default::default()
                },
            ),
            (
                "cancelled",
                ScopePlan {
                    failure: Some(ChildError::Cancelled),
                    ..Default::default()
                },
            ),
            (
                "invalid",
                ScopePlan {
                    duplicate_service: true,
                    ..Default::default()
                },
            ),
        ] {
            h.factory.plans.lock().unwrap().insert(name.into(), plan);
            assert!(
                bounded(root.spawner().spawn(h.request(&root, name)).unwrap().join())
                    .await
                    .is_err()
            );
        }
        {
            let diagnostics = h.diagnostics.0.lock().unwrap();
            let starts = diagnostics
                .iter()
                .filter(|event| event.event == "child_scope_start")
                .collect::<Vec<_>>();
            let completed = diagnostics
                .iter()
                .filter(|event| event.event == "child_scope_completed")
                .collect::<Vec<_>>();
            assert_eq!(starts.len(), 4);
            assert_eq!(completed.len(), 4);
            for (start, finish) in starts.iter().zip(completed.iter()) {
                assert_eq!(start.session_id, finish.session_id);
                assert_eq!(start.correlation_id, root.identity().session.session_id);
                assert_eq!(finish.correlation_id, root.identity().session.session_id);
                assert!(start.duration_milliseconds.is_none());
                assert!(start.outcome.is_none());
                assert!(start.error_code.is_none());
                assert!(finish.duration_milliseconds.is_some());
                assert!(!finish.session_id.contains("PRIVATE"));
                assert!(!finish.correlation_id.contains("PRIVATE"));
                assert!(!finish.outcome.unwrap().contains("PRIVATE"));
                assert!(!finish.error_code.unwrap_or("").contains("PRIVATE"));
            }
            assert_eq!(
                completed[0].session_id,
                successful.identity().session.session_id
            );
            assert_eq!(
                completed
                    .iter()
                    .map(|event| event.outcome.unwrap())
                    .collect::<Vec<_>>(),
                ["succeeded", "failed", "cancelled", "failed"]
            );
            assert_eq!(
                completed
                    .iter()
                    .map(|event| event.error_code)
                    .collect::<Vec<_>>(),
                [
                    None,
                    Some("unexpected"),
                    Some("cancelled"),
                    Some("invalid_operation")
                ]
            );
            let invalid_id = h
                .factory
                .get("invalid")
                .history
                .agent_session_id()
                .to_owned();
            assert!(
                diagnostics
                    .iter()
                    .all(|event| event.session_id != invalid_id
                        || !matches!(event.event, "created" | "closed" | "close_failed")),
                "rejected construction is not a completed lifecycle scope"
            );
            assert_eq!(
                h.factory
                    .get("invalid")
                    .output
                    .closes
                    .load(Ordering::SeqCst),
                1,
                "Scope.create validation failure must close the actual constructed runtime"
            );
        }
        h.close().await;
    })
    .await;
}
