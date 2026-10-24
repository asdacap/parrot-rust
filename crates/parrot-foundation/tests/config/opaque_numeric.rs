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

fn opaque_numeric_supplement() -> Vec<(serde_json::Value, serde_json::Value)> {
    let inputs: Vec<serde_json::Value> =
        serde_json::from_str(include_str!("fixtures/opaque-numeric/input.json")).unwrap();
    let oracle: serde_json::Value =
        serde_json::from_str(include_str!("fixtures/opaque-numeric/oracle.json")).unwrap();
    let outputs = oracle["cases"].as_array().unwrap();
    assert_eq!(inputs.len(), 131);
    assert_eq!(inputs.len(), outputs.len());
    inputs.into_iter().zip(outputs.iter().cloned()).collect()
}

#[test]
fn opaque_numeric_matches_independent_dotnet_supplement() {
    for (input, expected) in opaque_numeric_supplement()
        .into_iter()
        .filter(|(_, expected)| expected["output"]["error"].is_null())
    {
        assert_eq!(input["id"], expected["id"]);
        assert_eq!(
            serde_json::Value::String(
                yaml_json(&Node::scalar(input["scalar"].as_str().unwrap())).unwrap()
            ),
            expected["output"]["value"],
            "{}",
            input["id"]
        );
    }
}

#[test]
#[ignore = "Rust configuration errors no longer recommend .NET serializer settings"]
fn opaque_numeric_nonfinite_errors_match_dotnet_supplement() {
    for (input, expected) in opaque_numeric_supplement()
        .into_iter()
        .filter(|(_, expected)| expected["output"]["error"].is_string())
    {
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

#[test]
fn opaque_nonfinite_numbers_report_native_json_errors() {
    let mut rejected = 0;
    for (input, _) in opaque_numeric_supplement()
        .into_iter()
        .filter(|(_, expected)| expected["output"]["error"].is_string())
    {
        let error = yaml_json(&Node::scalar(input["scalar"].as_str().unwrap())).unwrap_err();
        assert_eq!(
            error.to_string(),
            "Non-finite number values cannot be written as valid JSON.",
            "{}",
            input["id"]
        );
        rejected += 1;
    }
    assert_eq!(rejected, 42);
}
