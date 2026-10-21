use super::projection::{observe_outbound, observe_stream_async_for_provider};
use serde_json::{Value, json};

const ORACLE: &str = include_str!("fixtures/source-cases-v1/expected.json");
const PROJECTION_INPUTS: &str = include_str!("fixtures/source-cases-v1/projection-inputs.json");

#[test]
fn independently_captured_source_case_v1_matches_rust_projection() {
    let oracle: Value = serde_json::from_str(ORACLE).unwrap();
    let inputs: Value = serde_json::from_str(PROJECTION_INPUTS).unwrap();
    assert_eq!(
        oracle["source_commit"],
        "5f27a8a92625ee31d8bec094e5c3e09550193781"
    );
    assert_eq!(oracle["contract"], "t27-responses-source-cases-v1");
    assert_eq!(inputs["contract"], "t27-responses-source-projection-v1");
    let expected = oracle["cases"].as_array().unwrap();
    let actual = inputs["cases"].as_array().unwrap();
    assert_eq!(expected.len(), 14);
    assert_eq!(actual.len(), expected.len());

    for (case, projected) in expected.iter().zip(actual) {
        let id = case["case_id"].as_str().unwrap();
        assert_eq!(projected["case_id"], format!("source-v1.{id}"));
        assert_eq!(projected["kind"], case["kind"]);
        assert_eq!(projected["input"], case["input"]);
        let kind = case["kind"].as_str().unwrap();
        match kind {
            "stream" => {
                let (events, exception) =
                    observe_stream_async_for_provider(&projected["input"], "test");
                assert_eq!(
                    json!({"events":events,"exception":exception}),
                    case["observed"],
                    "{id}"
                );
            }
            "outbound" => {
                assert_eq!(
                    observe_outbound(&projected["input"]),
                    case["observed"],
                    "{id}"
                );
            }
            "roundtrip" => assert_roundtrip(case, projected),
            other => panic!("unknown source case kind: {other}"),
        }
    }
}

fn assert_roundtrip(case: &Value, projected: &Value) {
    let id = case["case_id"].as_str().unwrap();
    let input = &projected["input"];
    let (events, exception) = observe_stream_async_for_provider(input, "test");
    let expected = &case["observed"];
    let expected_exception = if expected["exception"].is_null() {
        None
    } else {
        Some(expected["exception"].clone())
    };
    assert_eq!(exception, expected_exception, "{id} parse exception");
    assert_eq!(json!(events), expected["events"], "{id} stream events");
    let completed = events
        .iter()
        .find(|event| event["Kind"] == "Completed")
        .unwrap_or_else(|| panic!("{id} missing Completed event"));
    let spec = &input["request"];
    let mut messages = Vec::new();
    if let Some(text) = spec["initial_user"]
        .as_str()
        .filter(|text| !text.is_empty())
    {
        messages.push(json!({"role":"User","contents":[{"kind":"text","text":text}]}));
    }
    let provider_content = &completed["ProviderContent"];
    let content = provider_content["Content"].as_str().unwrap_or_default();
    let provider_id = provider_content["ProviderId"].as_str().unwrap_or_default();
    let assistant = json!({
        "role":"Assistant",
        "contents":[{"kind":"text","text":completed["AssistantText"].as_str().unwrap_or_default()}],
        "tool_calls":completed["ToolCalls"].as_array().unwrap().iter().map(|call| json!({
            "id":call["Id"], "name":call["Name"], "arguments_json":call["ArgumentsJson"]
        })).collect::<Vec<_>>(),
        "provider_content":{"provider_id":provider_id,"content":content}
    });
    messages.push(assistant);
    if let Some(result) = spec.get("tool_result") {
        messages.push(json!({
            "role":"Tool",
            "tool_call_id":result["call_id"],
            "contents":[{"kind":"text","text":result["content"]}]
        }));
    }
    if let Some(text) = spec["following_user"]
        .as_str()
        .filter(|text| !text.is_empty())
    {
        messages.push(json!({"role":"User","contents":[{"kind":"text","text":text}]}));
    }
    let mut request = json!({"model":spec["model"],"provider_id":"test","messages":messages});
    if let Some(reasoning) = spec.get("reasoning") {
        request["reasoning"] = reasoning.clone();
    }
    let prepared = observe_outbound(&request);
    let expected_prepared = &expected["prepared"];
    for key in [
        "prepared_provider_id",
        "prepared_input",
        "prepared_body",
        "prepared_http_utf8",
        "prepared_http_base64",
        "encoded_utf8",
        "encoded_base64",
    ] {
        assert_eq!(prepared[key], expected_prepared[key], "{id} {key}");
    }
    assert_eq!(
        expected["parsed_provider_content"]["ProviderId"],
        provider_id
    );
    assert_eq!(expected["parsed_provider_content"]["Content"], content);
}
