//! Pure skill metadata parsing and path normalization; no filesystem access or authorization.

use parrot_domain::values::config::{PathPlatform, PlatformPath};
use parrot_foundation::config::{node::Node, yaml};
use std::fmt;
use std::path::{Component, Path};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SkillScope {
    Repo,
    User,
    System,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SkillRoot {
    pub path: String,
    pub scope: SkillScope,
    pub follow_directory_links: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SkillRootLocation {
    pub path: String,
    pub scope: SkillScope,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SkillMetadata {
    pub name: String,
    pub description: String,
    pub path: String,
    pub scope: SkillScope,
    pub display_name: Option<String>,
    pub short_description: Option<String>,
    pub enabled: bool,
    pub prompt_visible: bool,
    pub discovery_path: String,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct SkillDisplayMetadata {
    pub display_name: Option<String>,
    pub short_description: Option<String>,
    pub allow_implicit_invocation: Option<bool>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SkillParseError {
    pub path: String,
    pub message: String,
}

impl fmt::Display for SkillParseError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}: {}", self.path, self.message)
    }
}

impl std::error::Error for SkillParseError {}

/// Lexically normalizes Unix paths without resolving links or consulting the host.
/// Relative inputs require an explicit absolute working directory.
pub fn normalize_skill_path(path: &str, working_directory: Option<&str>) -> Result<String, String> {
    if path.is_empty() || path.contains('\0') {
        return Err("skill path must be nonempty and contain no NUL".into());
    }
    let path = Path::new(path);
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        let directory = working_directory
            .filter(|directory| Path::new(directory).is_absolute() && !directory.contains('\0'))
            .ok_or("relative skill path requires an explicit absolute working directory")?;
        Path::new(directory).join(path)
    };
    let mut result = std::path::PathBuf::from("/");
    for component in absolute.components() {
        match component {
            Component::Normal(part) => result.push(part),
            Component::ParentDir => {
                result.pop();
            }
            Component::RootDir | Component::CurDir => {}
            Component::Prefix(_) => return Err("unsupported skill path prefix".into()),
        }
    }
    Ok(result.to_string_lossy().into_owned())
}

fn sanitize(value: &str) -> String {
    value.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn scalar<'a>(node: &'a Node, key: &str) -> Option<&'a str> {
    node.get(key).and_then(Node::as_str)
}

fn boolean(value: &str) -> Option<bool> {
    let value =
        value.trim_matches(|character: char| character.is_whitespace() || character == '\0');
    if value.eq_ignore_ascii_case("true") {
        Some(true)
    } else if value.eq_ignore_ascii_case("false") {
        Some(false)
    } else {
        None
    }
}

fn first_mapping(content: &str) -> Option<Node> {
    yaml::parse_documents(content)
        .ok()?
        .into_iter()
        .next()
        .filter(|node| node.as_mapping().is_some())
}

/// Parses the first frontmatter document. The caller supplies an absolute path;
/// metadata path normalization is not permission or physical-target resolution.
pub fn parse_skill_frontmatter(
    path: &str,
    content: &str,
    scope: SkillScope,
    prompt_visible: bool,
) -> Result<SkillMetadata, SkillParseError> {
    let fail = |message: &str| SkillParseError {
        path: path.into(),
        message: message.into(),
    };
    let normalized = normalize_skill_path(path, None).map_err(|message| fail(&message))?;
    let content = content.replace("\r\n", "\n");
    let lines: Vec<_> = content.split('\n').collect();
    if lines[0].trim() != "---" {
        return Err(fail("SKILL.md must begin with YAML frontmatter."));
    }
    let closing = lines
        .iter()
        .enumerate()
        .skip(1)
        .find(|(_, line)| line.trim() == "---")
        .map(|(index, _)| index)
        .filter(|index| *index > 1)
        .ok_or_else(|| fail("SKILL.md frontmatter is not closed or is empty."))?;
    let mapping = first_mapping(&lines[1..closing].join("\n"))
        .ok_or_else(|| fail("SKILL.md frontmatter contains invalid YAML."))?;
    let name = sanitize(scalar(&mapping, "name").unwrap_or(""));
    let name = if name.is_empty() {
        Path::new(&normalized)
            .parent()
            .and_then(Path::file_name)
            .and_then(|name| name.to_str())
            .unwrap_or("")
            .to_owned()
    } else {
        name
    };
    let description = sanitize(scalar(&mapping, "description").unwrap_or(""));
    if name.is_empty() || name.encode_utf16().count() > 64 {
        return Err(fail("skill name must be between 1 and 64 characters."));
    }
    if description.is_empty() || description.encode_utf16().count() > 1024 {
        return Err(fail(
            "skill description must be between 1 and 1024 characters.",
        ));
    }
    let short_description = mapping
        .get("metadata")
        .and_then(|metadata| scalar(metadata, "short-description"))
        .map(sanitize)
        .filter(|value| !value.is_empty());
    Ok(SkillMetadata {
        name,
        description,
        discovery_path: normalized.clone(),
        path: normalized,
        scope,
        display_name: None,
        short_description,
        enabled: true,
        prompt_visible: scalar(&mapping, "disable-model-invocation")
            .and_then(boolean)
            .map_or(prompt_visible, |disabled| !disabled),
    })
}

/// Optional display YAML fails open exactly at the metadata boundary.
pub fn parse_skill_display_metadata(content: &str) -> SkillDisplayMetadata {
    let Some(root) = first_mapping(content) else {
        return SkillDisplayMetadata::default();
    };
    let bounded = |key: &str, limit: usize| {
        root.get("interface")
            .and_then(|interface| scalar(interface, key))
            .map(sanitize)
            .filter(|value| !value.is_empty() && value.encode_utf16().count() <= limit)
    };
    SkillDisplayMetadata {
        display_name: bounded("display_name", 64),
        short_description: bounded("short_description", 1024),
        allow_implicit_invocation: root
            .get("policy")
            .and_then(|policy| scalar(policy, "allow_implicit_invocation"))
            .and_then(boolean),
    }
}

pub fn normalize_skill_match_path(
    path: &str,
    working_directory: Option<&str>,
    platform: PathPlatform,
) -> Result<String, String> {
    let lexical = normalize_skill_path(path, working_directory)?;
    PlatformPath::normalize(&lexical, "/", platform)
        .map(|path| {
            if path == "/" {
                path
            } else {
                path.trim_end_matches('/').to_owned()
            }
        })
        .map_err(|error| error.to_string())
}
