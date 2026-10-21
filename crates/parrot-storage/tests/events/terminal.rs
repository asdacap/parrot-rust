//! Controlled storage-boundary schedules for A003; no AgentSession/T46 claim.
use super::Fixture;
use parrot_domain::contracts::async_ops::{BoxFuture, Cancellation, OperationError};
use parrot_domain::contracts::events::EventPublisher;
use parrot_domain::contracts::storage::{AdmissionError, Committed};
use parrot_domain::protocol::messages::parrot::{
    Event, RequestUsageRecorded, TurnEnded, TurnStarted,
};
use parrot_storage::events::terminal::{CloseTurnError, TurnClosure, TurnCommit, TurnKey};
use parrot_storage::events::{
    ConversationMessage, EventError, EventRepository, MessageRole, NeverCancelled,
};
use protobuf::Message;
use std::io::{BufRead, BufReader};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Barrier, Mutex, mpsc};

fn key(agent: &str, start: &str) -> TurnKey {
    TurnKey {
        agent_session_id: agent.into(),
        start_event_id: start.into(),
    }
}
fn start(agent: &str, id: &str) -> Event {
    let mut event = Event::new();
    event.id = id.into();
    event.agent_session_id = agent.into();
    event.set_turn_started(TurnStarted::new());
    event
}
fn end(agent: &str, id: &str, finish: &str, input: i64, output: i64) -> Event {
    let mut event = Event::new();
    event.id = id.into();
    event.agent_session_id = agent.into();
    event.set_turn_ended(TurnEnded {
        finish_reason: finish.into(),
        input_tokens: input,
        output_tokens: output,
        ..TurnEnded::new()
    });
    event
}
fn unknown(event: Event) -> (Event, Vec<u8>) {
    let mut raw = event.write_to_bytes().unwrap();
    raw.extend_from_slice(&[0xa0, 0x06, 0x07]);
    (Event::parse_from_bytes(&raw).unwrap(), raw)
}
fn message(text: &str) -> Option<ConversationMessage> {
    Some(ConversationMessage::text(MessageRole::Assistant, text))
}
async fn finish(
    repository: &EventRepository,
    turn: TurnKey,
    proposed: Event,
    text: &str,
) -> TurnCommit {
    repository
        .admit_close_turn(turn, proposed, None, message(text), &NeverCancelled)
        .unwrap()
        .into_completion()
        .await
        .unwrap()
        .into_value()
        .into_output()
}
async fn counts(repository: &EventRepository) -> (i64, i64, i64, i64) {
    repository
        .transaction(|tx| {
            let count = |table: &str| {
                tx.query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |row| {
                    row.get(0)
                })
            };
            Ok((
                count("event")?,
                count("message")?,
                count("conversation_item")?,
                count("agent_history")?,
            ))
        })
        .await
        .unwrap()
}
struct Cancel(AtomicBool);
impl Cancellation for Cancel {
    fn is_cancelled(&self) -> bool {
        self.0.load(Ordering::SeqCst)
    }
    fn cancelled(&self) -> BoxFuture<'_, ()> {
        Box::pin(std::future::pending())
    }
}

#[tokio::test]
async fn cancellation_before_admission_keeps_turn_open_without_projection() {
    let fixture = Fixture::new();
    let repository = fixture.repository();
    repository
        .append(start("agent", "start"), None)
        .await
        .unwrap();
    let cancelled = Cancel(AtomicBool::new(true));
    let ticket = repository.admit_close_turn(
        key("agent", "start"),
        end("agent", "canceled", "interrupted", 17, 9),
        None,
        message("losing"),
        &cancelled,
    );
    assert!(matches!(ticket, Err(AdmissionError::Cancelled)));
    assert_eq!(counts(&repository).await, (1, 0, 0, 0));
    let outcome = finish(
        &repository,
        key("agent", "start"),
        end("agent", "winner", "stop", 17, 9),
        "answer",
    )
    .await;
    assert_eq!(outcome.closure, TurnClosure::NewlyClosed);
    repository.database().close().await.unwrap();
}

