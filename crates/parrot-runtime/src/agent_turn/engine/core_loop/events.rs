use super::*;

pub(super) struct RuntimeEventPublisher<'a> {
    pub(super) events: ContextEmitter<'a>,
    pub(super) prompt: &'a SessionPrompt,
    pub(super) state: &'a Mutex<State>,
    pub(super) cancellation: &'a dyn Cancellation,
}

impl parrot_core::EventPublisher for RuntimeEventPublisher<'_> {
    fn publish<'a>(
        &'a self,
        event: parrot_core::ConversationEvent,
        cancellation: parrot_core::CancellationToken<'a>,
    ) -> BoxFuture<'a, Result<(), CoreError>> {
        adapt_operation(async move {
            self.events
                .emit(
                    conversation::render_event(self.events.event(), event),
                    &cancellation,
                )
                .await
        })
    }
    fn on_epoch_started(&self) -> BoxFuture<'_, Result<(), CoreError>> {
        adapt_operation(async move {
            self.prompt
                .sample_epoch(self.cancellation)
                .await
                .map_err(Into::into)
        })
    }
    fn report_compaction_started(&self) -> BoxFuture<'_, Result<(), CoreError>> {
        adapt_operation(async move {
            self.state
                .lock()
                .await
                .context
                .report_epoch_started(&self.events)
                .await
                .map_err(Into::into)
        })
    }
    fn report_compaction_failed(&self, message: String) -> BoxFuture<'_, Result<(), CoreError>> {
        adapt_operation(async move {
            self.state
                .lock()
                .await
                .context
                .fail_epoch_compaction(message, &self.events)
                .await
                .map_err(Into::into)
        })
    }
}
