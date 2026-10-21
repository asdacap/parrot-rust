use super::projection;
use serde_json::{Value, json};

#[tokio::test]
async fn supplemental_projector_observes_early_failure_and_real_tail_effects() {
    let raw: Value = serde_json::from_str(
        &projection::project(include_str!("fixtures/source/review-cases-v1/inputs.json"))
            .await
            .unwrap(),
    )
    .unwrap();
    assert_eq!(raw["cases"].as_array().unwrap().len(), 8);
    for case in &raw["cases"].as_array().unwrap()[..6] {
        let o = &case["observation"];
        assert_eq!(o["read_calls"], 1);
        assert_eq!(o["tail_actions"], 0);
        assert_eq!(o["unique_body_disposals"], 1);
        assert_eq!(
            o["semantic_error"]["message"],
            "provider: invalid image response JSON"
        );
    }
    for case in &raw["cases"].as_array().unwrap()[6..] {
        assert_eq!(case["observation"]["read_calls"], 2);
        assert_eq!(case["observation"]["tail_actions"], 1);
        assert_eq!(case["observation"]["read_requested"], json!([16384, 16384]));
    }
    let selected = projection::selected(raw);
    assert_eq!(
        selected["cases"][7]["observation"]["error"],
        json!({"classification":"cancellation"})
    );
}
