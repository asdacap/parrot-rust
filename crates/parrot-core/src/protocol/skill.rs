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

fn word(character: u8) -> bool {
    character.is_ascii_alphanumeric() || character == b'_'
}

fn name_character(character: u8) -> bool {
    word(character) || matches!(character, b':' | b'-')
}

fn linked_mention(text: &str, start: usize) -> Option<(usize, &str, &str)> {
    let rest = text.get(start..)?.strip_prefix("[$")?;
    let name_end = rest.find([']', '\r', '\n'])?;
    if name_end == 0 || !rest[name_end..].starts_with("](") {
        return None;
    }
    let path = &rest[name_end + 2..];
    let path_end = path.find(')')?;
    if path_end == 0 {
        return None;
    }
    let end = start + 2 + name_end + 2 + path_end + 1;
    if text
        .as_bytes()
        .get(end)
        .copied()
        .is_some_and(name_character)
    {
        return None;
    }
    Some((end, rest[..name_end].trim(), path[..path_end].trim()))
}

/// Finds bare and linked skill mentions, including inside Markdown and code.
/// Bare environment-variable names retain their existing exclusions.
pub fn parse_skill_mentions(text: &str) -> Vec<SkillMention> {
    const ENVIRONMENT_NAMES: &[&str] = &[
        "HOME",
        "PATH",
        "USER",
        "SHELL",
        "PWD",
        "TMP",
        "TMPDIR",
        "PARROT_HOST_TMPDIR",
        "TEMP",
        "LANG",
        "TERM",
        "XDG_CONFIG_HOME",
    ];
    let mut mentions = Vec::new();
    let mut start = 0;
    let mut position = 0;
    while start < text.len() {
        let previous_is_word = start > 0 && word(text.as_bytes()[start - 1]);
        let matched = if previous_is_word {
            None
        } else if let Some((end, name, path)) = linked_mention(text, start) {
            Some((end, name, Some(path.to_owned())))
        } else if text.as_bytes()[start] == b'$' {
            let mut end = start + 1;
            while text
                .as_bytes()
                .get(end)
                .copied()
                .is_some_and(name_character)
            {
                end += 1;
            }
            (end > start + 1).then(|| (end, &text[start + 1..end], None))
        } else {
            None
        };
        if let Some((end, name, path)) = matched {
            if path.is_some() || !ENVIRONMENT_NAMES.contains(&name) {
                mentions.push(SkillMention {
                    name: name.into(),
                    path,
                    position,
                });
            }
            position += text[start..end].encode_utf16().count();
            start = end;
        } else {
            let character = text[start..].chars().next().expect("in-bounds character");
            position += character.len_utf16();
            start += character.len_utf8();
        }
    }
    mentions
}
