use super::*;
use parrot_storage::history_files::SessionMeta;

#[tokio::test]
async fn actual_metadata_publication_failure_preserves_identity_and_history_then_explicit_retry_resumes()
 {
    let Fixture {
        root: fixture_root,
        store,
        factory,
        observations,
        requests,
        ..
    } = fixture().await;
    let opened = bounded(store.create_fresh(
        SessionModelRouter::resolve(&Policies, "local/model").unwrap(),
        "build",
        true,
    ))
    .await
    .unwrap();
    let hosted = bounded(factory.bind_opened(opened)).await.unwrap();
    let (user, root) = {
        let observed = observations.lock().unwrap();
        (
            observed.user.clone().unwrap(),
            observed.bound.clone().unwrap(),
        )
    };
    let id = UserSessionId::parse(&user.id).unwrap();
    let selected = user.lease.selected_root().clone();
    let index = user.lease.metadata();
    let metadata_path = index.metadata_path().to_owned();
    let staging_path = metadata_path.with_file_name("meta.json.staging");
    send(
        &hosted,
        "t76-publication-before-fault",
        "t76-publication-first",
    )
    .await;
    idle(&root).await;
    let original_bytes = std::fs::read(&metadata_path).unwrap();
    let mut original: SessionMeta = serde_json::from_slice(&original_bytes).unwrap();
    original.last_opened_at = Some("2000-01-01T00:00:00Z".into());
    let original_bytes = serde_json::to_vec(&original).unwrap();
    std::fs::write(&metadata_path, &original_bytes).unwrap();
    let before_raw = user.lease.events.replay_raw().await.unwrap();
    let before_history = root.history().conversation().await.unwrap();
    let before_id = hosted.snapshot().id.clone();
    assert!(!staging_path.exists());

    std::fs::create_dir(&staging_path).unwrap();
    let failed = hosted.record_opened();
    let error = failed.unwrap_err();
    assert_eq!(error.code, RpcCode::Internal);
    assert!(error.message.contains("session metadata staging file"));
    assert_eq!(std::fs::read(&metadata_path).unwrap(), original_bytes);
    assert_eq!(index.find().unwrap(), Some(original.clone()));
    assert_eq!(hosted.snapshot().id, before_id);
    {
        let observed = observations.lock().unwrap();
        assert!(Arc::ptr_eq(observed.user.as_ref().unwrap(), &user));
        assert!(Arc::ptr_eq(observed.bound.as_ref().unwrap(), &root));
    }
    let after_failure_raw = user.lease.events.replay_raw().await.unwrap();
    let after_failure_history = root.history().conversation().await.unwrap();

    std::fs::remove_dir(&staging_path).unwrap();
    let before_retry = chrono::DateTime::<chrono::Utc>::from(std::time::SystemTime::now());
    hosted.record_opened().unwrap();
    let after_retry = chrono::DateTime::<chrono::Utc>::from(std::time::SystemTime::now());
    let updated = index.find().unwrap().unwrap();
    let opened_at =
        chrono::DateTime::parse_from_rfc3339(updated.last_opened_at.as_ref().unwrap()).unwrap();
    assert!(opened_at.timestamp() >= before_retry.timestamp());
    assert!(opened_at.timestamp() <= after_retry.timestamp());
    let mut expected = original;
    expected.last_opened_at = updated.last_opened_at.clone();
    assert_eq!(updated, expected);
    assert_eq!(
        serde_json::from_slice::<Value>(&std::fs::read(&metadata_path).unwrap()).unwrap(),
        serde_json::to_value(&expected).unwrap()
    );
    assert!(!staging_path.exists());
    let after_retry_raw = user.lease.events.replay_raw().await.unwrap();
    let after_retry_history = root.history().conversation().await.unwrap();
    bounded(hosted.close()).await.unwrap();
    assert_database_closed(&user);

    let opened = bounded(store.resume(&id, true, false)).await.unwrap();
    let resumed = bounded(factory.bind_opened(opened)).await.unwrap();
    let (resumed_user, resumed_root) = {
        let observed = observations.lock().unwrap();
        (
            observed.user.clone().unwrap(),
            observed.bound.clone().unwrap(),
        )
    };
    assert_eq!(resumed.snapshot().id, id.value());
    assert_eq!(resumed_user.lease.selected_root(), &selected);
    let resumed_prefix_raw = resumed_user.lease.events.replay_raw().await.unwrap();
    let resumed_prefix_history = resumed_root.history().conversation().await.unwrap();
    send(
        &resumed,
        "t76-publication-after-resume",
        "t76-publication-second",
    )
    .await;
    idle(&resumed_root).await;
    let final_raw = resumed_user.lease.events.replay_raw().await.unwrap();
    let final_events = resumed_user.lease.events.replay().await.unwrap();
    let final_history = resumed_root.history().conversation().await.unwrap();
    bounded(resumed.close()).await.unwrap();
    assert_database_closed(&resumed_user);

    for rows in [&after_failure_raw, &after_retry_raw, &resumed_prefix_raw] {
        assert_eq!(
            before_raw
                .iter()
                .map(|row| (row.sequence, &row.raw))
                .collect::<Vec<_>>(),
            rows.iter()
                .map(|row| (row.sequence, &row.raw))
                .collect::<Vec<_>>()
        );
    }
    assert_eq!(before_history, after_failure_history);
    assert_eq!(before_history, after_retry_history);
    assert_eq!(before_history, resumed_prefix_history);
    assert_eq!(
        final_raw[..before_raw.len()]
            .iter()
            .map(|row| (row.sequence, &row.raw))
            .collect::<Vec<_>>(),
        before_raw
            .iter()
            .map(|row| (row.sequence, &row.raw))
            .collect::<Vec<_>>()
    );
    assert_eq!(
        final_events
            .iter()
            .filter(|event| event.has_turn_ended())
            .map(|event| event.turn_ended().finish_reason.as_str())
            .collect::<Vec<_>>(),
        vec!["stop", "stop"]
    );
    for input in [
        "t76-publication-before-fault",
        "t76-publication-after-resume",
    ] {
        assert!(
            final_history
                .iter()
                .any(|item| parrot_storage::events::content(&item.message.parts) == input)
        );
    }
    assert_eq!(requests.lock().unwrap().len(), 2);
    std::fs::remove_dir_all(fixture_root).unwrap();
}
