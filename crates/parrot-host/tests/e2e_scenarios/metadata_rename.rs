use super::*;
use parrot_storage::history_files::{SessionIndex, SessionMeta};
use std::sync::atomic::{AtomicBool, Ordering};

#[tokio::test]
async fn actual_metadata_final_rename_failure_preserves_stage_and_history_then_explicit_retry_resumes()
 {
    let Fixture {
        root: fixture_root,
        store,
        factory,
        observations,
        requests,
        ..
    } = fixture().await;
    let armed = Arc::new(Mutex::new(None::<(PathBuf, PathBuf)>));
    let injected = Arc::new(AtomicBool::new(false));
    let store = store.with_clock(Arc::new({
        let armed = armed.clone();
        let injected = injected.clone();
        move || {
            if let Some((metadata_path, backup_path)) = armed.lock().unwrap().take() {
                // record_opened has read valid metadata before calling this public clock.
                // Only this fresh fixture's file is moved; the obstacle forces real rename.
                std::fs::rename(&metadata_path, &backup_path).unwrap();
                std::fs::create_dir(&metadata_path).unwrap();
                injected.store(true, Ordering::SeqCst);
            }
            if injected.load(Ordering::SeqCst) {
                "2026-10-21T00:01:00Z".into()
            } else {
                "2026-10-21T00:00:00Z".into()
            }
        }
    }));
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
    let index =
        SessionIndex::new(selected.clone(), id.clone(), fixture_root.join("workspace")).unwrap();
    let metadata_path = index.metadata_path().to_owned();
    let backup_path = metadata_path.with_file_name("meta.json.t76-before-rename");
    let staging_path = metadata_path.with_file_name("meta.json.staging");
    send(&hosted, "t76-rename-before-fault", "t76-rename-first").await;
    idle(&root).await;
    let original_bytes = std::fs::read(&metadata_path).unwrap();
    let original: SessionMeta = serde_json::from_slice(&original_bytes).unwrap();
    let before_raw = user.lease.events.replay_raw().await.unwrap();
    let before_history = root.history().conversation().await.unwrap();
    let before_id = hosted.snapshot().id.clone();
    assert!(!backup_path.exists());
    assert!(!staging_path.exists());

    *armed.lock().unwrap() = Some((metadata_path.clone(), backup_path.clone()));
    let failed = hosted.record_opened();
    let target_is_directory = metadata_path.is_dir();
    let obstructed_index_hit = index.find().unwrap();
    let backup_bytes = std::fs::read(&backup_path).unwrap();
    let stage_is_regular_file = std::fs::symlink_metadata(&staging_path)
        .unwrap()
        .file_type()
        .is_file();
    let failed_stage_bytes = std::fs::read(&staging_path).unwrap();
    let staged_document: Value = serde_json::from_slice(&failed_stage_bytes).unwrap();
    let staged_metadata: SessionMeta = serde_json::from_slice(&failed_stage_bytes).unwrap();
    let after_failure_raw = user.lease.events.replay_raw().await.unwrap();
    let after_failure_history = root.history().conversation().await.unwrap();
    let after_failure_id = hosted.snapshot().id.clone();
    let same_bound_identity = {
        let observed = observations.lock().unwrap();
        Arc::ptr_eq(observed.user.as_ref().unwrap(), &user)
            && Arc::ptr_eq(observed.bound.as_ref().unwrap(), &root)
    };

    // The injected directory intentionally makes the published path unreadable.
    // Restore this test-owned obstacle and backup explicitly: this is not rollback.
    std::fs::remove_dir(&metadata_path).unwrap();
    std::fs::rename(&backup_path, &metadata_path).unwrap();
    let restored_bytes = std::fs::read(&metadata_path).unwrap();
    let retried = hosted.record_opened();
    let retry_metadata = index.find().unwrap();
    let retry_consumed_stage = !staging_path.exists();
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
    let resumed_id = resumed.snapshot().id.clone();
    let resumed_selected = resumed_user.lease.selected_root().clone();
    let resumed_prefix_raw = resumed_user.lease.events.replay_raw().await.unwrap();
    let resumed_prefix_history = resumed_root.history().conversation().await.unwrap();
    send(&resumed, "t76-rename-after-resume", "t76-rename-second").await;
    idle(&resumed_root).await;
    let final_raw = resumed_user.lease.events.replay_raw().await.unwrap();
    let final_events = resumed_user.lease.events.replay().await.unwrap();
    let final_history = resumed_root.history().conversation().await.unwrap();
    bounded(resumed.close()).await.unwrap();
    assert_database_closed(&resumed_user);

    let error = failed.unwrap_err();
    assert_eq!(error.code, RpcCode::Internal);
    assert!(
        error
            .message
            .starts_with("publish staged session metadata:")
    );
    assert!(injected.load(Ordering::SeqCst));
    assert!(armed.lock().unwrap().is_none());
    assert!(target_is_directory);
    assert_eq!(obstructed_index_hit, None);
    assert_eq!(backup_bytes, original_bytes);
    assert_eq!(restored_bytes, original_bytes);
    assert!(stage_is_regular_file);
    assert_eq!(original.id.as_deref(), Some(id.value()));
    assert_eq!(
        original.last_opened_at.as_deref(),
        Some("2026-10-21T00:00:00Z")
    );
    let mut expected = original;
    expected.last_opened_at = Some("2026-10-21T00:01:00Z".into());
    assert_eq!(staged_metadata, expected);
    assert_eq!(staged_document, serde_json::to_value(&expected).unwrap());
    assert_eq!(before_id, after_failure_id);
    assert!(same_bound_identity);
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
    assert!(
        retried.is_ok(),
        "explicit restored-target retry: {retried:?}"
    );
    assert_eq!(retry_metadata, Some(expected));
    assert!(retry_consumed_stage);
    assert!(!backup_path.exists());
    assert_eq!(resumed_id, id.value());
    assert_eq!(resumed_selected, selected);
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
    for input in ["t76-rename-before-fault", "t76-rename-after-resume"] {
        assert!(
            final_history
                .iter()
                .any(|item| { parrot_storage::events::content(&item.message.parts) == input })
        );
    }
    assert_eq!(requests.lock().unwrap().len(), 2);
}
