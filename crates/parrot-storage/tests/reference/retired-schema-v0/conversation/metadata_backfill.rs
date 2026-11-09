#[tokio::test]
async fn malformed_conversation_metadata_and_missing_item_are_observable_without_partial_projection()
 {
    let fixture = Fixture::new();
    let repo = fixture.repository();
    repo.events().transaction(|tx| { tx.execute("INSERT INTO message (agent_session, role, content, created_at) VALUES ('agent', 'unexpected', 'legacy', 'at')", [])?; Ok(()) }).await.unwrap();
    assert!(
        matches!(repo.conversation("agent".into()).await, Err(EventError::Invalid(message)) if message == "unknown conversation role unexpected")
    );
    let counts = repo
        .events()
        .transaction(|tx| {
            Ok((
                tx.query_row("SELECT COUNT(*) FROM conversation_item", [], |row| {
                    row.get::<_, i64>(0)
                })?,
                tx.query_row(
                    "SELECT COUNT(*) FROM projection_version WHERE name = 'conversation'",
                    [],
                    |row| row.get::<_, i64>(0),
                )?,
            ))
        })
        .await
        .unwrap();
    assert_eq!(counts, (0, 0));
    assert!(
        matches!(repo.events().transaction(|tx| parrot_storage::conversation::reads::read_conversation_item(tx, 42)).await, Err(EventError::Invalid(message)) if message == "Conversation item 42 does not exist.")
    );
    repo.events().database().close().await.unwrap();
}
