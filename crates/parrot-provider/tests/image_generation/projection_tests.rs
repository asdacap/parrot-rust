#[path = "projection.rs"]
mod projection;
#[path = "review_projection_tests.rs"]
mod review_projection_tests;
use serde_json::{Value, json};

fn input(ids: &[&str]) -> String {
    let mut input: Value =
        serde_json::from_str(include_str!("fixtures/source/inputs.json")).unwrap();
    input["cases"]
        .as_array_mut()
        .unwrap()
        .retain(|case| ids.contains(&case["case_id"].as_str().unwrap()));
    assert_eq!(input["cases"].as_array().unwrap().len(), ids.len());
    input.to_string()
}
async fn project(ids: &[&str]) -> Value {
    serde_json::from_str(&projection::project(&input(ids)).await.unwrap()).unwrap()
}
#[tokio::test]
async fn projector_calls_actual_generation_factories_and_preserves_raw_errors() {
    let raw = project(&[
        "generation-compatible-generate",
        "generation-chatgpt-edit",
        "generation-paid-redacted-retry-429",
        "generation-cancel-before",
    ])
    .await;
    assert_eq!(raw["cases"].as_array().unwrap().len(), 4);
    assert!(
        raw["native_errors"]
            .as_array()
            .unwrap()
            .iter()
            .any(
                |record| record["errors"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|error| error["variant"]
                        .as_str()
                        .is_some_and(|message| message.contains("Cancelled")))
            )
    );
    let selected = projection::selected(raw);
    assert!(selected.get("native_errors").is_none());
    assert_eq!(
        selected["cases"][3]["observation"]["errors"][0],
        json!({"classification":"cancellation"})
    );
}
#[tokio::test]
async fn projector_uses_actual_signer_and_shared_bucket_credentials() {
    let raw = project(&[
        "signer-published-1",
        "bucket-concurrent-credentials",
        "bucket-prepare-false",
    ])
    .await;
    let cases = raw["cases"].as_array().unwrap();
    assert!(
        cases[0]["observation"]["headers"]["authorization"]
            .as_str()
            .unwrap()
            .contains("Signature=")
    );
    assert_eq!(cases[1]["observation"]["credential_reads"], 1);
    assert_eq!(
        cases[1]["observation"]["requests"]
            .as_array()
            .unwrap()
            .len(),
        4
    );
    assert_eq!(cases[2]["observation"]["credential_reads"], 1);
    assert_eq!(
        cases[2]["observation"]["requests"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
}
#[tokio::test]
async fn projector_observes_real_owned_rewrite_and_error_provenance() {
    let raw = project(&[
        "linking-share-false",
        "linking-inline-session-local",
        "linking-failure-permanent",
    ])
    .await;
    let cases = raw["cases"].as_array().unwrap();
    assert_eq!(
        cases[0]["observation"]["forwarded_text_only"],
        json!([false, false, true])
    );
    assert_eq!(
        cases[1]["observation"]["forwarded_original"],
        json!([false, true, true, false, false])
    );
    assert_eq!(
        cases[2]["observation"]["same_failure_reference"],
        json!([true])
    );
    assert_eq!(raw["ownership_relation"].as_array().unwrap().len(), 3);
}
#[tokio::test]
async fn projector_rejects_unknown_input_families() {
    assert!(
        projection::project(r#"{"cases":[{"case_id":"x","family":"oracle"}]}"#)
            .await
            .is_err()
    );
}
