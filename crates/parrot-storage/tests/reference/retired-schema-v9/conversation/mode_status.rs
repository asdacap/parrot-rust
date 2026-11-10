use super::Fixture;
use parrot_domain::protocol::messages::parrot::Event;
use parrot_storage::conversation::{ConversationPart, PendingStatus};

#[tokio::test]
async fn initial_status_is_delivered_once_and_stays_consumed_after_reopen() {
    let fixture = Fixture::new();
    let repository = fixture.repository();
    let state = fixture.1.read_session_state().unwrap();
    assert_eq!(state.root_agent_session_id, "agent");
    assert_eq!(state.mode.mode, "initial");
    let initial = PendingStatus {
        mode: "initial".into(),
        version: 0,
    };
    assert_eq!(
        repository.pending_status("agent".into()).await.unwrap(),
        Some(initial.clone())
    );
    let published = Event {
        agent_session_id: "agent".into(),
        ..Default::default()
    };
    assert!(
        repository
            .append_status_prompt(published.clone(), initial.clone(), "initial status".into())
            .await
            .unwrap()
    );
    assert!(
        !repository
            .append_status_prompt(published, initial, "duplicate".into())
            .await
            .unwrap()
    );
    repository.events().database().close().await.unwrap();

    let reopened = fixture.repository();
    assert!(
        reopened
            .pending_status("agent".into())
            .await
            .unwrap()
            .is_none()
    );
    let messages = reopened.conversation("agent".into()).await.unwrap();
    assert_eq!(messages.len(), 1);
    assert_eq!(
        messages[0].message.parts,
        [ConversationPart::text("initial status")]
    );
    assert!(reopened.events().replay().await.unwrap().is_empty());
    reopened.events().database().close().await.unwrap();
}

#[tokio::test]
async fn local_revisions_reject_stale_status_and_projection_failure_keeps_it_pending() {
    let fixture = Fixture::new();
    let repository = fixture.repository();
    let initial = repository
        .pending_status("agent".into())
        .await
        .unwrap()
        .unwrap();
    fixture.1.update_mode("next").await.unwrap();
    let next = repository
        .pending_status("agent".into())
        .await
        .unwrap()
        .unwrap();
    assert_eq!((next.mode.as_str(), next.version), ("next", 1));
    fixture.1.update_mode("next").await.unwrap();
    let repeated = repository
        .pending_status("agent".into())
        .await
        .unwrap()
        .unwrap();
    assert_eq!((repeated.mode.as_str(), repeated.version), ("next", 2));
    let published = Event {
        agent_session_id: "agent".into(),
        ..Default::default()
    };
    for stale in [
        initial,
        next,
        PendingStatus {
            mode: "wrong".into(),
            version: repeated.version,
        },
    ] {
        assert!(
            !repository
                .append_status_prompt(published.clone(), stale, "stale".into())
                .await
                .unwrap()
        );
    }
    repository.events().transaction(|tx| {
        tx.execute_batch("CREATE TRIGGER fail_mode_projection BEFORE INSERT ON conversation_part BEGIN SELECT RAISE(ABORT, 'mode projection failure'); END;")?;
        Ok(())
    }).await.unwrap();
    assert!(
        repository
            .append_status_prompt(published.clone(), repeated.clone(), "failed".into())
            .await
            .is_err()
    );
    assert_eq!(
        repository.pending_status("agent".into()).await.unwrap(),
        Some(repeated.clone())
    );
    assert!(
        repository
            .conversation("agent".into())
            .await
            .unwrap()
            .is_empty()
    );
    repository
        .events()
        .transaction(|tx| {
            tx.execute_batch("DROP TRIGGER fail_mode_projection;")?;
            Ok(())
        })
        .await
        .unwrap();
    assert!(
        repository
            .append_status_prompt(published, repeated, "next status".into())
            .await
            .unwrap()
    );
    let messages = repository.conversation("agent".into()).await.unwrap();
    assert_eq!(messages.len(), 1);
    assert_eq!(
        messages[0].message.parts,
        [ConversationPart::text("next status")]
    );
    repository.events().database().close().await.unwrap();

    let reopened = fixture.repository();
    assert!(
        reopened
            .pending_status("agent".into())
            .await
            .unwrap()
            .is_none()
    );
    let state = fixture.1.read_session_state().unwrap();
    assert_eq!(state.root_agent_session_id, "agent");
    assert_eq!(state.mode.mode, "next");
    fixture.1.update_mode("last").await.unwrap();
    assert_eq!(
        reopened.pending_status("agent".into()).await.unwrap(),
        Some(PendingStatus {
            mode: "last".into(),
            version: 3
        })
    );
    reopened.events().database().close().await.unwrap();
}

