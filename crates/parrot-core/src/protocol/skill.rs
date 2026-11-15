use super::{BoxFuture, Error};

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SkillReference {
    pub name: String,
    pub path: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Skill {
    /// Opaque, stable canonical identity used for turn-wide deduplication.
    pub id: String,
    pub name: String,
    /// Presentation or discovery path, not the record identity or read authority.
    pub path: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SkillContent {
    Text(String),
    Unavailable(String),
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct PreparedSkillPrompt {
    pub content: String,
    pub loaded_paths: Vec<String>,
}

/// Resolves skill metadata and reads instruction bodies without preparing prompts.
pub trait SkillProvider: Send + Sync {
    /// Retrieves skill metadata in one batch without reading instruction bodies.
    /// Returns one result per reference in the supplied order, preserving backing-store
    /// precedence and name/path matching semantics.
    fn resolve_skill_references<'a>(
        &'a self,
        _references: &'a [SkillReference],
    ) -> BoxFuture<'a, Result<Vec<Option<Skill>>, Error>> {
        Box::pin(async { Err(Error::unsupported("SkillProvider.resolve_skill_references")) })
    }
    /// Reads the selected record's complete instruction body or unavailable diagnostic.
    /// Does not format prompt text, update turn deduplication, or record context.
    fn read_skill_content<'a>(
        &'a self,
        _skill: &'a Skill,
    ) -> BoxFuture<'a, Result<SkillContent, Error>> {
        Box::pin(async { Err(Error::unsupported("SkillProvider.read_skill_content")) })
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SkillMention {
    pub name: String,
    pub path: Option<String>,
    /// UTF-16 code-unit offset within the input text.
    pub position: usize,
}
