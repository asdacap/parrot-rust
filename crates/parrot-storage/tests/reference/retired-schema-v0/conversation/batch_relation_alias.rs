#[tokio::test]
async fn complete_message_group_without_terminal_and_duplicate_results_roll_back() {
    let fixture = Fixture::new();
    let repo = fixture.repository();
    let owner = assistant(&repo, "agent", "call", "read").await;
    let mut tool = message(MessageRole::Tool, "missing terminal");
    tool.tool_call_id = "call".into();
    let result = append(&repo, "agent", tool).await;
    repo.events()
        .transaction(move |tx| {
            tx.execute(
                "INSERT INTO tool_batch_result VALUES ('agent',?1,'call',?2)",
                rusqlite::params![owner, result],
            )?;
            Ok(())
        })
        .await
        .unwrap();
    assert!(
        matches!(fork(&repo,"agent","child",HistoryForkBoundary::AfterCompletedHistory,"full").await,Err(ForkError::Repository(EventError::Invalid(message))) if message=="a complete tool group has no terminal")
    );
    assert_eq!(row_counts(&repo, "child").await, vec![0; 8]);
    settle(&repo, "agent", owner, "call").await;
    let mut duplicate = message(MessageRole::Tool, "duplicate");
    duplicate.tool_call_id = "call".into();
    let duplicate = append(&repo, "agent", duplicate).await;
    repo.events()
        .transaction(move |tx| {
            tx.execute(
                "INSERT INTO tool_batch_result VALUES ('agent',?1,'alias',?2)",
                rusqlite::params![owner, duplicate],
            )?;
            Ok(())
        })
        .await
        .unwrap();
    assert!(
        matches!(fork(&repo,"agent","child",HistoryForkBoundary::AfterCompletedHistory,"full").await,Err(ForkError::Repository(EventError::Invalid(message))) if message=="Sequence contains more than one matching element")
    );
    assert_eq!(row_counts(&repo, "child").await, vec![0; 8]);
    repo.events().database().close().await.unwrap();
}
