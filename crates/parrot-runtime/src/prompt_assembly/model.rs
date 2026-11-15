use super::{
    AgentIdentityDescription, PromptError, PromptResult, SystemPrompt, SystemPromptProvider,
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

#[derive(Clone)]
struct ModelAliasDescription {
    name: String,
    model: String,
    usage: String,
    model_usage: Option<String>,
}

struct ModelPrompt {
    profiles: ModelProfiles,
    router: Arc<ModelRouter>,
    templates: Arc<PromptTemplateCatalog>,
    alias_section: Option<(Arc<ModelRoutingSnapshot>, Vec<ModelAliasDescription>)>,
}
impl ModelPrompt {
    fn resolve_aliases(
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

    fn read_aliases(
        &mut self,
        selection: &ResolvedModelSelection,
    ) -> PromptResult<Vec<ModelAliasDescription>> {
        let snapshot = &selection.routing_snapshot;
        if !self
            .alias_section
            .as_ref()
            .is_some_and(|(previous, _)| Arc::ptr_eq(previous, snapshot))
        {
            let aliases = self.resolve_aliases(snapshot)?;
            self.alias_section = Some((snapshot.clone(), aliases));
        }
        Ok(self
            .alias_section
            .as_ref()
            .map(|(_, aliases)| aliases.clone())
            .unwrap_or_default())
    }

    fn render_prompt(
        &self,
        aliases: &[ModelAliasDescription],
        guidance: &Option<String>,
    ) -> PromptResult<String> {
        let template = self
            .templates
            .get_template("context.model-aliases")
            .map_err(|error| PromptError::Template(error.to_string()))?;
        let aliases = aliases
            .iter()
            .filter(|alias| !alias.model.is_empty())
            .map(|alias| {
                let usage = match alias
                    .model_usage
                    .as_deref()
                    .filter(|usage| !usage.is_empty())
                {
                    None => alias.usage.clone(),
                    Some(usage) if alias.usage.is_empty() => usage.to_owned(),
                    Some(usage) => format!("{}\n{usage}", alias.usage),
                };
                json!({"name": alias.name, "model": alias.model, "usage": usage})
            })
            .collect::<Vec<_>>();
        let alias_section = if aliases.is_empty() {
            String::new()
        } else {
            template
                .render(
                    &[("aliases".into(), aliases.into())],
                    parrot_core::CancellationToken::uncancelled(),
                )
                .map_err(PromptError::from)?
        };
        Ok([Some(alias_section.as_str()), guidance.as_deref()]
            .into_iter()
            .flatten()
            .filter(|text| !text.is_empty())
            .collect::<Vec<_>>()
            .join("\n\n"))
    }
}
impl SystemPrompt for ModelPrompt {
    fn renew_epoch(&mut self) {}
    fn read_system_prompts(
        &mut self,
        selection: &ResolvedModelSelection,
    ) -> PromptResult<Vec<(String, String)>> {
        let aliases = self.read_aliases(selection)?;
        let guidance = self
            .profiles
            .resolve(&selection.canonical_model.selector())
            .system_prompt;
        let model_section = if aliases.is_empty() && guidance.is_none() {
            String::new()
        } else {
            self.render_prompt(&aliases, &guidance)?
        };
        let augmentation = selection
            .alias
            .as_ref()
            .and_then(|alias| alias.augment_system_prompt.as_deref());
        let content = [Some(model_section.as_str()), augmentation]
            .into_iter()
            .flatten()
            .filter(|text| !text.is_empty())
            .collect::<Vec<_>>()
            .join("\n\n");
        if content.is_empty() {
            Ok(Vec::new())
        } else {
            Ok(vec![(
                "runtime:system-context:10-model-prompt".into(),
                content,
            )])
        }
    }
}
