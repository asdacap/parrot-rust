use super::Fixture;
use parrot_domain::protocol::messages::parrot::Event;
use parrot_storage::conversation::{
    CompactionSnapshot, HistoryForkBoundary, HistoryForkSelection, PendingStatus,
};
use parrot_storage::database::Connection;
use parrot_storage::events::NeverCancelled;
use std::future::Future;
use std::task::Poll;
use std::time::{Duration, Instant};

#[tokio::test]
async fn cancelled_status_observer_keeps_mode_serialized_through_sql_commit() {
    let fixture = Fixture::new();
    let repository = fixture.repository();
    let expected = repository
        .pending_status("agent".into())
        .await
        .unwrap()
        .unwrap();
    let reader = Connection::open(fixture.0.join("session.db")).unwrap();
    reader.execute_batch("BEGIN;").unwrap();
    let before: i64 = reader
        .query_row("SELECT count(*) FROM conversation_item;", [], |row| {
            row.get(0)
        })
        .unwrap();
    assert_eq!(before, 0);
    let mut append = Box::pin(repository.append_status_prompt(
        Event {
            agent_session_id: "agent".into(),
            ..Default::default()
        },
        expected,
        "old mode status".into(),
    ));
    let poll = std::future::poll_fn(|cx| Poll::Ready(append.as_mut().poll(cx))).await;
    assert!(poll.is_pending());
    drop(append);

    let observer = Connection::open(fixture.0.join("session.db")).unwrap();
    observer.busy_timeout(Duration::ZERO).unwrap();
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        match observer.query_row("SELECT count(*) FROM conversation_item;", [], |row| {
            row.get::<_, i64>(0)
        }) {
            Err(rusqlite::Error::SqliteFailure(error, _))
                if error.code == rusqlite::ErrorCode::DatabaseBusy =>
            {
                break;
            }
            Ok(count) => assert_eq!(count, 0),
            Err(error) => panic!("failed to observe pending commit: {error}"),
        }
        assert!(
            Instant::now() < deadline,
            "status transaction did not reach its reader-blocked commit"
        );
        std::thread::yield_now();
    }

    let mut update = Box::pin(fixture.1.update_mode("next"));
    let poll = std::future::poll_fn(|cx| Poll::Ready(update.as_mut().poll(cx))).await;
    assert!(
        poll.is_pending(),
        "mode changed while the old status SQL commit was blocked"
    );
    assert_eq!(
        fixture.1.read_session_state().unwrap().mode,
        PendingStatus {
            mode: "initial".into(),
            version: 0
        }
    );
    reader.execute_batch("ROLLBACK;").unwrap();
    update.await.unwrap();
    let committed: (i64, String) = observer.query_row(
        "SELECT item.mode_status_revision, part.text FROM conversation_item AS item JOIN conversation_part AS part ON part.item_sequence = item.sequence;",
        [],
        |row| Ok((row.get(0)?, row.get(1)?)),
    ).unwrap();
    assert_eq!(committed, (0, "old mode status".into()));
    assert_eq!(
        repository.pending_status("agent".into()).await.unwrap(),
        Some(PendingStatus {
            mode: "next".into(),
            version: 1
        })
    );
    repository.events().database().close().await.unwrap();
}

