use super::{BoxFuture, CancellationToken, Error};

/// Publishes core-selected notifications independently of conversation state.
pub trait EventPublisher: Send + Sync {
    /// Records and publishes the supplied notification.
    fn publish<'a>(
        &'a self,
        _event: crate::ConversationEvent,
        _cancellation: CancellationToken<'a>,
    ) -> BoxFuture<'a, Result<(), Error>> {
        Box::pin(async { Err(Error::unsupported("EventPublisher.publish")) })
    }
}
