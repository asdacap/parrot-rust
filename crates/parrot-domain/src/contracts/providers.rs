//! Provider ownership and lossless associated request/event values.

use super::async_ops::{BoxFuture, Cancellation, OperationResult};

/// Read-idle timeout starts before the first read and resets with read activity;
/// it is not a total-request deadline. Retry policy belongs to T25–T35.
pub trait ProviderStream: Send + Sync {
    type Event: Send + 'static;
    type Error: Send + Sync + 'static;

    fn next<'a>(
        &'a self,
        cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, OperationResult<Option<Self::Event>, Self::Error>>;
}

/// Requests/events retain complete provider content, including unknown fields;
/// implementations must not reconstruct them from just known protocol fields.
pub trait ProviderSession: Send + Sync {
    type Request: Send + 'static;
    type Event: Send + 'static;
    type Error: Send + Sync + 'static;

    /// Returns an owning stream, not one borrowing the send call's inputs.
    fn send<'a>(
        &'a self,
        request: Self::Request,
        cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, ProviderSendResult<Self::Event, Self::Error>>;

    /// Joins session-owned work and attempts remaining cleanup after errors;
    /// caller cancellation cannot release resources early. Idempotent.
    fn close(&self) -> BoxFuture<'_, Result<(), Self::Error>>;
}

/// Successful send transfers an owning stream to the observer.
pub type ProviderSendResult<Event, Error> =
    OperationResult<OwnedProviderStream<Event, Error>, Error>;

pub type OwnedProviderStream<Event, Error> =
    Box<dyn ProviderStream<Event = Event, Error = Error> + 'static>;
