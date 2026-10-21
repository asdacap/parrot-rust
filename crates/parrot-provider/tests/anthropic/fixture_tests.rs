use super::projection;
use serde_json::Value;

async fn assert_native_stdout(inputs: &str, source: &[u8], contract: &str, count: usize) {
    let inputs: Value = serde_json::from_str(inputs).unwrap();
    assert_eq!(inputs["cases"].as_array().unwrap().len(), count);
    let actual = projection::produce_raw(&inputs, contract).await;
    let first_difference = actual
        .as_bytes()
        .iter()
        .zip(source)
        .position(|(a, b)| a != b);
    assert!(
        actual.as_bytes() == source,
        "{contract}: full native stdout differs at byte {:?}; actual length {}, source length {}",
        first_difference.or(Some(actual.len().min(source.len()))),
        actual.len(),
        source.len()
    );
}

#[tokio::test]
async fn boundaries_v2_match_full_native_stdout_bytes() {
    assert_native_stdout(
        include_str!("fixtures/boundaries-v2/inputs.json"),
        include_bytes!("fixtures/boundaries-v2/expected.json"),
        "t28-anthropic-boundaries-v2",
        14,
    )
    .await;
}

#[tokio::test]
async fn surrogates_v3_match_full_native_stdout_bytes() {
    assert_native_stdout(
        include_str!("fixtures/surrogates-v3/inputs.json"),
        include_bytes!("fixtures/surrogates-v3/expected.json"),
        "t28-anthropic-surrogates-v3",
        5,
    )
    .await;
}

#[tokio::test]
async fn review_v4_matches_full_native_stdout_bytes() {
    assert_native_stdout(
        include_str!("fixtures/review-v4/inputs.json"),
        include_bytes!("fixtures/review-v4/expected.json"),
        "t28-anthropic-review-v4",
        18,
    )
    .await;
}

#[tokio::test]
async fn prefix_v5_matches_full_native_stdout_bytes() {
    assert_native_stdout(
        include_str!("fixtures/prefix-v5/inputs.json"),
        include_bytes!("fixtures/prefix-v5/expected.json"),
        "t28-anthropic-prefix-v5",
        27,
    )
    .await;
}

#[tokio::test]
async fn all_source_observations_match_production_apis() {
    let oracle: Value = serde_json::from_str(include_str!("fixtures/expected.json")).unwrap();
    let inputs: Value = serde_json::from_str(include_str!("fixtures/inputs.json")).unwrap();
    let cases = inputs["cases"].as_array().unwrap();
    assert_eq!(cases.len(), 80);
    assert_eq!(oracle["cases"].as_array().unwrap().len(), cases.len());
    let mut failures = Vec::new();
    for case in cases {
        let id = case["case_id"].as_str().unwrap();
        let expected = oracle["cases"]
            .as_array()
            .unwrap()
            .iter()
            .find(|candidate| candidate["case_id"] == id)
            .expect("source observation for every input");
        assert_eq!(
            expected["input"], case["input"],
            "source input identity: {id}"
        );
        let actual = projection::observe(id, &case["input"]).await;
        if actual != expected["observed"] {
            failures.push(format!(
                "{id}\nactual: {}\nsource: {}",
                actual, expected["observed"]
            ));
        }
    }
    assert!(
        failures.is_empty(),
        "{} source mismatches:\n{}",
        failures.len(),
        failures.join("\n\n")
    );
}
