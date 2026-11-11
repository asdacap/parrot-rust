use super::Fixture;
use parrot_core::{StatusKey, StatusStanding};
use parrot_domain::protocol::messages::parrot::Event;
use parrot_storage::conversation::{ConversationPart, HistoryForkBoundary, HistoryForkSelection};

fn standing(entries: &[(&str, &str)]) -> StatusStanding {
    entries
        .iter()
        .map(|(slot, hash)| (StatusKey::new(*slot), (*hash).into()))
        .collect()
}

fn event() -> Event {
    Event {
        agent_session_id: "agent".into(),
        ..Default::default()
    }
}

#[tokio::test]
async fn first_assembly_stands_and_duplicate_appends_stay_visible() {
    let fixture = Fixture::new();
    let repository = fixture.repository();
    assert!(
        repository
            .standing_statuses("agent".into())
            .await
            .unwrap()
            .is_none()
    );
    repository
        .append_status_assembly(
            event(),
            standing(&[("runtime:selection", "h1")]),
            "first".into(),
        )
        .await
        .unwrap();
    repository
        .append_status_assembly(
            event(),
            standing(&[("runtime:selection", "h1")]),
            "second".into(),
        )
        .await
        .unwrap();
    let messages = repository.conversation("agent".into()).await.unwrap();
    assert_eq!(messages.len(), 2);
    assert_eq!(messages[0].message.parts, [ConversationPart::text("first")]);
    assert_eq!(
        messages[1].message.parts,
        [ConversationPart::text("second")]
    );
    assert_eq!(
        repository.standing_statuses("agent".into()).await.unwrap(),
        Some(standing(&[("runtime:selection", "h1")]))
    );
    repository.events().database().close().await.unwrap();

    let reopened = fixture.repository();
    assert_eq!(
        reopened.standing_statuses("agent".into()).await.unwrap(),
        Some(standing(&[("runtime:selection", "h1")]))
    );
    assert_eq!(
        reopened.conversation("agent".into()).await.unwrap().len(),
        2
    );
    assert!(reopened.events().replay().await.unwrap().is_empty());
    reopened.events().database().close().await.unwrap();
}

#[tokio::test]
async fn partial_status_text_preserves_unchanged_identities_after_reopen() {
    let fixture = Fixture::new();
    let repository = fixture.repository();
    repository
        .append_status_assembly(
            event(),
            standing(&[("profile", "p1"), ("tasks", ""), ("time", "t1")]),
            "profile\n\ntasks\n\ntime one".into(),
        )
        .await
        .unwrap();
    let updated = standing(&[("profile", "p1"), ("tasks", ""), ("time", "t2")]);
    repository
        .append_status_assembly(event(), updated.clone(), "time two".into())
        .await
        .unwrap();
    repository.events().database().close().await.unwrap();

    let reopened = fixture.repository();
    let restored = reopened
        .standing_statuses("agent".into())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(restored, updated);
    let messages = reopened.conversation("agent".into()).await.unwrap();
    assert_eq!(
        messages.last().unwrap().message.parts,
        [ConversationPart::text("time two")]
    );
    let offering = restored
        .iter()
        .map(|(key, hash)| {
            (
                key.clone(),
                parrot_core::StatusOffer {
                    hash: hash.clone(),
                    text: key.to_string(),
                    is_auto_update: true,
                },
            )
        })
        .collect();
    assert!(!parrot_core::status_steering_needed(&offering, &restored));
    reopened.events().database().close().await.unwrap();
}

#[tokio::test]
async fn standing_follows_the_latest_record() {
    let fixture = Fixture::new();
    let repository = fixture.repository();
    repository
        .append_status_assembly(event(), standing(&[("a", "1")]), "first".into())
        .await
        .unwrap();
    repository
        .append_status_assembly(
            event(),
            standing(&[("a", "2"), ("b", "3")]),
            "second".into(),
        )
        .await
        .unwrap();
    assert_eq!(
        repository.standing_statuses("agent".into()).await.unwrap(),
        Some(standing(&[("a", "2"), ("b", "3")]))
    );
    repository.events().database().close().await.unwrap();
}

