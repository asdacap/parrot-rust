#[path = "projection.rs"]
pub(super) mod projection;

#[tokio::test(start_paused = true)]
async fn input_only_projector_exercises_actual_retry_and_pool() {
    let input = r#"{"cases":[{"case_id":"independent-retry","family":"retry","mode":"session","failure":"protocol","failures":1,"visible":"none","fallback":true,"header_retries":5},{"case_id":"independent-pool","family":"sessions","mode":"identity-turns"}]}"#;
    let output = projection::project(input).await.unwrap();
    let output: serde_json::Value = serde_json::from_str(&output).unwrap();
    let retry = &output["cases"][0]["observation"];
    assert_eq!(retry["calls"], 2);
    assert_eq!(retry["events"][0]["kind"], "Retry");
    assert_eq!(retry["timer_delays_ms"], serde_json::json!([200]));
    assert_eq!(retry["events"][1]["kind"], "Completed");
    assert_eq!(retry["enumerator_disposals"], 2);
    let pool = &output["cases"][1]["observation"];
    assert_eq!(pool["same_reference"], true);
    assert_eq!(pool["isolated_reference"], true);
    assert_eq!(pool["a_turns"], 2);
}

#[tokio::test(start_paused = true)]
async fn input_only_projector_preserves_explicit_event_text_metadata() {
    let input = r#"{"cases":[{"case_id":"visible","family":"retry","mode":"session","failure":"protocol","failures":1,"visible":"ToolCallDelta","fallback":true,"header_retries":5}]}"#;
    let output: serde_json::Value =
        serde_json::from_str(&projection::project(input).await.unwrap()).unwrap();
    assert_eq!(
        output["cases"][0]["observation"]["events"][0]["text"],
        "synthetic fragment"
    );
    assert_eq!(output["cases"][0]["observation"]["calls"], 1);
}