#[tokio::test]
async fn projection_failure_after_terminal_insert_rolls_back_whole_close() {
    let fixture = Fixture::new();
    let repository = fixture.repository();
    repository
        .append(start("agent", "start"), None)
        .await
        .unwrap();
    repository.transaction(|tx| { tx.execute_batch("CREATE TRIGGER fail_message BEFORE INSERT ON message BEGIN SELECT RAISE(ABORT, 'controlled projection failure'); END;")?; Ok(()) }).await.unwrap();
    let failure = repository
        .admit_close_turn(
            key("agent", "start"),
            end("agent", "failed", "stop", 17, 9),
            None,
            message("must rollback"),
            &NeverCancelled,
        )
        .unwrap()
        .into_completion()
        .await;
    assert!(matches!(failure, Err(EventError::Database(_))));
    assert_eq!(counts(&repository).await, (1, 0, 0, 0));
    repository
        .transaction(|tx| {
            tx.execute_batch("DROP TRIGGER fail_message;")?;
            Ok(())
        })
        .await
        .unwrap();
    let winner = finish(
        &repository,
        key("agent", "start"),
        end("agent", "winner", "stop", 17, 9),
        "answer",
    )
    .await;
    assert_eq!(winner.closure, TurnClosure::NewlyClosed);
    assert_eq!(counts(&repository).await, (2, 1, 1, 1));
    repository.database().close().await.unwrap();
}

#[tokio::test]
async fn admitted_close_survives_dropped_ticket_late_cancel_and_reopen() {
    let fixture = Fixture::new();
    let repository = fixture.repository();
    repository
        .append(start("agent", "start"), None)
        .await
        .unwrap();
    let (entered_tx, entered_rx) = mpsc::channel();
    let (release_tx, release_rx) = mpsc::channel();
    let blocker = repository
        .admit("barrier", &NeverCancelled, move |_| {
            entered_tx.send(()).unwrap();
            release_rx.recv().unwrap();
            Ok(())
        })
        .unwrap();
    entered_rx.recv().unwrap();
    let cancellation = Cancel(AtomicBool::new(false));
    let (event, raw) = unknown(end("agent", "winner", "stop", 17, 9));
    let ticket = repository
        .admit_close_turn(
            key("agent", "start"),
            event.clone(),
            Some(raw.clone()),
            message("winning answer"),
            &cancellation,
        )
        .unwrap();
    cancellation.0.store(true, Ordering::SeqCst);
    drop(ticket);
    release_tx.send(()).unwrap();
    blocker.into_completion().await.unwrap();
    let retry = finish(
        &repository,
        key("agent", "start"),
        end("agent", "new-interrupted-id", "interrupted", 0, 0),
        "losing interruption",
    )
    .await;
    assert_eq!(retry.closure, TurnClosure::AlreadyClosed);
    assert_eq!(retry.terminal.event, event);
    assert_eq!(retry.terminal.raw, raw);
    assert!(retry.conversation.is_none());
    assert_eq!(counts(&repository).await, (2, 1, 1, 1));
    repository.database().close().await.unwrap();
    let reopened = fixture.repository();
    let retried = finish(
        &reopened,
        key("agent", "start"),
        end("agent", "restart-id", "interrupted", 999, 999),
        "not committed",
    )
    .await;
    assert_eq!(retried.terminal, retry.terminal);
    assert_eq!(retried.closure, TurnClosure::AlreadyClosed);
    assert_eq!(counts(&reopened).await, (2, 1, 1, 1));
    reopened.database().close().await.unwrap();
}

