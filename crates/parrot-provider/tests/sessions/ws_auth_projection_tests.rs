//! Tests use public input declarations only, never source observations.
use super::projection_tests::projection;
use serde_json::{Value, json};

async fn run(dual: bool) -> Value {
    let mut input: Value = serde_json::from_str(if dual {
        include_str!("../routing/fixtures/source/ws-auth-diagnostics-v1/inputs.json")
    } else {
        include_str!("../routing/fixtures/source/ws-auth-v1/inputs.json")
    })
    .unwrap();
    input["collaborators"] = serde_json::from_str(include_str!(
        "../routing/fixtures/source/ws-auth-v1/collaborators.json"
    ))
    .unwrap();
    serde_json::from_str(&projection::project(&input.to_string()).await.unwrap()).unwrap()
}
fn observation<'a>(output: &'a Value, id: &str) -> &'a Value {
    &output["cases"]
        .as_array()
        .unwrap()
        .iter()
        .find(|case| case["case_id"] == id)
        .unwrap()["observation"]
}
fn native<'a>(output: &'a Value, id: &str) -> &'a Value {
    output["native_errors"]
        .as_array()
        .unwrap()
        .iter()
        .find(|case| case["case_id"] == id)
        .unwrap()
}

#[tokio::test(start_paused = true)]
async fn both_public_catalogues_use_actual_session_lifecycle_and_no_connector() {
    for dual in [false, true] {
        let output = run(dual).await;
        assert_eq!(output["cases"].as_array().unwrap().len(), 12);
        assert_eq!(output["native_errors"].as_array().unwrap().len(), 12);
        for case in output["cases"].as_array().unwrap() {
            let result = &case["observation"];
            assert_eq!(result["physical_calls"], result["enumerator_disposals"]);
            assert_eq!(result["session_opens"], 1);
            assert_eq!(result["session_disposals"], 1);
            assert_eq!(result["begin_turns"], 1);
            assert_eq!(result["connector_calls"], 0);
            if case["case_id"].as_str().unwrap().ends_with("-missing") {
                assert_eq!(result["physical_calls"], 1);
                assert_eq!(result["fallback_calls"], 0);
                assert_eq!(result["http_sends"], 0);
                assert_eq!(result["error"]["type"], "Parrot.Llm.LLMProviderException");
            }
            if case["case_id"].as_str().unwrap().ends_with("-cancel") {
                assert_eq!(result["credential_gets"], 1);
                assert_eq!(result["fallback_calls"], 0);
                assert_eq!(result["caller_cancelled"], true);
                assert_eq!(result["error"]["type"], "System.OperationCanceledException");
            }
        }
    }
}
#[tokio::test(start_paused = true)]
async fn actual_ws_wrapper_is_not_unwrapped_or_relabelled_as_bare_http_auth() {
    let output = run(false).await;
    let ws = observation(&output, "ws-raw_session-auth_always");
    let http = observation(&output, "http-raw_session-auth_always");
    assert_eq!(
        ws["error"]["type"],
        "Parrot.Llm.Wire.ResponsesWebSocketTransportException"
    );
    assert_eq!(ws["error"]["classification"], "io");
    assert_eq!(ws["error"]["cause"]["type"], "Parrot.Auth.AuthException");
    assert_eq!(ws["error"]["cause"]["message"], "synthetic auth failure");
    assert_eq!(http["error"]["type"], "Parrot.Auth.AuthException");
    assert_eq!(http["error"]["classification"], "unexpected");
    assert!(http["error"]["cause"].is_null());
    let actual = &native(&output, "ws-raw_session-auth_always")["error"];
    assert!(
        actual["variant"]
            .as_str()
            .unwrap()
            .starts_with("WebSocketTransport {")
    );
    assert_eq!(actual["actual_request_classification"], "unexpected");
    assert_eq!(
        actual["source_chain"][0]["message"],
        "synthetic auth failure"
    );
}
#[tokio::test(start_paused = true)]
async fn actual_retry_fallback_sends_original_request_with_actual_raw_body_and_headers() {
    let output = run(false).await;
    let result = observation(&output, "ws-retry_session-auth_six_then_key");
    assert!(result["error"].is_null());
    assert_eq!(result["fallback_calls"], 1);
    assert_eq!(result["http_sends"], 1);
    assert_eq!(
        result["http_requests"][0]["authorization"],
        "Bearer synthetic-key"
    );
    assert_eq!(result["http_requests"][0]["method"], "POST");
    let raw = result["http_requests"][0]["body_raw"].as_str().unwrap();
    let body: Value = serde_json::from_str(raw).unwrap();
    // Inspect actual encoding, but do not synthesize or reorder body_raw.
    assert_eq!(body["model"], "synthetic-model");
    assert!(raw.contains("source-input"));
    assert!(!raw.contains("previous_response_id"));
    assert!(
        result["events"]
            .as_array()
            .unwrap()
            .iter()
            .any(|event| event["kind"] == "Completed")
    );
}
#[tokio::test(start_paused = true)]
async fn actual_diagnostic_session_request_context_remains_distinct_from_generic_counterpart() {
    let output = run(true).await;
    let result = observation(&output, "ws-raw_session-auth_always");
    assert_eq!(result["error"]["generic_classification"], "io");
    assert_eq!(result["error"]["request_classification"], "unexpected");
    assert_eq!(
        result["error"]["cause"]["generic_classification"],
        "unexpected"
    );
    for operation in ["request_finished", "call_finished"] {
        let terminal = result["diagnostics"]
            .as_array()
            .unwrap()
            .iter()
            .find(|event| event["operation"] == operation)
            .unwrap();
        assert_eq!(terminal["error_code"], "unexpected");
        assert_eq!(terminal["outcome"], "failed");
    }
    let actual = native(&output, "ws-raw_session-auth_always");
    assert_eq!(actual["actual_session_diagnostics"], result["diagnostics"]);
    assert_ne!(
        result["error"]["generic_classification"],
        result["error"]["request_classification"]
    );
    assert_eq!(result["error"]["cause"]["cause"], json!(null));
}
