use super::{harness::Harness, resources::*, support::*};
use parrot_domain::values::accounting::RetainedAgentBudget;
use parrot_runtime::children::*;
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};

fn publisher(
    trace: Arc<Trace>,
    name: &'static str,
    failures: usize,
    gate: Option<Arc<Gate>>,
) -> Arc<Publisher> {
    Arc::new(Publisher {
        name,
        trace,
        failures: AtomicUsize::new(failures),
        gate,
        captures: AtomicUsize::new(0),
    })
}
pub(super) fn available(budget: &RetainedAgentBudget) -> usize {
    let mut reservations = Vec::new();
    while let Ok(reservation) = budget.reserve() {
        reservations.push(reservation);
    }
    let count = reservations.len();
    for reservation in reservations {
        reservation.rollback();
    }
    count
}

#[tokio::test]
async fn snapshot_registration_rolls_back_and_retry_restarts_publishers_on_same_scope() {
    bounded(async {
        let h = Harness::new();
        let trace = h.factory.trace.clone();
        let first = publisher(trace.clone(), "first", 0, None);
        let fail = publisher(trace.clone(), "second", 1, None);
        h.factory.plans.lock().unwrap().insert(
            "root".into(),
            ScopePlan {
                publishers: vec![first.clone(), fail.clone()],
                ..Default::default()
            },
        );
        let root = h.unregistered_root("root").await;
        assert!(h.registry.register_root(root.clone()).is_err());
        assert!(h.registry.snapshot_roots().is_empty());
        h.registry.register_root(root.clone()).unwrap();
        root.publish_snapshots().unwrap();
        let events = root.capture_snapshot_events().unwrap();
        assert_eq!(
            events
                .iter()
                .map(|event| event.id.as_str())
                .collect::<Vec<_>>(),
            ["first", "second"]
        );
        assert_eq!(first.captures.load(Ordering::SeqCst), 1);
        assert_eq!(
            trace
                .values()
                .iter()
                .filter(|value| *value == "publish-start:first")
                .count(),
            2
        );
        assert_eq!(
            trace
                .values()
                .iter()
                .filter(|value| *value == "publish-start:second")
                .count(),
            2
        );
        h.close().await;
    })
    .await;
}

#[tokio::test]
async fn scope_close_orders_work_session_reverse_children_resources_and_joins_publications_after_final_failures()
 {
    bounded(async {
        let h = Harness::new();
        let trace = h.factory.trace.clone();
        let publish_gate = Gate::new();
        let questions_gate = Gate::new();
        h.factory.plans.lock().unwrap().insert(
            "root".into(),
            ScopePlan {
                publishers: vec![publisher(
                    trace.clone(),
                    "inventory",
                    0,
                    Some(publish_gate.clone()),
                )],
                work: vec![
                    Work::new("first", trace.clone(), true),
                    Work::new("second", trace.clone(), true),
                ],
                resources: vec![
                    Resource::new("composition-first", trace.clone(), true),
                    Resource::new("composition-second", trace.clone(), true),
                ],
                final_resources: vec![
                    Resource::new("queues", trace.clone(), true),
                    Arc::new(Resource {
                        name: "questions".into(),
                        trace: trace.clone(),
                        fail: true,
                        gate: Some(questions_gate.clone()),
                    }),
                ],
                ..Default::default()
            },
        );
        let root = h.root("root").await;
        let a = h.spawn(&root, "a").await;
        h.spawn(&root, "b").await;
        assert_eq!(available(&h.budget), 30);
        publish_gate.entered().await;
        let close = root.close();
        let second = root.close();
        drop(close);
        questions_gate.entered().await;
        assert!(
            trace.values().contains(&"resource:queues".into()),
            "failed final stage must not skip the next stage"
        );
        assert_eq!(available(&h.budget), 32);
        assert!(!second.is_finished());
        questions_gate.open();
        trace.wait_for("resource:questions").await;
        assert!(
            !second.is_finished(),
            "failed final resources must not skip retained publication join"
        );
        publish_gate.open();
        let error = bounded(second.join()).await.err().unwrap();
        assert_eq!(error.code(), "unexpected");
        assert_eq!(error.to_string(), "queues");
        let values = trace.values();
        let position = |value: &str| values.iter().position(|actual| actual == value).unwrap();
        assert!(position("work:first") < position("session:root"));
        assert!(position("work:second") < position("session:root"));
        assert!(position("session:root") < position("session:b"));
        assert!(position("session:b") < position("session:a"));
        assert!(position("session:a") < position("resource:composition-first"));
        assert!(position("resource:composition-first") < position("resource:composition-second"));
        assert!(position("resource:composition-second") < position("resource:queues"));
        assert!(position("resource:queues") < position("resource:questions"));
        assert_eq!(available(&h.budget), 32);
        assert_eq!(h.factory.get("a").output.closes.load(Ordering::SeqCst), 1);
        assert_eq!(h.factory.get("b").output.closes.load(Ordering::SeqCst), 1);
        assert_eq!(a.get_service::<Marker>().unwrap().0, "a");
        assert!(bounded(h.registry.shutdown().join()).await.is_err());
    })
    .await;
}

