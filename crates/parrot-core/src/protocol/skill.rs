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
pub struct DerivedContext {
    pub content: String,
    pub loaded_paths: Vec<String>,
}
