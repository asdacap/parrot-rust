use super::{Cancel, Fixture, active};
use parrot_domain::contracts::storage::{SessionTransactions, TransactionCommand};
use parrot_storage::database::{DatabaseCommand, DatabaseError, SessionDatabase, Transaction};
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
    mpsc,
};

#[tokio::test]
async fn panic_rolls_back_and_does_not_kill_worker() {
    let fixture = Fixture::new();
    let db = SessionDatabase::open(fixture.root()).unwrap();
    let failed = db
        .admit("panic", &active(), |tx| -> Result<(), DatabaseError> {
            tx.execute("INSERT INTO projection_version VALUES('panic',1)", [])?;
            panic!("controlled transaction failure");
        })
        .unwrap()
        .into_completion()
        .await;
    assert!(matches!(failed, Err(DatabaseError::CommandPanicked)));
    let rows = db
        .admit("after", &active(), |tx| {
            Ok(
                tx.query_row("SELECT count(*) FROM projection_version", [], |r| {
                    r.get::<_, i64>(0)
                })?,
            )
        })
        .unwrap()
        .into_completion()
        .await
        .unwrap()
        .into_value()
        .into_output();
    assert_eq!(rows, 0);
    db.close().await.unwrap();
}

#[tokio::test]
async fn dropped_completion_future_does_not_own_queued_transaction() {
    let fixture = Fixture::new();
    let db = SessionDatabase::open(fixture.root()).unwrap();
    let (entered, entry) = mpsc::channel();
    let (release, released) = mpsc::channel();
    let blocking = db
        .admit("blocking", &active(), move |_| {
            entered.send(()).unwrap();
            released.recv().unwrap();
            Ok(())
        })
        .unwrap();
    entry.recv().unwrap();
    let cancellation = Cancel(AtomicBool::new(false));
    let queued = db
        .admit("queued", &cancellation, |tx| {
            tx.execute("INSERT INTO projection_version VALUES('queued',7)", [])?;
            Ok(())
        })
        .unwrap();
    drop(queued.into_completion());
    cancellation.0.store(true, Ordering::SeqCst);
    release.send(()).unwrap();
    blocking.into_completion().await.unwrap();
    db.close().await.unwrap();
    let reopened = rusqlite::Connection::open(fixture.0.join("session.db")).unwrap();
    assert_eq!(
        reopened
            .query_row(
                "SELECT version FROM projection_version WHERE name='queued'",
                [],
                |r| r.get::<_, i64>(0)
            )
            .unwrap(),
        7
    );
}

#[tokio::test]
async fn every_admitted_command_runs_serially_on_same_non_executor_thread() {
    let fixture = Fixture::new();
    let db = SessionDatabase::open(fixture.root()).unwrap();
    let caller = std::thread::current().id();
    let mut tickets = Vec::new();
    for i in 0..32 {
        tickets.push(
            db.admit(i, &active(), move |tx| {
                let prior: i64 =
                    tx.query_row("SELECT count(*) FROM projection_version", [], |r| r.get(0))?;
                assert_eq!(prior, i);
                tx.execute(
                    "INSERT INTO projection_version VALUES(?1,?2)",
                    rusqlite::params![i.to_string(), i],
                )?;
                Ok(std::thread::current().id())
            })
            .unwrap(),
        );
    }
    let mut worker = None;
    for ticket in tickets {
        let id = ticket
            .into_completion()
            .await
            .unwrap()
            .into_value()
            .into_output();
        assert_ne!(id, caller);
        assert_eq!(*worker.get_or_insert(id), id);
    }
    db.close().await.unwrap();
}

struct Command {
    key: String,
}
impl TransactionCommand for Command {
    type Key = String;
    type Output = i64;
    fn key(&self) -> &String {
        &self.key
    }
}
impl DatabaseCommand for Command {
    fn execute(self, tx: &Transaction<'_>) -> Result<i64, DatabaseError> {
        tx.execute("INSERT INTO projection_version VALUES(?1,9)", [self.key])?;
        Ok(9)
    }
}
#[tokio::test]
async fn typed_shared_contract_retains_key_receipt_and_selected_root() {
    let fixture = Fixture::new();
    let db = SessionDatabase::open(fixture.root()).unwrap();
    let transactions = db.transactions::<Command>();
    assert_eq!(transactions.selected_root(), &fixture.root());
    let ticket = transactions
        .admit(
            Command {
                key: "typed".into(),
            },
            &active(),
        )
        .await
        .unwrap();
    assert_eq!(ticket.key(), "typed");
    let receipt = ticket.into_completion().await.unwrap().into_value();
    assert_eq!(receipt.key(), "typed");
    assert_eq!(*receipt.output(), 9);
    db.close().await.unwrap();
}

#[tokio::test]
async fn close_from_another_clone_drains_and_closes_every_clone() {
    let fixture = Fixture::new();
    let db = SessionDatabase::open(fixture.root()).unwrap();
    let clone = db.clone();
    let ticket = db
        .admit("close", &active(), |tx| {
            tx.execute("INSERT INTO projection_version VALUES('closing',1)", [])?;
            Ok(())
        })
        .unwrap();
    clone.close().await.unwrap();
    ticket.into_completion().await.unwrap();
    assert!(db.admit("no", &active(), |_| Ok(())).is_err());
    db.close().await.unwrap();
}

#[tokio::test]
async fn dropping_last_database_handle_still_finishes_admitted_transaction() {
    let fixture = Fixture::new();
    let db = SessionDatabase::open(fixture.root()).unwrap();
    let written = Arc::new(AtomicBool::new(false));
    let result = written.clone();
    let ticket = db
        .admit("last", &active(), move |tx| {
            tx.execute("INSERT INTO projection_version VALUES('last',1)", [])?;
            result.store(true, Ordering::SeqCst);
            Ok(())
        })
        .unwrap();
    drop(db);
    ticket.into_completion().await.unwrap();
    assert!(written.load(Ordering::SeqCst));
    let reader = rusqlite::Connection::open(fixture.0.join("session.db")).unwrap();
    assert_eq!(
        reader
            .query_row("SELECT count(*) FROM projection_version", [], |r| r
                .get::<_, i64>(0))
            .unwrap(),
        1
    );
}

#[tokio::test]
async fn in_memory_database_uses_memory_journal_but_still_enforces_foreign_keys() {
    let fixture = Fixture::new();
    let db = SessionDatabase::open_memory(fixture.root()).unwrap();
    let mode = db
        .admit("memory", &active(), |tx| {
            Ok(tx.query_row("PRAGMA journal_mode", [], |r| r.get::<_, String>(0))?)
        })
        .unwrap()
        .into_completion()
        .await
        .unwrap()
        .into_value()
        .into_output();
    assert_eq!(mode, "memory");
    db.close().await.unwrap();
    assert!(!fixture.0.join("session.db").exists());
}
