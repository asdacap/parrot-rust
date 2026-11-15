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
    /// Publishes the turn start event and returns its durable event id, which keys the turn.
    fn report_turn_started<'a>(
        &'a self,
        _model: &'a crate::TurnModel,
        _cancellation: CancellationToken<'a>,
    ) -> BoxFuture<'a, Result<String, Error>> {
        Box::pin(async { Err(Error::unsupported("EventPublisher.report_turn_started")) })
    }
}
