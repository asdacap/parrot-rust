use super::projection_tests::projection;
use serde_json::{Value, json};

async fn assert_physical(index: usize, response_bytes: Option<&[u64]>) {
    let mut input: Value =
        serde_json::from_str(include_str!("fixtures/t29-compatible-physical-inputs.json")).unwrap();
    let case = input["cases"][index].clone();
    let behavior = case["behavior"].as_str().unwrap().to_owned();
    input["cases"] = json!([case]);
    let output: Value =
        serde_json::from_str(&projection::project(&input.to_string()).await.unwrap()).unwrap();
    assert_eq!(output["cases"].as_array().unwrap().len(), 1);
    assert_eq!(output["cases"][0]["case_id"], input["cases"][0]["case_id"]);
    let observation = &output["cases"][0]["observation"];
    let native = &output["native_observations"][0]["observation"];
    let reuse = behavior == "reuse";
    let fallback = behavior == "fallback";
    let calls = if reuse { 2 } else { 1 };
    let transports = if fallback {
        json!(["websocket", "http_sse"])
    } else {
        json!(["websocket", "websocket"])
    };
    assert_eq!(observation["call_starts"], calls);
    assert_eq!(observation["call_finishes"], calls);
    assert_eq!(observation["request_starts"], 2);
    assert_eq!(observation["request_finishes"], 2);
    assert_eq!(observation["start_transports"], transports);
    assert_eq!(observation["finish_transports"], transports);
    assert_eq!(
        observation["finish_outcomes"],
        if reuse {
            json!(["completed", "completed"])
        } else {
            json!(["failed", "completed"])
        }
    );
    for field in [
        "call_correlations_unique",
        "request_ids_unique",
        "request_ids_nonempty",
        "finish_ids_match",
        "finish_correlations_match",
        "single_call_match_per_request",
        "finish_durations_present",
        "size_event_at_most_one",
        "size_correlations_match",
        "size_transports_match",
        "size_bytes_match_finish",
        "missing_size_finish_request_bytes_null",
        "request_entry_counts_match",
    ] {
        assert_eq!(observation[field], true, "{behavior}: {field}");
    }
    assert_eq!(
        observation["connector_calls"],
        if reuse || fallback { 1 } else { 2 }
    );
    assert_eq!(observation["http_calls"], usize::from(fallback));
    assert_eq!(
        observation["total_sent_count"],
        if fallback { 0 } else { 2 }
    );
    assert_eq!(
        native["connections"].as_array().unwrap().len(),
        observation["connector_calls"].as_u64().unwrap() as usize
    );
    for connection in native["connections"].as_array().unwrap() {
        assert_eq!(connection["endpoint"], "wss://example.test/v1/responses");
        assert!(
            connection["headers"]
                .as_array()
                .unwrap()
                .iter()
                .any(|header| { header[0] == "Authorization" && header[1] == "Bearer key" })
        );
    }
    match response_bytes {
        Some(expected) => {
            assert_eq!(observation["request_bytes_match_sent"], true);
            assert_eq!(observation["response_bytes_match_received"], true);
            assert_eq!(native["finish_request_bytes"], native["sent_frame_bytes"]);
            assert_eq!(native["finish_response_bytes"], json!(expected));
            assert_eq!(native["received_frame_bytes"], json!(expected));
            let failed = native["failed_sent_bytes"].as_array().unwrap();
            let recovered = native["recovered_sent_bytes"].as_array().unwrap();
            assert_eq!(failed.len(), usize::from(!reuse));
            assert_eq!(recovered.len(), if reuse { 2 } else { 1 });
            let actual_sent = failed
                .iter()
                .chain(recovered)
                .map(|bytes| bytes.as_array().unwrap().len())
                .collect::<Vec<_>>();
            assert_eq!(native["sent_frame_bytes"], json!(actual_sent));
        }
        None => {
            assert!(observation["request_bytes_match_sent"].is_null());
            assert!(observation["response_bytes_match_received"].is_null());
            assert_eq!(native["sent_frame_bytes"], json!([]));
            assert_eq!(native["received_frame_bytes"], json!([]));
            assert_eq!(native["requests"].as_array().unwrap().len(), 1);
        }
    }
}

#[tokio::test(start_paused = true)]
async fn original_compatible_physical_reuse_has_two_calls_and_exact_attempt_bytes() {
    assert_physical(0, Some(&[220, 220])).await;
}

#[tokio::test(start_paused = true)]
async fn original_compatible_physical_fallback_has_two_transports_and_no_socket_sends() {
    assert_physical(1, None).await;
}

#[tokio::test(start_paused = true)]
async fn original_compatible_physical_previous_response_recovery_is_failure_first() {
    assert_physical(2, Some(&[98, 220])).await;
}

#[tokio::test(start_paused = true)]
async fn original_compatible_physical_connection_limit_recovery_is_failure_first() {
    assert_physical(3, Some(&[105, 220])).await;
}
