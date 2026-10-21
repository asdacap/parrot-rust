use crate::config::node::Node;
use crate::config::opaque_json::yaml_json;

#[test]
fn opaque_numeric_p2_counterexamples_preserve_source_inference() {
    assert_eq!(yaml_json(&Node::scalar("inf")).unwrap(), "\"inf\"");
    assert_eq!(yaml_json(&Node::scalar("-inf")).unwrap(), "\"-inf\"");
    assert_eq!(
        yaml_json(&Node::scalar("\u{a0}123\u{a0}")).unwrap(),
        "\"\\u00A0123\\u00A0\""
    );
    assert_eq!(yaml_json(&Node::scalar("123\0")).unwrap(), "123");
}

#[test]
fn opaque_numeric_matches_independent_dotnet_supplement() {
    let inputs: serde_json::Value =
        serde_json::from_str(include_str!("fixtures/opaque-numeric/input.json")).unwrap();
    let oracle: serde_json::Value =
        serde_json::from_str(include_str!("fixtures/opaque-numeric/oracle.json")).unwrap();
    let inputs = inputs.as_array().unwrap();
    let outputs = oracle["cases"].as_array().unwrap();
    assert_eq!(inputs.len(), 131);
    assert_eq!(inputs.len(), outputs.len());
    for (input, expected) in inputs.iter().zip(outputs) {
        assert_eq!(input["id"], expected["id"]);
        let result = yaml_json(&Node::scalar(input["scalar"].as_str().unwrap()));
        match result {
            Ok(value) => assert_eq!(
                serde_json::Value::String(value),
                expected["output"]["value"],
                "{}",
                input["id"]
            ),
            Err(error) => assert_eq!(
                serde_json::Value::String(error.to_string()),
                expected["output"]["error"],
                "{}",
                input["id"]
            ),
        }
    }
}
