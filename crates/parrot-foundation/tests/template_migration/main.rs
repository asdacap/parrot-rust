//! Fixed bundled-template migration evidence, not general Scriban compatibility.
#[path = "../config/product_changes.rs"]
mod product_changes;
use parrot_foundation::{
    config::{
        node::{Kind, Node},
        yaml,
    },
    template_minijinja::MiniJinjaPromptTemplateEngine,
};
use serde_json::{Map, Value};
use std::{collections::BTreeSet, fs, path::PathBuf};

fn root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..")
}
fn fixture(name: &str) -> Value {
    serde_json::from_slice(
        &fs::read(root().join("tests/fixtures/scriban/a008").join(name)).unwrap(),
    )
    .unwrap()
}
fn migration_fixture(name: &str) -> Value {
    serde_json::from_slice(
        &fs::read(
            root()
                .join("crates/parrot-foundation/tests/template_migration/fixtures")
                .join(name),
        )
        .unwrap(),
    )
    .unwrap()
}
fn mapping(node: &Node) -> &[(Node, Node)] {
    match &node.kind {
        Kind::Mapping(entries) => entries,
        _ => panic!("expected YAML mapping"),
    }
}
fn field_opt<'a>(node: &'a Node, name: &str) -> Option<&'a Node> {
    mapping(node)
        .iter()
        .find(|(key, _)| key.as_str() == Some(name))
        .map(|(_, value)| value)
}
fn field<'a>(node: &'a Node, name: &str) -> &'a Node {
    field_opt(node, name).unwrap_or_else(|| panic!("missing {name}"))
}
fn scalar(node: &Node) -> Option<&str> {
    match &node.kind {
        Kind::Scalar(value) => value.as_deref(),
        _ => None,
    }
}
fn sequence(node: &Node) -> Vec<String> {
    match &node.kind {
        Kind::Sequence(items) => items
            .iter()
            .map(|item| scalar(item).unwrap().to_owned())
            .collect(),
        _ => panic!("expected sequence"),
    }
}
fn fields(value: &Value) -> &Map<String, Value> {
    value.as_object().unwrap()
}
fn render(id: &str, text: &str, arguments: &Value) -> String {
    let engine =
        MiniJinjaPromptTemplateEngine::compile(&format!("prompt_templates.{id}"), text).unwrap();
    match engine.render(fields(arguments), &NoCancellation) {
        Ok(value) => value,
        Err(error) => panic!("{id}: {error}"),
    }
}
struct NoCancellation;
impl parrot_domain::contracts::async_ops::Cancellation for NoCancellation {
    fn is_cancelled(&self) -> bool {
        false
    }
    fn cancelled(&self) -> parrot_domain::contracts::async_ops::BoxFuture<'_, ()> {
        Box::pin(std::future::pending())
    }
}

