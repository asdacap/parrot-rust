use super::BoxFuture;
use std::fmt;

/// The caller cancelled an operation before its cancellation boundary.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Cancelled;

impl fmt::Display for Cancelled {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("operation cancelled")
    }
}

impl std::error::Error for Cancelled {}

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

/// Borrows live cancellation observation without owning a signal or background work.
#[derive(Clone, Copy)]
pub struct CancellationToken<'a> {
    cancellation: Option<&'a dyn Cancellation>,
}

impl<'a> CancellationToken<'a> {
    pub fn new(cancellation: &'a dyn Cancellation) -> Self {
        Self {
            cancellation: Some(cancellation),
        }
    }

    pub fn uncancelled() -> CancellationToken<'static> {
        CancellationToken { cancellation: None }
    }

    pub fn is_cancelled(&self) -> bool {
        self.cancellation
            .is_some_and(|cancellation| cancellation.is_cancelled())
    }

    pub fn cancelled(&self) -> BoxFuture<'a, ()> {
        match self.cancellation {
            Some(cancellation) => cancellation.cancelled(),
            None => Box::pin(std::future::pending()),
        }
    }
}

impl Cancellation for CancellationToken<'_> {
    fn is_cancelled(&self) -> bool {
        self.is_cancelled()
    }

    fn cancelled(&self) -> BoxFuture<'_, ()> {
        self.cancelled()
    }
}
