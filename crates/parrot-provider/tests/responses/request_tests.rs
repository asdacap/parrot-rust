use super::projection::observe_outbound;
use serde_json::Value;

const OUTBOUND_ORACLE: &str = include_str!("fixtures/outbound/expected.json");

#[test]
fn source_encoded_responses_requests_match_actual_dotnet_adapter() {
    let expected: Value = serde_json::from_str(OUTBOUND_ORACLE).unwrap();
    assert_eq!(expected["cases"].as_array().unwrap().len(), 16);
    let selected = [
        "prepared_provider_id",
        "prepared_input",
        "prepared_body",
        "prepared_http_utf8",
        "prepared_http_base64",
        "encoded_utf8",
        "encoded_base64",
    ];
    for case in expected["cases"].as_array().unwrap() {
        let observed = observe_outbound(&case["input"]);
        let projected = Value::Object(
            selected
                .iter()
                .map(|key| ((*key).to_owned(), observed[*key].clone()))
                .collect(),
        );
        let expected_projection = Value::Object(
            selected
                .iter()
                .map(|key| ((*key).to_owned(), case["observed"][*key].clone()))
                .collect(),
        );
        assert_eq!(projected, expected_projection, "{}", case["case_id"]);
    }
}
