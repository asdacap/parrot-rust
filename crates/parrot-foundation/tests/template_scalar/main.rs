use parrot_core::template::{ScalarPromptTemplateEngine, validate_arguments};
use parrot_core::{CancellationToken, Error};
use parrot_domain::contracts::async_ops::{BoxFuture, Cancellation};
use parrot_foundation::config::{configuration::source_predefined_content, node::Node, yaml};
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};

struct CancellationState(bool);
impl Cancellation for CancellationState {
    fn is_cancelled(&self) -> bool {
        self.0
    }

    fn cancelled(&self) -> BoxFuture<'_, ()> {
        Box::pin(std::future::pending())
    }
}

fn as_set(values: &[String]) -> BTreeSet<String> {
    values.iter().cloned().collect()
}

fn run_render(input: &Value) -> Result<String, Error> {
    let path = input["path"].as_str().unwrap();
    let text = input["template"].as_str().unwrap();
    let allowed = as_set(
        &input["allowed"]
            .as_array()
            .unwrap()
            .iter()
            .map(|value| value.as_str().unwrap().to_owned())
            .collect::<Vec<_>>(),
    );
    let required = input["required"]
        .as_array()
        .unwrap()
        .iter()
        .map(|value| value.as_str().unwrap().to_owned())
        .collect::<Vec<_>>();
    let (names, values): (Vec<String>, BTreeMap<String, String>) =
        if let Some(structured) = input["structured"].as_array() {
            let mut names = Vec::with_capacity(structured.len());
            let mut values = BTreeMap::new();
            for pair in structured {
                let name = pair[0].as_str().unwrap().to_owned();
                names.push(name.clone());
                if let Some(value) = pair[1].as_str() {
                    values.insert(name, value.to_owned());
                }
            }
            (names, values)
        } else {
            let arguments = input["arguments"].as_array().unwrap();
            let names = arguments
                .iter()
                .map(|pair| pair[0].as_str().unwrap().to_owned())
                .collect();
            let values = arguments
                .iter()
                .map(|pair| {
                    (
                        pair[0].as_str().unwrap().to_owned(),
                        pair[1].as_str().unwrap().to_owned(),
                    )
                })
                .collect();
            (names, values)
        };
    validate_arguments("prompt_templates.case", &allowed, &required, &names)?;
    let engine = ScalarPromptTemplateEngine::compile(path, text, &allowed)?;
    engine.render(&values, CancellationToken::new(&CancellationState(false)))
}

#[test]
fn scalar_values_are_inserted_once_and_unicode_multiline_is_unchanged() {
    let input: Vec<Value> = serde_json::from_str(include_str!("fixtures/input.json")).unwrap();
    for id in ["format-multiline", "unicode-and-zero-like-values"] {
        let case = input.iter().find(|case| case["id"] == id).unwrap();
        assert!(run_render(case).is_ok());
    }
    let unicode = input
        .iter()
        .find(|case| case["id"] == "unicode-and-zero-like-values")
        .unwrap();
    assert_eq!(
        run_render(unicode).unwrap(),
        "[0] e\u{301} \u{1F468}\u{200D}\u{1F469}\u{200D}\u{1F467}\u{200D}\u{1F466} "
    );
}

fn merged_template_fields(user_yaml: &str, id: &str) -> Node {
    let mut root = source_predefined_content().unwrap();
    let user = yaml::parse_root(user_yaml).unwrap();
    yaml::merge(&mut root, &user).unwrap();
    root.get("prompt_templates")
        .unwrap()
        .get(id)
        .unwrap()
        .clone()
}

fn engine_from_descriptor(path: &str, fields: &Node) -> ScalarPromptTemplateEngine {
    let text = fields.get("template").and_then(Node::as_str).unwrap();
    let allowed = fields
        .get("allowed_arguments")
        .and_then(Node::as_sequence)
        .unwrap()
        .iter()
        .map(|value| value.as_str().unwrap().to_owned())
        .collect();
    ScalarPromptTemplateEngine::compile(path, text, &allowed).unwrap()
}

#[test]
fn user_override_of_bundled_template_renders_multiline_values_with_literal_braces() {
    let fields = merged_template_fields(
        "prompt_templates:\n  agent-session.child-completion:\n    template: |-\n      Agent {agent_name} says:\n      {result}\n",
        "agent-session.child-completion",
    );
    let engine = engine_from_descriptor("prompt_templates.agent-session.child-completion", &fields);
    let values = BTreeMap::from([
        ("agent_name".to_owned(), "worker".to_owned()),
        (
            "result".to_owned(),
            "value {not_a_placeholder}\nnext".to_owned(),
        ),
    ]);
    assert_eq!(
        engine
            .render(&values, CancellationToken::new(&CancellationState(false)))
            .unwrap(),
        "Agent worker says:\nvalue {not_a_placeholder}\nnext"
    );
}

#[test]
fn literal_braces_stay_opaque_in_template_text_and_argument_values() {
    let input: Vec<Value> = serde_json::from_str(include_str!("fixtures/input.json")).unwrap();
    let literal = input
        .iter()
        .find(|case| case["id"] == "literal-braces-never-reparsed")
        .unwrap();

    let allowed = ["value".to_owned()].into();
    let engine = ScalarPromptTemplateEngine::compile(
        "prompt_templates.custom.literal",
        "{{prefix}} {value}",
        &allowed,
    )
    .unwrap();
    let value = literal["arguments"][0][1].as_str().unwrap();
    let arguments = BTreeMap::from([("value".to_owned(), value.to_owned())]);
    assert_eq!(
        engine
            .render(
                &arguments,
                CancellationToken::new(&CancellationState(false))
            )
            .unwrap(),
        "{prefix} {{ hostile }} {unresolved}\nnext"
    );
}

#[test]
fn scalar_checker_prioritizes_duplicate_disallowed_and_required_checks_in_source_order() {
    let allowed = ["known".to_owned()].into();
    let required = vec!["known".to_owned()];
    assert_eq!(
        validate_arguments(
            "prompt_templates.example",
            &allowed,
            &required,
            &["other".into(), "other".into()]
        )
        .unwrap_err()
        .message,
        "prompt_templates.example received duplicate argument 'other'"
    );
    assert_eq!(
        validate_arguments(
            "prompt_templates.example",
            &allowed,
            &required,
            &["other".into()]
        )
        .unwrap_err()
        .message,
        "prompt_templates.example does not allow argument 'other'"
    );
    assert_eq!(
        validate_arguments("prompt_templates.example", &allowed, &required, &[])
            .unwrap_err()
            .message,
        "prompt_templates.example requires argument 'known'"
    );
}

#[test]
fn scalar_render_checks_cancellation_before_observing_missing_values() {
    let allowed = ["name".to_owned()].into();
    let engine =
        ScalarPromptTemplateEngine::compile("prompt_templates.example", "{name}", &allowed)
            .unwrap();
    assert_eq!(
        engine
            .render(
                &BTreeMap::new(),
                CancellationToken::new(&CancellationState(true))
            )
            .unwrap_err(),
        Error::cancelled()
    );
    assert_eq!(
        engine
            .render(
                &BTreeMap::new(),
                CancellationToken::new(&CancellationState(false))
            )
            .unwrap_err(),
        Error::template("prompt_templates.example requires scalar argument 'name'")
    );
}
