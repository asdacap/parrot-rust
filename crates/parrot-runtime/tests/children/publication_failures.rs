use super::{harness::Harness, lifetime::available, resources::Resource, support::*};
use parrot_domain::protocol::messages::parrot::Event;
use parrot_runtime::children::*;
use parrot_storage::events::NeverCancelled;
use std::sync::Arc;

struct CompletionFailure {
    trace: Arc<Trace>,
    gate: Arc<Gate>,
    name: &'static str,
    code: &'static str,
}
impl SnapshotPublisher for CompletionFailure {
    fn capture(&self) -> ChildResult<Vec<Event>> {
        Ok(vec![])
    }
    fn start(&self) -> ChildResult<Operation<()>> {
        let (trace, gate, name, code) =
            (self.trace.clone(), self.gate.clone(), self.name, self.code);
        Ok(Operation::start(async move {
            gate.wait(&NeverCancelled).await.unwrap();
            trace.push(format!("publication-failed:{name}"));
            Err(ChildError::failure(
                code,
                format!("PRIVATE-{name}-publication-body"),
            ))
        }))
    }
}
// Only the publisher operation is injected: ownership and cleanup run on the
// production Scope containing a real retained RuntimeBackend/AgentSession.
#[tokio::test]
async fn all_failing_publications_are_joined_and_first_publication_failure_overrides_final_resource_error()
 {
    bounded(async {
        let h = Harness::new();
        let root = h.root("root").await;
        let trace = h.factory.trace.clone();
        let first = Gate::new();
        let second = Gate::new();
        h.factory.plans.lock().unwrap().insert(
            "child".into(),
            ScopePlan {
                publishers: vec![
                    Arc::new(CompletionFailure {
                        trace: trace.clone(),
                        gate: first.clone(),
                        name: "first",
                        code: "io",
                    }),
                    Arc::new(CompletionFailure {
                        trace: trace.clone(),
                        gate: second.clone(),
                        name: "second",
                        code: "timeout",
                    }),
                ],
                final_resources: vec![Resource::new(
                    "PRIVATE-final-resource-body",
                    trace.clone(),
                    true,
                )],
                ..Default::default()
            },
        );
        let child = h.spawn(&root, "child").await;
        first.entered().await;
        second.entered().await;
        let close = child.close();
        trace.wait_for("resource:PRIVATE-final-resource-body").await;
        assert_eq!(available(&h.budget), 32);
        assert!(!close.is_finished());
        first.open();
        trace.wait_for("publication-failed:first").await;
        assert!(
            !close.is_finished(),
            "later owned publication must still be joined after first failure"
        );
        second.open();
        let error = bounded(close.join()).await.err().unwrap();
        assert_eq!(error.code(), "io");
        assert_eq!(error.to_string(), "PRIVATE-first-publication-body");
        assert!(trace.values().contains(&"publication-failed:second".into()));
        {
            let diagnostics = h.diagnostics.0.lock().unwrap();
            let failures = diagnostics
                .iter()
                .filter(|event| {
                    event.event == "cleanup_failed"
                        && event.session_id == child.identity().session.session_id
                })
                .collect::<Vec<_>>();
            assert_eq!(
                failures
                    .iter()
                    .map(|event| event.error_code)
                    .collect::<Vec<_>>(),
                [Some("unexpected"), Some("io"), Some("timeout")]
            );
            assert!(
                failures
                    .iter()
                    .all(|event| !event.session_id.contains("PRIVATE")
                        && event.correlation_id.is_empty()
                        && event.outcome.is_none()
                        && event.duration_milliseconds.is_none())
            );
            let failed_close = diagnostics
                .iter()
                .find(|event| {
                    event.event == "close_failed"
                        && event.session_id == child.identity().session.session_id
                })
                .unwrap();
            assert_eq!(failed_close.error_code, Some("io"));
        }
        assert!(bounded(h.registry.shutdown().join()).await.is_err());
    })
    .await;
}
