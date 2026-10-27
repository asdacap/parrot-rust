use super::*;
use parrot_domain::values::identifiers::UserSessionId;

#[tokio::test]
async fn actual_task_limit_updates_live_budget_without_selection_side_effects_or_session_leakage() {
    let fixture = Fixture::new(false, false).await;
    let hosted = fixture.open().await.unwrap();
    let user = fixture.user();
    let root = fixture.root();
    let other = fixture.open().await.unwrap();
    let other_user = fixture.user();
    let path = user.lease.selected_root().path().join("meta.json");
    let before: Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    let state = serde_json::to_value(root.agent_state().load().await.unwrap()).unwrap();
    let events = user.lease.events.replay().await.unwrap();
    assert_eq!(hosted.snapshot().extra_concurrent_tasks, 2);
    for limit in [0, 5, 1] {
        hosted.update(None, None, Some(limit)).await.unwrap();
        assert_eq!(hosted.snapshot().extra_concurrent_tasks, limit);
        assert_eq!(user.task_concurrency.get_limit(), limit as usize);
        assert_eq!(other.snapshot().extra_concurrent_tasks, 2);
        assert_eq!(other_user.task_concurrency.get_limit(), 2);
        let mut expected = before.clone();
        expected["ExtraConcurrentTasks"] = json!(limit);
        let saved: Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        assert_eq!(saved, expected);
        assert_eq!(
            serde_json::to_value(root.agent_state().load().await.unwrap()).unwrap(),
            state
        );
        assert_eq!(user.lease.events.replay().await.unwrap(), events);
    }
    hosted.close().await.unwrap();
    other.close().await.unwrap();
}

#[tokio::test]
async fn actual_task_limit_persistence_failure_and_negative_value_leave_live_budget_unchanged() {
    let fixture = Fixture::new(false, false).await;
    let hosted = fixture.open().await.unwrap();
    let user = fixture.user();
    let path = user.lease.selected_root().path().join("meta.json");
    let before = std::fs::read(&path).unwrap();
    assert_eq!(
        hosted.update(None, None, Some(-1)).await.unwrap_err().code,
        RpcCode::InvalidArgument
    );
    assert_eq!(std::fs::read(&path).unwrap(), before);
    let staging = path.with_file_name("meta.json.staging");
    std::fs::create_dir(&staging).unwrap();
    let error = hosted.update(None, None, Some(0)).await.unwrap_err();
    assert_eq!(error.code, RpcCode::Internal);
    assert!(error.message.contains("write staged session metadata"));
    assert_eq!(hosted.snapshot().extra_concurrent_tasks, 2);
    assert_eq!(user.task_concurrency.get_limit(), 2);
    std::fs::remove_dir(&staging).unwrap();
    assert_eq!(std::fs::read(&path).unwrap(), before);
    hosted.close().await.unwrap();
}

#[tokio::test]
async fn actual_task_limit_override_survives_selection_updates_and_reopen_with_changed_default() {
    for limit in [None, Some(0), Some(4)] {
        let fixture = Fixture::new(false, false).await;
        let hosted = fixture.open().await.unwrap();
        let id = UserSessionId::parse(&hosted.snapshot().id).unwrap();
        let root = fixture.root.clone();
        if let Some(limit) = limit {
            hosted.update(None, None, Some(limit)).await.unwrap();
        }
        hosted
            .update(
                Some(fixture.router.resolve("local/model-next").unwrap()),
                Some("build".into()),
                None,
            )
            .await
            .unwrap();
        let path = fixture
            .user()
            .lease
            .selected_root()
            .path()
            .join("meta.json");
        let saved: Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        assert_eq!(
            saved.get("ExtraConcurrentTasks"),
            limit.as_ref().map(|limit| json!(limit)).as_ref()
        );
        hosted.close().await.unwrap();
        drop(hosted);
        drop(fixture);
        let reopened = Fixture::with_root_config(
            root,
            false,
            false,
            None,
            None,
            Arc::new(Policies),
            |config| config.extra_concurrent_tasks = 7,
        )
        .await;
        let opened = reopened.store.resume(&id, true, false).await.unwrap();
        let hosted = reopened.factory.bind_opened(opened).await.unwrap();
        let expected = limit.unwrap_or(7);
        assert_eq!(hosted.snapshot().extra_concurrent_tasks, expected);
        assert_eq!(
            reopened.user().task_concurrency.get_limit(),
            expected as usize
        );
        assert_eq!(hosted.snapshot().model, "local/model-next");
        hosted.close().await.unwrap();
    }
}

#[tokio::test]
async fn actual_concurrent_task_limit_and_selection_updates_preserve_both_metadata_fields() {
    let fixture = Fixture::new(false, false).await;
    let hosted = fixture.open().await.unwrap();
    let (selection, limit) = tokio::join!(
        hosted.update(
            Some(fixture.router.resolve("local/model-next").unwrap()),
            Some("build".into()),
            None,
        ),
        hosted.update(None, None, Some(0)),
    );
    selection.unwrap();
    limit.unwrap();
    let user = fixture.user();
    let saved: Value = serde_json::from_slice(
        &std::fs::read(user.lease.selected_root().path().join("meta.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(saved["ExtraConcurrentTasks"], 0);
    assert_eq!(saved["Model"], "local/model-next");
    assert_eq!(hosted.snapshot().extra_concurrent_tasks, 0);
    assert_eq!(hosted.snapshot().model, "local/model-next");
    hosted.close().await.unwrap();
}