#[tokio::test]
async fn root_shutdown_is_forward_children_reverse_all_roots_attempted_and_concurrent_observers_join()
 {
    bounded(async {
        let h = Harness::new();
        let trace = h.factory.trace.clone();
        h.factory.plans.lock().unwrap().insert(
            "first".into(),
            ScopePlan {
                resources: vec![Resource::new("first-error", trace.clone(), true)],
                ..Default::default()
            },
        );
        h.factory.plans.lock().unwrap().insert(
            "second".into(),
            ScopePlan {
                resources: vec![Resource::new("second-error", trace.clone(), true)],
                ..Default::default()
            },
        );
        let first = h.root("first").await;
        h.spawn(&first, "a").await;
        h.spawn(&first, "b").await;
        h.root("second").await;
        let gate = Gate::new();
        *h.factory.get("first").output.gate.lock().unwrap() = Some(gate.clone());
        let initial = h.registry.shutdown();
        gate.entered().await;
        let follower = h.registry.shutdown();
        drop(initial);
        assert!(!follower.is_finished());
        assert!(h.registry.snapshot_roots().is_empty());
        assert!(
            AgentResolver::new(&first, &h.registry)
                .resolve_recipient("a")
                .is_err()
        );
        assert!(
            first
                .spawner()
                .spawn(h.request(&first, "rejected"))
                .is_err()
        );
        gate.open();
        let error = bounded(follower.join()).await.err().unwrap();
        assert_eq!(error.to_string(), "first-error");
        let values = trace.values();
        let sessions = values
            .iter()
            .filter(|value| value.starts_with("session:"))
            .map(String::as_str)
            .collect::<Vec<_>>();
        assert_eq!(
            sessions,
            ["session:first", "session:b", "session:a", "session:second"]
        );
        assert!(values.contains(&"resource:second-error".into()));
    })
    .await;
}

#[tokio::test]
async fn rejected_constructed_scope_error_is_eager_but_retention_and_shutdown_wait_for_disposal() {
    bounded(async {
        let h = Harness::new();
        let root = h.root("root").await;
        let gate = Gate::new();
        let trace = h.factory.trace.clone();
        h.factory.plans.lock().unwrap().insert(
            "rejected".into(),
            ScopePlan {
                publishers: vec![publisher(trace.clone(), "reject-inventory", 1, None)],
                resources: vec![Arc::new(Resource {
                    name: "rejected-cleanup".into(),
                    trace,
                    fail: false,
                    gate: Some(gate.clone()),
                })],
                ..Default::default()
            },
        );
        let error = bounded(
            root.spawner()
                .spawn(h.request(&root, "rejected"))
                .unwrap()
                .join(),
        )
        .await
        .err()
        .unwrap();
        assert_eq!(error.code(), "unexpected");
        gate.entered().await;
        assert!(root.children().find("rejected").is_none());
        assert_eq!(available(&h.budget), 31);
        let cleanup = root.spawner().close();
        let shutdown = h.registry.shutdown();
        assert!(!cleanup.is_finished());
        assert!(!shutdown.is_finished());
        gate.open();
        bounded(cleanup.join()).await.unwrap();
        bounded(shutdown.join()).await.unwrap();
        assert_eq!(available(&h.budget), 32);
        assert!(
            h.factory
                .get("rejected")
                .history
                .conversation()
                .await
                .unwrap()
                .is_empty()
        );
        assert_eq!(
            h.factory
                .get("rejected")
                .output
                .closes
                .load(Ordering::SeqCst),
            1
        );
    })
    .await;
}

#[tokio::test]
async fn construction_racing_shutdown_is_rejected_cleaned_and_history_and_budget_are_released() {
    bounded(async {
        let h = Harness::new();
        let root = h.root("root").await;
        h.histories
            .append(root.history(), "fork must be removed")
            .await;
        let gate = Gate::new();
        *h.factory.construction.lock().unwrap() = Some(gate.clone());
        let mut request = h.request(&root, "racing");
        request.fork = parrot_storage::conversation::HistoryForkSelection::parse("full");
        let spawn = root.spawner().spawn(request).unwrap();
        gate.entered().await;
        assert_eq!(available(&h.budget), 31);
        let shutdown = h.registry.shutdown();
        assert!(!shutdown.is_finished());
        gate.open();
        assert!(bounded(spawn.join()).await.is_err());
        bounded(shutdown.join()).await.unwrap();
        assert_eq!(available(&h.budget), 32);
        let built = h.factory.get("racing");
        assert!(built.history.conversation().await.unwrap().is_empty());
        assert_eq!(built.output.closes.load(Ordering::SeqCst), 1);
        assert!(
            std::fs::read_to_string(
                h.histories
                    .path
                    .join(format!("{}.jsonl", built.history.agent_session_id()))
            )
            .unwrap()
            .trim()
            .is_empty()
        );
    })
    .await;
}
