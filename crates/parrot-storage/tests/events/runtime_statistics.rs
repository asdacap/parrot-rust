use super::Fixture;
use parrot_domain::protocol::messages::parrot::{AgentStarted, Event, RequestUsageRecorded};
use parrot_domain::values::accounting::{AgentUsageIncrement, AgentUsageKey, AgentUsageTotals};
use parrot_storage::events::{EventRepository, NeverCancelled};
use std::sync::{Arc, Mutex, mpsc};

fn request(id: &str) -> Event {
    let mut event = Event::new();
    event.id = id.into();
    event.agent_session_id = "root".into();
    event.set_request_usage_recorded(RequestUsageRecorded {
        request_id: id.into(),
        provider: "p".into(),
        model: "m".into(),
        input_tokens: 17,
        cached_input_tokens: 3,
        output_tokens: 9,
        context_size: 32,
        context_limit: 100,
        ..Default::default()
    });
    event
}
fn increment() -> AgentUsageIncrement {
    AgentUsageIncrement {
        key: AgentUsageKey::from_request("p", "m", None),
        totals: AgentUsageTotals {
            input_tokens: 17,
            cached_input_tokens: 3,
            output_tokens: 9,
            ..Default::default()
        },
        context_size: Some(32),
        context_limit: Some(100),
    }
}
async fn seeded(repository: &EventRepository) {
    let mut event = Event::new();
    event.id = "start-root".into();
    event.agent_session_id = "root".into();
    event.set_agent_started(AgentStarted::new());
    repository.append(event, None).await.unwrap();
}
#[tokio::test]
async fn one_cached_t23_coordinator_real_commit_before_apply_and_publish_reopens() {
    let fixture = Fixture::new();
    let repository = fixture.repository();
    seeded(&repository).await;
    let statistics = repository.get_runtime_statistics().await.unwrap();
    let other = repository.clone().get_runtime_statistics().await.unwrap();
    assert!(Arc::ptr_eq(&statistics, &other));
    let root = statistics.get_agent_statistics("root");
    assert!(Arc::ptr_eq(&root, &other.get_agent_statistics("root")));
    let trace = Arc::new(Mutex::new(Vec::new()));
    let applytrace = trace.clone();
    let publishtrace = trace.clone();
    let applyroot = root.clone();
    let db = fixture.0.join("session.db");
    assert!(
        repository
            .record_usage(
                request("usage"),
                increment(),
                root.clone(),
                root.clone(),
                move |value| {
                    let connection = rusqlite::Connection::open(&db).unwrap();
                    let count: i64 = connection
                        .query_row("SELECT COUNT(*) FROM event WHERE id='usage'", [], |row| {
                            row.get(0)
                        })
                        .unwrap();
                    assert_eq!(count, 1);
                    applytrace.lock().unwrap().push("apply");
                    applyroot.add_own(value).unwrap();
                },
                move |event| {
                    publishtrace
                        .lock()
                        .unwrap()
                        .push(if event.has_request_usage_recorded() {
                            "fact"
                        } else if event.has_agent_statistics_updated() {
                            "agent"
                        } else if event.has_session_usage_snapshot() {
                            "session"
                        } else {
                            panic!("unexpected publication")
                        });
                }
            )
            .await
            .unwrap()
    );
    assert_eq!(
        *trace.lock().unwrap(),
        vec!["apply", "fact", "agent", "session"]
    );
    assert_eq!(statistics.capture_usage(&root).input_tokens, 17);
    assert!(
        !repository
            .record_usage(
                request("usage"),
                increment(),
                root.clone(),
                root.clone(),
                |_| panic!("duplicate apply"),
                |_| panic!("duplicate publish")
            )
            .await
            .unwrap()
    );
    repository.database().close().await.unwrap();
    drop(other);
    drop(statistics);
    drop(repository);
    let reopened = fixture.repository();
    let replay = reopened.replay_statistics().await.unwrap();
    assert_eq!(replay.agents["root"].own.totals.input_tokens, 17);
    let fresh = reopened.get_runtime_statistics().await.unwrap();
    assert_eq!(
        fresh
            .capture_usage(&fresh.get_agent_statistics("root"))
            .output_tokens,
        9
    );
    reopened.database().close().await.unwrap();
}
#[tokio::test]
async fn append_failure_never_applies_or_publishes() {
    let fixture = Fixture::new();
    let repository = fixture.repository();
    seeded(&repository).await;
    let stats = repository.get_runtime_statistics().await.unwrap();
    let root = stats.get_agent_statistics("root");
    repository.database().close().await.unwrap();
    let result = repository
        .record_usage(
            request("closed"),
            increment(),
            root.clone(),
            root.clone(),
            |_| panic!("failedappend apply"),
            |_| panic!("failedappend publish"),
        )
        .await;
    assert!(result.is_err());
    assert_eq!(root.capture().own.totals.input_tokens, 0);
}
#[tokio::test]
async fn blocked_completed_append_does_not_block_current_thread_executor() {
    let fixture = Fixture::new();
    let repository = fixture.repository();
    seeded(&repository).await;
    let stats = repository.get_runtime_statistics().await.unwrap();
    let root = stats.get_agent_statistics("root");
    let (entered_tx, entered_rx) = tokio::sync::oneshot::channel();
    let (release_tx, release_rx) = mpsc::channel();
    let blocker = repository
        .admit("block", &NeverCancelled, move |_| {
            entered_tx.send(()).unwrap();
            release_rx.recv().unwrap();
            Ok(())
        })
        .unwrap();
    entered_rx.await.unwrap();
    let (called_tx, called_rx) = tokio::sync::oneshot::channel();
    let record_repo = repository.clone();
    let applyroot = root.clone();
    let operation = tokio::spawn(async move {
        record_repo
            .record_usage(
                request("blocked"),
                increment(),
                root.clone(),
                root,
                move |value| {
                    applyroot.add_own(value).unwrap();
                    called_tx.send(()).unwrap();
                },
                |_| {},
            )
            .await
    });
    // This task can keep executing while the synchronous coordinator waits on
    // its actual actor receipt on the blocking pool, rather than this executor.
    tokio::task::yield_now().await;
    assert!(!operation.is_finished());
    release_tx.send(()).unwrap();
    blocker.into_completion().await.unwrap();
    called_rx.await.unwrap();
    assert!(operation.await.unwrap().unwrap());
    repository.database().close().await.unwrap();
}
#[tokio::test]
async fn publication_panic_keeps_durable_fact_advanced_state_and_suppresses_retry() {
    let fixture = Fixture::new();
    let repository = fixture.repository();
    seeded(&repository).await;
    let stats = repository.get_runtime_statistics().await.unwrap();
    let root = stats.get_agent_statistics("root");
    let applyroot = root.clone();
    assert!(
        repository
            .record_usage(
                request("publishpanic"),
                increment(),
                root.clone(),
                root.clone(),
                move |value| applyroot.add_own(value).unwrap(),
                |_| panic!("controlled publication failure")
            )
            .await
            .is_err()
    );
    assert_eq!(root.capture().own.totals.input_tokens, 17);
    assert_eq!(
        repository
            .replay()
            .await
            .unwrap()
            .iter()
            .filter(|event| event.id == "publishpanic")
            .count(),
        1
    );
    assert!(
        !repository
            .record_usage(
                request("publishpanic"),
                increment(),
                root.clone(),
                root.clone(),
                |_| panic!("duplicate apply"),
                |_| panic!("duplicate publish")
            )
            .await
            .unwrap()
    );
    repository.database().close().await.unwrap();
}
