use super::{ConfigError, Result};
use std::sync::atomic::{AtomicU64, Ordering};

static NEXT_ANCHOR_MUTATION: AtomicU64 = AtomicU64::new(1);

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum Style {
    #[default]
    Plain,
    SingleQuoted,
    DoubleQuoted,
    Literal,
    Folded,
    Flow,
    Any,
}
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Kind {
    Scalar(Option<String>),
    Sequence(Vec<Node>),
    Mapping(Vec<(Node, Node)>),
}
#[derive(Clone, Debug, Eq)]
pub struct Node {
    pub kind: Kind,
    pub tag: String,
    pub style: Style,
    pub anchor: Option<String>,
    pub alias: Option<String>,
    /// Shared identity of an explicit anchor and its retained alias occurrences.
    /// The name alone is not an identity: YAML can rebind an anchor name.
    pub anchor_identity: Option<u64>,
    /// Mutable access through an anchored occurrence determines its current payload.
    pub anchor_revision: u64,
}
impl PartialEq for Node {
    fn eq(&self, other: &Self) -> bool {
        self.kind == other.kind
            && self.tag == other.tag
            && self.style == other.style
            && self.anchor == other.anchor
            && self.alias == other.alias
    }
}

impl Node {
    pub fn scalar(value: impl Into<String>) -> Self {
        Self {
            kind: Kind::Scalar(Some(value.into())),
            tag: String::new(),
            style: Style::Any,
            anchor: None,
            alias: None,
            anchor_identity: None,
            anchor_revision: 0,
        }
    }
    pub fn null() -> Self {
        Self {
            kind: Kind::Scalar(None),
            tag: String::new(),
            style: Style::Any,
            anchor: None,
            alias: None,
            anchor_identity: None,
            anchor_revision: 0,
        }
    }
    pub fn mapping(entries: Vec<(Node, Node)>) -> Self {
        Self {
            kind: Kind::Mapping(entries),
            tag: String::new(),
            style: Style::Plain,
            anchor: None,
            alias: None,
            anchor_identity: None,
            anchor_revision: 0,
        }
    }
    pub fn sequence(items: Vec<Node>) -> Self {
        Self {
            kind: Kind::Sequence(items),
            tag: String::new(),
            style: Style::Plain,
            anchor: None,
            alias: None,
            anchor_identity: None,
            anchor_revision: 0,
        }
    }
    fn mark_anchor_mutation(&mut self) {
        if self.anchor_identity.is_some() {
            self.anchor_revision = NEXT_ANCHOR_MUTATION.fetch_add(1, Ordering::Relaxed);
        }
    }
    pub fn as_str(&self) -> Option<&str> {
        match &self.kind {
            Kind::Scalar(value) => value.as_deref(),
            _ => None,
        }
    }
    pub fn as_mapping(&self) -> Option<&[(Node, Node)]> {
        match &self.kind {
            Kind::Mapping(value) => Some(value),
            _ => None,
        }
    }
    pub fn as_sequence(&self) -> Option<&[Node]> {
        match &self.kind {
            Kind::Sequence(value) => Some(value),
            _ => None,
        }
    }
    pub fn as_sequence_mut(&mut self) -> Option<&mut Vec<Node>> {
        super::yaml::synchronize_aliases(self);
        self.mark_anchor_mutation();
        match &mut self.kind {
            Kind::Sequence(value) => Some(value),
            _ => None,
        }
    }
    pub fn get(&self, key: &str) -> Option<&Node> {
        let value = self
            .as_mapping()?
            .iter()
            .find(|(name, _)| name.as_str() == Some(key))
            .map(|(_, value)| value)?;
        Some(super::yaml::latest_alias_occurrence(self, value))
    }
    pub fn get_mut(&mut self, key: &str) -> Option<&mut Node> {
        super::yaml::synchronize_aliases(self);
        self.mark_anchor_mutation();
        match &mut self.kind {
            Kind::Mapping(entries) => entries
                .iter_mut()
                .find(|(name, _)| name.as_str() == Some(key))
                .map(|(_, value)| value),
            _ => None,
        }
    }
    pub fn set(&mut self, key: impl Into<String>, value: Node) {
        let key = key.into();
        if let Some(current) = self.get_mut(&key) {
            *current = value;
        } else if let Kind::Mapping(entries) = &mut self.kind {
            entries.push((Node::scalar(key), value));
        }
    }
    pub fn remove(&mut self, key: &str) {
        super::yaml::synchronize_aliases(self);
        self.mark_anchor_mutation();
        if let Kind::Mapping(entries) = &mut self.kind {
            entries.retain(|(name, _)| name.as_str() != Some(key));
        }
    }
}
pub fn mapping<'a>(node: &'a Node, path: &str) -> Result<&'a [(Node, Node)]> {
    node.as_mapping()
        .ok_or_else(|| ConfigError::invalid_data(format!("{path} must be a mapping")))
}
pub fn scalar(node: &Node, path: &str) -> Result<String> {
    node.as_str()
        .map(str::to_owned)
        .ok_or_else(|| ConfigError::invalid_data(format!("{path} must be a string")))
}
pub fn scalar_or_empty(parent: &Node, key: &str) -> String {
    parent
        .get(key)
        .and_then(Node::as_str)
        .unwrap_or("")
        .to_owned()
}
pub fn validate_keys(node: &Node, path: &str, allowed: &[&str]) -> Result<()> {
    for (key, _) in mapping(node, path)? {
        if !key.as_str().is_some_and(|key| allowed.contains(&key)) {
            return Err(ConfigError::invalid_data(format!(
                "{path} contains an unsupported key"
            )));
        }
    }
    Ok(())
}
pub fn boolean(node: Option<&Node>, path: &str) -> Result<bool> {
    match node.and_then(Node::as_str) {
        Some("true") => Ok(true),
        Some("false") => Ok(false),
        _ => Err(ConfigError::invalid_data(format!(
            "{path} must be true or false"
        ))),
    }
}
fn integer(node: Option<&Node>) -> Option<i32> {
    let text = node?.as_str()?;
    if text.is_empty() || !text.bytes().all(|c| c.is_ascii_digit()) {
        None
    } else {
        text.parse().ok()
    }
}
pub fn positive_integer(parent: &Node, key: &str, path: &str) -> Result<i32> {
    integer(parent.get(key))
        .filter(|v| *v > 0)
        .ok_or_else(|| ConfigError::invalid_data(format!("{path} must be a positive integer")))
}
pub fn nonnegative_integer(parent: &Node, key: &str, path: &str) -> Result<i32> {
    integer(parent.get(key))
        .filter(|v| *v >= 0)
        .ok_or_else(|| ConfigError::invalid_data(format!("{path} must be a non-negative integer")))
}
