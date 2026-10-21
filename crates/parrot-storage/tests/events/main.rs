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
async fn raw_cost_messages_preserve_nan_bits_without_partial_eq_rejection() {
    use parrot_domain::protocol::messages::parrot::{
        AgentStatisticsUpdatedEvent, RequestUsageRecorded, SessionUsageSnapshot,
    };
    use protobuf::Message;
    let fixture = Fixture::new();
    let repository = fixture.repository();
    let nan = f64::from_bits(0x7ff8_0000_0000_0001);
    for kind in 0..3 {
        let mut event = Event::new();
        event.id = format!("nan-{kind}");
        event.agent_session_id = "agent".into();
        match kind {
            0 => event.set_request_usage_recorded(RequestUsageRecorded {
                input_cost: nan,
                ..Default::default()
            }),
            1 => event.set_agent_statistics_updated(AgentStatisticsUpdatedEvent {
                input_cost: nan,
                ..Default::default()
            }),
            _ => event.set_session_usage_snapshot(SessionUsageSnapshot {
                input_cost: nan,
                ..Default::default()
            }),
        }
        let bytes = event.write_to_bytes().unwrap();
        let expected = bytes.clone();
        let stored = repository
            .transaction(move |tx| {
                parrot_storage::events::record(
                    tx,
                    &event,
                    Some(&bytes),
                    "2026-01-02T03:04:05.0000000+00:00",
                )
            })
            .await
            .unwrap();
        assert_eq!(stored.raw, expected);
    }
    assert_eq!(repository.replay_raw().await.unwrap().len(), 3);
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
    let expected = raw.clone();
    repository
        .transaction(move |tx| {
            parrot_storage::events::record(
                tx,
                &event,
                Some(&raw),
                "2026-01-02T03:04:05.0000000+00:00",
            )
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