#[tokio::test]
async fn concurrent_assemblies_leave_a_coherent_standing_map() {
    let fixture = Fixture::new();
    let repository = fixture.repository();
    let (left, right) = tokio::join!(
        repository.append_status_assembly(event(), standing(&[("left:slot", "l1")]), "left".into()),
        repository.append_status_assembly(
            event(),
            standing(&[("right:slot", "r1")]),
            "right".into()
        ),
    );
    left.unwrap();
    right.unwrap();
    let messages = repository.conversation("agent".into()).await.unwrap();
    assert_eq!(messages.len(), 2);
    let records = repository
        .events()
        .transaction(|tx| {
            let mut read = tx.prepare(
                "SELECT status_slots FROM conversation_item WHERE status_slots IS NOT NULL ORDER BY sequence;",
            )?;
            Ok(read
                .query_map([], |row| row.get::<_, String>(0))?
                .collect::<Result<Vec<_>, _>>()?)
        })
        .await
        .unwrap();
    assert_eq!(records.len(), 2);
    let latest: std::collections::BTreeMap<String, String> =
        serde_json::from_str(records.last().unwrap()).unwrap();
    let expected = match latest.keys().next().map(String::as_str) {
        Some("left:slot") => standing(&[("left:slot", "l1")]),
        _ => standing(&[("right:slot", "r1")]),
    };
    assert_eq!(
        repository.standing_statuses("agent".into()).await.unwrap(),
        Some(expected)
    );
    repository.events().database().close().await.unwrap();
}

#[tokio::test]
async fn forked_history_carries_the_standing_trail() {
    let fixture = Fixture::new();
    let repository = fixture.repository();
    repository
        .append_status_assembly(
            event(),
            standing(&[("runtime:selection", "h1")]),
            "mode status".into(),
        )
        .await
        .unwrap();
    repository
        .initialize_forked_agent_history(
            "agent".into(),
            "child".into(),
            HistoryForkBoundary::AfterCompletedHistory,
            HistoryForkSelection::parse("full"),
            None,
        )
        .await
        .unwrap();
    let child = repository.conversation("child".into()).await.unwrap();
    assert_eq!(child.len(), 1);
    assert_eq!(
        child[0].message.parts,
        [ConversationPart::text("mode status")]
    );
    assert_eq!(
        repository.standing_statuses("child".into()).await.unwrap(),
        Some(standing(&[("runtime:selection", "h1")]))
    );
    repository.events().database().close().await.unwrap();
}

#[tokio::test]
async fn malformed_status_record_is_an_explicit_read_error() {
    let fixture = Fixture::new();
    let repository = fixture.repository();
    repository
        .events()
        .transaction(|tx| {
            tx.execute(
                "INSERT INTO conversation_item(agent_session,origin,role,tool_call_id,created_at,status_slots) VALUES('agent','system','system','','at','[\"bad\"]')",
                [],
            )?;
            Ok(())
        })
        .await
        .unwrap();
    assert!(matches!(
        repository.standing_statuses("agent".into()).await,
        Err(parrot_storage::events::EventError::Invalid(_))
    ));
    repository.events().database().close().await.unwrap();
}

#[tokio::test]
async fn compaction_retains_the_standing_trail() {
    let fixture = Fixture::new();
    let repository = fixture.repository();
    repository
        .append_status_assembly(
            event(),
            standing(&[("runtime:selection", "h1")]),
            "status".into(),
        )
        .await
        .unwrap();
    repository
        .save_compaction(
            "agent".into(),
            parrot_storage::conversation::CompactionSnapshot {
                summary: "summary".into(),
                watermark: 1,
            },
        )
        .await
        .unwrap();
    repository.events().database().close().await.unwrap();
    let reopened = fixture.repository();
    assert_eq!(
        reopened.standing_statuses("agent".into()).await.unwrap(),
        Some(standing(&[("runtime:selection", "h1")]))
    );
    reopened.events().database().close().await.unwrap();
}

#[tokio::test]
async fn mode_updates_do_not_disturb_status_standing() {
    let fixture = Fixture::new();
    let repository = fixture.repository();
    repository
        .append_status_assembly(
            event(),
            standing(&[("runtime:selection", "h1")]),
            "status".into(),
        )
        .await
        .unwrap();
    fixture.1.update_mode("next").await.unwrap();
    assert_eq!(
        repository.standing_statuses("agent".into()).await.unwrap(),
        Some(standing(&[("runtime:selection", "h1")]))
    );
    assert_eq!(fixture.1.read_session_state().unwrap().mode.mode, "next");
    repository.events().database().close().await.unwrap();
}
