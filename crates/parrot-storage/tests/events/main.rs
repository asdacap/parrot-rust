mod inputs;
mod projection;
mod runtime_statistics;
mod statistics;
mod terminal;
mod tools;
mod upstream_inputs;
mod upstream_storage;

use parrot_domain::contracts::paths::{RootOrigin, SelectedSessionRoot};
use parrot_domain::protocol::messages::parrot::Event;
use parrot_storage::database::SessionDatabase;
use parrot_storage::events::{EventRepository, NeverCancelled};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

static NEXT: AtomicU64 = AtomicU64::new(0);
struct Fixture(PathBuf);
impl Fixture {
    fn image(
        artifact_id: impl Into<String>,
        media_type: impl Into<String>,
        display_name: impl Into<String>,
    ) -> parrot_storage::conversation::ConversationPart {
        let artifact_id = artifact_id.into();
        parrot_storage::conversation::ConversationPart::image(
            parrot_storage::artifacts::ImageArtifactMetadata {
                sha256: artifact_id.clone(),
                artifact_id,
                media_type: media_type.into(),
                display_name: display_name.into(),
                byte_length: 70,
                width: 24,
                height: 12,
                frame_count: 3,
                aggregate_pixels: 864,
                origin: "test".into(),
            },
        )
    }
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!(
            "parrot-t17-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&path).unwrap();
        Self(path)
    }
    fn root(&self) -> SelectedSessionRoot {
        SelectedSessionRoot::new(self.0.clone(), RootOrigin::Current).unwrap()
    }
    fn repository(&self) -> EventRepository {
        EventRepository::new(SessionDatabase::open(self.root()).unwrap())
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

#[tokio::test]
async fn replay_after_cursor_returns_the_suffix_and_unknown_cursor_returns_everything() {
    let fixture = Fixture::new();
    let repository = fixture.repository();
    for id in ["before", "cursor", "after", "later"] {
        let mut event = Event::new();
        event.id = id.into();
        event.agent_session_id = "agent".into();
        repository
            .transaction(move |tx| {
                parrot_storage::events::record(tx, &event, "2026-01-02T03:04:05Z")
            })
            .await
            .unwrap();
    }
    let all = repository.replay().await.unwrap();
    assert_eq!(
        all.iter()
            .map(|event| event.id.as_str())
            .collect::<Vec<_>>(),
        ["before", "cursor", "after", "later"]
    );
    let suffix = repository.replay_after("cursor").await.unwrap();
    assert_eq!(
        suffix
            .iter()
            .map(|event| event.id.as_str())
            .collect::<Vec<_>>(),
        ["after", "later"]
    );
    assert_eq!(repository.replay_after("missing").await.unwrap(), all);
    assert_eq!(repository.replay_after("").await.unwrap(), all);
    repository.database().close().await.unwrap();
}

#[tokio::test]
async fn actor_repository_errors_roll_back_and_replay_retains_exact_raw_bytes() {
    use protobuf::Message;
    let fixture = Fixture::new();
    let repository = fixture.repository();
    let mut event = Event::new();
    event.id = "raw-event".into();
    event.agent_session_id = "agent".into();
    let mut raw = event.write_to_bytes().unwrap();
    raw.extend_from_slice(&[0xa0, 0x06, 0x07]);
    let event = Event::parse_from_bytes(&raw).unwrap();
    let expected = raw;
    repository
        .transaction(move |tx| {
            parrot_storage::events::record(tx, &event, "2026-01-02T03:04:05.0000000+00:00")
        })
        .await
        .unwrap();
    let ticket = repository
        .admit("rollback", &NeverCancelled, |tx| {
            tx.execute("DELETE FROM event", [])?;
            Err::<(), _>(parrot_storage::events::EventError::Conflict(
                "conflict".into(),
            ))
        })
        .unwrap();
    assert!(
        matches!(ticket.into_completion().await,Err(parrot_storage::events::EventError::Conflict(message)) if message=="conflict")
    );
    let rows = repository.replay_raw().await.unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].raw, expected);
    repository.database().close().await.unwrap();
}
