//! One selected-session SQLite connection and actor-owned whole transactions.
//!
//! Opening is synchronous, like the source database constructor. Async callers
//! should open on their blocking pool. Once open, SQLite work runs exclusively
//! on the dedicated worker, never on the caller's executor thread.

mod schema;

use parrot_domain::contracts::async_ops::{BoxFuture, Cancellation};
use parrot_domain::contracts::paths::SelectedSessionRoot;
use parrot_domain::contracts::storage::{
    AdmissionError, CommitReceipt, CommitTicket, Committed, SessionTransactions, TransactionCommand,
};
use std::fmt;
use std::marker::PhantomData;
use std::path::Path;
use std::sync::{Arc, Mutex, mpsc};
use tokio::sync::{oneshot, watch};

pub use rusqlite::{Connection, Transaction};
pub use schema::{extract_usage_keys, record_agent_start};

/// Database failures stay observable through admission or completion.
#[derive(Debug)]
pub enum DatabaseError {
    Sqlite(rusqlite::Error),
    Io(std::io::Error),
    Protobuf(protobuf::Error),
    Closed,
    CommandPanicked,
    WorkerStopped,
}

impl fmt::Display for DatabaseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Sqlite(error) => error.fmt(f),
            Self::Io(error) => error.fmt(f),
            Self::Protobuf(error) => error.fmt(f),
            Self::Closed => f.write_str("session database is closed"),
            Self::CommandPanicked => f.write_str("database command panicked"),
            Self::WorkerStopped => f.write_str("database worker stopped before completion"),
        }
    }
}

impl std::error::Error for DatabaseError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Sqlite(error) => Some(error),
            Self::Io(error) => Some(error),
            Self::Protobuf(error) => Some(error),
            _ => None,
        }
    }
}

impl From<rusqlite::Error> for DatabaseError {
    fn from(error: rusqlite::Error) -> Self {
        Self::Sqlite(error)
    }
}
impl From<std::io::Error> for DatabaseError {
    fn from(error: std::io::Error) -> Self {
        Self::Io(error)
    }
}
impl From<protobuf::Error> for DatabaseError {
    fn from(error: protobuf::Error) -> Self {
        Self::Protobuf(error)
    }
}

type Work = Box<dyn FnOnce(&mut Connection) + Send>;
enum Job {
    Transaction(Work),
    Close,
}

struct Inner {
    root: SelectedSessionRoot,
    sender: Mutex<Option<mpsc::Sender<Job>>>,
    settled: watch::Receiver<bool>,
}

/// Clones share one worker and connection, not independent SQLite gates.
#[derive(Clone)]
pub struct SessionDatabase {
    inner: Arc<Inner>,
}

impl SessionDatabase {
    /// Opens `session.db` inside an already selected session directory.
    pub fn open(root: SelectedSessionRoot) -> Result<Self, DatabaseError> {
        let path = root.path().join("session.db");
        Self::open_file(root, path)
    }

    /// Opens an in-memory synthetic database with the same schema and worker.
    /// Production session stores should use `open`; no file fallback occurs.
    pub fn open_memory(root: SelectedSessionRoot) -> Result<Self, DatabaseError> {
        Self::open_file(root, Path::new(":memory:"))
    }

    fn open_file(root: SelectedSessionRoot, path: impl AsRef<Path>) -> Result<Self, DatabaseError> {
        let path = path.as_ref().to_owned();
        let (sender, receiver) = mpsc::channel();
        let (started_tx, started_rx) = mpsc::sync_channel(1);
        let (settled_tx, settled) = watch::channel(false);
        std::thread::Builder::new()
            .name("session-database".into())
            .spawn(move || {
                let opened = schema::open(&path);
                match opened {
                    Ok(mut connection) => {
                        if started_tx.send(Ok(())).is_ok() {
                            for job in receiver {
                                match job {
                                    Job::Transaction(work) => work(&mut connection),
                                    Job::Close => break,
                                }
                            }
                        }
                        drop(connection);
                    }
                    Err(error) => {
                        let _ = started_tx.send(Err(error));
                    }
                }
                let _ = settled_tx.send(true);
            })?;
        started_rx
            .recv()
            .map_err(|_| DatabaseError::WorkerStopped)??;
        Ok(Self {
            inner: Arc::new(Inner {
                root,
                sender: Mutex::new(Some(sender)),
                settled,
            }),
        })
    }

