//! Fixed bundled-template migration evidence, not general Scriban compatibility.
#[path = "../config/product_changes.rs"]
mod product_changes;
use parrot_foundation::{
    config::{
        node::{Kind, Node},
        yaml,
    },
    template_minijinja::MiniJinjaPromptTemplateEngine,
    template_scalar::ScalarPromptTemplateEngine,
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
    assert_eq!(mapping(old_templates).len(), 73);
    assert_eq!(mapping(new_templates).len(), 74);
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
        .filter(|(id, _)| scalar(id) != Some("session.legacy-resume-warning"))
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
                assert_eq!(scalar(old_value), row["old"]["text"].as_str(), "{id}");
                assert_eq!(scalar(new_value), row["new"]["text"].as_str(), "{id}");
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
        if scalar(old_key) == Some("system_prompts") {
            let mut expected = old_value.clone();
            let base = expected.get_mut("runtime:system-context:01-base").unwrap();
            // 4f1117e added this human guidance to the legacy resource only.
            let text = base
                .as_str()
                .unwrap()
                .strip_suffix(concat!(
                    "\n## Minimize processes\n",
                    "Do not lose sight of your objective by writing reports that is not part of the objective."
                ))
                .unwrap()
                .to_owned();
            base.kind = Kind::Scalar(Some(text));
            assert_eq!(&expected, new_value, "system prompt resource changed");
        } else if scalar(old_key) != Some("prompt_templates") {
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
        62
    );
}

#[test]
fn all_thirty_m2_cases_match_independent_original_net_golden_outputs() {
    let mapping = fixture("bundled-migration-map.json");
    let inputs = migration_fixture("m2-source-input.json");
    let oracle = migration_fixture("m2-source-oracle.json");
    let input_rows = inputs.as_array().unwrap();
    let expected_rows = oracle["cases"].as_array().unwrap();
    assert_eq!(input_rows.len(), 30);
    assert_eq!(input_rows.len(), expected_rows.len());
    let descriptors = mapping["entries"].as_array().unwrap();
    let mut counts = std::collections::BTreeMap::<String, usize>::new();
    for (input, expected) in input_rows.iter().zip(expected_rows) {
        assert_eq!(input["id"], expected["id"]);
        let id = input["template_id"].as_str().unwrap();
        let descriptor = descriptors.iter().find(|row| row["id"] == id).unwrap();
        let output = render(
            id,
            descriptor["new"]["text"].as_str().unwrap(),
            &input["arguments"],
        );
        assert_eq!(
            output,
            expected["output"]["value"].as_str().unwrap(),
            "{}",
            input["id"].as_str().unwrap()
        );
        *counts.entry(id.to_owned()).or_default() += 1;
    }
    assert_eq!(counts.len(), 10);
    assert!(counts.values().all(|count| *count == 3));
}

