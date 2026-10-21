use super::transport_projection::{INPUTS, observe_case};
use serde_json::Value;

// The oracle is deliberately test-only; the standalone producer compiles only
// input scripts and the real Rust observer, never source expected observations.
#[cfg(test)]
const ORACLE: &str = include_str!("fixtures/expected.json");

#[test]
fn source_transport_cases_match_owned_observations() {
    let inputs: Value = serde_json::from_str(INPUTS).unwrap();
    let oracle: Value = serde_json::from_str(ORACLE).unwrap();
    let cases = inputs["cases"].as_array().unwrap();
    let expected = oracle["cases"].as_array().unwrap();
    assert_eq!(cases.len(), 56);
    assert_eq!(expected.len(), cases.len());
    for (kind, count) in [("send", 37), ("client", 12), ("authority", 7)] {
        assert_eq!(
            cases.iter().filter(|case| case["kind"] == kind).count(),
            count
        );
    }
    let mut mismatches = Vec::new();
    for input in cases {
        let id = input["case_id"].as_str().unwrap();
        let matches = expected
            .iter()
            .filter(|case| case["case_id"] == id)
            .collect::<Vec<_>>();
        assert_eq!(matches.len(), 1, "unique source observation for {id}");
        let actual = observe_case(input);
        let expected = selected_source(matches[0].clone());
        if actual != expected {
            mismatches.push(format!(
                "{id}:\nactual: {}\nsource: {}",
                serde_json::to_string_pretty(&actual).unwrap(),
                serde_json::to_string_pretty(&expected).unwrap()
            ));
        }
    }
    assert!(
        mismatches.is_empty(),
        "{} transport mismatches:\n{}",
        mismatches.len(),
        mismatches.join("\n\n")
    );
}

/// The source-only ParseState internals and CLR implementation details are not
/// transport observations. Events retain every T27 externally visible field.
fn selected_source(mut source: Value) -> Value {
    for key in [
        "state_done",
        "response_id",
        "output",
        "state_assistant_text",
    ] {
        source.as_object_mut().unwrap().remove(key);
    }
    fn error(value: &mut Value) {
        if let Some(object) = value.as_object_mut() {
            object.remove("hresult");
            // Injected Rust errors carry their public category/message/status,
            // not CLR exception chains. Do not claim inner-exception parity.
            object.remove("inner");
            if object.get("type").and_then(Value::as_str)
                == Some("System.Text.Json.JsonReaderException")
            {
                object.remove("message");
                object.remove("path");
            }
        }
    }
    if source.get("exception").is_some() {
        error(&mut source["exception"]);
    }
    if source.get("endpoint_exception").is_some() {
        error(&mut source["endpoint_exception"]);
    }
    source
}
