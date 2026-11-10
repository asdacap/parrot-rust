use super::{
    AgentIdentityDescription, render, render_agent_identity, render_agents_files,
    render_model_prompt, render_skill_catalog_entries, render_standing_instruction,
    render_system_prompt_sections,
};
use parrot_core::{ModelAliasDescription, SkillCatalogEntry};
use parrot_domain::contracts::async_ops::{BoxFuture, Cancellation};
use parrot_foundation::template_catalog::PromptTemplateCatalog;
use parrot_provider::routing::aliases::ResolvedModelSelection;
use serde_json::Value;
use std::{collections::HashSet, fmt, sync::Arc};

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum PromptError {
    InvalidProviderKey,
    DuplicateProviderKey(String),
    Template(String),
    Model(String),
    Context(String),
    Read(parrot_domain::contracts::filesystem::FileReadError),
    Cancelled,
}
impl fmt::Display for PromptError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidProviderKey => {
                f.write_str("A system prompt provider key must be namespaced.")
            }
            Self::DuplicateProviderKey(key) => {
                write!(f, "Duplicate system prompt provider key: {key}")
            }
            Self::Template(message) | Self::Model(message) | Self::Context(message) => {
                f.write_str(message)
            }
            Self::Read(error) => error.fmt(f),
            Self::Cancelled => f.write_str("operation cancelled"),
        }
    }
}
impl std::error::Error for PromptError {}
impl From<parrot_core::Error> for PromptError {
    fn from(error: parrot_core::Error) -> Self {
        if error.kind == parrot_core::ErrorKind::Cancelled {
            Self::Cancelled
        } else if error.detail == Some(parrot_core::ErrorDetail::PromptContext) {
            Self::Context(error.message)
        } else {
            Self::Template(error.message)
        }
    }
}
pub type PromptResult<T> = Result<T, PromptError>;

/// Owns raw contributor state. Empty defaults mean this source does not own that contribution.
/// Template expansion is performed only by the shared rendering conveniences.
pub trait SystemPrompt: Send {
    fn renew_epoch(&mut self);
    fn sample_epoch<'a>(
        &'a mut self,
        cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, PromptResult<()>> {
        Box::pin(async move {
            cancellation.check().map_err(|_| PromptError::Cancelled)?;
            self.renew_epoch();
            Ok(())
        })
    }
    fn read_system_prompts(
        &mut self,
        _selection: &ResolvedModelSelection,
    ) -> PromptResult<Vec<(String, String)>> {
        Ok(Vec::new())
    }
    fn read_system_prompts_live<'a>(
        &'a mut self,
        selection: &'a ResolvedModelSelection,
        cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, PromptResult<Vec<(String, String)>>> {
        Box::pin(async move {
            cancellation.check().map_err(|_| PromptError::Cancelled)?;
            self.read_system_prompts(selection)
        })
    }
    fn read_prompt_templates(&self) -> Vec<(String, String)> {
        Vec::new()
    }
    fn read_template_arguments(&self, _source_key: &str) -> Vec<(String, Value)> {
        Vec::new()
    }
    fn get_template_catalog(&self, _source_key: &str) -> Option<&PromptTemplateCatalog> {
        None
    }
    fn read_agents_files(&self) -> PromptResult<Vec<(String, String)>> {
        Ok(Vec::new())
    }
    fn read_model_aliases(
        &mut self,
        _selection: &ResolvedModelSelection,
    ) -> PromptResult<Vec<ModelAliasDescription>> {
        Ok(Vec::new())
    }
    fn read_model_guidance(
        &self,
        _selection: &ResolvedModelSelection,
    ) -> (Option<String>, Option<String>) {
        (None, None)
    }
    fn read_agent_identity(&self) -> Option<AgentIdentityDescription> {
        None
    }
    fn read_standing_instruction(&self) -> String {
        String::new()
    }
    fn read_skill_catalog(&self) -> PromptResult<Vec<SkillCatalogEntry>> {
        Ok(Vec::new())
    }
    fn read_skill_catalog_live<'a>(
        &'a mut self,
        cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, PromptResult<Vec<SkillCatalogEntry>>> {
        Box::pin(async move {
            cancellation.check().map_err(|_| PromptError::Cancelled)?;
            self.read_skill_catalog()
        })
    }
}

