use super::*;
use parrot_domain::values::identifiers::UserSessionId;
use parrot_storage::{
    database::Connection, history_files::SessionMeta, ownership::ClaimDisposition,
};
use std::os::unix::fs::symlink;

fn workspaces(fixture: &Fixture) -> (PathBuf, PathBuf) {
    let physical = fixture.root.join("physical/sub");
    std::fs::create_dir_all(&physical).unwrap();
    std::fs::create_dir_all(fixture.root.join("unrelated/sub")).unwrap();
    symlink(fixture.root.join("physical"), fixture.root.join("alias")).unwrap();
    (physical, fixture.root.join("alias/sub"))
}

fn persisted_identity(fixture: &Fixture, id: &str) -> (SessionMeta, String) {
    let directory = fixture.roots.current().state().join("sessions").join(id);
    let metadata =
        serde_json::from_slice(&std::fs::read(directory.join("meta.json")).unwrap()).unwrap();
    let database = Connection::open(directory.join("session.db")).unwrap();
    let root = database
        .query_row(
            "SELECT agent_session FROM session_state WHERE user_session = ?1",
            [id],
            |row| row.get(0),
        )
        .unwrap();
    (metadata, root)
}

fn assert_claim_key(fixture: &Fixture, launch: &Path, other: &Path, id: &UserSessionId) {
    let claim = WorkingDirectoryClaim::new(
        fixture.roots.current().state(),
        capture_runtime_identity(&host_key().unwrap()).unwrap(),
    );
    let key = WorkingDirectoryClaim::canonicalize(launch).unwrap();
    let other_key = WorkingDirectoryClaim::canonicalize(other).unwrap();
    assert_ne!(key, other_key);
    assert_eq!(
        claim.workspace_associations(&key).unwrap().unwrap(),
        vec![id.clone()]
    );
    assert!(
        claim
            .workspace_associations(&other_key)
            .unwrap()
            .unwrap()
            .is_empty()
    );
    let discovered = claim.discover_latest(launch).unwrap();
    assert_eq!(discovered.disposition, ClaimDisposition::Resumed);
    assert_eq!(discovered.session_id.as_ref(), Some(id));
    let other_discovery = claim.discover_latest(other).unwrap();
    assert_eq!(other_discovery.disposition, ClaimDisposition::Fresh);
    assert!(other_discovery.session_id.is_none());
}

async fn create_and_resume(
    fixture: &Fixture,
    launch: &Path,
    request_alias: &Path,
    provider: Arc<NoCallProvider>,
) -> String {
    let host = fixture.host_at(launch, provider.clone());
    let session = bounded(host.service().create_session(
        &wire::CreateSessionRequest {
            model: "local/test".into(),
            mode: "build".into(),
            ..Default::default()
        },
        Arc::new(NeverCancelled),
    ))
    .await
    .unwrap();
    assert_eq!(session.working_directory, launch.to_string_lossy());
    bounded(host.close()).await.unwrap();

    let id = UserSessionId::parse(&session.id).unwrap();
    let (metadata, root) = persisted_identity(fixture, &session.id);
    assert_eq!(metadata.working_directory.as_deref(), launch.to_str());
    assert_eq!(metadata.root_agent_name.as_deref(), Some("main"));
    assert!(!root.is_empty());
    assert_claim_key(fixture, launch, request_alias, &id);

    let resumed = fixture.host_at(launch, provider);
    for filter in [launch, request_alias] {
        let listed = resumed
            .service()
            .list_sessions(&wire::ListSessionsRequest {
                working_directory: filter.to_string_lossy().into_owned(),
                ..Default::default()
            })
            .unwrap();
        assert_eq!(listed.sessions.len(), 1);
        assert_eq!(listed.sessions[0].user_session_id, session.id);
    }
    let unrelated = wire::ResumeSessionRequest {
        user_session_id: session.id.clone(),
        working_directory: fixture
            .root
            .join("unrelated/sub")
            .to_string_lossy()
            .into_owned(),
        ..Default::default()
    };
    let error = bounded(
        resumed
            .service()
            .resume_session(&unrelated, Arc::new(NeverCancelled)),
    )
    .await
    .unwrap_err();
    assert_eq!(error.code, RpcCode::FailedPrecondition);
    let result = bounded(resumed.service().resume_session(
        &wire::ResumeSessionRequest {
            working_directory: request_alias.to_string_lossy().into_owned(),
            ..unrelated
        },
        Arc::new(NeverCancelled),
    ))
    .await
    .unwrap();
    assert_eq!(result.id, session.id);
    assert_eq!(result.working_directory, session.working_directory);
    bounded(resumed.close()).await.unwrap();

    let (mut reopened_metadata, reopened_root) = persisted_identity(fixture, &session.id);
    reopened_metadata.last_opened_at = metadata.last_opened_at.clone();
    assert_eq!(reopened_metadata, metadata);
    assert_eq!(reopened_root, root);
    assert_claim_key(fixture, launch, request_alias, &id);
    session.id
}

#[tokio::test]
async fn logical_launch_preserves_persistent_identity_with_physical_requests() {
    let fixture = Fixture::new();
    let (physical, logical) = workspaces(&fixture);
    let provider = Arc::new(NoCallProvider::default());
    create_and_resume(&fixture, &logical, &physical, provider.clone()).await;
    assert_eq!(provider.0.load(Ordering::SeqCst), 0);
    std::fs::remove_dir_all(&fixture.root).unwrap();
}

#[tokio::test]
async fn physical_launch_preserves_existing_key_and_rejects_retargeted_request_alias() {
    let fixture = Fixture::new();
    let (physical, logical) = workspaces(&fixture);
    let provider = Arc::new(NoCallProvider::default());
    let id = create_and_resume(&fixture, &physical, &logical, provider.clone()).await;
    let identity = persisted_identity(&fixture, &id);
    let final_link = fixture.root.join("final-link");
    symlink(&physical, &final_link).unwrap();
    assert_eq!(
        WorkingDirectoryClaim::canonicalize(&final_link).unwrap(),
        WorkingDirectoryClaim::canonicalize(&physical).unwrap()
    );

    std::fs::remove_file(fixture.root.join("alias")).unwrap();
    symlink(fixture.root.join("unrelated"), fixture.root.join("alias")).unwrap();
    let host = fixture.host_at(&physical, provider.clone());
    let error = bounded(host.service().resume_session(
        &wire::ResumeSessionRequest {
            user_session_id: id.clone(),
            working_directory: logical.to_string_lossy().into_owned(),
            ..Default::default()
        },
        Arc::new(NeverCancelled),
    ))
    .await
    .unwrap_err();
    assert_eq!(error.code, RpcCode::FailedPrecondition);
    bounded(host.close()).await.unwrap();
    assert_eq!(persisted_identity(&fixture, &id), identity);
    assert_eq!(provider.0.load(Ordering::SeqCst), 0);
    std::fs::remove_dir_all(&fixture.root).unwrap();
}
