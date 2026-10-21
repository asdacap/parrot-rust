use super::{
    AgentIdentityDescription, PromptError, PromptResult, SystemPrompt, SystemPromptProvider, render,
};
use parrot_domain::values::config::ModelProfiles;
use parrot_foundation::template_catalog::PromptTemplateCatalog;
use parrot_provider::routing::aliases::{
    ModelRouter, ModelRoutingSnapshot, ResolvedModelSelection,
};
use serde_json::json;
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
    alias_section: Option<(Arc<ModelRoutingSnapshot>, String)>,
}
impl ModelPrompt {
    fn render_aliases(&self, snapshot: &Arc<ModelRoutingSnapshot>) -> PromptResult<String> {
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
            let usage = match model_usage {
                None => alias.usage.clone(),
                Some(usage) if alias.usage.is_empty() => usage,
                Some(usage) => format!("{}\n{usage}", alias.usage),
            };
            aliases.push(json!({"name": alias.name, "model": alias.model_string, "usage": usage}));
        }
        if aliases.is_empty() {
            Ok(String::new())
        } else {
            render(
                &self.templates,
                "context.model-aliases",
                &[("aliases".into(), aliases.into())],
            )
        }
    }
}
impl SystemPrompt for ModelPrompt {
    fn renew_epoch(&mut self) {}
    fn build(&mut self, selection: &ResolvedModelSelection) -> PromptResult<String> {
        let snapshot = &selection.routing_snapshot;
        if !self
            .alias_section
            .as_ref()
            .is_some_and(|(previous, _)| Arc::ptr_eq(previous, snapshot))
        {
            let text = self.render_aliases(snapshot)?;
            self.alias_section = Some((snapshot.clone(), text));
        }
        let profile = self.profiles.resolve(&selection.canonical_model.selector());
        let sections = [
            self.alias_section.as_ref().map(|(_, text)| text.as_str()),
            profile.system_prompt.as_deref(),
            selection
                .alias
                .as_ref()
                .and_then(|alias| alias.augment_system_prompt.as_deref()),
        ];
        // ModelPrompt excludes only null/empty strings; unlike CompositeSystemPrompt,
        // whitespace guidance is retained. The selected agent-profile prompt is not an input.
        Ok(sections
            .into_iter()
            .flatten()
            .filter(|text| !text.is_empty())
            .collect::<Vec<_>>()
            .join("\n\n"))
    }
}
