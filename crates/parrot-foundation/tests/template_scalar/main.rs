use parrot_domain::contracts::async_ops::{BoxFuture, Cancellation, OperationError};
use parrot_foundation::config::{
    ConfigError, configuration::source_predefined_content, node::Node, yaml,
};
use parrot_foundation::template_scalar::{ScalarPromptTemplateEngine, validate_arguments};
use serde_json::{Value, json};
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

fn run_render(input: &Value) -> Result<String, ConfigError> {
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
    engine
        .render(&values, &CancellationState(false))
        .map_err(|failure| match failure {
            OperationError::Cancelled => ConfigError::invalid_data("operation cancelled".into()),
            OperationError::Failed(error) => error,
        })
}

#[test]
fn independent_dotnet_scalar_catalog_cases_match_exact_bytes_and_errors() {
    let inputs: Vec<Value> = serde_json::from_str(include_str!("fixtures/input.json")).unwrap();
    let expected: Value = serde_json::from_str(include_str!("fixtures/oracle.json")).unwrap();
    assert_eq!(inputs.len(), 18);
    let cases = expected["cases"].as_array().unwrap();
    assert_eq!(inputs.len(), cases.len());
    for (input, case) in inputs.iter().zip(cases) {
        assert_eq!(input["id"], case["id"]);
        if !matches!(input["kind"].as_str(), Some("render")) {
            // Load-time syntax/descriptor errors require T11's mandatory real-engine callback.
            // The independent .NET oracle captures these; T12's catalog integration exercises
            // the corresponding Rust callbacks. Never replace them here with a no-op validator.
            continue;
        }
        match run_render(input) {
            Ok(actual) => assert_eq!(json!(actual), case["output"]["value"], "{}", case["id"]),
            Err(error) => assert_eq!(
                error.0,
                case["output"]["error"].as_str().unwrap(),
                "{}",
                case["id"]
            ),
        }
    }
}

fn merged_template_fields(user_yaml: &str, id: &str) -> Node {
    let mut root = yaml::parse_root(&source_predefined_content().unwrap()).unwrap();
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
fn delegated_source_multiline_override_render_matches_exact_source_bytes() {
    let inputs: Vec<Value> = serde_json::from_str(include_str!("fixtures/input.json")).unwrap();
    let oracle: Value = serde_json::from_str(include_str!("fixtures/oracle.json")).unwrap();
    let source = inputs
        .iter()
        .position(|input| input["id"] == "delegated-t11-multiline-and-literal-insertion")
        .unwrap();
    let input = &inputs[source];
    let source_case = &oracle["cases"][source]["output"]["value"]["rendered"][0]["value"];
    let template = merged_template_fields(
        input["yaml"].as_str().unwrap(),
        "agent-session.child-completion",
    );
    let engine =
        engine_from_descriptor("prompt_templates.agent-session.child-completion", &template);
    let values = BTreeMap::from([
        ("agent_name".to_owned(), "worker".to_owned()),
        (
            "result".to_owned(),
            "value {not_a_placeholder}\nnext".to_owned(),
        ),
    ]);
    assert_eq!(
        engine.render(&values, &CancellationState(false)).unwrap(),
        source_case.as_str().unwrap()
    );
}

#[test]
fn inherited_bundle_scalar_insert_keeps_literal_braces_opaque_against_source_output() {
    let fields = merged_template_fields("", "tool-result.output-blob-notice");
    let engine = engine_from_descriptor("prompt_templates.tool-result.output-blob-notice", &fields);
    let arguments = BTreeMap::from([("path".to_owned(), "/tmp/{runtime}".to_owned())]);
    let actual = engine
        .render(&arguments, &CancellationState(false))
        .unwrap();
    let inputs: Vec<Value> = serde_json::from_str(include_str!("fixtures/input.json")).unwrap();
    let expected: Value = serde_json::from_str(include_str!("fixtures/oracle.json")).unwrap();
    let index = inputs
        .iter()
        .position(|input| input["id"] == "delegated-t11-multiline-and-literal-insertion")
        .unwrap();
    assert_eq!(
        actual,
        expected["cases"][index]["output"]["value"]["rendered"][1]["value"]
            .as_str()
            .unwrap()
    );
}

#[test]
fn scalar_values_are_inserted_once_and_unicode_multiline_is_unchanged() {
    let input: Vec<Value> = serde_json::from_str(include_str!("fixtures/input.json")).unwrap();
    for id in ["format-multiline", "unicode-and-zero-like-values"] {
        let case = input.iter().find(|case| case["id"] == id).unwrap();
        assert!(run_render(case).is_ok());
    }
    let literal = input
        .iter()
        .find(|case| case["id"] == "literal-braces-never-reparsed")
        .unwrap();
    let oracle: Value = serde_json::from_str(include_str!("fixtures/oracle.json")).unwrap();
    let expected = oracle["cases"]
        .as_array()
        .unwrap()
        .iter()
        .find(|case| case["id"] == literal["id"])
        .unwrap()["output"]["error"]
        .as_str()
        .unwrap();
    assert_eq!(run_render(literal).unwrap_err().0, expected);

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
            .render(&arguments, &CancellationState(false))
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
        .0,
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
        .0,
        "prompt_templates.example does not allow argument 'other'"
    );
    assert_eq!(
        validate_arguments("prompt_templates.example", &allowed, &required, &[])
            .unwrap_err()
            .0,
        "prompt_templates.example requires argument 'known'"
    );
}

#[test]
fn scalar_render_checks_cancellation_before_observing_missing_values() {
    let allowed = ["name".to_owned()].into();
    let engine =
        ScalarPromptTemplateEngine::compile("prompt_templates.example", "{name}", &allowed)
            .unwrap();
    assert!(matches!(
        engine.render(&BTreeMap::new(), &CancellationState(true)),
        Err(OperationError::Cancelled)
    ));
    assert_eq!(
        engine
            .render(&BTreeMap::new(), &CancellationState(false))
            .unwrap_err(),
        OperationError::Failed(ConfigError::invalid_data(
            "prompt_templates.example requires scalar argument 'name'".into()
        ))
    );
}
