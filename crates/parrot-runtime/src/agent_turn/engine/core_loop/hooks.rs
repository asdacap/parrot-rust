use super::*;
use parrot_core::TurnHooks;

pub(super) struct RuntimeTurnHooks<'a, S: AccessSnapshot + 'static> {
    pub(super) backend: &'a RuntimeBackend<S>,
    pub(super) binding: Arc<TurnBinding<S>>,
    pub(super) cancellation: &'a dyn Cancellation,
}

impl<S: AccessSnapshot + 'static> RuntimeTurnHooks<'_, S> {
    /// Notifies provider sessions before the turn start event is published.
    pub(super) fn begin_conversation_turn(&self) -> BoxFuture<'_, Result<(), CoreError>> {
        adapt_operation(async move {
            self.backend.sessions.begin_turn().await?;
            Ok(())
        })
    }
}

impl<S: AccessSnapshot + 'static> TurnHooks for RuntimeTurnHooks<'_, S> {
    fn activate_conversation_turn(&self) -> BoxFuture<'_, Result<(), CoreError>> {
        adapt_operation(async move {
            self.backend.activity.turn_started();
            self.backend
                .state
                .lock()
                .await
                .context
                .ensure_epoch(&self.backend.prompt, self.cancellation)
                .await?;
            self.backend.skills.begin_turn(self.cancellation).await?;
            for source in &self.backend.completion_sources {
                source.begin_turn();
            }
            Ok(())
        })
    }

    fn begin_step(&self) -> BoxFuture<'_, Result<(), CoreError>> {
        adapt_operation(async move {
            self.binding.bind(self.backend.selection.capture()?);
            Ok(())
        })
    }
}
