use parrot_core::template::{MiniJinjaPromptTemplateEngine, ScalarPromptTemplateEngine};
use parrot_foundation::config::{
    ConfigError, Result,
    configuration::{Configuration, source_predefined_content},
    templates::{self, TemplateSpec},
};
use std::collections::BTreeMap;
use std::path::Path;

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

pub const CONSOLIDATED_IDS: [&str; 2] = [
    "agent-task.assignment-context",
    "compact-context-tool.result",
];

fn shipped_specs() -> Result<BTreeMap<String, TemplateSpec>> {
    let root = source_predefined_content()?;
    let actual = templates::read_templates(&root, &mut |_| Ok(()))?;
    for spec in actual.values() {
        if spec.engine == "minijinja" {
            MiniJinjaPromptTemplateEngine::compile(&spec.path, &spec.text).map(drop)
        } else {
            ScalarPromptTemplateEngine::compile(
                &spec.path,
                &spec.text,
                &spec.allowed.iter().cloned().collect(),
            )
            .map(drop)
        }
        .map_err(|error| ConfigError::invalid_data(error.message))?;
    }
    Ok(actual)
}
pub fn fixture_configuration(content: &str) -> Result<Configuration> {
    let known = shipped_specs()?;
    Configuration::from_content(content, &environment(), |spec| {
        validate_fixture(spec, &known)
    })
}
pub fn fixture_templates(content: &str) -> Result<BTreeMap<String, TemplateSpec>> {
    let known = shipped_specs()?;
    let mut configured = BTreeMap::new();
    Configuration::from_content(content, &environment(), |spec| {
        validate_fixture(spec, &known)?;
        let id = spec
            .path
            .strip_prefix("prompt_templates.")
            .unwrap_or(&spec.path);
        configured.insert(id.to_owned(), spec.clone());
        Ok(())
    })?;
    Ok(configured)
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