struct CancellingPublisher {
    path: PathBuf,
    key: TurnKey,
    closed: Mutex<Option<TurnKey>>,
    cancellation: Arc<Cancel>,
    calls: AtomicUsize,
}
impl EventPublisher for CancellingPublisher {
    type Event = Event;
    type Error = std::io::Error;
    fn publish_committed<'a>(
        &'a self,
        event: &'a Committed<Event>,
        _: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<(), OperationError<Self::Error>>> {
        Box::pin(async move {
            self.calls.fetch_add(1, Ordering::SeqCst);
            assert_eq!(self.closed.lock().unwrap().as_ref(), Some(&self.key));
            assert!(self.cancellation.is_cancelled());
            let connection = rusqlite::Connection::open(&self.path).unwrap();
            let bytes: Vec<u8> = connection
                .query_row(
                    "SELECT payload FROM event WHERE id=?1",
                    [&event.value().id],
                    |row| row.get(0),
                )
                .unwrap();
            assert_eq!(Event::parse_from_bytes(&bytes).unwrap(), *event.value());
            let messages: i64 = connection
                .query_row(
                    "SELECT COUNT(*) FROM message WHERE content='winning answer'",
                    [],
                    |row| row.get(0),
                )
                .unwrap();
            assert_eq!(messages, 1);
            Err(OperationError::Cancelled)
        })
    }
    fn publish_transient<'a>(
        &'a self,
        _: Event,
        _: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<(), OperationError<Self::Error>>> {
        Box::pin(async { panic!("unexpected transient event") })
    }
}

#[tokio::test]
async fn publication_cancellation_reconciles_key_after_commit_and_never_republishes_retry() {
    let fixture = Fixture::new();
    let repository = fixture.repository();
    repository
        .append(start("agent", "start"), None)
        .await
        .unwrap();
    let cancellation = Arc::new(Cancel(AtomicBool::new(false)));
    let publisher = CancellingPublisher {
        path: fixture.0.join("session.db"),
        key: key("agent", "start"),
        closed: Mutex::new(None),
        cancellation: cancellation.clone(),
        calls: AtomicUsize::new(0),
    };
    let proposed = end("agent", "winner", "stop", 17, 9);
    let result = repository
        .close_turn_and_publish(
            key("agent", "start"),
            proposed.clone(),
            message("winning answer"),
            &publisher,
            cancellation.as_ref(),
            |receipt| {
                assert_eq!(receipt.key, publisher.key);
                assert_eq!(receipt.closure, TurnClosure::NewlyClosed);
                *publisher.closed.lock().unwrap() = Some(receipt.key.clone());
                cancellation.0.store(true, Ordering::SeqCst);
            },
        )
        .await;
    let committed = match result {
        Err(CloseTurnError::Publication {
            committed,
            error: OperationError::Cancelled,
        }) => committed,
        other => panic!("unexpected result: {other:?}"),
    };
    assert_eq!(committed.terminal.event, proposed);
    let retried = repository
        .close_turn_and_publish(
            key("agent", "start"),
            end("agent", "interrupt", "interrupted", 0, 0),
            message("losing interruption"),
            &publisher,
            &NeverCancelled,
            |receipt| {
                assert_eq!(receipt.key, publisher.key);
                assert_eq!(receipt.closure, TurnClosure::AlreadyClosed);
            },
        )
        .await
        .unwrap();
    assert_eq!(retried.terminal, committed.terminal);
    assert_eq!(publisher.calls.load(Ordering::SeqCst), 1);
    assert_eq!(counts(&repository).await, (2, 1, 1, 1));
    repository.database().close().await.unwrap();
}

