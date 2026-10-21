//! Representative date and YAML explicit-key regressions with independent expected output.

use parrot_cli::terminal_layout::{EnhancedFinalMessageRenderer, ToolYamlFormatter};
use serde_json::{Value, json};

fn assert_format(json: &str, expected: &str) {
    assert_eq!(
        ToolYamlFormatter::try_format(json).unwrap().as_deref(),
        Some(expected),
        "TryFormat: {json}"
    );
    assert_eq!(
        ToolYamlFormatter::format(json).unwrap(),
        expected,
        "Format: {json}"
    );
    assert_eq!(
        EnhancedFinalMessageRenderer::render(json, "", 80, false, true).unwrap(),
        EnhancedFinalMessageRenderer::render(
            &format!("```yaml\n{expected}\n```"),
            "",
            80,
            false,
            false
        )
        .unwrap(),
        "final-message: {json}"
    );
}

#[test]
fn matches_representative_date_and_yaml_explicit_key_rows() {
    let inputs: Vec<Value> = include_str!("round3_final_inputs.jsonl")
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    let outputs: Vec<Value> = include_str!("round3_final_outputs.jsonl")
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    assert_eq!(inputs.len(), outputs.len());
    for (input, output) in inputs.iter().zip(&outputs) {
        let source = input["source"].as_str().unwrap();
        let (json, expected) = if input["op"] == "date" {
            (
                json!({ (source): 1 }).to_string(),
                output["yaml"].as_str().unwrap(),
            )
        } else {
            assert_eq!(output["ok"], true);
            (source.to_owned(), output["formatted"].as_str().unwrap())
        };
        assert_format(&json, expected);
    }
}

#[test]
fn long_keys_use_utf16_length_for_the_1024_unit_boundary() {
    // Templates retain the independent expected syntax without storing repeated keys.
    for (character, count, yaml_character, template) in [
        ("a", 1023, "a", "{key}: 1"),
        ("a", 1024, "a", "{key}: 1"),
        ("a", 1025, "a", "? {key}\n: 1"),
        ("😀", 511, "\\U0001F600", "\"{key}\": 1"),
        ("😀", 512, "\\U0001F600", "\"{key}\": 1"),
        ("😀", 513, "\\U0001F600", "? \"{key}\"\n: 1"),
        ("\"", 1024, "\"", "'{key}': 1"),
        ("\"", 1025, "\"", "? '{key}'\n: 1"),
    ] {
        let key = character.repeat(count);
        let expected = template.replace("{key}", &yaml_character.repeat(count));
        assert_format(&json!({ (key): 1 }).to_string(), &expected);
    }
}

#[test]
fn explicit_long_keys_preserve_nested_value_indentation() {
    let key = "a".repeat(1025);
    for (value, root, nested, sequence) in [
        (
            json!(1),
            "? {key}\n: 1",
            "outer:\n  ? {key}\n  : 1",
            "- ? {key}\n  : 1",
        ),
        (
            json!({ "nested": [1, { "x": 2 }] }),
            "? {key}\n: nested:\n  - 1\n  - x: 2",
            "outer:\n  ? {key}\n  : nested:\n    - 1\n    - x: 2",
            "- ? {key}\n  : nested:\n    - 1\n    - x: 2",
        ),
        (
            json!([1, [2], { "x": 3 }]),
            "? {key}\n: - 1\n  - - 2\n  - x: 3",
            "outer:\n  ? {key}\n  : - 1\n    - - 2\n    - x: 3",
            "- ? {key}\n  : - 1\n    - - 2\n    - x: 3",
        ),
    ] {
        let object = json!({ (key.clone()): value });
        for (input, template) in [
            (object.clone(), root),
            (json!({ "outer": object.clone() }), nested),
            (json!([object]), sequence),
        ] {
            assert_format(&input.to_string(), &template.replace("{key}", &key));
        }
    }
}
