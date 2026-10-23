use super::{fixture::*, harness::Harness, support::*};
use parrot_domain::contracts::filenames::FilenameGenerator;
use parrot_runtime::{children::*, task_scheduler::*};
use parrot_storage::{
    artifacts::output_blobs::ToolOutputBlobStore, conversation::HistoryForkBoundary,
};
use std::sync::Arc;

struct Names;
impl FilenameGenerator for Names {
    fn next_filename(&self) -> String {
        format!("{}-arse.dat", id())
    }
}
fn config(h: &Harness, owner: &Arc<Scope<Security>>) -> SchedulerConfig<Security, Parser> {
    let built = h.factory.get(&owner.identity().session.name);
    SchedulerConfig {
        owner_identity: owner.identity().clone(),
        state: owner.agent_state().clone(),
        registry: Arc::downgrade(&h.registry),
        owner_session: built.task_session.clone(),
        history: owner.history().clone(),
        parser: h.factory.parser.clone(),
        router: h.factory.router.clone(),
        configuration: TaskConfiguration::defaults(templates()),
        output_blobs: Arc::new(
            ToolOutputBlobStore::for_session(
                h.histories.repository.events().database().selected_root(),
                "scratch/blobs",
                Arc::new(Names),
            )
            .unwrap(),
        ),
        publisher: built.publisher.clone(),
        diagnostics: h.factory.task_diagnostics.clone(),
        lifetime: h.lifetime.clone(),
    }
}

#[tokio::test]
async fn scheduler_rejects_owner_identity_and_same_named_registered_service_mismatches() {
    let h = Harness::new();
    let owner = h.root("owner").await;
    let forged = AgentIdentity::root(
        owner.identity().session.session_id.clone(),
        "owner".into(),
        templates(),
    );
    let mut mismatch = config(&h, &owner);
    mismatch.owner_identity = forged;
    let error = TaskScheduler::new(mismatch)
        .await
        .err()
        .expect("same ID is insufficient without exact owner identity");
    assert!(error.to_string().contains("does not match owner identity"));
    let alien_service = TaskScheduler::new(config(&h, &owner)).await.unwrap();
    let before = owner.agent_state().load().await.unwrap().unwrap();
    let error = bounded(
        alien_service
            .set_tasks(
                vec![task("a")],
                owner.selection().current().unwrap(),
                HistoryForkBoundary::AfterCompletedHistory,
            )
            .unwrap()
            .join(),
    )
    .await
    .unwrap_err();
    assert!(
        error
            .to_string()
            .contains("owner service does not match registered scope")
    );
    assert!(alien_service.snapshot().await.is_empty());
    assert_eq!(owner.agent_state().load().await.unwrap().unwrap(), before);
    assert!(snapshot(&owner).await.is_empty());
    assert_eq!(h.registry.snapshot_scopes().len(), 1);
    bounded(alien_service.settle().join()).await.unwrap();
    h.close().await;
}
