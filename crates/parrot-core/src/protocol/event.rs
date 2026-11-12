use super::{BoxFuture, CancellationToken, Error};

/// Publishes core-selected notifications independently of conversation state.
pub trait EventPublisher: Send + Sync {
    /// Records and publishes the supplied event without adding conversation content.
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
    /// Begins a prompt epoch before instructions are read, awaiting native refresh hooks.
    fn on_epoch_started(&self) -> BoxFuture<'_, Result<(), Error>>;
    /// Publishes compaction start without request cancellation.
    fn report_compaction_started(&self) -> BoxFuture<'_, Result<(), Error>>;
    /// Publishes compaction failure without request cancellation.
    fn report_compaction_failed(&self, message: String) -> BoxFuture<'_, Result<(), Error>>;
    /// Publishes compaction completion without request cancellation.
    fn report_compaction_finished(&self) -> BoxFuture<'_, Result<(), Error>>;
    /// Publishes the saving phase before fitted compaction persistence.
    fn report_compaction_saving<'a>(
        &'a self,
        _cancellation: CancellationToken<'a>,
    ) -> BoxFuture<'a, Result<(), Error>> {
        Box::pin(async {
            Err(Error::unsupported(
                "EventPublisher.report_compaction_saving",
            ))
        })
    }
    /// Publishes aggregate compaction progress without disclosing generated content.
    fn report_compaction_progress<'a, 'c>(
        &'a self,
        progress: super::compaction::CompactionProgress,
        cancellation: CancellationToken<'c>,
    ) -> BoxFuture<'a, Result<(), Error>>
    where
        'c: 'a;
}
