//! Cancellation observation and explicit ownership of asynchronous work.

use std::fmt;
use std::future::Future;
use std::pin::Pin;
use std::time::Instant;

/// A Send future whose lifetime exposes any borrowed operation inputs.
pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// The caller cancelled an operation before its cancellation boundary.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Cancelled;

impl fmt::Display for Cancelled {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("operation cancelled")
    }
}

impl std::error::Error for Cancelled {}

/// Cancellation is distinct from an operation-specific failure.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum OperationError<E> {
    Cancelled,
    Failed(E),
}

impl<E: fmt::Display> fmt::Display for OperationError<E> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Cancelled => Cancelled.fmt(formatter),
            Self::Failed(error) => error.fmt(formatter),
        }
    }
}

impl<E: std::error::Error + 'static> std::error::Error for OperationError<E> {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Cancelled => None,
            Self::Failed(error) => Some(error),
        }
    }
}

/// A result with cancellation distinct from an operation-specific failure.
pub type OperationResult<T, E> = Result<T, OperationError<E>>;

/// Read-only cancellation observation; only the scope owner may cancel.
pub trait Cancellation: Send + Sync {
    fn is_cancelled(&self) -> bool;

    /// Resolves when cancellation is observed, including prior cancellation.
    fn cancelled(&self) -> BoxFuture<'_, ()>;

    /// Checks the actual current state, rather than assuming success.
    fn check(&self) -> Result<(), Cancelled> {
        if self.is_cancelled() {
            Err(Cancelled)
        } else {
            Ok(())
        }
    }
}

/// Monotonic time, with an observable cancellation boundary for sleeping.
pub trait Clock: Send + Sync {
    fn now(&self) -> Instant;

    /// UTC wall time for expiry and ownership timestamps; not idle deadlines.
    /// Exact source serialization belongs to T13/T14/T21.
    fn utc_now(&self) -> std::time::SystemTime;

    fn sleep_until<'a>(
        &'a self,
        deadline: Instant,
        cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<(), Cancelled>>;
}

/// Work belongs to its scope, not to a caller's observation future.
pub trait OwnedWork: Send + Sync {
    type Error: Send + Sync + 'static;

    /// Joins all owned work without caller cancellation. Attempt remaining
    /// cleanup after failures before releasing any session resources.
    fn settle(&self) -> BoxFuture<'_, Result<(), Self::Error>>;
}

/// Explicit scoped lifetime; dropping an observer does not close the scope.
pub trait ScopeLifetime: Send + Sync {
    type Error: Send + Sync + 'static;

    /// Idempotently closes and joins owned work without caller cancellation.
    /// Attempt all remaining cleanup even after earlier cleanup errors.
    fn close(&self) -> BoxFuture<'_, Result<(), Self::Error>>;
}
