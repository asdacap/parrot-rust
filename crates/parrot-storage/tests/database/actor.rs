use super::{Cancel, Fixture, active};
use parrot_storage::database::DatabaseError;
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
    mpsc,
};

#[tokio::test]
async fn panic_rolls_back_and_does_not_kill_worker() {
    let fixture = Fixture::new();
    let db = fixture.open_actor_database().await;
    let failed = db
        .admit("panic", &active(), |tx| -> Result<(), DatabaseError> {
            tx.execute("INSERT INTO actor_probe VALUES('panic',1)", [])?;
            panic!("controlled transaction failure");
        })
        .unwrap()
        .into_completion()
        .await;
    assert!(matches!(failed, Err(DatabaseError::CommandPanicked)));
    let rows = db
        .admit("after", &active(), |tx| {
            Ok(tx.query_row("SELECT count(*) FROM actor_probe", [], |r| {
                r.get::<_, i64>(0)
            })?)
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
    let db = fixture.open_actor_database().await;
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
            tx.execute("INSERT INTO actor_probe VALUES('queued',7)", [])?;
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
                "SELECT value FROM actor_probe WHERE name='queued'",
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
    let db = fixture.open_actor_database().await;
    let caller = std::thread::current().id();
    let mut tickets = Vec::new();
    for i in 0..32 {
        tickets.push(
            db.admit(i, &active(), move |tx| {
                let prior: i64 =
                    tx.query_row("SELECT count(*) FROM actor_probe", [], |r| r.get(0))?;
                assert_eq!(prior, i);
                tx.execute(
                    "INSERT INTO actor_probe VALUES(?1,?2)",
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

#[tokio::test]
async fn close_from_another_clone_drains_and_closes_every_clone() {
    let fixture = Fixture::new();
    let db = fixture.open_actor_database().await;
    let clone = db.clone();
    let ticket = db
        .admit("close", &active(), |tx| {
            tx.execute("INSERT INTO actor_probe VALUES('closing',1)", [])?;
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
    let db = fixture.open_actor_database().await;
    let written = Arc::new(AtomicBool::new(false));
    let result = written.clone();
    let ticket = db
        .admit("last", &active(), move |tx| {
            tx.execute("INSERT INTO actor_probe VALUES('last',1)", [])?;
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
            .query_row("SELECT count(*) FROM actor_probe", [], |r| r
                .get::<_, i64>(0))
            .unwrap(),
        1
    );
}
