use super::*;
use parrot_domain::contracts::paths::{RootOrigin, SelectedSessionRoot};
use parrot_storage::{
    history_files::{SessionIndex, SessionMeta},
    ownership::WorkingDirectoryClaim,
};

#[tokio::test]
async fn fresh_synthetic_legacy_metadata_binds_real_host_and_reopens_in_selected_old_root() {
    let fixture = fixture().await;
    let id = Identifier::user_session().unwrap();
    let workspace = fixture.root.join("workspace");
    let selected = SelectedSessionRoot::new(
        fixture.root.join("old/sessions").join(id.value()),
        RootOrigin::Legacy,
    )
    .unwrap();
    let index = SessionIndex::new(selected.clone(), id.clone(), &workspace).unwrap();
    let mut metadata = SessionMeta::new(id.value(), workspace.to_str().unwrap(), "local", "model");
    metadata.created_at = Some("2026-01-01T00:00:00Z".into());
    metadata.root_agent_name = Some("t76-legacy-root".into());
    metadata.mode = Some("build".into());
    metadata.selector = None;
    index.publish(&metadata).unwrap();
    let runtime = RuntimeIdentity::new(
        "host".into(),
        "boot".into(),
        123,
        "start".into(),
        "runtime".into(),
        Arc::new(|_| ProcessIdentity {
            status: ProcessIdentityStatus::Found,
            process_start_token: Some("start".into()),
        }),
    )
    .with_proven_comparable_process_start_tokens();
    let claim = WorkingDirectoryClaim::new(fixture.root.join("old"), runtime)
        .create_fresh(&workspace, &id)
        .unwrap();
    claim.activation_lease.unwrap().release().unwrap();
    let opened = bounded(fixture.store.resume(&id, true, false))
        .await
        .unwrap();
    let hosted = bounded(fixture.factory.bind_opened(opened)).await.unwrap();
    let root = fixture.root();
    let user = fixture.user();
    let image = bounded(hosted.persist_image(image_upload(), &NeverCancelled))
        .await
        .unwrap();
    send(&hosted, "t76-legacy-first", "t76-legacy-message-one").await;
    idle(&root).await;
    let actual_selected = user.lease.selected_root().clone();
    bounded(hosted.close()).await.unwrap();
    assert_database_closed(&user);
    let reopened = bounded(fixture.store.resume(&id, true, false))
        .await
        .unwrap();
    let resumed = bounded(fixture.factory.bind_opened(reopened))
        .await
        .unwrap();
    let resumed_user = fixture.user();
    let resolved = bounded(resumed.resolve_image(&image.artifact_id))
        .await
        .unwrap();
    send(&resumed, "t76-legacy-second", "t76-legacy-message-two").await;
    idle(&fixture.root()).await;
    let history = fixture.root().history().conversation().await.unwrap();
    let final_metadata = index.find().unwrap().unwrap();
    let final_selected = resumed_user.lease.selected_root().clone();
    bounded(resumed.close()).await.unwrap();
    assert_database_closed(&resumed_user);

    assert_eq!(actual_selected, selected);
    assert_eq!(final_selected, selected);
    assert!(!fixture.root.join("new/sessions").join(id.value()).exists());
    assert_eq!(resolved, Some(image));
    assert_eq!(final_metadata.id, metadata.id);
    assert_eq!(final_metadata.created_at, metadata.created_at);
    assert_eq!(final_metadata.root_agent_name, metadata.root_agent_name);
    assert_eq!(final_metadata.working_directory, metadata.working_directory);
    assert_eq!(final_metadata.selector.as_deref(), Some("local/model"));
    assert!(
        history
            .iter()
            .any(|item| parrot_storage::events::content(&item.message.parts) == "t76-legacy-first")
    );
    assert!(
        history
            .iter()
            .any(|item| parrot_storage::events::content(&item.message.parts) == "t76-legacy-second")
    );
}
