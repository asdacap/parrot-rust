//! Persisted selections that are no longer available must pass through the real
//! store/factory error boundary, not a synthetic boxed provider failure.
use super::*;
use parrot_domain::values::identifiers::UserSessionId;

async fn unavailable_selection(mode: &str, selector: &str) {
    let f = RpcFixture::new().await;
    let session = f.create().await;
    let user = f.user();
    assert_eq!(user.id, session.id);
    // Persist the selection a previous installation could have supplied. The
    // current real factory has only build mode and the local provider.
    user.lease
        .update_selection_metadata("local", "local/model", selector, mode)
        .unwrap();
    f.close().await;
    let id = UserSessionId::try_parse(&session.id).unwrap();
    let constructors = f.observations.lock().unwrap().constructors;

    let error = bounded(f.store.resume(&id, true, false))
        .await
        .err()
        .expect("the real factory must reject the unavailable persisted selection");
    let SessionStoreError::Failure(cause) = error else {
        panic!("expected the factory's boxed typed error, got {error}");
    };
    let factory_error = cause
        .downcast_ref::<RpcError>()
        .expect("the actual factory boxes RpcError, not provider::Error");
    assert_eq!(factory_error.code, RpcCode::InvalidArgument);
    assert!(
        cause
            .downcast_ref::<parrot_provider::sessions::Error>()
            .is_none()
    );
    assert!(!f.claim.is_active(&id, Path::new(&f.workspace)).unwrap());

    let service = f.new_service();
    let request = wire::ResumeSessionRequest {
        user_session_id: session.id,
        working_directory: f.workspace.clone(),
        ..Default::default()
    };
    for _ in 0..2 {
        code(
            bounded(service.resume_session(&request, active())).await,
            RpcCode::FailedPrecondition,
        );
        assert!(!f.claim.is_active(&id, Path::new(&f.workspace)).unwrap());
    }
    assert_eq!(f.observations.lock().unwrap().constructors, constructors);
    assert_eq!(f.host.0.load(Ordering::SeqCst), 1);
    bounded(service.close()).await.unwrap();
}

#[tokio::test]
async fn persisted_unavailable_mode_is_failed_precondition_through_actual_factory() {
    unavailable_selection("removed-mode", "local/model").await;
}

#[tokio::test]
async fn persisted_unavailable_model_is_failed_precondition_through_actual_factory() {
    unavailable_selection("build", "removed-provider/model").await;
}
