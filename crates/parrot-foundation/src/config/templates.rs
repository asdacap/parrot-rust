use super::node::{Node, Style, mapping, validate_keys};
use super::{ConfigError, Result};
use std::collections::BTreeMap;

const BUILTIN_PROMPTS: [(&str, &str); 16] = [
    (
        "system-prompts/base.md",
        include_str!("../../../../assets/config/system-prompts/base.md"),
    ),
    (
        "system-prompts/delegation.md",
        include_str!("../../../../assets/config/system-prompts/delegation.md"),
    ),
    (
        "system-prompts/agent-tasks.md",
        include_str!("../../../../assets/config/system-prompts/agent-tasks.md"),
    ),
    (
        "system-prompts/subagent-patterns.md",
        include_str!("../../../../assets/config/system-prompts/subagent-patterns.md"),
    ),
    (
        "system-prompts/behavior-guidelines.md",
        include_str!("../../../../assets/config/system-prompts/behavior-guidelines.md"),
    ),
    (
        "system-prompts/toolings.md",
        include_str!("../../../../assets/config/system-prompts/toolings.md"),
    ),
    (
        "profile-prompts/general.md",
        include_str!("../../../../assets/config/profile-prompts/general.md"),
    ),
    (
        "profile-prompts/coordinator.md",
        include_str!("../../../../assets/config/profile-prompts/coordinator.md"),
    ),
    (
        "profile-prompts/build.md",
        include_str!("../../../../assets/config/profile-prompts/build.md"),
    ),
    (
        "profile-prompts/plan.md",
        include_str!("../../../../assets/config/profile-prompts/plan.md"),
    ),
    (
        "profile-prompts/query.md",
        include_str!("../../../../assets/config/profile-prompts/query.md"),
    ),
    (
        "profile-prompts/explorer.md",
        include_str!("../../../../assets/config/profile-prompts/explorer.md"),
    ),
    (
        "profile-prompts/review.md",
        include_str!("../../../../assets/config/profile-prompts/review.md"),
    ),
    (
        "profile-prompts/worker.md",
        include_str!("../../../../assets/config/profile-prompts/worker.md"),
    ),
    (
        "profile-prompts/thinker.md",
        include_str!("../../../../assets/config/profile-prompts/thinker.md"),
    ),
    (
        "profile-prompts/agent-task-payload.md",
        include_str!("../../../../assets/config/profile-prompts/agent-task-payload.md"),
    ),
];

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TemplateSpec {
    pub path: String,
    pub text: String,
    pub engine: String,
    pub allowed: Vec<String>,
    pub required: Vec<String>,
}

pub fn nonempty_scalar(parent: &Node, key: &str, path: &str) -> Result<String> {
    let node = parent
        .get(key)
        .ok_or_else(|| ConfigError::invalid_data(format!("{path} must be a non-empty string")))?;
    let value = node
        .as_str()
        .ok_or_else(|| ConfigError::invalid_data(format!("{path} must be a non-empty string")))?;
    if value.trim().is_empty()
        || (node.style == Style::Plain && matches!(value, "null" | "Null" | "NULL" | "~"))
    {
        return Err(ConfigError::invalid_data(format!(
            "{path} must be a non-empty string"
        )));
    }
    Ok(value.to_owned())
}

pub(super) fn resolve_builtin_prompt(text: String, path: &str) -> Result<String> {
    if let Some(reference) = text.strip_prefix("@builtin:") {
        let resolved = BUILTIN_PROMPTS
            .iter()
            .find_map(|(name, text)| (*name == reference).then_some(*text))
            .ok_or_else(|| {
                ConfigError::invalid_data(format!(
                    "{path}: unknown built-in prompt reference {text:?}"
                ))
            })?;
        if resolved.trim().is_empty() {
            return Err(ConfigError::invalid_data(format!(
                "{path} must be a non-empty string"
            )));
        }
        Ok(resolved.to_owned())
    } else {
        Ok(text)
    }
}