impl dyn SystemPrompt + '_ {
    pub fn build(&mut self, selection: &ResolvedModelSelection) -> PromptResult<String> {
        let sections = self.read_system_prompts(selection)?;
        let skills = self.read_skill_catalog()?;
        self.render_sections(selection, sections, skills)
    }

    fn render_sections(
        &mut self,
        selection: &ResolvedModelSelection,
        mut sections: Vec<(String, String)>,
        skills: Vec<SkillCatalogEntry>,
    ) -> PromptResult<String> {
        for (key, template) in self.read_prompt_templates() {
            let arguments = self.read_template_arguments(&key);
            sections.push((
                key.clone(),
                render(self.require_template_catalog(&key)?, &template, &arguments)?,
            ));
        }
        let files = self.read_agents_files()?;
        if !files.is_empty() {
            sections.push(render_agents_files(
                &files,
                self.require_template_catalog("runtime:system-context:07a-agents")?,
            )?);
        }
        let aliases = self.read_model_aliases(selection)?;
        let guidance = self.read_model_guidance(selection);
        if !aliases.is_empty() || guidance.0.is_some() || guidance.1.is_some() {
            sections.push(render_model_prompt(
                &aliases,
                &guidance,
                self.require_template_catalog("runtime:system-context:10-model-prompt")?,
            )?);
        }
        if let Some(identity) = self.read_agent_identity() {
            sections.push(render_agent_identity(
                &identity,
                self.require_template_catalog("runtime:system-context:14-session-identity")?,
            )?);
        }
        let instruction = self.read_standing_instruction();
        if !instruction.is_empty() {
            sections.push(render_standing_instruction(
                &instruction,
                self.require_template_catalog("runtime:system-context:94-standing-instruction")?,
            )?);
        }
        if !skills.is_empty() {
            sections.push(render_skill_catalog_entries(
                &skills,
                self.require_template_catalog("runtime:system-context:09b-skills")?,
            )?);
        }
        Ok(render_system_prompt_sections(sections))
    }

    pub fn build_live<'a>(
        &'a mut self,
        selection: &'a ResolvedModelSelection,
        cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, PromptResult<String>> {
        Box::pin(async move {
            cancellation.check().map_err(|_| PromptError::Cancelled)?;
            let sections = self
                .read_system_prompts_live(selection, cancellation)
                .await?;
            let skills = self.read_skill_catalog_live(cancellation).await?;
            self.render_sections(selection, sections, skills)
        })
    }

    fn require_template_catalog(&self, source_key: &str) -> PromptResult<&PromptTemplateCatalog> {
        self.get_template_catalog(source_key).ok_or_else(|| {
            PromptError::Context(format!(
                "The prompt source '{source_key}' has no template catalog."
            ))
        })
    }
}

/// Source-local ISystemPromptProvider: one product per agent, not a shared session cache.
pub trait SystemPromptProvider: Send + Sync {
    fn key(&self) -> &str;
    fn materialize(
        &self,
        identity: &AgentIdentityDescription,
    ) -> PromptResult<Box<dyn SystemPrompt>>;
}

pub fn is_valid_provider_key(key: &str) -> bool {
    key.find(':').is_some_and(|i| i > 0 && i + 1 < key.len())
        && !key.contains(['\r', '\n', '\t', ' '])
}

pub struct CompositeSystemPromptProvider {
    key: String,
    providers: Vec<Arc<dyn SystemPromptProvider>>,
}
impl CompositeSystemPromptProvider {
    /// Keep the input flat, matching the source's single global ordinal sort.
    pub fn new(
        key: impl Into<String>,
        mut providers: Vec<Arc<dyn SystemPromptProvider>>,
    ) -> PromptResult<Self> {
        let key = key.into();
        if !is_valid_provider_key(&key) {
            return Err(PromptError::InvalidProviderKey);
        }
        let mut keys = HashSet::new();
        for provider in &providers {
            if !is_valid_provider_key(provider.key()) {
                return Err(PromptError::InvalidProviderKey);
            }
            if !keys.insert(provider.key().to_owned()) {
                return Err(PromptError::DuplicateProviderKey(provider.key().to_owned()));
            }
        }
        providers.sort_by(|a, b| a.key().encode_utf16().cmp(b.key().encode_utf16()));
        Ok(Self { key, providers })
    }
}
impl SystemPromptProvider for CompositeSystemPromptProvider {
    fn key(&self) -> &str {
        &self.key
    }
    fn materialize(
        &self,
        identity: &AgentIdentityDescription,
    ) -> PromptResult<Box<dyn SystemPrompt>> {
        let prompts = self
            .providers
            .iter()
            .map(|provider| provider.materialize(identity))
            .collect::<PromptResult<Vec<_>>>()?;
        Ok(Box::new(CompositeSystemPrompt::new(prompts)))
    }
}

