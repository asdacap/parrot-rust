use super::{
    AgentIdentityDescription, PromptError, PromptResult, SystemPrompt, SystemPromptProvider,
};
use parrot_core::ModelAliasDescription;
use parrot_domain::values::config::ModelProfiles;
use parrot_foundation::template_catalog::PromptTemplateCatalog;
use parrot_provider::routing::aliases::{
    ModelRouter, ModelRoutingSnapshot, ResolvedModelSelection,
};
use std::sync::Arc;

pub struct ModelPromptProvider {
    profiles: ModelProfiles,
    router: Arc<ModelRouter>,
    templates: Arc<PromptTemplateCatalog>,
}
impl ModelPromptProvider {
    pub fn new(
        profiles: ModelProfiles,
        router: Arc<ModelRouter>,
        templates: Arc<PromptTemplateCatalog>,
    ) -> Self {
        Self {
            profiles,
            router,
            templates,
        }
    }
}
impl SystemPromptProvider for ModelPromptProvider {
    fn key(&self) -> &str {
        "runtime:system-context:10-model-prompt"
    }
    fn materialize(&self, _: &AgentIdentityDescription) -> PromptResult<Box<dyn SystemPrompt>> {
        Ok(Box::new(ModelPrompt {
            profiles: self.profiles.clone(),
            router: self.router.clone(),
            templates: self.templates.clone(),
            alias_section: None,
        }))
    }
}

struct ModelPrompt {
    profiles: ModelProfiles,
    router: Arc<ModelRouter>,
    templates: Arc<PromptTemplateCatalog>,
    alias_section: Option<(Arc<ModelRoutingSnapshot>, Vec<ModelAliasDescription>)>,
}
impl ModelPrompt {
    fn read_aliases(
        &self,
        snapshot: &Arc<ModelRoutingSnapshot>,
    ) -> PromptResult<Vec<ModelAliasDescription>> {
        let mut aliases = Vec::new();
        // The actual ModelAliasSnapshot has source ordinal ordering already.
        for alias in snapshot
            .aliases
            .definitions()
            .iter()
            .filter(|alias| !alias.model_string.is_empty())
        {
            let model_usage = match self.router.resolve_from(snapshot.clone(), &alias.name) {
                Ok(resolved) => {
                    self.profiles
                        .resolve(&resolved.canonical_model.selector())
                        .usage
                }
                // Source swallows LLMProviderException, not arbitrary failures.
                Err(parrot_provider::sessions::Error::Provider(_)) => None,
                Err(error) => return Err(PromptError::Model(error.to_string())),
            }
            .filter(|usage| !usage.is_empty());
            aliases.push(ModelAliasDescription {
                name: alias.name.clone(),
                model: alias.model_string.clone(),
                usage: alias.usage.clone(),
                model_usage,
            });
        }
        Ok(aliases)
    }
}
impl SystemPrompt for ModelPrompt {
    fn renew_epoch(&mut self) {}
    fn read_model_aliases(
        &mut self,
        selection: &ResolvedModelSelection,
    ) -> PromptResult<Vec<ModelAliasDescription>> {
        let snapshot = &selection.routing_snapshot;
        if !self
            .alias_section
            .as_ref()
            .is_some_and(|(previous, _)| Arc::ptr_eq(previous, snapshot))
        {
            let text = self.read_aliases(snapshot)?;
            self.alias_section = Some((snapshot.clone(), text));
        }
        Ok(self
            .alias_section
            .as_ref()
            .map(|(_, aliases)| aliases.clone())
            .unwrap_or_default())
    }
    fn read_system_prompts(
        &mut self,
        selection: &ResolvedModelSelection,
    ) -> PromptResult<Vec<(String, String)>> {
        Ok(selection
            .alias
            .as_ref()
            .and_then(|alias| alias.augment_system_prompt.clone())
            .map(|content| ("runtime:system-context:10-model-prompt".into(), content))
            .into_iter()
            .collect())
    }
    fn read_model_system_prompt(&self, selection: &ResolvedModelSelection) -> Option<String> {
        self.profiles
            .resolve(&selection.canonical_model.selector())
            .system_prompt
    }
    fn get_template_catalog(&self, source_key: &str) -> Option<&PromptTemplateCatalog> {
        (source_key == "runtime:system-context:10-model-prompt").then_some(self.templates.as_ref())
    }
}
