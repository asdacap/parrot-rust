use crate::support::Context;
use serde_json::{Value, json};

#[test]
fn source_assertions() {
    let context = Context::new();
    let configuration = context.configured("tools:\n  question:\n    parameters:\n      properties:\n        questions:\n          minItems: 2\n          items:\n            properties:\n              prompt:\n                description: Custom question prompt.\n      required: [questions, custom]\n").unwrap();
    let definitions = &configuration.runtime.tools;
    assert_eq!(definitions.len(), 30);
    assert!(definitions.contains_key("set_exit_reminder"));
    assert!(definitions.contains_key("clear_exit_reminder"));
    let schema =
        |id: &str| serde_json::from_str::<Value>(&definitions[id].parameters_json).unwrap();
    let compact = schema("compact_context");
    assert_eq!(compact["type"], "object");
    assert_eq!(compact["additionalProperties"], false);
    let exit = schema("set_exit_reminder");
    assert_eq!(exit["additionalProperties"], false);
    let mut required: Vec<_> = exit["required"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap())
        .collect();
    required.sort();
    assert_eq!(required, ["description", "title"]);
    let clear = schema("clear_exit_reminder");
    assert_eq!(clear["additionalProperties"], false);
    assert_eq!(clear["required"], json!(["title"]));
    assert!(
        definitions["question"]
            .description
            .starts_with("Ask ordered questions")
    );
    let question = schema("question");
    assert_eq!(question["properties"]["questions"]["minItems"], 2);
    assert_eq!(
        question["properties"]["questions"]["items"]["properties"]["prompt"]["description"],
        "Custom question prompt."
    );
    assert_eq!(
        question["required"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap())
            .collect::<Vec<_>>()
            .join(","),
        "questions,custom"
    );
    assert_eq!(schema("status")["type"], "object");
}
