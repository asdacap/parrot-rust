use super::*;
use parrot_domain::values::identifiers::UserSessionId;

#[tokio::test]
async fn fast_mode_updates_session_routing_without_selection_side_effects_or_session_leakage() {
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
    assert!(!hosted.snapshot().fast_mode);
    for enabled in [true, false] {
        hosted
            .update(None, None, None, Some(enabled))
            .await
            .unwrap();
        assert_eq!(hosted.snapshot().fast_mode, enabled);
        assert_eq!(user.fast_mode.is_enabled(), enabled);
        assert!(!other.snapshot().fast_mode);
        assert!(!other_user.fast_mode.is_enabled());
        let mut expected = before.clone();
        expected["FastMode"] = json!(enabled);
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
async fn fast_mode_persistence_failure_leaves_live_routing_unchanged() {
    let fixture = Fixture::new(false, false).await;
    let hosted = fixture.open().await.unwrap();
    let user = fixture.user();
    let path = user.lease.selected_root().path().join("meta.json");
    let before = std::fs::read(&path).unwrap();
    let staging = path.with_file_name("meta.json.staging");
    std::fs::create_dir(&staging).unwrap();
    let error = hosted
        .update(None, None, None, Some(true))
        .await
        .unwrap_err();
    assert_eq!(error.code, RpcCode::Internal);
    assert!(!hosted.snapshot().fast_mode);
    assert!(!user.fast_mode.is_enabled());
    assert_eq!(std::fs::read(&path).unwrap(), before);
    std::fs::remove_dir(&staging).unwrap();
    hosted.close().await.unwrap();
}

#[tokio::test]
async fn fast_mode_override_survives_selection_updates_and_reopen_with_changed_default() {
    for enabled in [None, Some(false), Some(true)] {
        let fixture = Fixture::new(false, false).await;
        let hosted = fixture.open().await.unwrap();
        let id = UserSessionId::parse(&hosted.snapshot().id).unwrap();
        let root = fixture.root.clone();
        if let Some(enabled) = enabled {
            hosted
                .update(None, None, None, Some(enabled))
                .await
                .unwrap();
        }
        hosted
            .update(
                Some(fixture.router.resolve("local/model-next").unwrap()),
                Some("build".into()),
                None,
                None,
            )
            .await
            .unwrap();
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
            |config| config.fast_mode = true,
        )
        .await;
        let opened = reopened.store.resume(&id, true, false).await.unwrap();
        let hosted = reopened.factory.bind_opened(opened).await.unwrap();
        assert_eq!(hosted.snapshot().fast_mode, enabled.unwrap_or(true));
        assert_eq!(
            reopened.user().fast_mode.is_enabled(),
            enabled.unwrap_or(true)
        );
        assert_eq!(hosted.snapshot().model, "local/model-next");
        hosted.close().await.unwrap();
    }
}
