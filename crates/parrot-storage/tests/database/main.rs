mod actor;
mod migrations;
mod observe;
mod reference_comparison;
mod scenario;
mod selected_tree;
mod source_boundaries;

use parrot_domain::contracts::async_ops::{BoxFuture, Cancellation};
use parrot_domain::contracts::paths::{RootOrigin, SelectedSessionRoot};
use parrot_domain::contracts::storage::AdmissionError;
use parrot_storage::database::{Connection, DatabaseError, SessionDatabase};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, mpsc};

static NEXT: AtomicU64 = AtomicU64::new(0);
struct Fixture(PathBuf);
impl Fixture {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!(
            "parrot-t16-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&path).unwrap();
        Self(path)
    }
    fn root(&self) -> SelectedSessionRoot {
        SelectedSessionRoot::new(self.0.clone(), RootOrigin::Current).unwrap()
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
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
fn active() -> Cancel {
    Cancel(AtomicBool::new(false))
}

#[tokio::test]
async fn schema_foreign_keys_truncate_and_no_wal() {
    let fixture = Fixture::new();
    let database = SessionDatabase::open(fixture.root()).unwrap();
    let value = database.admit("schema", &active(), |tx| {
        let mode: String = tx.query_row("PRAGMA journal_mode", [], |row| row.get(0))?;
        let foreign_keys: i64 = tx.query_row("PRAGMA foreign_keys", [], |row| row.get(0))?;
        let tables: i64 = tx.query_row("SELECT count(*) FROM sqlite_master WHERE type='table' AND name NOT LIKE 'sqlite_%'", [], |row| row.get(0))?;
        let versions: i64 = tx.query_row("SELECT count(*) FROM projection_version", [], |row| row.get(0))?;
        assert!(tx.execute("INSERT INTO conversation_part VALUES(999,0,'text','','','','')", []).is_err());
        Ok((mode, foreign_keys, tables, versions))
    }).unwrap().into_completion().await.unwrap().into_value().into_output();
    assert_eq!(value, ("truncate".into(), 1, 29, 0));
    database.close().await.unwrap();
    assert!(!fixture.0.join("session.db-wal").exists());
    assert!(!fixture.0.join("session.db-shm").exists());
}

#[tokio::test]
async fn dropped_ticket_and_late_cancellation_cannot_split_transaction() {
    let fixture = Fixture::new();
    let database = SessionDatabase::open(fixture.root()).unwrap();
    let cancellation = active();
    let (entered_tx, entered_rx) = mpsc::channel();
    let (release_tx, release_rx) = mpsc::channel();
    let ticket = database
        .admit("whole", &cancellation, move |tx| {
            tx.execute("INSERT INTO projection_version VALUES('first',1)", [])?;
            entered_tx.send(()).unwrap();
            release_rx.recv().unwrap();
            tx.execute("INSERT INTO projection_version VALUES('second',2)", [])?;
            Ok(())
        })
        .unwrap();
    entered_rx.recv().unwrap();
    cancellation.0.store(true, Ordering::SeqCst);
    drop(ticket);
    // A separate connection cannot observe the first half while the worker waits.
    let observer = Connection::open(fixture.0.join("session.db")).unwrap();
    let before: i64 = observer
        .query_row("SELECT count(*) FROM projection_version", [], |r| r.get(0))
        .unwrap();
    assert_eq!(before, 0);
    release_tx.send(()).unwrap();
    database.close().await.unwrap();
    let after: i64 = observer
        .query_row("SELECT count(*) FROM projection_version", [], |r| r.get(0))
        .unwrap();
    assert_eq!(after, 2);
}

#[tokio::test]
async fn command_error_rolls_back_every_statement_and_worker_continues() {
    let fixture = Fixture::new();
    let database = SessionDatabase::open(fixture.root()).unwrap();
    let failed = database
        .admit("failure", &active(), |tx| {
            tx.execute("INSERT INTO projection_version VALUES('first',1)", [])?;
            tx.execute("INSERT INTO projection_version VALUES('first',2)", [])?;
            Ok(())
        })
        .unwrap()
        .into_completion()
        .await;
    assert!(matches!(failed, Err(DatabaseError::Sqlite(_))));
    let count = database
        .admit("count", &active(), |tx| {
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
    assert_eq!(count, 0);
    database.close().await.unwrap();
}

#[tokio::test]
async fn cancellation_before_admission_and_close_reject_without_execution() {
    let fixture = Fixture::new();
    let database = SessionDatabase::open(fixture.root()).unwrap();
    let called = Arc::new(AtomicBool::new(false));
    let observed = called.clone();
    let result = database.admit("cancelled", &Cancel(AtomicBool::new(true)), move |_| {
        observed.store(true, Ordering::SeqCst);
        Ok(())
    });
    assert!(matches!(result, Err(AdmissionError::Cancelled)));
    assert!(!called.load(Ordering::SeqCst));
    database.close().await.unwrap();
    database.close().await.unwrap();
    assert!(matches!(
        database.admit("closed", &active(), |_| Ok(())),
        Err(AdmissionError::Rejected(DatabaseError::Closed))
    ));
}