#[tokio::test]
async fn missing_metadata_and_revision_overflow_do_not_change_the_mode() {
    let fixture = Fixture::new();
    let repository = fixture.repository();
    let path = fixture.1.metadata_path();
    let bytes = std::fs::read(path).unwrap();
    std::fs::remove_file(path).unwrap();
    assert!(fixture.1.update_mode("orphan").await.is_err());
    assert!(repository.pending_status("agent".into()).await.is_err());
    let mut metadata: parrot_storage::history_files::SessionMeta =
        serde_json::from_slice(&bytes).unwrap();
    metadata.mode_revision = Some(i64::MAX);
    std::fs::write(path, serde_json::to_vec(&metadata).unwrap()).unwrap();
    assert!(fixture.1.update_mode("overflowed").await.is_err());
    assert_eq!(
        repository.pending_status("agent".into()).await.unwrap(),
        Some(PendingStatus {
            mode: "initial".into(),
            version: i64::MAX
        })
    );
    assert_eq!(fixture.1.read_session_state().unwrap().mode.mode, "initial");
    assert!(repository.events().replay().await.unwrap().is_empty());
    repository.events().database().close().await.unwrap();
}

#[tokio::test]
async fn competing_status_appends_have_one_durable_winner() {
    let fixture = Fixture::new();
    let repository = fixture.repository();
    let expected = repository
        .pending_status("agent".into())
        .await
        .unwrap()
        .unwrap();
    let published = Event {
        agent_session_id: "agent".into(),
        ..Default::default()
    };
    let (left, right) = tokio::join!(
        repository.append_status_prompt(published.clone(), expected.clone(), "left".into()),
        repository.append_status_prompt(published, expected, "right".into()),
    );
    let left = left.unwrap();
    let right = right.unwrap();
    assert_ne!(left, right);
    let messages = repository.conversation("agent".into()).await.unwrap();
    assert_eq!(messages.len(), 1);
    assert_eq!(
        messages[0].message.parts,
        [ConversationPart::text(if left { "left" } else { "right" })]
    );
    assert!(
        repository
            .pending_status("agent".into())
            .await
            .unwrap()
            .is_none()
    );
    repository.events().database().close().await.unwrap();
}

#[tokio::test]
async fn returning_to_a_mode_does_not_revive_its_old_status_token() {
    let fixture = Fixture::new();
    let repository = fixture.repository();
    let initial = repository
        .pending_status("agent".into())
        .await
        .unwrap()
        .unwrap();
    fixture.1.update_mode("next").await.unwrap();
    fixture.1.update_mode("initial").await.unwrap();
    assert!(
        !repository
            .append_status_prompt(
                Event {
                    agent_session_id: "agent".into(),
                    ..Default::default()
                },
                initial,
                "stale".into(),
            )
            .await
            .unwrap()
    );
    assert_eq!(
        repository.pending_status("agent".into()).await.unwrap(),
        Some(PendingStatus {
            mode: "initial".into(),
            version: 2
        })
    );
    assert!(
        repository
            .conversation("agent".into())
            .await
            .unwrap()
            .is_empty()
    );
    repository.events().database().close().await.unwrap();
}

#[tokio::test]
async fn conversation_rejects_metadata_from_another_selected_root() {
    let fixture = Fixture::new();
    let other = Fixture::new();
    let repository = fixture.repository();
    assert!(matches!(
        parrot_storage::conversation::ConversationRepository::new(
            repository.events().clone(),
            other.1.clone()
        ),
        Err(parrot_storage::events::EventError::Argument(_))
    ));
    repository.events().database().close().await.unwrap();
}
