use super::*;
use integrated_adapters::{ChildProfiles, reply, script};
use parrot_domain::values::{
    identifiers::UserSessionId,
    progress::{
        PROGRESS_FILE_NAME, PROGRESS_INVENTORY_KIND, Progress, ProgressSnapshot, ProgressUpdate,
    },
};
use parrot_runtime::{
    active_work::progress::AgentProgress,
    children::{CompletionDelivery, HistorySource, LaunchRequest},
};
use parrot_storage::conversation::{HistoryForkBoundary, HistoryForkSelection};
use tokio::sync::Semaphore;

#[tokio::test]
async fn progress_blocks_partial_and_full_completion_until_explicit_close() {
    for steps in [2_u64, 5] {
        let path = {
            let fixture = Fixture::new(false, false).await;
            let hosted = fixture.open().await.unwrap();
            let root = fixture.root();
            let gate = Arc::new(Semaphore::new(0));
            let mut start = reply("");
            start.tool_calls.push(parrot_provider::sessions::ToolCall {
                id: "acceptance-progress-start".into(),
                name: "progress".into(),
                arguments_json:
                    json!({"steps":steps,"total_steps":5,"current_step":"Acceptance work"})
                        .to_string(),
            });
            script(&fixture, "progress-acceptance-marker", start, None);
            script(
                &fixture,
                "progress-acceptance-marker",
                reply("Premature completion must be retained"),
                None,
            );
            let mut close = reply("");
            close.tool_calls.push(parrot_provider::sessions::ToolCall {
                id: "acceptance-progress-close".into(),
                name: "progress".into(),
                arguments_json: json!({"close":true}).to_string(),
            });
            script(
                &fixture,
                "progress-acceptance-marker",
                close,
                Some(gate.clone()),
            );
            script(
                &fixture,
                "progress-acceptance-marker",
                reply("Accepted progress completion"),
                None,
            );
            let session = root.get_service::<RuntimeSession<Security>>().unwrap();
            let sent = bounded(send_text(&session, "progress-acceptance-marker"))
                .await
                .unwrap();
            bounded(async {
                loop {
                    if fixture.requests.lock().unwrap().len() >= 3 {
                        break;
                    }
                    tokio::task::yield_now().await;
                }
            })
            .await;
            let reminder = fixture
                .user()
                .templates
                .render(
                    "agent-session.progress-reminder",
                    &[
                        ("steps".into(), steps.to_string().into()),
                        ("total_steps".into(), "5".into()),
                        ("current_step".into(), "Acceptance work".into()),
                    ],
                    &NeverCancelled,
                )
                .unwrap();
            {
                let requests = fixture.requests.lock().unwrap();
                assert_eq!(requests.len(), 3);
                assert!(
                    requests[2]
                        .messages
                        .iter()
                        .any(|message| message.content().contains(&reminder))
                );
            }
            let progress = root.get_service::<AgentProgress>().unwrap();
            assert_eq!(
                progress
                    .capture_snapshot()
                    .unwrap()
                    .progress
                    .as_ref()
                    .unwrap()
                    .steps,
                steps
            );
            let events = fixture.user().lease.events.replay().await.unwrap();
            assert!(events.iter().any(|event| event.has_agent_notice()
                && event.agent_notice().kind == "active_work_reminder"));
            assert_eq!(
                events
                    .iter()
                    .filter(|event| event.has_turn_ended()
                        && event.agent_session_id == root.identity().session.session_id)
                    .count(),
                0
            );
            gate.add_permits(1);
            let completed = bounded(sent.execution.join()).await.unwrap();
            assert_eq!(completed.output, "Accepted progress completion");
            assert_eq!(fixture.requests.lock().unwrap().len(), 4);
            let events = fixture.user().lease.events.replay().await.unwrap();
            assert_eq!(
                events
                    .iter()
                    .filter(|event| event.has_turn_ended()
                        && event.agent_session_id == root.identity().session.session_id)
                    .count(),
                1
            );
            assert_eq!(progress.capture_snapshot().unwrap().progress, None);
            let names = root
                .identity()
                .name_path
                .iter()
                .map(String::as_str)
                .collect::<Vec<_>>();
            let file = fixture
                .user()
                .resources
                .agent_scratch(&names)
                .unwrap()
                .root()
                .join(PROGRESS_FILE_NAME);
            let saved: ProgressSnapshot =
                serde_json::from_slice(&std::fs::read(file).unwrap()).unwrap();
            assert_eq!(saved.progress, None);
            assert_eq!(saved.revision, 2);
            bounded(hosted.close()).await.unwrap();
            fixture.root.clone()
        };
        std::fs::remove_dir_all(path).unwrap();
    }
}

