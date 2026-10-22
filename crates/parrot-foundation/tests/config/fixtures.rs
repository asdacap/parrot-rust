use parrot_domain::values::context::ContextSize;
use parrot_foundation::config::{
    ConfigError, Result,
    configuration::{Configuration, source_predefined_content},
    models::{ModelPresetConfig, ModelPresetContextLimits},
    node::Node,
    templates::{self, TemplateSpec},
    yaml, yaml_json,
};
use serde_json::{Value, json};
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
pub const CONSOLIDATED_IDS: [&str; 3] = [
    "agent-task.assignment-context",
    "agent-session.nonempty-queues-reminder",
    "compact-context-tool.result",
];

pub const AGENT_COMMUNICATION_SOURCE_IDS: [&str; 5] = [
    "agent-session.child-question",
    "agent-session.pending-child-question-reminder",
    "tool-result.question-replied",
    "agent-session.final-request",
    "agent-session.final-request-interrupted",
];
pub const AGENT_COMMUNICATION_IDS: [&str; 6] = [
    "agent-session.agent-ask",
    "agent-session.pending-ask-reminder",
    "tool-result.ask-reply",
    "tool-result.ask-replied",
    "agent-session.final-request",
    "agent-session.final-request-interrupted",
];

fn shipped_specs() -> Result<BTreeMap<String, TemplateSpec>> {
    let root = yaml::parse_root(&source_predefined_content()?)?;
    let actual = templates::read_templates(&root, &mut |_| Ok(()))?;
    let mut expected = a008_fixture::specs()?;
    expected.retain(|id, _| {
        !CONSOLIDATED_SOURCE_IDS.contains(&id.as_str())
            && !AGENT_COMMUNICATION_SOURCE_IDS.contains(&id.as_str())
    });
    for id in CONSOLIDATED_IDS {
        let spec = actual.get(id).ok_or_else(|| {
            ConfigError::invalid_data(format!("consolidated template {id} is missing"))
        })?;
        parrot_foundation::template_minijinja::MiniJinjaPromptTemplateEngine::compile(
            &spec.path, &spec.text,
        )?;
        expected.insert(id.to_owned(), spec.clone());
    }
    for id in AGENT_COMMUNICATION_IDS {
        let spec = actual.get(id).ok_or_else(|| {
            ConfigError::invalid_data(format!("agent communication template {id} is missing"))
        })?;
        parrot_foundation::template_scalar::ScalarPromptTemplateEngine::compile(
            &spec.path,
            &spec.text,
            &spec.allowed.iter().cloned().collect(),
        )?;
        expected.insert(id.to_owned(), spec.clone());
    }
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
fn text<'a>(value: &'a Value, key: &str) -> &'a str {
    value.get(key).and_then(Value::as_str).unwrap_or("")
}

