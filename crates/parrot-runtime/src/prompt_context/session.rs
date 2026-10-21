use super::{PromptError, PromptResult, SystemPrompt};
use parrot_domain::contracts::async_ops::Cancellation;
use parrot_provider::routing::aliases::ResolvedModelSelection;
use tokio::sync::{Mutex, MutexGuard};

/// The one live prompt product and its epoch. Guards cover sampling/building,
/// never a provider call, tool, status observation, or persistence operation.
pub struct SessionPrompt {
    state: Mutex<State>,
}
struct State {
    prompt: Box<dyn SystemPrompt>,
    sampled: bool,
}
impl SessionPrompt {
    pub fn new(prompt: Box<dyn SystemPrompt>) -> Self {
        Self {
            state: Mutex::new(State {
                prompt,
                sampled: false,
            }),
        }
    }

    async fn lock(&self, cancellation: &dyn Cancellation) -> PromptResult<MutexGuard<'_, State>> {
        cancellation.check().map_err(|_| PromptError::Cancelled)?;
        tokio::select! {
            biased;
            () = cancellation.cancelled() => Err(PromptError::Cancelled),
            guard = self.state.lock() => Ok(guard),
        }
    }

    pub async fn ensure_epoch(&self, cancellation: &dyn Cancellation) -> PromptResult<()> {
        let mut state = self.lock(cancellation).await?;
        if !state.sampled {
            state.prompt.sample_epoch(cancellation).await?;
            state.sampled = true;
        }
        Ok(())
    }

    pub async fn sample_epoch(&self, cancellation: &dyn Cancellation) -> PromptResult<()> {
        let mut state = self.lock(cancellation).await?;
        state.prompt.sample_epoch(cancellation).await?;
        state.sampled = true;
        Ok(())
    }

    pub async fn build(
        &self,
        selection: &ResolvedModelSelection,
        cancellation: &dyn Cancellation,
    ) -> PromptResult<String> {
        self.lock(cancellation)
            .await?
            .prompt
            .build_live(selection, cancellation)
            .await
    }

    /// Capture the live synchronous inputs while the sampled prompt epoch is
    /// stable. The callback cannot await; the guard is released before return.
    pub async fn capture<R>(
        &self,
        selection: &ResolvedModelSelection,
        cancellation: &dyn Cancellation,
        capture: impl FnOnce(&str) -> R,
    ) -> PromptResult<R> {
        let mut state = self.lock(cancellation).await?;
        if !state.sampled {
            state.prompt.sample_epoch(cancellation).await?;
            state.sampled = true;
        }
        let instructions = state.prompt.build_live(selection, cancellation).await?;
        Ok(capture(&instructions))
    }
}
