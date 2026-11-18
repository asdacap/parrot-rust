use parrot_domain::contracts::paths::{RootOrigin, SelectedSessionRoot};
use parrot_domain::protocol::messages::parrot::{Event, TurnEnded};
use parrot_storage::database::SessionDatabase;
use parrot_storage::events::terminal::{TurnClosure, TurnCommit};
use parrot_storage::events::{ConversationMessage, EventRepository, MessageRole, NeverCancelled};
use protobuf::Message;
use std::{
    io::{BufRead, BufReader},
    path::PathBuf,
    process::{Child, Command, Stdio},
    sync::atomic::{AtomicU64, Ordering},
};

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
    agent: &str,
    proposed: Event,
    text: &str,
) -> TurnCommit {
    repository
        .admit_close_turn(agent.to_owned(), proposed, message(text), &NeverCancelled)
        .unwrap()
        .into_completion()
        .await
        .unwrap()
        .into_value()
        .into_output()
}
async fn counts(repository: &EventRepository) -> (i64, i64, i64) {
    repository
        .transaction(|tx| {
            let count = |table: &str| {
                tx.query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |row| {
                    row.get(0)
                })
            };
            Ok((
                count("event")?,
                count("conversation_part")?,
                count("conversation_item")?,
            ))
        })
        .await
        .unwrap()
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
            if committed { (2, 1, 1) } else { (1, 0, 0) },
            "crash mode {mode}"
        );
        let before = repository.replay_raw().await.unwrap();
        let retry = finish(
            &repository,
            "agent",
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
