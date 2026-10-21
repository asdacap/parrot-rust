//! Full supplemental boundary comparisons; the observer receives inputs only.
use super::boundary_projection;
use serde_json::Value;

async fn compare_family(prefix: &str, count: usize) {
    let oracle: Value =
        serde_json::from_str(include_str!("fixtures/boundaries-v1/expected.json")).unwrap();
    let cases: Vec<_> = oracle["cases"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|case| case["case_id"].as_str().unwrap().starts_with(prefix))
        .collect();
    assert_eq!(cases.len(), count);
    for case in cases {
        let id = case["case_id"].as_str().unwrap();
        let actual = boundary_projection::observe(id, &case["input"])
            .await
            .expect("known supplemental boundary input");
        assert_eq!(
            actual,
            boundary_projection::expected_scope(id, &case["observed"]),
            "{id}"
        );
    }
}

#[tokio::test]
async fn source_error_boundaries() {
    compare_family("http.error.boundary.", 18).await;
}

#[tokio::test]
async fn source_endpoint_boundaries() {
    compare_family("endpoint.boundary.", 5).await;
}

#[tokio::test]
async fn source_authority_boundaries() {
    compare_family("catalog.authority.boundary.", 6).await;
}

