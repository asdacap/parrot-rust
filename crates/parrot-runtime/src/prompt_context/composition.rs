use super::AgentIdentityDescription;
use parrot_domain::contracts::async_ops::{BoxFuture, Cancellation};
use parrot_provider::routing::aliases::ResolvedModelSelection;
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
pub type PromptResult<T> = Result<T, PromptError>;

/// Source-local ISystemPrompt product. Every materialization owns its epoch state.
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
    fn build(&mut self, selection: &ResolvedModelSelection) -> PromptResult<String>;
    /// Builds live contributors without renewing the prompt epoch.
    fn build_live<'a>(
        &'a mut self,
        selection: &'a ResolvedModelSelection,
        cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, PromptResult<String>> {
        Box::pin(async move {
            cancellation.check().map_err(|_| PromptError::Cancelled)?;
            self.build(selection)
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
    fn build(&mut self, selection: &ResolvedModelSelection) -> PromptResult<String> {
        let mut sections = Vec::with_capacity(self.prompts.len());
        for prompt in &mut self.prompts {
            let section = prompt.build(selection)?;
            if !section.chars().all(char::is_whitespace) {
                sections.push(section);
            }
        }
        Ok(sections.join("\n\n"))
    }
    fn build_live<'a>(
        &'a mut self,
        selection: &'a ResolvedModelSelection,
        cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, PromptResult<String>> {
        Box::pin(async move {
            let mut sections = Vec::with_capacity(self.prompts.len());
            for prompt in &mut self.prompts {
                let section = prompt.build_live(selection, cancellation).await?;
                if !section.chars().all(char::is_whitespace) {
                    sections.push(section);
                }
            }
            Ok(sections.join("\n\n"))
        })
    }
}

pub struct StaticSystemPrompt(pub String);
impl SystemPrompt for StaticSystemPrompt {
    fn renew_epoch(&mut self) {}
    fn build(&mut self, _: &ResolvedModelSelection) -> PromptResult<String> {
        Ok(self.0.clone())
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
        Ok(Box::new(StaticSystemPrompt(self.content.clone())))
    }
}