#[tokio::test]
async fn competing_closes_have_one_exact_winner_and_other_agent_events_do_not_bound_turn() {
    let fixture = Fixture::new();
    let repository = fixture.repository();
    repository
        .append(start("agent", "start"), None)
        .await
        .unwrap();
    repository
        .append(start("other", "other-start"), None)
        .await
        .unwrap();
    repository
        .append(end("other", "other-end", "stop", 1, 2), None)
        .await
        .unwrap();
    let barrier = Arc::new(Barrier::new(3));
    let spawn = |id: &'static str| {
        let repository = repository.clone();
        let barrier = barrier.clone();
        std::thread::spawn(move || {
            barrier.wait();
            repository
                .admit_close_turn(
                    key("agent", "start"),
                    end("agent", id, "stop", 17, 9),
                    None,
                    message(id),
                    &NeverCancelled,
                )
                .unwrap()
        })
    };
    let a = spawn("winner-a");
    let b = spawn("winner-b");
    barrier.wait();
    let a = a
        .join()
        .unwrap()
        .into_completion()
        .await
        .unwrap()
        .into_value()
        .into_output();
    let b = b
        .join()
        .unwrap()
        .into_completion()
        .await
        .unwrap()
        .into_value()
        .into_output();
    assert_eq!(a.terminal, b.terminal);
    assert_ne!(a.closure, b.closure);
    assert!(["winner-a", "winner-b"].contains(&a.terminal.event.id.as_str()));
    assert_eq!(counts(&repository).await, (4, 1, 1, 1));
    repository.database().close().await.unwrap();
}

#[tokio::test]
async fn exact_invalid_turn_keys_and_stale_start_are_rejected_without_writes() {
    let fixture = Fixture::new();
    let repository = fixture.repository();
    repository
        .append(start("agent", "start"), None)
        .await
        .unwrap();
    repository
        .append(start("other", "other-start"), None)
        .await
        .unwrap();
    repository
        .append(end("agent", "not-start", "stop", 0, 0), None)
        .await
        .unwrap();
    for (turn, proposed, expected) in [
        (
            key("agent", "missing"),
            end("agent", "m", "stop", 0, 0),
            "turn start does not exist",
        ),
        (
            key("agent", "other-start"),
            end("agent", "w", "stop", 0, 0),
            "turn key does not name that agent's TurnStarted",
        ),
        (
            key("agent", "not-start"),
            end("agent", "k", "stop", 0, 0),
            "turn key does not name that agent's TurnStarted",
        ),
        (
            key("agent", "start"),
            end("other", "p", "stop", 0, 0),
            "terminal proposal must be TurnEnded for the keyed agent",
        ),
        (
            key("agent", "start"),
            start("agent", "wrong-payload"),
            "terminal proposal must be TurnEnded for the keyed agent",
        ),
    ] {
        let result = repository
            .admit_close_turn(turn, proposed, None, message("never"), &NeverCancelled)
            .unwrap()
            .into_completion()
            .await;
        assert!(matches!(result, Err(EventError::Invalid(actual)) if actual==expected));
    }
    repository
        .append(start("stale-agent", "old"), None)
        .await
        .unwrap();
    repository
        .append(start("stale-agent", "new"), None)
        .await
        .unwrap();
    let result = repository
        .admit_close_turn(
            key("stale-agent", "old"),
            end("stale-agent", "stale", "stop", 0, 0),
            None,
            message("never"),
            &NeverCancelled,
        )
        .unwrap()
        .into_completion()
        .await;
    assert!(
        matches!(result, Err(EventError::Invalid(message)) if message=="turn key is stale: a later turn has started")
    );
    assert_eq!(counts(&repository).await, (5, 0, 0, 0));
    repository.database().close().await.unwrap();
}

#[tokio::test]
async fn historical_duplicate_terminals_remain_in_replay_without_repair_or_normalization() {
    let fixture = Fixture::new();
    let repository = fixture.repository();
    repository
        .append(start("agent", "start"), None)
        .await
        .unwrap();
    repository
        .append(end("agent", "old-normal", "stop", 17, 9), message("answer"))
        .await
        .unwrap();
    repository
        .append(
            end("agent", "old-interrupted", "interrupted", 17, 9),
            message("old interruption"),
        )
        .await
        .unwrap();
    let before = repository.replay_raw().await.unwrap();
    let outcome = finish(
        &repository,
        key("agent", "start"),
        end("agent", "new", "interrupted", 99, 99),
        "must not insert",
    )
    .await;
    assert_eq!(outcome.closure, TurnClosure::AlreadyClosed);
    assert_eq!(outcome.terminal.event.id, "old-normal");
    assert_eq!(repository.replay_raw().await.unwrap(), before);
    assert_eq!(
        before
            .iter()
            .filter(|row| row.event.has_turn_ended())
            .count(),
        2
    );
    assert_eq!(counts(&repository).await, (3, 2, 2, 2));
    repository.database().close().await.unwrap();
}

