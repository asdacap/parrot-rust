use super::*;
use integrated_adapters::{reply, script};
use parrot_domain::values::{
    identifiers::UserSessionId,
    progress::{
        PROGRESS_FILE_NAME, PROGRESS_INVENTORY_KIND, Progress, ProgressSnapshot, ProgressUpdate,
    },
};
use parrot_runtime::active_work::progress::AgentProgress;
use tokio::sync::Semaphore;

fn progress_path(fixture: &Fixture, scope: &Scope<Security>) -> PathBuf {
    let names = scope
        .identity()
        .name_path
        .iter()
        .map(String::as_str)
        .collect::<Vec<_>>();
    fixture
        .user()
        .resources
        .agent_scratch(&names)
        .unwrap()
        .root()
        .join(PROGRESS_FILE_NAME)
}

#[tokio::test]
async fn actual_catalog_progress_uses_the_scoped_file_blocker_and_snapshot_owner() {
    let path = {
        let fixture = Fixture::new(false, false).await;
        let hosted = fixture.open().await.unwrap();
        let root = fixture.root();
        let owner = root.get_service::<AgentProgress>().unwrap();
        assert!(!progress_path(&fixture, &root).exists());
        let subscription = fixture.user().broker.subscribe();
        let mut completion = reply("");
        completion
            .tool_calls
            .push(parrot_provider::sessions::ToolCall {
                id: "progress-start".into(),
                name: "progress".into(),
                arguments_json:
                    json!({"steps":2,"total_steps":5,"current_step":"Running actual tests"})
                        .to_string(),
            });
        script(&fixture, "progress-catalog-marker", completion, None);
        let gate = Arc::new(Semaphore::new(0));
        let mut completion = reply("");
        completion
            .tool_calls
            .push(parrot_provider::sessions::ToolCall {
                id: "progress-close".into(),
                name: "progress".into(),
                arguments_json: json!({"close":true}).to_string(),
            });
        script(
            &fixture,
            "progress-catalog-marker",
            completion,
            Some(gate.clone()),
        );
        script(
            &fixture,
            "progress-catalog-marker",
            reply("progress finished"),
            None,
        );
        let session = root.get_service::<RuntimeSession<Security>>().unwrap();
        let sent = bounded(
            session.send_text_message("progress-catalog-marker".into(), Arc::new(NeverCancelled)),
        )
        .await
        .unwrap();
        let event = bounded(async {
            loop {
                let event = subscription.next(&NeverCancelled).await.unwrap().unwrap();
                if event.has_tool_snapshot()
                    && event.tool_snapshot().kind == PROGRESS_INVENTORY_KIND
                {
                    let state: ProgressSnapshot =
                        serde_json::from_slice(&event.tool_snapshot().data).unwrap();
                    if state.progress.is_some() {
                        break event;
                    }
                }
            }
        })
        .await;
        let snapshot: ProgressSnapshot =
            serde_json::from_slice(&std::fs::read(progress_path(&fixture, &root)).unwrap())
                .unwrap();
        assert_eq!(snapshot, owner.capture_snapshot().unwrap());
        assert_eq!(snapshot.progress.as_ref().unwrap().steps, 2);
        assert_eq!(
            snapshot.owner_session_id,
            root.identity().session.session_id
        );
        assert_eq!(event.agent_session_id, snapshot.owner_session_id);
        assert_eq!(
            event.tool_snapshot().owner_agent_session_id,
            snapshot.owner_session_id
        );
        assert_eq!(event.tool_snapshot().revision, snapshot.revision);
        assert!(!event.tool_snapshot().removed);
        assert!(!event.tool_snapshot().inventory_instance_id.is_empty());
        assert_eq!(
            serde_json::from_slice::<ProgressSnapshot>(&event.tool_snapshot().data).unwrap(),
            snapshot
        );
        let services = root
            .get_service::<AgentSessionServices<Security>>()
            .unwrap();
        assert!(
            services
                .active_work
                .build()
                .await
                .unwrap()
                .unwrap()
                .contains("progress")
        );
        session
            .backend()
            .reminders()
            .set("manual", "ordinary reminder", &NeverCancelled)
            .await
            .unwrap();
        session
            .backend()
            .reminders()
            .clear("manual", &NeverCancelled)
            .await
            .unwrap();
        assert!(owner.capture_snapshot().unwrap().progress.is_some());
        gate.add_permits(1);
        bounded(sent.execution.join()).await.unwrap();
        assert_eq!(owner.capture_snapshot().unwrap().progress, None);
        assert_eq!(services.active_work.build().await.unwrap(), None);
        let saved: ProgressSnapshot =
            serde_json::from_slice(&std::fs::read(progress_path(&fixture, &root)).unwrap())
                .unwrap();
        assert_eq!(saved.progress, None);
        assert_eq!(saved.revision, 2);
        let events = fixture.user().lease.events.replay().await.unwrap();
        for id in ["progress-start", "progress-close"] {
            let finished = events
                .iter()
                .find(|event| event.has_tool_finished() && event.tool_finished().tool_call_id == id)
                .unwrap();
            assert!(
                !finished
                    .tool_finished()
                    .result
                    .as_deref()
                    .unwrap()
                    .starts_with("error:")
            );
        }
        bounded(hosted.close()).await.unwrap();
        fixture.root.clone()
    };
    std::fs::remove_dir_all(path).unwrap();
}