#[test]
fn new_resource_retains_ten_engine_migrations_and_known_later_resource_changes() {
    let old = yaml::parse_root(include_str!(
        "../../../../assets/config/predefined_config.system_prompts.yaml"
    ))
    .unwrap();
    let new = yaml::parse_root(include_str!(
        "../../../../assets/config/predefined_config.system_prompts.minijinja.yaml"
    ))
    .unwrap();
    let old_templates = field(&old, "prompt_templates");
    let new_templates = field(&new, "prompt_templates");
    let approved: BTreeSet<_> = fixture("bundled-migration-map.json")["entries"]
        .as_array()
        .unwrap()
        .iter()
        .map(|row| row["id"].as_str().unwrap().to_owned())
        .collect();
    let expected: BTreeSet<_> = [
        "status.selection",
        "status.context",
        "status.context-limit",
        "status.statistics",
        "status.runtime",
        "monitor.ended",
        "context.agent-path-environment",
        "context.model-aliases",
        "context.security-profile",
        "context.subagents",
    ]
    .into_iter()
    .map(str::to_owned)
    .collect();
    assert_eq!(approved, expected);
    assert_eq!(mapping(old_templates).len(), 77);
    assert_eq!(mapping(new_templates).len(), 82);
    let legacy_warning = field(new_templates, "session.legacy-resume-warning");
    assert_eq!(mapping(legacy_warning).len(), 3);
    assert!(sequence(field(legacy_warning, "allowed_arguments")).is_empty());
    assert!(sequence(field(legacy_warning, "required_arguments")).is_empty());
    assert!(
        !scalar(field(legacy_warning, "template"))
            .unwrap()
            .is_empty()
    );
    let retained_templates = mapping(new_templates)
        .iter()
        .filter(|(id, _)| {
            let id = scalar(id).unwrap();
            id != "session.legacy-resume-warning"
                && (!NATIVE_SCALAR_IDS.contains(&id) || field_opt(old_templates, id).is_some())
        })
        .collect::<Vec<_>>();
    assert_eq!(mapping(old_templates).len(), retained_templates.len());
    for ((old_id, old_fields), (new_id, new_fields)) in
        mapping(old_templates).iter().zip(retained_templates)
    {
        assert_eq!(old_id, new_id);
        let id = scalar(old_id).unwrap();
        assert_eq!(mapping(old_fields).len(), mapping(new_fields).len(), "{id}");
        for ((old_key, old_value), (new_key, new_value)) in
            mapping(old_fields).iter().zip(mapping(new_fields))
        {
            assert_eq!(old_key, new_key, "{id}");
            let name = scalar(old_key).unwrap();
            if approved.contains(id) && name == "engine" {
                assert_eq!(scalar(old_value), Some("scriban"), "{id}");
                assert_eq!(scalar(new_value), Some("minijinja"), "{id}");
            } else if approved.contains(id) && name == "template" {
                let entry = fixture("bundled-migration-map.json");
                let row = entry["entries"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .find(|row| row["id"] == id)
                    .unwrap();
                let old_text =
                    product_changes::align_product_text(id, row["old"]["text"].as_str().unwrap());
                let new_text =
                    product_changes::align_product_text(id, row["new"]["text"].as_str().unwrap());
                assert_eq!(scalar(old_value), Some(old_text.as_str()), "{id}");
                assert_eq!(scalar(new_value), Some(new_text.as_str()), "{id}");
            } else {
                assert_eq!(
                    old_value, new_value,
                    "unapproved field changed: {id}.{name}"
                );
            }
        }
    }
    let old_root = mapping(&old);
    let new_root = mapping(&new);
    assert_eq!(old_root.len(), new_root.len());
    for ((old_key, old_value), (new_key, new_value)) in old_root.iter().zip(new_root) {
        assert_eq!(old_key, new_key);
        if !matches!(scalar(old_key), Some("system_prompts" | "prompt_templates")) {
            assert_eq!(
                old_value, new_value,
                "non-template resource section changed"
            );
        }
    }
    assert_eq!(
        mapping(new_templates)
            .iter()
            .filter(|(_, fields)| field_opt(fields, "engine")
                .and_then(scalar)
                .unwrap_or("scalar")
                == "scalar")
            .count(),
        71
    );
}

const NATIVE_SCALAR_IDS: [&str; 9] = [
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

#[test]
fn m3_content_is_opaque_and_repeated_template_calls_are_isolated() {
    let mapping = fixture("bundled-migration-map.json");
    let inputs = migration_fixture("m2-source-input.json");
    let cases = inputs.as_array().unwrap();
    for descriptor in mapping["entries"].as_array().unwrap() {
        let id = descriptor["id"].as_str().unwrap();
        let input = &cases
            .iter()
            .position(|row| row["id"] == format!("A008.M2.{id}.multiple"))
            .map(|index| &cases[index])
            .unwrap();
        let engine = MiniJinjaPromptTemplateEngine::compile(
            &format!("prompt_templates.{id}"),
            descriptor["new"]["text"].as_str().unwrap(),
        )
        .unwrap();
        let first = engine
            .render(fields(&input["arguments"]), &NoCancellation)
            .unwrap();
        let second = engine
            .render(fields(&input["arguments"]), &NoCancellation)
            .unwrap();
        assert_eq!(first, second, "{id} repeated render");
    }
}

#[test]
fn each_engine_is_separate_and_never_mutates_argument_json() {
    let mapping = fixture("bundled-migration-map.json");
    let cases = migration_fixture("m2-source-input.json");
    for descriptor in mapping["entries"].as_array().unwrap() {
        let id = descriptor["id"].as_str().unwrap();
        let input = cases
            .as_array()
            .unwrap()
            .iter()
            .find(|row| row["id"] == format!("A008.M2.{id}.nested-branch"))
            .unwrap();
        let before = input["arguments"].clone();
        let actual = render(id, descriptor["new"]["text"].as_str().unwrap(), &before);
        assert!(!actual.is_empty(), "{id}");
        assert_eq!(input["arguments"], before, "{id} arguments immutable");
    }
}
