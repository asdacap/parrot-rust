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
fn opaque_nonfinite_numbers_report_native_json_errors() {
    let inputs: Vec<serde_json::Value> =
        serde_json::from_str(include_str!("fixtures/opaque-numeric/input.json")).unwrap();
    assert_eq!(inputs.len(), 131);
    let mut rejected = 0;
    for input in inputs {
        let result = yaml_json(&Node::scalar(input["scalar"].as_str().unwrap()));
        match result {
            Ok(_) => {}
            Err(error) => {
                assert_eq!(
                    error.to_string(),
                    "Non-finite number values cannot be written as valid JSON.",
                    "{}",
                    input["id"]
                );
                rejected += 1;
            }
        }
    }
    assert_eq!(rejected, 42);
}