pub fn read_system_prompts(root: &Node) -> Result<BTreeMap<String, String>> {
    let configured = root
        .get("system_prompts")
        .ok_or_else(|| ConfigError::invalid_data("system_prompts must be a mapping".into()))?;
    let mut prompts = BTreeMap::new();
    for (key, value) in mapping(configured, "system_prompts")? {
        let key = key
            .as_str()
            .filter(|key| {
                key.find(':').is_some_and(|i| i > 0 && i + 1 < key.len())
                    && !key.contains(['\r', '\n', '\t', ' '])
            })
            .ok_or_else(|| {
                ConfigError::invalid_data(
                    "system_prompts keys must be namespaced system prompt provider keys".into(),
                )
            })?;
        let path = format!("system_prompts.{key}");
        let wrapper = Node::mapping(vec![(Node::scalar("value"), value.clone())]);
        let text = nonempty_scalar(&wrapper, "value", &path)?;
        let text = resolve_builtin_prompt(text, &path)?;
        prompts.insert(key.to_owned(), text);
    }
    Ok(prompts)
}

/// Every descriptor must be validated by the caller's real scalar/Scriban engine.
/// There is intentionally no default validator or production fallback.
pub fn read_templates<F>(root: &Node, validator: &mut F) -> Result<BTreeMap<String, TemplateSpec>>
where
    F: FnMut(&TemplateSpec) -> Result<()>,
{
    let configured = root
        .get("prompt_templates")
        .ok_or_else(|| ConfigError::invalid_data("prompt_templates must be a mapping".into()))?;
    let mut templates = BTreeMap::new();
    for (id, fields) in mapping(configured, "prompt_templates")? {
        let id = id
            .as_str()
            .filter(|id| !id.trim().is_empty() && id.trim() == *id)
            .ok_or_else(|| {
                ConfigError::invalid_data(
                    "prompt_templates keys must be nonblank trimmed IDs".into(),
                )
            })?;
        let path = format!("prompt_templates.{id}");
        validate_keys(
            fields,
            &path,
            &[
                "template",
                "allowed_arguments",
                "required_arguments",
                "engine",
            ],
        )?;
        let text = nonempty_scalar(fields, "template", &format!("{path}.template"))?;
        let allowed = arguments(fields, "allowed_arguments", &path)?;
        let required = arguments(fields, "required_arguments", &path)?;
        if !required.iter().all(|name| allowed.contains(name)) {
            return Err(ConfigError::invalid_data(format!(
                "{path}.required_arguments must be included in allowed_arguments"
            )));
        }
        let engine = if fields.get("engine").is_some() {
            nonempty_scalar(fields, "engine", &format!("{path}.engine"))?
        } else {
            "scalar".to_owned()
        };
        if engine == "scriban" {
            return Err(ConfigError::invalid_data(format!(
                "{path}.engine: Scriban is no longer supported; manually migrate the template to MiniJinja native syntax and set engine: minijinja (see docs/template-migration-guide.md)"
            )));
        }
        if !matches!(engine.as_str(), "scalar" | "minijinja") {
            return Err(ConfigError::invalid_data(format!(
                "{path}.engine must be scalar or minijinja"
            )));
        }
        let spec = TemplateSpec {
            path,
            text,
            engine,
            allowed,
            required,
        };
        validator(&spec)?;
        templates.insert(id.to_owned(), spec);
    }
    Ok(templates)
}

fn arguments(fields: &Node, key: &str, path: &str) -> Result<Vec<String>> {
    let sequence = fields
        .get(key)
        .and_then(Node::as_sequence)
        .ok_or_else(|| ConfigError::invalid_data(format!("{path}.{key} must be a sequence")))?;
    let mut arguments = Vec::new();
    for item in sequence {
        let name = item
            .as_str()
            .filter(|value| !value.trim().is_empty() && value.trim() == *value)
            .ok_or_else(|| {
                ConfigError::invalid_data(format!(
                    "{path}.{key} must contain nonblank trimmed names"
                ))
            })?;
        if arguments.iter().any(|argument| argument == name) {
            return Err(ConfigError::invalid_data(format!(
                "{path}.{key} contains duplicate argument '{name}'"
            )));
        }
        arguments.push(name.to_owned());
    }
    Ok(arguments)
}
