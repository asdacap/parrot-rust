use super::projection_tests::projection;

async fn observation(input: &str) -> serde_json::Value {
    let output = projection::project(input).await.unwrap();
    let output: serde_json::Value = serde_json::from_str(&output).unwrap();
    assert_eq!(output["contract"], "t30-sessions-review-source-v2");
    output
}

#[tokio::test(start_paused = true)]
async fn review_projector_runs_actual_malformed_recovery_and_exhaustion() {
    let output = observation(r#"{"cases":[{"case_id":"recover","family":"malformed_chat","protocol":"chat","mode":"direct","outcome":"recover"},{"case_id":"exhaust","family":"malformed_chat","protocol":"chat","mode":"session","outcome":"exhaust"}]}"#).await;
    let recover = &output["cases"][0]["observation"];
    assert_eq!(recover["http_calls"], 2);
    assert_eq!(recover["credential_calls"], 2);
    assert_eq!(recover["timer_delays_ms"], serde_json::json!([200]));
    assert_eq!(recover["error"], serde_json::Value::Null);
    let exhaust = &output["cases"][1]["observation"];
    assert_eq!(exhaust["http_calls"], 6);
    assert_eq!(exhaust["body_disposals"], 6);
    assert_eq!(
        exhaust["error"],
        serde_json::json!({"classification":"json_parse"})
    );
    assert_eq!(
        output["native_errors"][1]["error"]["type"],
        std::any::type_name::<parrot_provider::sessions::Error>()
    );
    assert!(
        output["native_errors"][1]["error"]["variant"]
            .as_str()
            .unwrap()
            .starts_with("Encode(")
    );
    assert!(
        output["native_errors"][1]["error"]["message"]
            .as_str()
            .unwrap()
            .contains("key must be a string")
    );
}

#[tokio::test(start_paused = true)]
async fn review_projector_preserves_original_websocket_credential_errors() {
    for protocol in ["responses", "chatgpt"] {
        for credential in ["empty", "cancelled"] {
            let input = serde_json::json!({"cases":[{"case_id":"auth","family":"websocket_auth","protocol":protocol,"credential":credential}]}).to_string();
            let output = observation(&input).await;
            let result = &output["cases"][0]["observation"];
            assert_eq!(result["credential_calls"], 1);
            assert_eq!(result["http_calls"], 0);
            assert_eq!(result["connect_calls"], 0);
            assert_eq!(result["caller_cancelled"], false);
            assert_eq!(result["events"], serde_json::json!([]));
            assert_eq!(result["timer_delays_ms"], serde_json::json!([]));
            if credential == "cancelled" {
                assert_eq!(
                    result["error"],
                    serde_json::json!({"type":"OperationCanceledException","message":"synthetic credential cancellation"})
                );
            }
        }
    }
}

#[tokio::test(start_paused = true)]
async fn review_projector_resumes_owned_credentials_after_observer_drop() {
    for protocol in ["chat", "responses", "chatgpt"] {
        for mode in ["direct", "session"] {
            let input = serde_json::json!({"cases":[{"case_id":"pending","family":"pending_credentials","protocol":protocol,"mode":mode}]}).to_string();
            let output = observation(&input).await;
            let result = &output["cases"][0]["observation"];
            assert_eq!(result["credential_calls"], 1);
            assert_eq!(result["http_calls"], 1);
            assert_eq!(result["error"], serde_json::Value::Null);
            assert_eq!(result["snapshots"][0]["pull_completed"], false);
            assert_eq!(result["snapshots"][1]["pull_completed"], false);
            assert_eq!(result["snapshots"][2]["pull_completed"], true);
            assert_eq!(result["body_disposals"], 1);
        }
    }
}

#[tokio::test(start_paused = true)]
async fn review_projector_cancels_partial_http_with_live_six_byte_counter() {
    for protocol in ["chat", "responses"] {
        let input = serde_json::json!({"cases":[{"case_id":"partial","family":"cancel_partial_http","protocol":protocol}]}).to_string();
        let output = observation(&input).await;
        let result = &output["cases"][0]["observation"];
        assert_eq!(result["pending_before_cancel"], true);
        assert_eq!(result["consumed_response_bytes"], 6);
        assert_eq!(result["request_terminals"], 1);
        assert_eq!(result["call_terminals"], 1);
        assert_eq!(result["body_disposals"], 1);
        let terminal = result["diagnostics"]
            .as_array()
            .unwrap()
            .iter()
            .find(|event| event["operation"] == "request_finished")
            .unwrap();
        assert_eq!(terminal["response_bytes"], 6);
    }
}
