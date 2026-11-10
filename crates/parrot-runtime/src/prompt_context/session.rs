use super::{PromptError, PromptResult, SystemPrompt};
use parrot_core::{AgentIdentityDescription, ModelAliasDescription, SkillCatalogEntry};
use parrot_domain::contracts::async_ops::Cancellation;
use parrot_provider::routing::aliases::ResolvedModelSelection;
use serde_json::Value;
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

    pub async fn read_system_prompts(
        &self,
        selection: &ResolvedModelSelection,
        cancellation: &dyn Cancellation,
    ) -> PromptResult<Vec<(String, String)>> {
        self.lock(cancellation)
            .await?
            .prompt
            .read_system_prompts_live(selection, cancellation)
            .await
    }

    pub async fn read_prompt_templates(
        &self,
        cancellation: &dyn Cancellation,
    ) -> PromptResult<Vec<(String, String)>> {
        Ok(self
            .lock(cancellation)
            .await?
            .prompt
            .read_prompt_templates())
    }

    pub async fn read_core_prompt_bindings(
        &self,
        cancellation: &dyn Cancellation,
    ) -> PromptResult<Vec<(String, parrot_core::ConfigKey)>> {
        self.read_prompt_templates(cancellation)
            .await?
            .into_iter()
            .map(|(source, id)| {
                parrot_core::ConfigKey::from_prompt_template_id(&id)
                    .map(|key| (source, key))
                    .map_err(Into::into)
            })
            .collect()
    }

    pub async fn read_agents_files(
        &self,
        cancellation: &dyn Cancellation,
    ) -> PromptResult<Vec<(String, String)>> {
        self.lock(cancellation).await?.prompt.read_agents_files()
    }

    pub async fn read_agent_identity(
        &self,
        cancellation: &dyn Cancellation,
    ) -> PromptResult<Option<AgentIdentityDescription>> {
        Ok(self.lock(cancellation).await?.prompt.read_agent_identity())
    }

    pub async fn read_standing_instruction(
        &self,
        cancellation: &dyn Cancellation,
    ) -> PromptResult<String> {
        Ok(self
            .lock(cancellation)
            .await?
            .prompt
            .read_standing_instruction())
    }

    pub async fn read_template_arguments(
        &self,
        source_key: &str,
        cancellation: &dyn Cancellation,
    ) -> PromptResult<Vec<(String, Value)>> {
        Ok(self
            .lock(cancellation)
            .await?
            .prompt
            .read_template_arguments(source_key))
    }

    pub async fn read_model_aliases(
        &self,
        selection: &ResolvedModelSelection,
        cancellation: &dyn Cancellation,
    ) -> PromptResult<Vec<ModelAliasDescription>> {
        self.lock(cancellation)
            .await?
            .prompt
            .read_model_aliases(selection)
    }

    pub async fn read_model_guidance(
        &self,
        selection: &ResolvedModelSelection,
        cancellation: &dyn Cancellation,
    ) -> PromptResult<(Option<String>, Option<String>)> {
        Ok(self
            .lock(cancellation)
            .await?
            .prompt
            .read_model_guidance(selection))
    }

    pub async fn read_skill_catalog(
        &self,
        cancellation: &dyn Cancellation,
    ) -> PromptResult<Vec<SkillCatalogEntry>> {
        self.lock(cancellation)
            .await?
            .prompt
            .read_skill_catalog_live(cancellation)
            .await
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