#[tokio::test]
async fn main_and_child_progress_restore_independent_files_snapshots_and_blockers() {
    let path = {
        let fixture =
            Fixture::with_profiles(false, false, None, None, Arc::new(ChildProfiles)).await;
        let hosted = fixture.open().await.unwrap();
        let root = fixture.root();
        let child = bounded(
            root.spawner()
                .spawn(LaunchRequest {
                    parent: root.session().clone(),
                    requested_name: "saved-worker".into(),
                    requested_profile: "adapter".into(),
                    model: fixture.router.resolve("local/model").unwrap(),
                    requested_scope: "Retained progress owner".into(),
                    delivery: CompletionDelivery::RetainedOnly,
                    boundary: HistoryForkBoundary::AfterCompletedHistory,
                    fork: HistoryForkSelection::parse("empty"),
                    history_source: HistorySource::Parent,
                })
                .unwrap()
                .join(),
        )
        .await
        .unwrap();
        let files = [&root, &child].map(|scope| {
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
        });
        assert_ne!(files[0], files[1]);
        assert!(files.iter().all(|file| !file.exists()));
        for (scope, steps, total_steps, label) in [
            (&root, 1, 4, "Root saved work"),
            (&child, 3, 5, "Child saved work"),
        ] {
            scope
                .get_service::<AgentProgress>()
                .unwrap()
                .update(ProgressUpdate::Replace(Progress {
                    steps,
                    total_steps,
                    current_step: Some(label.into()),
                }))
                .await
                .unwrap();
        }
        let saved = files.each_ref().map(|file| std::fs::read(file).unwrap());
        let root_saved: ProgressSnapshot = serde_json::from_slice(&saved[0]).unwrap();
        let child_saved: ProgressSnapshot = serde_json::from_slice(&saved[1]).unwrap();
        assert_eq!(
            root_saved.owner_session_id,
            root.identity().session.session_id
        );
        assert_eq!(
            child_saved.owner_session_id,
            child.identity().session.session_id
        );
        assert_ne!(root_saved.owner_session_id, child_saved.owner_session_id);
        assert_eq!(root_saved.progress.as_ref().unwrap().steps, 1);
        assert_eq!(child_saved.progress.as_ref().unwrap().steps, 3);
        assert!(fixture.requests.lock().unwrap().is_empty());
        let id = UserSessionId::parse(&hosted.snapshot().id).unwrap();
        bounded(hosted.close()).await.unwrap();
        let opened = bounded(fixture.store.resume(&id, true, false))
            .await
            .unwrap();
        let resumed = bounded(fixture.factory.bind_opened(opened)).await.unwrap();
        let restored_root = fixture.root();
        let restored_child = restored_root.children().resolve("saved-worker").unwrap();
        assert_eq!(
            restored_root.identity().session.session_id,
            root_saved.owner_session_id
        );
        assert_eq!(
            restored_child.identity().session.session_id,
            child_saved.owner_session_id
        );
        for scope in [&restored_root, &restored_child] {
            assert!(
                scope
                    .get_service::<AgentSessionServices<Security>>()
                    .unwrap()
                    .active_work
                    .build()
                    .await
                    .unwrap()
                    .is_some()
            );
        }
        let mut stream = resumed.listen(None, Arc::new(NeverCancelled)).unwrap();
        let snapshots = bounded(async {
            let mut snapshots = BTreeMap::new();
            while snapshots.len() < 2 {
                let event = stream.next(&NeverCancelled).await.unwrap().unwrap();
                if event.has_tool_snapshot()
                    && event.tool_snapshot().kind == PROGRESS_INVENTORY_KIND
                {
                    let snapshot: ProgressSnapshot =
                        serde_json::from_slice(&event.tool_snapshot().data).unwrap();
                    assert_eq!(event.agent_session_id, snapshot.owner_session_id);
                    assert_eq!(
                        event.tool_snapshot().owner_agent_session_id,
                        snapshot.owner_session_id
                    );
                    assert_eq!(event.tool_snapshot().revision, snapshot.revision);
                    snapshots.insert(snapshot.owner_session_id.clone(), snapshot);
                }
            }
            snapshots
        })
        .await;
        assert_eq!(snapshots[&root_saved.owner_session_id], root_saved);
        assert_eq!(snapshots[&child_saved.owner_session_id], child_saved);
        assert_eq!(
            files.each_ref().map(|file| std::fs::read(file).unwrap()),
            saved
        );
        assert!(fixture.requests.lock().unwrap().is_empty());
        restored_root
            .get_service::<AgentProgress>()
            .unwrap()
            .update(ProgressUpdate::Close)
            .await
            .unwrap();
        assert_eq!(
            restored_root
                .get_service::<AgentSessionServices<Security>>()
                .unwrap()
                .active_work
                .build()
                .await
                .unwrap(),
            None
        );
        assert!(
            restored_child
                .get_service::<AgentSessionServices<Security>>()
                .unwrap()
                .active_work
                .build()
                .await
                .unwrap()
                .is_some()
        );
        drop(stream);
        bounded(resumed.close()).await.unwrap();
        let opened = bounded(fixture.store.resume(&id, true, false))
            .await
            .unwrap();
        let resumed = bounded(fixture.factory.bind_opened(opened)).await.unwrap();
        let restored_root = fixture.root();
        let restored_child = restored_root.children().resolve("saved-worker").unwrap();
        assert_eq!(
            restored_root
                .get_service::<AgentProgress>()
                .unwrap()
                .capture_snapshot()
                .unwrap()
                .progress,
            None
        );
        assert_eq!(
            restored_child
                .get_service::<AgentProgress>()
                .unwrap()
                .capture_snapshot()
                .unwrap(),
            child_saved
        );
        assert!(fixture.requests.lock().unwrap().is_empty());
        restored_child
            .get_service::<AgentProgress>()
            .unwrap()
            .update(ProgressUpdate::Close)
            .await
            .unwrap();
        bounded(resumed.close()).await.unwrap();
        fixture.root.clone()
    };
    std::fs::remove_dir_all(path).unwrap();
}
