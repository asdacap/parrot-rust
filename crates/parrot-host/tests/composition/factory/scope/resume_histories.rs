use super::*;
use parrot_domain::values::identifiers::UserSessionId;
use parrot_runtime::children::{CompletionDelivery, HistorySource, LaunchRequest};
use parrot_storage::conversation::{HistoryForkBoundary, HistoryForkSelection};

async fn finish_turn(scope: &Arc<Scope<Security>>, text: &str) {
    let runtime = scope.get_service::<RuntimeSession<Security>>().unwrap();
    let sent = bounded(runtime.send_text_message(text.into(), Arc::new(NeverCancelled)))
        .await
        .unwrap();
    bounded(sent.execution.join()).await.unwrap();
    assert!(runtime.is_idle());
}

#[tokio::test]
async fn actual_resume_repairs_all_completed_child_histories_without_reopening_children() {
    let fixture = Fixture::with_profiles(
        false,
        false,
        None,
        None,
        Arc::new(RoutingDiagnosticProfiles),
    )
    .await;
    let hosted = bounded(fixture.open()).await.unwrap();
    let root = fixture.root();
    let user = fixture.user();
    finish_turn(&root, "root history remains authoritative").await;
    let mut scopes = vec![root.clone()];
    for (index, name) in ["missing", "stale", "truncated"].into_iter().enumerate() {
        // The final child is nested under a completed child, not directly under root.
        let parent = if index == 2 { &scopes[1] } else { &root };
        let child = bounded(
            parent
                .spawner()
                .spawn(LaunchRequest {
                    parent: parent.session().clone(),
                    requested_name: name.into(),
                    requested_profile: "history-child".into(),
                    model: fixture.router.resolve("local/model").unwrap(),
                    requested_scope: String::new(),
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
        finish_turn(&child, &format!("durable child history {name}")).await;
        assert!(!child.session().is_active());
        scopes.push(child);
    }
    let events = bounded(user.lease.events.replay()).await.unwrap();
    let repository = root.history().repository();
    let mut expected = Vec::new();
    for scope in &scopes {
        let agent = scope.identity().session.session_id.clone();
        assert!(
            events
                .iter()
                .any(|event| { event.agent_session_id == agent && event.has_agent_finished() })
        );
        let names: Vec<_> = scope
            .identity()
            .name_path
            .iter()
            .map(String::as_str)
            .collect();
        let path = user.resources.agent_scratch(&names).unwrap().history_path();
        let bytes = std::fs::read(&path).unwrap();
        assert!(!bytes.is_empty());
        expected.push((
            agent.clone(),
            path,
            bytes,
            bounded(repository.agent_history(agent)).await.unwrap(),
        ));
    }
    let id = UserSessionId::parse(&hosted.snapshot().id).unwrap();
    bounded(hosted.close()).await.unwrap();
    std::fs::remove_file(&expected[1].1).unwrap();
    std::fs::write(&expected[2].1, vec![b'x'; expected[2].2.len()]).unwrap();
    std::fs::write(&expected[3].1, &expected[3].2[..expected[3].2.len() / 2]).unwrap();
    let constructions = fixture.observations.lock().unwrap().constructors;
    let requests = fixture.requests.lock().unwrap().len();

    let opened = bounded(fixture.store.resume(&id, true, false))
        .await
        .unwrap();
    let resumed = bounded(fixture.factory.bind_opened(opened)).await.unwrap();
    let resumed_root = fixture.root();
    assert_eq!(resumed.snapshot().id, id.value());
    assert_eq!(resumed_root.identity().session.session_id, expected[0].0);
    assert_eq!(
        fixture.observations.lock().unwrap().constructors,
        constructions + 1
    );
    assert_eq!(fixture.requests.lock().unwrap().len(), requests);
    assert_eq!(fixture.user().registry().unwrap().snapshot_roots().len(), 1);
    for (agent, path, bytes, history) in expected {
        assert_eq!(std::fs::read(&path).unwrap(), bytes, "projection {agent}");
        assert_eq!(
            bounded(
                resumed_root
                    .history()
                    .repository()
                    .agent_history(agent.clone())
            )
            .await
            .unwrap(),
            history,
            "durable history {agent} must not be rewritten during projection repair"
        );
    }
    bounded(resumed.close()).await.unwrap();
    std::fs::remove_dir_all(&fixture.root).unwrap();
}