#[tokio::test]
async fn a003_storage_schedule_retains_one_completion_and_two_separate_turn_endings() {
    let fixture = Fixture::new();
    let repository = fixture.repository();
    repository
        .append(start("agent", "first"), None)
        .await
        .unwrap();
    finish(
        &repository,
        key("agent", "first"),
        end("agent", "first-end", "stop", 0, 0),
        "first answer",
    )
    .await;
    finish(
        &repository,
        key("agent", "first"),
        end("agent", "first-interrupt", "interrupted", 0, 0),
        "losing",
    )
    .await;
    assert_eq!(
        repository
            .replay()
            .await
            .unwrap()
            .iter()
            .filter(|event| event.has_turn_ended())
            .count(),
        1
    );
    repository
        .append(start("agent", "second"), None)
        .await
        .unwrap();
    finish(
        &repository,
        key("agent", "second"),
        end("agent", "second-end", "stop", 0, 0),
        "second answer",
    )
    .await;
    finish(
        &repository,
        key("agent", "second"),
        end("agent", "second-interrupt", "interrupted", 0, 0),
        "losing",
    )
    .await;
    let replay = repository.replay().await.unwrap();
    assert_eq!(
        replay
            .iter()
            .filter(|event| event.has_turn_started())
            .count(),
        2
    );
    assert_eq!(
        replay.iter().filter(|event| event.has_turn_ended()).count(),
        2
    );
    repository.database().close().await.unwrap();
}

#[tokio::test]
async fn a003_storage_schedule_retains_17_9_then_23_11_and_all_three_raw_usage_facts() {
    let fixture = Fixture::new();
    let repository = fixture.repository();
    repository
        .append(start("agent", "first"), None)
        .await
        .unwrap();
    let fact = |id: &str, input: i64, cached: i64, output: i64| {
        let mut event = Event::new();
        event.id = id.into();
        event.agent_session_id = "agent".into();
        event.set_request_usage_recorded(RequestUsageRecorded {
            request_id: id.into(),
            provider: "test".into(),
            model: "model".into(),
            input_tokens: input,
            cached_input_tokens: cached,
            output_tokens: output,
            ..RequestUsageRecorded::new()
        });
        event
    };
    repository
        .append_usage_fact(fact("fact1", 10, 3, 4))
        .await
        .unwrap();
    repository
        .append_usage_fact(fact("fact2", 7, 2, 5))
        .await
        .unwrap();
    finish(
        &repository,
        key("agent", "first"),
        end("agent", "first-end", "stop", 17, 9),
        "first",
    )
    .await;
    finish(
        &repository,
        key("agent", "first"),
        end("agent", "first-loser", "interrupted", 17, 9),
        "losing",
    )
    .await;
    repository
        .append(start("agent", "second"), None)
        .await
        .unwrap();
    repository
        .append_usage_fact(fact("fact3", 6, 1, 2))
        .await
        .unwrap();
    let before: Vec<_> = repository
        .replay_raw()
        .await
        .unwrap()
        .into_iter()
        .filter(|row| row.event.has_request_usage_recorded())
        .collect();
    finish(
        &repository,
        key("agent", "second"),
        end("agent", "second-end", "stop", 23, 11),
        "second",
    )
    .await;
    finish(
        &repository,
        key("agent", "second"),
        end("agent", "second-loser", "interrupted", 23, 11),
        "losing",
    )
    .await;
    let replay = repository.replay_raw().await.unwrap();
    let actual: Vec<_> = replay
        .iter()
        .filter(|row| row.event.has_turn_ended())
        .map(|row| {
            let end = row.event.turn_ended();
            format!("{}:{}", end.input_tokens, end.output_tokens)
        })
        .collect();
    assert_eq!(actual, ["17:9", "23:11"]);
    let after: Vec<_> = replay
        .into_iter()
        .filter(|row| row.event.has_request_usage_recorded())
        .collect();
    assert_eq!(after, before);
    let triples: Vec<_> = after
        .iter()
        .map(|row| {
            let request = row.event.request_usage_recorded();
            format!(
                "{}:{}:{}",
                request.input_tokens, request.cached_input_tokens, request.output_tokens
            )
        })
        .collect();
    assert_eq!(triples, ["10:3:4", "7:2:5", "6:1:2"]);
    repository.database().close().await.unwrap();
}