pub struct CompositeSystemPrompt {
    prompts: Vec<Box<dyn SystemPrompt>>,
}
impl CompositeSystemPrompt {
    pub fn new(prompts: Vec<Box<dyn SystemPrompt>>) -> Self {
        Self { prompts }
    }
}
impl SystemPrompt for CompositeSystemPrompt {
    fn renew_epoch(&mut self) {
        for prompt in &mut self.prompts {
            prompt.renew_epoch();
        }
    }
    fn sample_epoch<'a>(
        &'a mut self,
        cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, PromptResult<()>> {
        Box::pin(async move {
            for prompt in &mut self.prompts {
                prompt.sample_epoch(cancellation).await?;
            }
            Ok(())
        })
    }
    fn read_system_prompts(
        &mut self,
        selection: &ResolvedModelSelection,
    ) -> PromptResult<Vec<(String, String)>> {
        let mut values = Vec::new();
        for prompt in &mut self.prompts {
            values.extend(prompt.read_system_prompts(selection)?);
        }
        Ok(values)
    }
    fn read_system_prompts_live<'a>(
        &'a mut self,
        selection: &'a ResolvedModelSelection,
        cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, PromptResult<Vec<(String, String)>>> {
        Box::pin(async move {
            let mut values = Vec::new();
            for prompt in &mut self.prompts {
                values.extend(
                    prompt
                        .read_system_prompts_live(selection, cancellation)
                        .await?,
                );
            }
            Ok(values)
        })
    }
    fn read_prompt_templates(&self) -> Vec<(String, String)> {
        self.prompts
            .iter()
            .flat_map(|prompt| prompt.read_prompt_templates())
            .collect()
    }
    fn read_template_arguments(&self, source_key: &str) -> Vec<(String, Value)> {
        self.prompts
            .iter()
            .flat_map(|prompt| prompt.read_template_arguments(source_key))
            .collect()
    }
    fn get_template_catalog(&self, source_key: &str) -> Option<&PromptTemplateCatalog> {
        self.prompts
            .iter()
            .find_map(|prompt| prompt.get_template_catalog(source_key))
    }
    fn read_agents_files(&self) -> PromptResult<Vec<(String, String)>> {
        let mut values = Vec::new();
        for prompt in &self.prompts {
            values.extend(prompt.read_agents_files()?);
        }
        Ok(values)
    }
    fn read_model_aliases(
        &mut self,
        selection: &ResolvedModelSelection,
    ) -> PromptResult<Vec<ModelAliasDescription>> {
        let mut values = Vec::new();
        for prompt in &mut self.prompts {
            values.extend(prompt.read_model_aliases(selection)?);
        }
        Ok(values)
    }
    fn read_model_guidance(
        &self,
        selection: &ResolvedModelSelection,
    ) -> (Option<String>, Option<String>) {
        self.prompts
            .iter()
            .map(|prompt| prompt.read_model_guidance(selection))
            .find(|guidance| guidance.0.is_some() || guidance.1.is_some())
            .unwrap_or((None, None))
    }
    fn read_agent_identity(&self) -> Option<AgentIdentityDescription> {
        self.prompts
            .iter()
            .find_map(|prompt| prompt.read_agent_identity())
    }
    fn read_standing_instruction(&self) -> String {
        self.prompts
            .iter()
            .map(|prompt| prompt.read_standing_instruction())
            .find(|value| !value.is_empty())
            .unwrap_or_default()
    }
    fn read_skill_catalog(&self) -> PromptResult<Vec<SkillCatalogEntry>> {
        let mut values = Vec::new();
        for prompt in &self.prompts {
            values.extend(prompt.read_skill_catalog()?);
        }
        Ok(values)
    }
    fn read_skill_catalog_live<'a>(
        &'a mut self,
        cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, PromptResult<Vec<SkillCatalogEntry>>> {
        Box::pin(async move {
            let mut values = Vec::new();
            for prompt in &mut self.prompts {
                values.extend(prompt.read_skill_catalog_live(cancellation).await?);
            }
            Ok(values)
        })
    }
}

pub struct StaticSystemPrompt(pub String);
impl SystemPrompt for StaticSystemPrompt {
    fn renew_epoch(&mut self) {}
    fn read_system_prompts(
        &mut self,
        _selection: &ResolvedModelSelection,
    ) -> PromptResult<Vec<(String, String)>> {
        Ok(vec![(
            "runtime:system-context:literal".into(),
            self.0.clone(),
        )])
    }
}

pub struct StaticSystemPromptProvider {
    key: String,
    content: String,
}
impl StaticSystemPromptProvider {
    pub fn new(key: impl Into<String>, content: impl Into<String>) -> Self {
        Self {
            key: key.into(),
            content: content.into(),
        }
    }
}
impl SystemPromptProvider for StaticSystemPromptProvider {
    fn key(&self) -> &str {
        &self.key
    }
    fn materialize(&self, _: &AgentIdentityDescription) -> PromptResult<Box<dyn SystemPrompt>> {
        Ok(Box::new(ConfiguredSystemPrompt {
            key: self.key.clone(),
            content: self.content.clone(),
        }))
    }
}

struct ConfiguredSystemPrompt {
    key: String,
    content: String,
}
impl SystemPrompt for ConfiguredSystemPrompt {
    fn renew_epoch(&mut self) {}
    fn read_system_prompts(
        &mut self,
        _selection: &ResolvedModelSelection,
    ) -> PromptResult<Vec<(String, String)>> {
        Ok(vec![(self.key.clone(), self.content.clone())])
    }
}
