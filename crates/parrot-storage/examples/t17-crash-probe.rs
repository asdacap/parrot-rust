//! Process-kill checkpoints for the terminal storage recovery integration test.

use parrot_domain::contracts::async_ops::{BoxFuture, Cancellation, OperationError};
use parrot_domain::contracts::events::EventPublisher;
use parrot_domain::contracts::paths::{RootOrigin, SelectedSessionRoot};
use parrot_domain::contracts::storage::Committed;
use parrot_domain::protocol::messages::parrot::{Event, TurnEnded, TurnStarted};
use parrot_storage::database::{Connection, SessionDatabase, Transaction};
use parrot_storage::events::terminal::close_turn_if_open;
use parrot_storage::events::{
    ConversationMessage, EventError, EventRepository, MessageRole, NeverCancelled, TurnClosure,
    TurnKey,
};
use protobuf::Message;
use std::io::Write;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};

fn wait_at_gate(line: &str) -> ! {
    println!("{line}");
    std::io::stdout().flush().unwrap();
    loop {
        std::thread::park();
    }
}

fn count_rows(transaction: &Transaction<'_>, table: &str) -> i64 {
    transaction
        .query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |row| {
            row.get(0)
        })
        .unwrap()
}

struct CrashPublisher {
    path: PathBuf,
    closed: AtomicBool,
}

impl EventPublisher for CrashPublisher {
    type Event = Event;
    type Error = std::io::Error;

    fn publish_committed<'a>(
        &'a self,
        event: &'a Committed<Event>,
        _: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<(), OperationError<Self::Error>>> {
        Box::pin(async move {
            assert!(self.closed.load(Ordering::SeqCst));
            let connection = Connection::open(&self.path).unwrap();
            let raw: Vec<u8> = connection
                .query_row(
                    "SELECT payload FROM event WHERE id = ?1",
                    [&event.value().id],
                    |row| row.get(0),
                )
                .unwrap();
            assert_eq!(Event::parse_from_bytes(&raw).unwrap(), *event.value());
            let messages: i64 = connection
                .query_row("SELECT COUNT(*) FROM message", [], |row| row.get(0))
                .unwrap();
            assert_eq!(messages, 1);
            wait_at_gate("publisher-entry committed=true closed=true");
        })
    }

    fn publish_transient<'a>(
        &'a self,
        _: Event,
        _: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<(), OperationError<Self::Error>>> {
        Box::pin(async { panic!("crash probe must not publish transient events") })
    }
}

#[tokio::main(flavor = "current_thread")]
async fn main() {
    let mut arguments = std::env::args_os().skip(1);
    let mode = arguments.next().expect("missing crash gate mode");
    let root = PathBuf::from(arguments.next().expect("missing session root"));
    assert!(
        arguments.next().is_none(),
        "unexpected crash probe argument"
    );
    let root = SelectedSessionRoot::new(root, RootOrigin::Current).unwrap();
    let repository = EventRepository::new(SessionDatabase::open(root).unwrap());
    let mut start = Event::new();
    start.id = "crash-start".into();
    start.agent_session_id = "agent".into();
    start.set_turn_started(TurnStarted::new());
    repository.append(start, None).await.unwrap();

    let key = TurnKey {
        agent_session_id: "agent".into(),
        start_event_id: "crash-start".into(),
    };
    let mut proposed = Event::new();
    proposed.id = "crash-terminal".into();
    proposed.agent_session_id = "agent".into();
    proposed.set_turn_ended(TurnEnded {
        finish_reason: "stop".into(),
        input_tokens: 17,
        output_tokens: 9,
        ..TurnEnded::new()
    });
    let mut raw = proposed.write_to_bytes().unwrap();
    raw.extend_from_slice(&[0xa0, 0x06, 0x07]);
    let proposed = Event::parse_from_bytes(&raw).unwrap();
    let message = ConversationMessage::text(MessageRole::Assistant, "winning answer");

    match mode.to_str().expect("crash gate mode must be UTF-8") {
        "before-admission" => {
            let events = repository.replay_raw().await.unwrap();
            assert_eq!(events.len(), 1);
            assert!(events[0].event.has_turn_started());
            wait_at_gate("before-admission open=true");
        }
        "during-transaction" => {
            let context = repository.context().clone();
            repository
                .transaction(move |transaction| -> Result<(), EventError> {
                    let outcome = close_turn_if_open(
                        transaction,
                        &context,
                        &key,
                        &proposed,
                        Some(&raw),
                        Some(&message),
                    )?;
                    assert_eq!(outcome.closure, TurnClosure::NewlyClosed);
                    assert_eq!(count_rows(transaction, "event"), 2);
                    for table in ["message", "conversation_item", "agent_history"] {
                        assert_eq!(count_rows(transaction, table), 1);
                    }
                    assert!(!transaction.is_autocommit());
                    wait_at_gate(
                        "during-transaction terminal=true projection=true committed=false",
                    );
                })
                .await
                .unwrap();
        }
        "after-commit-before-observation" => {
            let _unobserved_ticket = repository
                .admit_close_turn(key, proposed, Some(raw), Some(message), &NeverCancelled)
                .unwrap();
            repository
                .transaction(|transaction| {
                    assert_eq!(count_rows(transaction, "event"), 2);
                    for table in ["message", "conversation_item", "agent_history"] {
                        assert_eq!(count_rows(transaction, table), 1);
                    }
                    Ok(())
                })
                .await
                .unwrap();
            wait_at_gate("after-commit observed=false published=false");
        }
        "publisher-entry" => {
            let publisher = CrashPublisher {
                path: repository
                    .database()
                    .selected_root()
                    .path()
                    .join("session.db"),
                closed: AtomicBool::new(false),
            };
            repository
                .close_turn_and_publish(
                    key,
                    proposed,
                    Some(message),
                    &publisher,
                    &NeverCancelled,
                    |receipt| {
                        assert_eq!(receipt.closure, TurnClosure::NewlyClosed);
                        assert_eq!(receipt.key.start_event_id, "crash-start");
                        publisher.closed.store(true, Ordering::SeqCst);
                    },
                )
                .await
                .unwrap();
        }
        mode => panic!("unknown crash gate mode: {mode}"),
    }
    panic!("crash gate returned before the parent killed the probe");
}
