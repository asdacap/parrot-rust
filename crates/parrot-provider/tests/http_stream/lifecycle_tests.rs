use super::lifecycle_projection;
use serde_json::Value;

#[tokio::test(start_paused = true)]
async fn independent_http_idle_and_protocol_boundary_assertions() {
    let cases: Vec<Value> = serde_json::from_str(include_str!("fixtures/cases.json")).unwrap();
    let mut count = 0;
    for case in cases {
        let name = case["case_id"].as_str().unwrap();
        if let Some(mut actual) = lifecycle_projection::observe(name, &case["input"]).await {
            if name == "http.bytes.True.True" {
                let error = actual["error"].as_object_mut().unwrap();
                let detail = error.remove("detail").unwrap();
                assert_eq!(detail, "é".repeat(1024));
                assert_eq!(
                    error.remove("message").unwrap(),
                    format!(
                        "provider request failed with HTTP 400: {}",
                        "é".repeat(1024)
                    )
                );
                let body = error.remove("response_body").unwrap();
                let body = body.as_str().unwrap();
                assert_eq!(body.len(), 65_536);
                assert!(body.ends_with("\n… [truncated]"));
                assert!(body[..65_520].chars().all(|value| value == 'é'));
            }
            assert_eq!(actual, case["expected"], "{name}");
            count += 1;
        }
    }
    assert_eq!(count, 33);
}
