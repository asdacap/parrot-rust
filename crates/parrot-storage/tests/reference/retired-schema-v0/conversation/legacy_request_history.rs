#[tokio::test]
async fn delegated_legacy_upgrade_request_history_keeps_single_original_tool_call() {
    let fixture = Fixture::new();
    std::fs::write(
        fixture.0.join("session.db"),
        include_bytes!("../database/fixtures/legacy-canonical-before.sqlite"),
    )
    .unwrap();
    let repo = fixture.repository();
    let mut duplicate = repo
        .events()
        .replay()
        .await
        .unwrap()
        .into_iter()
        .find(|event| event.id == "request")
        .unwrap();
    duplicate.id = "duplicate-request".into();
    assert!(matches!(
        repo.events().append_usage_fact(duplicate).await,
        Err(EventError::Invalid(_))
    ));
    let history = repo.agent_history("agent".into()).await.unwrap();
    let requests: Vec<_> = history
        .iter()
        .filter_map(|entry| match entry {
            AgentHistoryEntry::Request { tool_calls, .. } => Some(*tool_calls),
            _ => None,
        })
        .collect();
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0], 1);
    repo.events().database().close().await.unwrap();
}
