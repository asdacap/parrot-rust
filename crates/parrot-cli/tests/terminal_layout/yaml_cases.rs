use parrot_cli::terminal_layout::ToolYamlFormatter;
use serde_json::{Value, json};

#[test]
fn matches_all_580_independent_dotnet_yaml_formatter_cases() {
    for (name, inputs, outputs) in [
        (
            "focused",
            include_str!("fixtures/a007-oracle/inputs.jsonl"),
            include_str!("fixtures/a007-oracle/outputs.jsonl"),
        ),
        (
            "extra",
            include_str!("fixtures/a007-oracle/extra-inputs.jsonl"),
            include_str!("fixtures/a007-oracle/extra-outputs.jsonl"),
        ),
        (
            "keys",
            include_str!("fixtures/a007-oracle/key-inputs.jsonl"),
            include_str!("fixtures/a007-oracle/key-outputs.jsonl"),
        ),
        (
            "decimal",
            include_str!("fixtures/a007-oracle/decimal-inputs.jsonl"),
            include_str!("fixtures/a007-oracle/decimal-outputs.jsonl"),
        ),
        (
            "nested-corpus",
            include_str!("fixtures/a007-oracle/corpus-inputs.jsonl"),
            include_str!("fixtures/a007-oracle/corpus-outputs.jsonl"),
        ),
    ] {
        let input_lines: Vec<_> = inputs.lines().collect();
        let output_lines: Vec<_> = outputs.lines().collect();
        assert_eq!(
            input_lines.len(),
            output_lines.len(),
            "{name} fixture length"
        );
        for (index, (input, expected)) in input_lines.iter().zip(output_lines).enumerate() {
            let source: String = serde_json::from_str(input).unwrap();
            let mut expected: Value = serde_json::from_str(expected).unwrap();
            // Exception messages are runtime/localization text, not the source
            // API boundary; compare the thrown exception class and input.
            if let Some(object) = expected.as_object_mut() {
                object.remove("message");
            }
            let actual = match ToolYamlFormatter::try_format(&source) {
                Ok(Some(formatted)) => json!({"input": source, "ok": true, "value": formatted}),
                Ok(None) => json!({"input": source, "ok": false, "value": source}),
                Err(_) => json!({"input": source, "error": "InvalidOperationException"}),
            };
            assert_eq!(actual, expected, "{name} case {index}: {source}");
        }
    }
}