const CONSOLIDATED_SOURCE_IDS: [&str; 13] = [
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
const CONSOLIDATED_IDS: [&str; 3] = [
    "agent-task.assignment-context",
    "agent-session.nonempty-queues-reminder",
    "compact-context-tool.result",
];

const AGENT_COMMUNICATION_SOURCE_IDS: [&str; 5] = [
    "agent-session.child-question",
    "agent-session.pending-child-question-reminder",
    "tool-result.question-replied",
    "agent-session.final-request",
    "agent-session.final-request-interrupted",
];
const AGENT_COMMUNICATION_IDS: [&str; 6] = [
    "agent-session.agent-ask",
    "agent-session.pending-ask-reminder",
    "tool-result.ask-reply",
    "tool-result.ask-replied",
    "agent-session.final-request",
    "agent-session.final-request-interrupted",
];

#[test]
fn retained_templates_match_the_independent_source_inventory_and_goldens() {
    let inventory = fixture("catalog-inventory.json");
    let expected = inventory["entries"].as_array().unwrap();
    assert_eq!(expected.len(), 107);
    let goldens = fixture("catalog-render-goldens.json");
    let golden_rows = goldens["entries"].as_array().unwrap();
    assert_eq!(golden_rows.len(), 107);
    let expected = expected
        .iter()
        .filter(|row| {
            !CONSOLIDATED_SOURCE_IDS.contains(&row["id"].as_str().unwrap())
                && !AGENT_COMMUNICATION_SOURCE_IDS.contains(&row["id"].as_str().unwrap())
        })
        .collect::<Vec<_>>();
    let golden_rows = golden_rows
        .iter()
        .filter(|row| {
            !CONSOLIDATED_SOURCE_IDS.contains(&row["template_id"].as_str().unwrap())
                && !AGENT_COMMUNICATION_SOURCE_IDS.contains(&row["template_id"].as_str().unwrap())
        })
        .collect::<Vec<_>>();
    assert_eq!(expected.len(), 89);
    assert_eq!(golden_rows.len(), 89);
    let system_old =
        include_str!("../../../../assets/config/predefined_config.system_prompts.yaml");
    let system_new =
        include_str!("../../../../assets/config/predefined_config.system_prompts.minijinja.yaml");
    let agent_tasks =
        fs::read_to_string(root().join("assets/config/predefined_config.agent_tasks.yaml"))
            .unwrap();
    let profiles =
        fs::read_to_string(root().join("assets/config/predefined_config.profiles.yaml")).unwrap();
    let runtime =
        fs::read_to_string(root().join("assets/config/predefined_config.runtime.yaml")).unwrap();
    let compose = |system| {
        yaml::compose(&[
            ("Config/predefined_config.system_prompts.yaml", Some(system)),
            (
                "Config/predefined_config.agent_tasks.yaml",
                Some(&agent_tasks),
            ),
            ("Config/predefined_config.profiles.yaml", Some(&profiles)),
            ("Config/predefined_config.runtime.yaml", Some(&runtime)),
        ])
        .unwrap()
    };
    let old = yaml::parse_root(&compose(system_old)).unwrap();
    let new = yaml::parse_root(&compose(system_new)).unwrap();
    let old_templates = mapping(field(&old, "prompt_templates"));
    let new_templates = mapping(field(&new, "prompt_templates"));
    assert_eq!(old_templates.len(), 98);
    assert_eq!(new_templates.len(), 99);
    let expected_ids = expected
        .iter()
        .map(|row| row["id"].as_str().unwrap())
        .chain(CONSOLIDATED_IDS)
        .chain(AGENT_COMMUNICATION_IDS)
        .chain(["session.legacy-resume-warning"])
        .collect::<BTreeSet<_>>();
    assert_eq!(
        new_templates
            .iter()
            .map(|(id, _)| scalar(id).unwrap())
            .collect::<BTreeSet<_>>(),
        expected_ids
    );
    let old_templates = old_templates
        .iter()
        .filter(|(id, _)| {
            !CONSOLIDATED_IDS.contains(&scalar(id).unwrap())
                && !AGENT_COMMUNICATION_IDS.contains(&scalar(id).unwrap())
        })
        .collect::<Vec<_>>();
    let new_templates = new_templates
        .iter()
        .filter(|(id, _)| {
            !CONSOLIDATED_IDS.contains(&scalar(id).unwrap())
                && !AGENT_COMMUNICATION_IDS.contains(&scalar(id).unwrap())
                && scalar(id) != Some("session.legacy-resume-warning")
        })
        .collect::<Vec<_>>();
    let mut scalar_count = 0;
    let mut minijinja_count = 0;
    for (index, ((old_id, old_template), (new_id, new_template))) in
        old_templates.into_iter().zip(new_templates).enumerate()
    {
        let id = expected[index]["id"].as_str().unwrap();
        assert_eq!(scalar(old_id), Some(id));
        assert_eq!(old_id, new_id);
        for key in ["allowed_arguments", "required_arguments"] {
            let old_names = sequence(field(old_template, key));
            let new_names = sequence(field(new_template, key));
            let expected_names: Vec<_> = expected[index][if key == "allowed_arguments" {
                "allowed"
            } else {
                "required"
            }]
            .as_array()
            .unwrap()
            .iter()
            .map(|x| x.as_str().unwrap())
            .collect();
            assert_eq!(old_names, expected_names, "{id}.{key}");
            assert_eq!(new_names, expected_names, "{id}.{key}");
        }
        let old_engine = field_opt(old_template, "engine")
            .and_then(Node::as_str)
            .unwrap_or("scalar");
        let new_engine = field_opt(new_template, "engine")
            .and_then(Node::as_str)
            .unwrap_or("scalar");
        assert_eq!(expected[index]["engine"], old_engine);
        assert_eq!(
            new_engine,
            if old_engine == "scriban" {
                "minijinja"
            } else {
                "scalar"
            }
        );
        let golden = &golden_rows[index];
        assert_eq!(golden["template_id"], id);
        let expected_output =
            product_changes::align_product_text(id, golden["expected"].as_str().unwrap());
        let argument_values = fields(&golden["arguments"]);
        let output = if new_engine == "scalar" {
            scalar_count += 1;
            let allowed: BTreeSet<String> = sequence(field(new_template, "allowed_arguments"))
                .into_iter()
                .collect();
            let arguments: std::collections::BTreeMap<String, String> = argument_values
                .iter()
                .map(|(key, value)| (key.clone(), value.as_str().unwrap().to_owned()))
                .collect();
            let engine = ScalarPromptTemplateEngine::compile(
                &format!("prompt_templates.{id}"),
                field(new_template, "template").as_str().unwrap(),
                &allowed,
            )
            .unwrap();
            engine.render(&arguments, &NoCancellation).unwrap()
        } else {
            minijinja_count += 1;
            let engine = MiniJinjaPromptTemplateEngine::compile(
                &format!("prompt_templates.{id}"),
                field(new_template, "template").as_str().unwrap(),
            )
            .unwrap();
            engine.render(argument_values, &NoCancellation).unwrap()
        };
        assert_eq!(output, expected_output, "source-golden M6: {id}");
    }
    assert_eq!(scalar_count, 79);
    assert_eq!(minijinja_count, 10);
}

#[test]
fn m3_content_is_opaque_and_repeated_template_calls_are_isolated() {
    let mapping = fixture("bundled-migration-map.json");
    let inputs = migration_fixture("m2-source-input.json");
    let oracle = migration_fixture("m2-source-oracle.json");
    let cases = inputs.as_array().unwrap();
    let results = oracle["cases"].as_array().unwrap();
    for descriptor in mapping["entries"].as_array().unwrap() {
        let id = descriptor["id"].as_str().unwrap();
        let index = cases
            .iter()
            .position(|row| row["id"] == format!("A008.M2.{id}.multiple"))
            .unwrap();
        let input = &cases[index];
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
        assert_eq!(
            first,
            results[index]["output"]["value"].as_str().unwrap(),
            "{id} opaque insertion"
        );
    }
    let unicode = cases
        .iter()
        .position(|row| row["id"] == "A008.M2.monitor.ended.nested-branch")
        .unwrap();
    assert!(
        results[unicode]["output"]["value"]
            .as_str()
            .unwrap()
            .contains("Unicode 🪶\nHostile {{ not_parsed }} $${NAME}")
    );
    let multiline = cases
        .iter()
        .position(|row| row["id"] == "A008.M2.context.model-aliases.nested-branch")
        .unwrap();
    assert!(
        results[multiline]["output"]["value"]
            .as_str()
            .unwrap()
            .contains("literal {{ value }}\nnext")
    );
    let selection = cases
        .iter()
        .position(|row| row["id"] == "A008.M2.status.selection.multiple")
        .unwrap();
    assert!(
        results[selection]["output"]["value"]
            .as_str()
            .unwrap()
            .contains("{{ opaque }}\nnext $${NAME} 🪶")
    );
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
