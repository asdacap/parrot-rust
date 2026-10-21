//! Whole-command admission is distinct from observation and durable completion.

use super::async_ops::{BoxFuture, Cancellation};
use super::paths::SelectedSessionRoot;

/// An entire repository transaction, never an individual SQL statement.
pub trait TransactionCommand: Send + 'static {
    type Key: Clone + Eq + Send + Sync + 'static;
    type Output: Send + 'static;

    fn key(&self) -> &Self::Key;
}

/// A fully durable value, distinct from an uncommitted event.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Committed<T>(T);

impl<T> Committed<T> {
    /// Implementer attestation that the entire transaction has durably committed.
    /// This constructor does not persist anything or prove SQLite correctness.
    pub fn after_commit(value: T) -> Self {
        Self(value)
    }

    pub fn value(&self) -> &T {
        &self.0
    }

    pub fn into_value(self) -> T {
        self.0
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CommitReceipt<Key, Output> {
    key: Key,
    output: Output,
}

impl<Key, Output> CommitReceipt<Key, Output> {
    pub fn new(key: Key, output: Output) -> Self {
        Self { key, output }
    }

    pub fn key(&self) -> &Key {
        &self.key
    }

    pub fn output(&self) -> &Output {
        &self.output
    }

    pub fn into_output(self) -> Output {
        self.output
    }
}

/// Observation of actor-owned execution. Dropping this ticket or its completion
/// future must not abort execution or partially apply the transaction.
pub struct CommitTicket<Key, Output, Error> {
    key: Key,
    completion: BoxFuture<'static, Result<Committed<CommitReceipt<Key, Output>>, Error>>,
}

impl<Key, Output, Error> CommitTicket<Key, Output, Error> {
    /// The actor must already own the whole command, independently of this future.
    pub fn new(
        key: Key,
        completion: BoxFuture<'static, Result<Committed<CommitReceipt<Key, Output>>, Error>>,
    ) -> Self {
        Self { key, completion }
    }

    pub fn key(&self) -> &Key {
        &self.key
    }

    pub fn into_completion(
        self,
    ) -> BoxFuture<'static, Result<Committed<CommitReceipt<Key, Output>>, Error>> {
        self.completion
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum AdmissionError<E> {
    Cancelled,
    Rejected(E),
}

/// Session-scoped transaction actor. Retries reuse the same key, preserve exact
/// payload conflict detection, and never duplicate durable admission.
pub trait SessionTransactions: Send + Sync {
    type Command: TransactionCommand;
    type Error: Send + Sync + 'static;

    fn selected_root(&self) -> &SelectedSessionRoot;

    /// Cancellation can reject only before the actor owns the whole command.
    /// Later failures belong to completion; observation never owns execution.
    fn admit<'a>(
        &'a self,
        command: Self::Command,
        cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<TransactionTicket<Self>, AdmissionError<Self::Error>>>;
}

/// The strongly typed ticket for a transaction actor's command.
pub type TransactionTicket<T> = CommitTicket<
    <<T as SessionTransactions>::Command as TransactionCommand>::Key,
    <<T as SessionTransactions>::Command as TransactionCommand>::Output,
    <T as SessionTransactions>::Error,
>;
