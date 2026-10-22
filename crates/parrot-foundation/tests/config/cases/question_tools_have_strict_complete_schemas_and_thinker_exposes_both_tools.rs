use crate::support::Context;
use serde_json::Value;
#[test]
fn source_assertions() {
    let context = Context::new();
    let config = context.load().unwrap();
    let question: Value =
        serde_json::from_str(&config.runtime.tools["question"].parameters_json).unwrap();
    assert_eq!(question["additionalProperties"], false);
    assert_eq!(question["required"], serde_json::json!(["questions"]));
    assert!(!config.runtime.tools.contains_key("answer"));
    for (name, target) in [("ask", "name"), ("reply", "request_id")] {
        let schema: Value =
            serde_json::from_str(&config.runtime.tools[name].parameters_json).unwrap();
        assert_eq!(schema["additionalProperties"], false);
        assert_eq!(schema["required"], serde_json::json!([target, "message"]));
        for field in [target, "message"] {
            assert_eq!(schema["properties"][field]["type"], "string");
            assert_eq!(schema["properties"][field]["minLength"], 1);
        }
    }
    let allowed = config.runtime.profiles["thinker"]
        .allowed_tools
        .as_ref()
        .unwrap();
    assert!(allowed.iter().any(|t| t == "question"));
    assert!(allowed.iter().any(|t| t == "ask"));
    assert!(allowed.iter().any(|t| t == "reply"));
    assert!(!allowed.iter().any(|t| t == "answer"));
}
