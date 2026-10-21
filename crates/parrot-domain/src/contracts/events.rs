//! Nonblocking bounded fanout contracts, not producer-blocking backpressure.

use super::async_ops::{BoxFuture, Cancellation, OperationError, OperationResult};
use super::storage::Committed;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RetentionClass {
    Retained,
    Transient,
}

/// Separate bounded lossy capacities; source defaults are 1024 per class.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SubscriptionLimits {
    pub retained: usize,
    pub transient: usize,
}

impl Default for SubscriptionLimits {
    fn default() -> Self {
        Self {
            retained: 1024,
            transient: 1024,
        }
    }
}

pub trait EventSubscription: Send + Sync {
    type Event: Send + 'static;
    type Error: Send + Sync + 'static;

    /// Reads until completion and drain; foreground turn completion does not
    /// complete the stream. Pending inventory replaces by kind/owner, and
    /// batches remain contiguous even with bounded per-class retention.
    fn next<'a>(
        &'a self,
        cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, OperationResult<Option<Self::Event>, Self::Error>>;

    /// Stops admission, but preserves queued events for draining before None.
    fn complete(&self) -> Result<(), Self::Error>;
}

pub trait EventPublisher: Send + Sync {
    type Event: Send + Sync + 'static;
    type Error: Send + Sync + 'static;

    /// Publishes an already durable event without performing persistence.
    /// Cancellation of publication cannot invalidate the committed value.
    fn publish_committed<'a>(
        &'a self,
        event: &'a Committed<Self::Event>,
        cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<(), OperationError<Self::Error>>>;

    /// Fanout is nonblocking and bounded/lossy, never producer backpressure.
    fn publish_transient<'a>(
        &'a self,
        event: Self::Event,
        cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<(), OperationError<Self::Error>>>;
}