#[tokio::test]
async fn actual_progress_survives_shutdown_and_reopen_without_provider_activation() {
    let path = {
        let fixture = Fixture::new(false, false).await;
        let hosted = fixture.open().await.unwrap();
        let root = fixture.root();
        let progress = root.get_service::<AgentProgress>().unwrap();
        progress
            .update(ProgressUpdate::Replace(Progress {
                steps: 5,
                total_steps: 5,
                current_step: Some("Full but still open".into()),
            }))
            .await
            .unwrap();
        let before = std::fs::read(progress_path(&fixture, &root)).unwrap();
        let id = UserSessionId::parse(&hosted.snapshot().id).unwrap();
        bounded(hosted.close()).await.unwrap();
        assert_eq!(
            std::fs::read(progress_path(&fixture, &root)).unwrap(),
            before
        );
        let opened = bounded(fixture.store.resume(&id, true, false))
            .await
            .unwrap();
        let resumed = bounded(fixture.factory.bind_opened(opened)).await.unwrap();
        let restored = fixture.root();
        let progress = restored.get_service::<AgentProgress>().unwrap();
        assert!(progress.capture_snapshot().unwrap().progress.is_some());
        assert!(
            restored
                .get_service::<AgentSessionServices<Security>>()
                .unwrap()
                .active_work
                .build()
                .await
                .unwrap()
                .is_some()
        );
        assert!(fixture.requests.lock().unwrap().is_empty());
        let mut stream = resumed.listen(None, Arc::new(NeverCancelled)).unwrap();
        let snapshot = bounded(async {
            loop {
                let event = stream.next(&NeverCancelled).await.unwrap().unwrap();
                if event.has_tool_snapshot()
                    && event.tool_snapshot().kind == PROGRESS_INVENTORY_KIND
                {
                    break serde_json::from_slice::<ProgressSnapshot>(&event.tool_snapshot().data)
                        .unwrap();
                }
            }
        })
        .await;
        assert_eq!(snapshot, progress.capture_snapshot().unwrap());
        assert_eq!(snapshot.progress.unwrap().steps, 5);
        progress.update(ProgressUpdate::Close).await.unwrap();
        drop(stream);
        bounded(resumed.close()).await.unwrap();
        let opened = bounded(fixture.store.resume(&id, true, false))
            .await
            .unwrap();
        let resumed = bounded(fixture.factory.bind_opened(opened)).await.unwrap();
        assert_eq!(
            fixture
                .root()
                .get_service::<AgentProgress>()
                .unwrap()
                .capture_snapshot()
                .unwrap()
                .progress,
            None
        );
        assert!(fixture.requests.lock().unwrap().is_empty());
        bounded(resumed.close()).await.unwrap();
        fixture.root.clone()
    };
    std::fs::remove_dir_all(path).unwrap();
}

#[test]
fn progress_obeys_existing_profile_allowlists_and_disabled_tools() {
    let (configuration, _) = PromptTemplateCatalog::from_content(
        "",
        &BTreeMap::from([("HOME".into(), "/synthetic-progress-policy".into())]),
    )
    .unwrap();
    let profile = &configuration.runtime.profiles["worker"];
    let allowed =
        MaterializedModeProfile::new("worker".into(), profile, &[], Arc::new(Security)).unwrap();
    assert!(allowed.is_tool_permitted("progress"));
    let disabled = MaterializedModeProfile::new(
        "worker".into(),
        profile,
        &["progress".into()],
        Arc::new(Security),
    )
    .unwrap();
    assert!(!disabled.is_tool_permitted("progress"));
    let thinker = MaterializedModeProfile::new(
        "thinker".into(),
        &configuration.runtime.profiles["thinker"],
        &[],
        Arc::new(Security),
    )
    .unwrap();
    assert!(!thinker.is_tool_permitted("progress"));
}