    pub fn selected_root(&self) -> &SelectedSessionRoot {
        &self.inner.root
    }

    /// Atomically transfers a complete transaction to the worker. Cancellation
    /// only wins before this transfer; dropping the returned ticket cannot cancel
    /// admitted work. The closure must not commit/rollback its supplied transaction.
    pub fn admit<K, T, F>(
        &self,
        key: K,
        cancellation: &dyn Cancellation,
        operation: F,
    ) -> Result<CommitTicket<K, T, DatabaseError>, AdmissionError<DatabaseError>>
    where
        K: Clone + Eq + Send + Sync + 'static,
        T: Send + 'static,
        F: FnOnce(&Transaction<'_>) -> Result<T, DatabaseError> + Send + 'static,
    {
        let sender = self
            .inner
            .sender
            .lock()
            .expect("database admission mutex poisoned");
        if cancellation.is_cancelled() {
            return Err(AdmissionError::Cancelled);
        }
        let sender = sender
            .as_ref()
            .ok_or(AdmissionError::Rejected(DatabaseError::Closed))?;
        let (finished_tx, finished_rx) = oneshot::channel();
        let receipt_key = key.clone();
        let work = Box::new(move |connection: &mut Connection| {
            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                // Microsoft.Data.Sqlite's default BeginTransaction is immediate.
                let transaction = connection
                    .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
                let output = operation(&transaction)?;
                transaction.commit()?;
                Ok(Committed::after_commit(CommitReceipt::new(
                    receipt_key,
                    output,
                )))
            }))
            .unwrap_or(Err(DatabaseError::CommandPanicked));
            let _ = finished_tx.send(result);
        });
        sender
            .send(Job::Transaction(work))
            .map_err(|_| AdmissionError::Rejected(DatabaseError::WorkerStopped))?;
        Ok(CommitTicket::new(
            key,
            Box::pin(async move {
                finished_rx
                    .await
                    .map_err(|_| DatabaseError::WorkerStopped)?
            }),
        ))
    }

    /// Rejects future admission, drains admitted work and closes the connection.
    /// Idempotent across clones; losing this wait does not stop worker settlement.
    pub async fn close(&self) -> Result<(), DatabaseError> {
        {
            let mut sender = self
                .inner
                .sender
                .lock()
                .expect("database admission mutex poisoned");
            if let Some(sender) = sender.take() {
                let _ = sender.send(Job::Close);
            }
        }
        let mut settled = self.inner.settled.clone();
        while !*settled.borrow_and_update() {
            settled
                .changed()
                .await
                .map_err(|_| DatabaseError::WorkerStopped)?;
        }
        Ok(())
    }

    /// Binds a repository command type to the shared T07 transaction contract.
    pub fn transactions<C: DatabaseCommand>(&self) -> TransactionSession<C> {
        TransactionSession {
            database: self.clone(),
            command: PhantomData,
        }
    }
}

/// Repository-owned typed command executed wholly inside a single transaction.
pub trait DatabaseCommand: TransactionCommand {
    fn execute(self, transaction: &Transaction<'_>) -> Result<Self::Output, DatabaseError>;
}

pub struct TransactionSession<C> {
    database: SessionDatabase,
    command: PhantomData<fn(C)>,
}

impl<C: DatabaseCommand> SessionTransactions for TransactionSession<C> {
    type Command = C;
    type Error = DatabaseError;
    fn selected_root(&self) -> &SelectedSessionRoot {
        self.database.selected_root()
    }
    fn admit<'a>(
        &'a self,
        command: C,
        cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<
        'a,
        Result<CommitTicket<C::Key, C::Output, DatabaseError>, AdmissionError<DatabaseError>>,
    > {
        Box::pin(async move {
            let key = command.key().clone();
            self.database.admit(key, cancellation, move |transaction| {
                command.execute(transaction)
            })
        })
    }
}
