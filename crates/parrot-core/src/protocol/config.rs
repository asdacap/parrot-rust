use super::Error;
use super::compaction::CompactionConfiguration;
use super::context::ContextConfiguration;
use super::turn::TurnOptions;
use std::sync::Arc;

/// Selects a typed runtime configuration value without an additional service contract.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub enum ConfigKey {
    Turn,
    Context,
    Compaction,
    SkillUnavailableTemplate,
    SkillSelectedTemplate,
    SkillTooLargeTemplate,
    ContextReminderTemplate,
    ContextLimitReminderTemplate,
    OversizedToolGroupTemplate,
    FinalRequestTemplate,
    RestoredToolsTemplate,
    InterruptedTemplate,
    TruncatedToolCallTemplate,
    InvalidToolCallTemplate,
    CrashedToolTemplate,
    ToolErrorTemplate,
}

pub enum ConfigValue {
    Turn(TurnOptions),
    Context(ContextConfiguration),
    Compaction(CompactionConfiguration),
    PromptTemplate(Arc<PromptTemplate>),
}

impl ConfigValue {
    pub fn into_turn(self) -> Result<TurnOptions, Error> {
        match self {
            Self::Turn(value) => Ok(value),
            _ => Err(Error::failed("expected turn configuration")),
        }
    }
    pub fn into_context(self) -> Result<ContextConfiguration, Error> {
        match self {
            Self::Context(value) => Ok(value),
            _ => Err(Error::failed("expected context configuration")),
        }
    }
    pub fn into_compaction(self) -> Result<CompactionConfiguration, Error> {
        match self {
            Self::Compaction(value) => Ok(value),
            _ => Err(Error::failed("expected compaction configuration")),
        }
    }
    pub fn into_prompt_template(self) -> Result<Arc<PromptTemplate>, Error> {
        match self {
            Self::PromptTemplate(value) => Ok(value),
            _ => Err(Error::failed("expected prompt template configuration")),
        }
    }
}

/// A compiled prompt template shared by core and native configuration catalogs.
pub struct PromptTemplate {
    pub(crate) path: String,
    pub(crate) allowed: std::collections::BTreeSet<String>,
    pub(crate) required: Vec<String>,
    pub(crate) engine: crate::template::Engine,
}
