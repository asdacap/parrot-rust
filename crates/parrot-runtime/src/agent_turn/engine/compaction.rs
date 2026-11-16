use super::*;
use parrot_provider::{
    compaction::{CompactionEffects, CompactionEvent, ContextCapture, ReplacementCommit},
    routing::aliases::ResolvedModelSelection,
};

pub(super) struct RuntimeCompactionEffects<'a> {
    pub(super) state: &'a Mutex<State>,
    pub(super) prompt: &'a SessionPrompt,
    pub(super) selection: &'a ResolvedModelSelection,
    pub(super) factory: &'a crate::active_work::status::StatusProvidersFactory,
    pub(super) query: crate::active_work::status::StatusQuery,
    pub(super) profile_prompt: String,
    pub(super) events: ContextEmitter<'a>,
}

impl CompactionEffects for RuntimeCompactionEffects<'_> {
    fn capture_context<'a>(
        &'a mut self,
        cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<ContextCapture, parrot_core::Error>> {
        core_loop::adapt_operation(async move {
            self.prompt
                .sample_epoch(cancellation)
                .await
                .map_err(ContextError::from)?;
            let instructions = self
                .prompt
                .build(self.selection, cancellation)
                .await
                .map_err(ContextError::from)?;
            let state = self.state.lock().await;
            let conversation = state
                .context
                .read_conversation_state(&state.conversation)
                .await?;
            Ok(ContextCapture {
                instructions,
                conversation,
            })
        })
    }

    fn offer_status<'a>(
        &'a self,
        context: parrot_core::ContextSnapshot,
        cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<parrot_core::StatusOffering, parrot_core::Error>> {
        core_loop::adapt_operation(async move {
            self.factory
                .steer(&self.query, &self.profile_prompt, &context, cancellation)
                .await
                .map_err(Into::into)
        })
    }

    fn commit_replacement<'a>(
        &'a mut self,
        replacement: ReplacementCommit,
        _cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<(), parrot_core::Error>> {
        core_loop::adapt_operation(async move {
            let mut guard = self.state.lock().await;
            let state = &mut *guard;
            state
                .context
                .commit_replacement(&mut state.conversation, &self.events, replacement)
                .await?;
            state.agent_state.last_request_statistic.reset();
            Ok(())
        })
    }

    fn publish<'a>(
        &'a self,
        event: CompactionEvent,
        cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<(), parrot_core::Error>> {
        core_loop::adapt_operation(async move {
            self.state
                .lock()
                .await
                .context
                .publish_compaction(event, &self.events, cancellation)
                .await
                .map_err(Into::into)
        })
    }
}
