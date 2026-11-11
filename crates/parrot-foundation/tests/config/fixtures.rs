use parrot_foundation::config::{
    ConfigError, Result,
    configuration::{Configuration, source_predefined_content},
    templates::{self, TemplateSpec},
    yaml,
};
use std::collections::BTreeMap;
use std::path::Path;

#[path = "a008_fixture.rs"]
mod a008_fixture;
#[cfg(test)]
pub use a008_fixture::inventory as migrated_fixture_inventory;

pub fn environment() -> BTreeMap<String, String> {
    [
        ("HOME", "/tmp/c/home"),
        ("XDG_CONFIG_HOME", "/tmp/c/xdg/config"),
        ("XDG_STATE_HOME", "/tmp/c/xdg/state"),
        ("XDG_DATA_HOME", "/tmp/c/xdg/data"),
        ("WORKDIR", "/tmp/c/workspace"),
        ("TEST_SET", "present"),
    ]
    .into_iter()
    .map(|(k, v)| (k.to_owned(), v.to_owned()))
    .collect()
}
pub const CONSOLIDATED_SOURCE_IDS: [&str; 13] = [
    "agent-task.dependency-header",
    "agent-task.dependency-item",
    "agent-task.sibling-scope",
    "agent-task.sibling-item",
    "agent-task.header",
    "agent-session.nonempty-queues-reminder",
    "agent-session.nonempty-queue-item",
    "compact-context-tool.compacted",
    "compact-context-tool.no-op",
    "compact-context-tool.unavailable",
    "compact-context-tool.compacted-with-limit",
    "compact-context-tool.no-op-with-limit",
    "compact-context-tool.unavailable-with-limit",
];
pub const CONSOLIDATED_IDS: [&str; 2] = [
    "agent-task.assignment-context",
    "compact-context-tool.result",
];

pub const NATIVE_MINIJINJA_IDS: [&str; 1] = ["agent-task.set-assigned"];

pub const AGENT_COMMUNICATION_SOURCE_IDS: [&str; 5] = [
    "agent-session.child-question",
    "agent-session.pending-child-question-reminder",
    "tool-result.question-replied",
    "agent-session.final-request",
    "agent-session.final-request-interrupted",
];
pub const AGENT_COMMUNICATION_IDS: [&str; 8] = [
    "agent-session.agent-ask",
    "agent-session.agent-ask-timeout",
    "agent-session.pending-ask-reminder",
    "tool-result.ask-reply",
    "tool-result.ask-timeout",
    "tool-result.ask-replied",
    "agent-session.final-request",
    "agent-session.final-request-interrupted",
];

pub const NATIVE_SCALAR_IDS: [&str; 9] = [
    "agent-task.self-dependency",
    "agent-task.notification-assigned-self",
    "agent-session.crashed-tool-result",
    "agent-session.progress-reminder",
    "exec-command.stderr-summary",
    "progress-tool.already-closed",
    "progress-tool.closed",
    "progress-tool.updated",
    "tool-result.path-variable-note",
];

fn shipped_specs() -> Result<BTreeMap<String, TemplateSpec>> {
    let root = yaml::parse_root(&source_predefined_content()?)?;
    let actual = templates::read_templates(&root, &mut |_| Ok(()))?;
    let mut expected = a008_fixture::specs()?;
    expected.retain(|id, _| {
        !CONSOLIDATED_SOURCE_IDS.contains(&id.as_str())
            && !AGENT_COMMUNICATION_SOURCE_IDS.contains(&id.as_str())
    });
    for id in CONSOLIDATED_IDS.into_iter().chain(NATIVE_MINIJINJA_IDS) {
        let spec = actual.get(id).ok_or_else(|| {
            ConfigError::invalid_data(format!("consolidated template {id} is missing"))
        })?;
        parrot_foundation::template_minijinja::MiniJinjaPromptTemplateEngine::compile(
            &spec.path, &spec.text,
        )?;
        expected.insert(id.to_owned(), spec.clone());
    }
    for id in AGENT_COMMUNICATION_IDS
        .into_iter()
        .chain(["session.legacy-resume-warning"])
        .chain(NATIVE_SCALAR_IDS)
    {
        let spec = actual
            .get(id)
            .ok_or_else(|| ConfigError::invalid_data(format!("scalar template {id} is missing")))?;
        parrot_foundation::template_scalar::ScalarPromptTemplateEngine::compile(
            &spec.path,
            &spec.text,
            &spec.allowed.iter().cloned().collect(),
        )?;
        expected.insert(id.to_owned(), spec.clone());
    }
    // The authorized self-assignment flow supersedes only this ported text-parity case.
    let id = "agent-task.composite";
    let current = &actual[id];
    let retained = expected.get_mut(id).unwrap();
    retained.text.clone_from(&current.text);
    parrot_foundation::template_scalar::ScalarPromptTemplateEngine::compile(
        &current.path,
        &current.text,
        &current.allowed.iter().cloned().collect(),
    )?;
    if actual != expected {
        return Err(ConfigError::invalid_data(
            "A008 fixture catalog differs from pinned source/migration descriptors".into(),
        ));
    }
    Ok(actual)
}
pub fn fixture_configuration(content: &str) -> Result<Configuration> {
    let known = shipped_specs()?;
    Configuration::from_content(content, &environment(), |spec| {
        validate_fixture(spec, &known)
    })
}
pub fn fixture_load(path: &Path, predefined: &Path) -> Result<Configuration> {
    let known = shipped_specs()?;
    Configuration::load(path, predefined, &environment(), |spec| {
        validate_fixture(spec, &known)
    })
}
pub fn fixture_load_with_environment(
    path: &Path,
    predefined: &Path,
    environment: &BTreeMap<String, String>,
) -> Result<Configuration> {
    let known = shipped_specs()?;
    Configuration::load(path, predefined, environment, |spec| {
        validate_fixture(spec, &known)
    })
}
fn validate_fixture(spec: &TemplateSpec, known: &BTreeMap<String, TemplateSpec>) -> Result<()> {
    let id = spec
        .path
        .strip_prefix("prompt_templates.")
        .unwrap_or(&spec.path);
    if known.get(id) == Some(spec) {
        Ok(())
    } else {
        Err(ConfigError::invalid_data(format!(
            "{} requires the completed T12 engine validator",
            spec.path
        )))
    }
}
