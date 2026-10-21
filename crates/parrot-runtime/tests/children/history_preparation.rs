use super::{harness::Harness, support::*};
use parrot_domain::contracts::async_ops::BoxFuture;
use parrot_runtime::{agent_turn::lifecycle::Signal, children::*};
use parrot_storage::{conversation::AgentHistoryView, events::NeverCancelled};
use std::sync::{Arc, Mutex, atomic::Ordering};

struct GatedPreparer {
    histories: Arc<Histories>,
    gate: Arc<Gate>,
    attempts: Mutex<Vec<String>>,
    failure: Mutex<Option<ChildError>>,
}
impl GatedPreparer {
    fn harness(failure: Option<ChildError>) -> (Harness, Arc<Self>) {
        let histories = Histories::new();
        let preparer = Arc::new(Self {
            histories: histories.clone(),
            gate: Gate::new(),
            attempts: Mutex::new(vec![]),
            failure: Mutex::new(failure),
        });
        (
            Harness::with_history_preparer(1, histories, preparer.clone()),
            preparer,
        )
    }
}
impl HistoryPreparer for GatedPreparer {
    fn prepare<'a>(
        &'a self,
        identity: &'a AgentIdentity,
    ) -> BoxFuture<'a, ChildResult<AgentHistoryView>> {
        Box::pin(async move {
            self.attempts
                .lock()
                .unwrap()
                .push(identity.session.session_id.clone());
            self.gate.wait(&NeverCancelled).await.unwrap();
            if let Some(error) = self.failure.lock().unwrap().take() {
                return Err(error);
            }
            Ok(self.histories.history(identity))
        })
    }
}

#[tokio::test]
async fn gated_history_preparation_precedes_fork_factory_and_child_publication() {
    bounded(async {
        let (h, preparer) = GatedPreparer::harness(None);
        let root = h.root("root").await;
        h.histories.append(root.history(), "parent history").await;
        let mut request = h.request(&root, "pending");
        request.fork = parrot_storage::conversation::HistoryForkSelection::parse("full");
        let pending = root.spawner().spawn(request).unwrap();
        preparer.gate.entered().await;
        assert!(!pending.is_finished());
        assert!(root.children().snapshot_descendants().is_empty());
        assert_eq!(h.registry.snapshot_scopes().len(), 1);
        assert_eq!(h.factory.build_calls.load(Ordering::SeqCst), 1);
        let session_id = preparer.attempts.lock().unwrap()[0].clone();
        assert!(
            !h.histories
                .path
                .join(format!("{session_id}.jsonl"))
                .exists()
        );
        assert!(h.budget.reserve().is_err());
        preparer.gate.open();
        let child = pending.join().await.unwrap();
        assert_eq!(child.identity().session.session_id, session_id);
        assert_eq!(child.history().conversation().await.unwrap().len(), 1);
        assert_eq!(h.factory.build_calls.load(Ordering::SeqCst), 2);
        assert!(Arc::ptr_eq(
            &root.children().resolve("pending").unwrap(),
            &child
        ));
        h.close().await;
        h.budget.reserve().unwrap().rollback();
    })
    .await;
}

#[tokio::test]
async fn gated_history_preparation_cancelled_dropped_observer_keeps_shutdown_cleanup_owned() {
    bounded(async {
        let (h, preparer) = GatedPreparer::harness(None);
        let root = h.root("root").await;
        h.histories
            .append(root.history(), "remove rejected fork")
            .await;
        let mut request = h.request(&root, "rejected");
        request.fork = parrot_storage::conversation::HistoryForkSelection::parse("full");
        let pending = root.spawner().spawn(request).unwrap();
        preparer.gate.entered().await;
        let cancelled = Signal::new();
        cancelled.cancel();
        assert!(matches!(
            pending.wait(&cancelled).await,
            Err(ChildError::Cancelled)
        ));
        assert!(!pending.is_finished());
        drop(pending);
        let close = root.spawner().close();
        let shutdown = h.registry.shutdown();
        assert!(!close.is_finished());
        assert!(!shutdown.is_finished());
        assert!(root.children().find("rejected").is_none());
        assert_eq!(h.factory.build_calls.load(Ordering::SeqCst), 1);
        assert!(h.budget.reserve().is_err());
        preparer.gate.open();
        close.join().await.unwrap();
        shutdown.join().await.unwrap();
        assert!(root.children().find("rejected").is_none());
        let built = h.factory.get("rejected");
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
        h.budget.reserve().unwrap().rollback();
    })
    .await;
}

async fn rejected_preparation_releases_name_and_reservation(error: ChildError) {
    bounded(async {
        let (h, preparer) = GatedPreparer::harness(Some(error.clone()));
        let root = h.root("root").await;
        let pending = root.spawner().spawn(h.request(&root, "retry")).unwrap();
        preparer.gate.entered().await;
        assert!(!pending.is_finished());
        assert!(root.children().find("retry").is_none());
        assert_eq!(h.factory.build_calls.load(Ordering::SeqCst), 1);
        assert!(h.budget.reserve().is_err());
        preparer.gate.open();
        assert_eq!(pending.join().await.err().unwrap(), error);
        assert!(root.children().find("retry").is_none());
        assert_eq!(h.registry.snapshot_scopes().len(), 1);
        assert_eq!(h.factory.build_calls.load(Ordering::SeqCst), 1);
        let failed_id = preparer.attempts.lock().unwrap()[0].clone();
        assert!(!h.histories.path.join(format!("{failed_id}.jsonl")).exists());
        h.budget.reserve().unwrap().rollback();
        let retry = root.spawner().spawn(h.request(&root, "retry")).unwrap();
        preparer.gate.entered().await;
        preparer.gate.open();
        let child = retry.join().await.unwrap();
        assert_eq!(child.identity().session.name, "retry");
        assert_ne!(child.identity().session.session_id, failed_id);
        assert_eq!(preparer.attempts.lock().unwrap().len(), 2);
        assert_eq!(h.factory.build_calls.load(Ordering::SeqCst), 2);
        h.close().await;
        h.budget.reserve().unwrap().rollback();
    })
    .await;
}

#[tokio::test]
async fn gated_history_preparation_failure_rolls_back_without_publishing_and_allows_same_name_retry()
 {
    rejected_preparation_releases_name_and_reservation(ChildError::failure(
        "io",
        "history preparation failed",
    ))
    .await;
}

#[tokio::test]
async fn gated_history_preparation_cancellation_rolls_back_without_publishing_and_allows_same_name_retry()
 {
    rejected_preparation_releases_name_and_reservation(ChildError::Cancelled).await;
}
