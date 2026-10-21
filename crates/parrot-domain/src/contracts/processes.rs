//! Agent-scoped managed process lifetimes, not detached observer lifetimes.

use std::time::Duration;

use super::async_ops::{BoxFuture, Cancellation, OperationError};
use super::security::AccessSnapshot;

pub trait ManagedProcess: Send + Sync {
    type Owner: Send + Sync + 'static;
    type Signal: Send + 'static;
    type WaitResult: Send + 'static;
    type Snapshot: Send + 'static;
    type Error: Send + Sync + 'static;

    fn owner(&self) -> &Self::Owner;
    fn snapshot(&self) -> Result<Self::Snapshot, Self::Error>;

    /// Writes stdin and observes for the specified yield interval. Observation
    /// cancellation does not detach the underlying agent-owned process.
    fn write_stdin<'a>(
        &'a self,
        input: String,
        yield_after: Duration,
        cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<Self::WaitResult, OperationError<Self::Error>>>;

    fn signal(&self, signal: Self::Signal) -> Result<(), Self::Error>;

    fn wait<'a>(
        &'a self,
        cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<Self::WaitResult, OperationError<Self::Error>>>;

    /// Joins owned process work without caller cancellation, attempting remaining
    /// cleanup after errors before session resources can be released.
    fn settle(&self) -> BoxFuture<'_, Result<(), Self::Error>>;
}

/// Actual launching and inter-agent resolution belong to T38–T42/T48.
pub trait ProcessOwner: Send + Sync {
    type StartRequest: Send + 'static;
    type Security: AccessSnapshot + 'static;
    type Process: ManagedProcess + 'static;
    type Error: Send + Sync + 'static;

    /// Consumes the invocation's captured security, not a mutable live provider.
    /// Dropped start/wait observers must not detach process ownership.
    fn start<'a>(
        &'a self,
        request: Self::StartRequest,
        security: Self::Security,
        cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<Self::Process, OperationError<Self::Error>>>;

    /// Joins all owned processes, attempting all cleanup despite earlier errors.
    fn settle(&self) -> BoxFuture<'_, Result<(), Self::Error>>;
}
