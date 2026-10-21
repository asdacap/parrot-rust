use crate::support::Context;
use serde_json::Value;
#[test]
fn source_assertions() {
    let context = Context::new();
    let config = context.load().unwrap();
    let question: Value =
        serde_json::from_str(&config.runtime.tools["question"].parameters_json).unwrap();
    let answer: Value =
        serde_json::from_str(&config.runtime.tools["answer"].parameters_json).unwrap();
    assert_eq!(question["additionalProperties"], false);
    assert_eq!(question["required"], serde_json::json!(["questions"]));
    assert_eq!(answer["additionalProperties"], false);
    assert_eq!(
        answer["required"],
        serde_json::json!(["agent_name", "answers"])
    );
    assert_eq!(answer["properties"]["answers"]["items"]["type"], "string");
    assert_eq!(answer["properties"]["answers"]["items"]["minLength"], 1);
    let allowed = config.runtime.profiles["thinker"]
        .allowed_tools
        .as_ref()
        .unwrap();
    assert!(allowed.iter().any(|t| t == "question"));
    assert!(allowed.iter().any(|t| t == "answer"));
}