#[tokio::test]
async fn pending_lookup_keeps_its_json_snapshot_coherent_with_consumption() {
    let fixture = Fixture::new();
    let repository = fixture.repository();
    let initial = repository
        .pending_status("agent".into())
        .await
        .unwrap()
        .unwrap();
    repository
        .append_status_prompt(
            Event {
                agent_session_id: "agent".into(),
                ..Default::default()
            },
            initial,
            "initial".into(),
        )
        .await
        .unwrap();
    let (entered_send, entered) = tokio::sync::oneshot::channel();
    let (release, blocked) = std::sync::mpsc::channel();
    let ticket = repository
        .events()
        .admit("blocked", &NeverCancelled, move |_| {
            let _ = entered_send.send(());
            blocked.recv().unwrap();
            Ok(())
        })
        .unwrap();
    entered.await.unwrap();
    let mut pending = Box::pin(repository.pending_status("agent".into()));
    let poll = std::future::poll_fn(|cx| Poll::Ready(pending.as_mut().poll(cx))).await;
    assert!(poll.is_pending());
    let mut update = Box::pin(fixture.1.update_mode("next"));
    let poll = std::future::poll_fn(|cx| Poll::Ready(update.as_mut().poll(cx))).await;
    assert!(poll.is_pending());
    release.send(()).unwrap();
    ticket.into_completion().await.unwrap();
    assert_eq!(pending.await.unwrap(), None);
    update.await.unwrap();
    assert_eq!(
        repository.pending_status("agent".into()).await.unwrap(),
        Some(PendingStatus {
            mode: "next".into(),
            version: 1
        })
    );
    repository.events().database().close().await.unwrap();
}

#[tokio::test]
async fn compaction_retains_consumption_and_forks_copy_messages_without_consumption() {
    let fixture = Fixture::new();
    let repository = fixture.repository();
    let expected = repository
        .pending_status("agent".into())
        .await
        .unwrap()
        .unwrap();
    repository
        .append_status_prompt(
            Event {
                agent_session_id: "agent".into(),
                ..Default::default()
            },
            expected,
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
        [parrot_storage::conversation::ConversationPart::text(
            "mode status"
        )]
    );
    let revisions = repository.events().transaction(|tx| {
        let mut read = tx.prepare("SELECT agent_session, mode_status_revision FROM conversation_item ORDER BY sequence;")?;
        Ok(read.query_map([], |row| Ok((row.get::<_, String>(0)?, row.get::<_, Option<i64>>(1)?)))?.collect::<Result<Vec<_>, _>>()?)
    }).await.unwrap();
    assert_eq!(
        revisions,
        [("agent".into(), Some(0)), ("child".into(), None)]
    );
    repository
        .save_compaction(
            "agent".into(),
            CompactionSnapshot {
                summary: "summary".into(),
                watermark: 1,
            },
        )
        .await
        .unwrap();
    repository.events().database().close().await.unwrap();
    let reopened = fixture.repository();
    assert_eq!(reopened.pending_status("agent".into()).await.unwrap(), None);
    assert_eq!(reopened.pending_status("child".into()).await.unwrap(), None);
    reopened.events().database().close().await.unwrap();
}

#[tokio::test]
async fn metadata_revision_rewind_is_an_explicit_error() {
    let fixture = Fixture::new();
    let repository = fixture.repository();
    fixture.1.update_mode("next").await.unwrap();
    let expected = repository
        .pending_status("agent".into())
        .await
        .unwrap()
        .unwrap();
    repository
        .append_status_prompt(
            Event {
                agent_session_id: "agent".into(),
                ..Default::default()
            },
            expected,
            "next".into(),
        )
        .await
        .unwrap();
    let path = fixture.1.metadata_path();
    let mut metadata: parrot_storage::history_files::SessionMeta =
        serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap();
    metadata.mode_revision = Some(0);
    std::fs::write(path, serde_json::to_vec(&metadata).unwrap()).unwrap();
    assert!(
        matches!(repository.pending_status("agent".into()).await, Err(parrot_storage::events::EventError::Invalid(message)) if message.contains("consumed mode revision"))
    );
    assert!(
        repository
            .append_status_prompt(
                Event {
                    agent_session_id: "agent".into(),
                    ..Default::default()
                },
                PendingStatus {
                    mode: "next".into(),
                    version: 0
                },
                "rewound".into(),
            )
            .await
            .is_err()
    );
    assert_eq!(
        repository.conversation("agent".into()).await.unwrap().len(),
        1
    );
    repository.events().database().close().await.unwrap();
}