pub fn execute(inputs: &Value, work: &Path) -> Result<Value> {
    let known = shipped_specs()?;
    let mut cases = Vec::new();
    for input in inputs
        .as_array()
        .ok_or_else(|| ConfigError::invalid_data("case inputs must be an array".into()))?
    {
        let id = text(input, "id");
        let kind = text(input, "kind");
        let output = match run_case(input, work, &known) {
            Ok(value) => json!({"value":value}),
            Err(error) => {
                let category = if error.0.starts_with(".NET number values") {
                    "ArgumentException"
                } else {
                    "InvalidDataException"
                };
                json!({"error":error.0,"errorType":category})
            }
        };
        cases.push(json!({"id":id,"kind":kind,"output":output}));
    }
    Ok(json!({"cases":cases}))
}
fn run_case(input: &Value, work: &Path, known: &BTreeMap<String, TemplateSpec>) -> Result<Value> {
    match text(input, "kind") {
        "emit" => {
            let root = if let Some(value) = input.get("constructed").and_then(Value::as_str) {
                Node::mapping(vec![(Node::scalar("value"), Node::scalar(value))])
            } else {
                yaml::parse_documents(text(input, "yaml"))?
                    .into_iter()
                    .next()
                    .ok_or_else(|| ConfigError::invalid_data("no YAML document".into()))?
            };
            Ok(json!(yaml::serialize(&root)))
        }
        "yaml_json" => {
            let root = yaml::parse_documents(text(input, "yaml"))?
                .into_iter()
                .next()
                .ok_or_else(|| ConfigError::invalid_data("no YAML document".into()))?;
            Ok(json!(yaml_json(&root)?))
        }
        "compose" => {
            let sources = input["sources"]
                .as_array()
                .ok_or_else(|| ConfigError::invalid_data("sources must be array".into()))?;
            let borrowed: Vec<_> = sources
                .iter()
                .map(|source| {
                    (
                        text(source, "name"),
                        source.get("content").and_then(Value::as_str),
                    )
                })
                .collect();
            Ok(json!(yaml::compose(&borrowed)?))
        }
        "compose_shipped" => Ok(json!(source_predefined_content()?)),
        "config" => run_config(input, work, known),
        _ => Err(ConfigError::invalid_data("unknown oracle case kind".into())),
    }
}
fn run_config(input: &Value, work: &Path, known: &BTreeMap<String, TemplateSpec>) -> Result<Value> {
    let id = text(input, "id");
    if id.contains('/') || id.contains('\\') || id == ".." {
        return Err(ConfigError::invalid_data(
            "case id must be path-free".into(),
        ));
    }
    let directory = work.join("config-cases").join(id);
    std::fs::create_dir_all(&directory)?;
    let path = directory.join("config.yaml");
    let predefined = directory.join("predefined.yaml");
    std::fs::write(&path, text(input, "yaml"))?;
    let env = environment();
    let mut configuration = Configuration::load(&path, &predefined, &env, |spec| {
        validate_fixture(spec, known)
    })?;
    if let Some(operations) = input.get("operations").and_then(Value::as_array) {
        for operation in operations {
            match text(operation, "type") {
                "set_model" => configuration.set_model(text(operation, "value"))?,
                "set_alias" => configuration
                    .set_model_alias(text(operation, "name"), text(operation, "value"))?,
                "set_routing" | "set_routing_with_limits" => {
                    let aliases: BTreeMap<String, String> = operation["aliases"]
                        .as_object()
                        .ok_or_else(|| {
                            ConfigError::invalid_data("routing aliases must be object".into())
                        })?
                        .iter()
                        .map(|(name, target)| {
                            (name.clone(), target.as_str().unwrap_or("").to_owned())
                        })
                        .collect();
                    if text(operation, "type") == "set_routing_with_limits" {
                        let limits = &operation["context_limits"];
                        let default = limits["default"]
                            .as_str()
                            .map(ContextSize::parse)
                            .transpose()
                            .map_err(|error| ConfigError::invalid_data(error.to_string()))?;
                        let model_aliases = limits["model_aliases"]
                            .as_object()
                            .ok_or_else(|| {
                                ConfigError::invalid_data(
                                    "routing context aliases must be object".into(),
                                )
                            })?
                            .iter()
                            .map(|(name, value)| {
                                ContextSize::parse(value.as_str().unwrap_or(""))
                                    .map(|size| (name.clone(), size))
                                    .map_err(|error| ConfigError::invalid_data(error.to_string()))
                            })
                            .collect::<Result<BTreeMap<_, _>>>()?;
                        configuration.set_model_routing_with_limits(
                            text(operation, "value"),
                            &aliases,
                            Some(&ModelPresetContextLimits {
                                default,
                                model_aliases,
                            }),
                        )?;
                    } else {
                        configuration.set_model_routing(text(operation, "value"), &aliases)?;
                    }
                }
                "set_context" => configuration.set_context_limit(
                    ContextSize::parse(text(operation, "value"))
                        .map_err(|error| ConfigError::invalid_data(error.to_string()))?,
                )?,
                "set_skill" => configuration.set_skill_enabled(
                    text(operation, "path"),
                    operation["enabled"].as_bool().unwrap_or(false),
                )?,
                "set_preset" => {
                    let aliases = operation["aliases"]
                        .as_object()
                        .ok_or_else(|| ConfigError::invalid_data("aliases must be object".into()))?
                        .iter()
                        .map(|(name, target)| {
                            (name.clone(), target.as_str().unwrap_or("").to_owned())
                        })
                        .collect();
                    configuration.set_model_preset(
                        text(operation, "name"),
                        &ModelPresetConfig {
                            model: text(operation, "model").to_owned(),
                            model_aliases: aliases,
                            context_limits: None,
                        },
                    )?;
                }
                _ => return Err(ConfigError::invalid_data("unknown oracle operation".into())),
            }
        }
        configuration = Configuration::load(&path, &predefined, &env, |spec| {
            validate_fixture(spec, known)
        })?;
    }
    let selectors: Vec<String> = input
        .get("selectors")
        .and_then(Value::as_array)
        .map(|values| {
            values
                .iter()
                .map(|v| v.as_str().unwrap_or("").to_owned())
                .collect()
        })
        .unwrap_or_else(|| vec!["p/m/fast".into(), "openai/gpt-5.4".into()]);
    let snapshot = configuration.snapshot(&selectors);
    let mut selected = serde_json::Map::new();
    for value in input["paths"]
        .as_array()
        .ok_or_else(|| ConfigError::invalid_data("paths must be array".into()))?
    {
        let path = value.as_str().unwrap_or("");
        let mut current = &snapshot;
        for segment in path.split('.') {
            current = current.get(segment).unwrap_or(&Value::Null);
        }
        selected.insert(path.to_owned(), current.clone());
    }
    let mut result = json!({"selected":selected});
    if input.get("operations").is_some() {
        result["savedYaml"] = json!(std::fs::read_to_string(path)?);
    }
    Ok(result)
}
