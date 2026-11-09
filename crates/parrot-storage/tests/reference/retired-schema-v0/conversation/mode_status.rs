#[tokio::test]
async fn session_mode_status_rechecks_exact_version_and_rolls_back_projection_failure() {
    let fixture = Fixture::new();
    let repo = fixture.repository();
    let generated = Arc::new(AtomicU64::new(0));
    let calls = generated.clone();
    let repo = repo.with_agent_session_id_generator(Arc::new(move || {
        calls.fetch_add(1, Ordering::SeqCst);
        Ok("agent".into())
    }));
    assert_eq!(
        repo.session_state("user".into(), "initial".into())
            .await
            .unwrap(),
        ("agent".into(), "initial".into())
    );
    assert_eq!(
        repo.session_state("user".into(), "ignored".into())
            .await
            .unwrap(),
        ("agent".into(), "initial".into())
    );
    assert_eq!(generated.load(Ordering::SeqCst), 2);
    let initial = PendingStatus {
        mode: "initial".into(),
        version: -1,
    };
    assert_eq!(
        repo.pending_status("agent".into()).await.unwrap(),
        Some(initial.clone())
    );
    assert!(
        repo.append_status_prompt(
            event("not-recorded"),
            initial.clone(),
            "initial status".into()
        )
        .await
        .unwrap()
    );
    assert!(
        !repo
            .append_status_prompt(event("not-recorded"), initial.clone(), "duplicate".into())
            .await
            .unwrap()
    );
    assert!(repo.pending_status("agent".into()).await.unwrap().is_none());
    repo.update_mode("user".into(), "agent".into(), "next".into())
        .await
        .unwrap();
    let next = repo.pending_status("agent".into()).await.unwrap().unwrap();
    assert_eq!(next.version, 1);
    assert!(
        !repo
            .append_status_prompt(event("not-recorded"), initial, "stale".into())
            .await
            .unwrap()
    );
    fail_projection(&repo).await;
    assert!(
        repo.append_status_prompt(event("not-recorded"), next.clone(), "failed".into())
            .await
            .is_err()
    );
    assert_eq!(
        repo.pending_status("agent".into()).await.unwrap(),
        Some(next.clone())
    );
    assert_eq!(count(&repo, "status_prompt").await, 1);
    resume_projection(&repo).await;
    assert!(
        repo.append_status_prompt(event("not-recorded"), next, "next status".into())
            .await
            .unwrap()
    );
    assert_eq!(count(&repo, "event").await, 0);
    assert_eq!(count(&repo, "status_prompt").await, 2);
    repo.update_mode(
        "missing-user".into(),
        "other-agent".into(),
        "orphan-mode".into(),
    )
    .await
    .unwrap();
    assert_eq!(count(&repo, "mode_change").await, 2);
    assert!(
        repo.pending_status("other-agent".into())
            .await
            .unwrap()
            .is_none()
    );
    repo.events().database().close().await.unwrap();
}
