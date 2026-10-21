use super::{fixture::*, harness::Harness, support::*};
use parrot_domain::values::agent_tasks::ExecutionStatus::Canceled;
use std::{future::Future, sync::atomic::Ordering, task::Poll};

// P06-authorized ownership correction: a final publication failure must not
// short-circuit cancellation, old-generation joins, or notifier drainage.
#[tokio::test]
async fn final_publish_failure_remains_primary_while_actual_execution_and_failing_notifier_are_joined()
 {
    let h = Harness::new();
    let owner = h.root("owner").await;
    let built = h.factory.get("owner");
    let initial_notification = Gate::new();
    let provider = Gate::new();
    let stop_record = Gate::new();
    *built.task_session.record_gate.lock().unwrap() = Some(initial_notification.clone());
    h.provider.script("a", [Reply::held("answer", &provider)]);
    set(&owner, vec![task("a")]).await;
    provider.entered().await;
    initial_notification.entered().await;
    let tasks: Vec<_> = (0..900)
        .map(|index| state(&format!("task-{index:04}-{}", "x".repeat(70)), Canceled))
        .collect();
    set(&owner, tasks).await;
    // A real synthetic storage failure, not a notifier mock returning a value.
    let blobs = h.histories.path.join("scratch/blobs");
    std::fs::remove_dir(&blobs).unwrap();
    std::fs::write(&blobs, "PRIVATE spill body").unwrap();
    let worker = h.factory.get("a");
    *worker.task_session.record_gate.lock().unwrap() = Some(stop_record.clone());
    built
        .publisher
        .reject_progress
        .store(true, Ordering::SeqCst);
    let settlement = built.scheduler.settle();
    stop_record.entered().await;
    let mut joined = Box::pin(settlement.join());
    assert!(
        std::future::poll_fn(|cx| Poll::Ready(matches!(joined.as_mut().poll(cx), Poll::Pending)))
            .await,
        "final Publish failure skipped an actual old-generation stop-record join"
    );
    stop_record.open();
    bounded(async {
        loop {
            let events = h.histories.repository.events().replay().await.unwrap();
            if events.iter().any(|event| {
                event.agent_session_id == worker.history.agent_session_id()
                    && event.has_input_admitted()
                    && event.input_admitted().content.contains(
                        "Your AgentTask turn was stopped: the agent session is shutting down.",
                    )
            }) {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await;
    assert!(
        std::future::poll_fn(|cx| Poll::Ready(matches!(joined.as_mut().poll(cx), Poll::Pending)))
            .await,
        "settlement skipped the still-gated initial notifier record"
    );
    initial_notification.open();
    let primary = bounded(joined).await.unwrap_err();
    assert!(
        primary
            .to_string()
            .contains("event retention capacities must be positive")
    );
    assert_eq!(primary.code(), "unexpected");
    {
        let diagnostics = h.factory.task_diagnostics.0.lock().unwrap();
        let secondary: Vec<_> = diagnostics
            .iter()
            .filter(|entry| entry.operation == "settlement_notifier")
            .collect();
        assert_eq!(secondary.len(), 1);
        let secondary = secondary[0];
        assert_eq!(secondary.session_id, owner.identity().session.session_id);
        assert_eq!(secondary.outcome, "failed");
        assert_eq!(secondary.error_code, Some("io"));
        assert!(secondary.correlation_id.is_empty());
        assert!(secondary.duration_milliseconds.is_none());
        for entry in diagnostics.iter() {
            assert!(!entry.session_id.contains("PRIVATE"));
            assert!(!entry.correlation_id.contains("PRIVATE"));
            assert!(!entry.error_code.unwrap_or("").contains("PRIVATE"));
        }
    }
    assert!(
        snapshot(&owner)
            .await
            .iter()
            .all(|task| task.state == Canceled)
    );
    assert!(
        requests(&h.provider, "owner").is_empty(),
        "settling must drain by Record, not wake"
    );
    let shutdown = bounded(h.registry.shutdown().join()).await.unwrap_err();
    assert_eq!(shutdown.to_string(), primary.to_string());
    assert_eq!(worker.output.closes.load(Ordering::SeqCst), 1);
    assert_eq!(built.output.closes.load(Ordering::SeqCst), 1);
}