struct Probe(Child);
impl Drop for Probe {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}
fn killed_at_gate(mode: &str, root: &std::path::Path, expected: &str) {
    let binary = std::env::var_os("T17_CRASH_PROBE_BINARY").expect("required crash gate missing T17_CRASH_PROBE_BINARY; build --example t17-crash-probe first; this test must not be skipped");
    assert!(
        std::path::Path::new(&binary).is_file(),
        "crash probe binary does not exist"
    );
    let child = Command::new(binary)
        .arg(mode)
        .arg(root)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let mut child = Probe(child);
    let mut stdout = BufReader::new(child.0.stdout.take().unwrap());
    let mut line = String::new();
    stdout.read_line(&mut line).unwrap();
    assert_eq!(
        line.trim_end(),
        expected,
        "child did not reach requested controlled crash gate {mode}"
    );
    child.0.kill().unwrap();
    let status = child.0.wait().unwrap();
    assert!(!status.success());
    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt;
        assert_eq!(status.signal(), Some(9));
    }
}

#[tokio::test]
async fn actual_process_kills_recover_before_admission_during_transaction_and_after_commit() {
    for (mode, expected, committed) in [
        ("before-admission", "before-admission open=true", false),
        (
            "during-transaction",
            "during-transaction terminal=true projection=true committed=false",
            false,
        ),
        (
            "after-commit-before-observation",
            "after-commit observed=false published=false",
            true,
        ),
        (
            "publisher-entry",
            "publisher-entry committed=true closed=true",
            true,
        ),
    ] {
        let fixture = Fixture::new();
        killed_at_gate(mode, &fixture.0, expected);
        let repository = fixture.repository();
        assert_eq!(
            counts(&repository).await,
            if committed {
                (2, 1, 1, 1)
            } else {
                (1, 0, 0, 0)
            },
            "crash mode {mode}"
        );
        let before = repository.replay_raw().await.unwrap();
        let retry = finish(
            &repository,
            key("agent", "crash-start"),
            end("agent", "recovery-interrupted", "interrupted", 0, 0),
            "losing recovery",
        )
        .await;
        if committed {
            assert_eq!(retry.closure, TurnClosure::AlreadyClosed);
            assert_eq!(retry.terminal.event.id, "crash-terminal");
            assert_eq!(
                (
                    retry.terminal.event.turn_ended().input_tokens,
                    retry.terminal.event.turn_ended().output_tokens
                ),
                (17, 9)
            );
            let (_, expected_raw) = unknown(end("agent", "crash-terminal", "stop", 17, 9));
            assert_eq!(retry.terminal.raw, expected_raw);
            assert_eq!(repository.replay_raw().await.unwrap(), before);
        } else {
            assert_eq!(retry.closure, TurnClosure::NewlyClosed);
            assert_eq!(retry.terminal.event.id, "recovery-interrupted");
        }
        repository
            .transaction(|tx| {
                let integrity: String =
                    tx.query_row("PRAGMA integrity_check", [], |row| row.get(0))?;
                assert_eq!(integrity, "ok");
                let mut foreign = tx.prepare("PRAGMA foreign_key_check")?;
                assert!(foreign.query([])?.next()?.is_none());
                Ok(())
            })
            .await
            .unwrap();
        assert!(!fixture.0.join("session.db-wal").exists());
        assert!(!fixture.0.join("session.db-shm").exists());
        repository.database().close().await.unwrap();
    }
}
