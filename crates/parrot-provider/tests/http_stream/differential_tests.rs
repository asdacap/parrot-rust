use super::family_projection;
use serde_json::{Value, json};

async fn compare_family(family: &str, count: usize) {
    let fixture: Vec<Value> = serde_json::from_str(include_str!("fixtures/cases.json")).unwrap();
    let cases: Vec<_> = fixture
        .iter()
        .filter(|case| case["case_id"].as_str().unwrap().starts_with(family))
        .collect();
    assert_eq!(cases.len(), count);
    for case in cases {
        let case_id = case["case_id"].as_str().unwrap();
        let actual = family_projection::observe(case_id, &case["input"])
            .await
            .expect("family has a production API observer");
        assert_eq!(actual, case["expected"], "{case_id}");
    }
}

#[tokio::test]
async fn catalog_regressions() {
    compare_family("catalog.", 11).await;
}

#[tokio::test]
async fn endpoint_regressions() {
    compare_family("endpoint.", 18).await;
}

#[tokio::test]
async fn header_regressions() {
    compare_family("header.", 16).await;
}

#[tokio::test]
async fn sse_records_have_fixed_results_across_byte_partitions() {
    let cases: Vec<Value> = serde_json::from_str(include_str!("fixtures/sse-cases.json")).unwrap();
    assert_eq!(cases.len(), 14);
    for case in cases {
        let id = case["case_id"].as_str().unwrap();
        let length = case["bytes_hex"].as_str().unwrap().len() / 2;
        let mut partitions = vec![vec![length], vec![1; length]];
        partitions.extend((1..length).map(|split| vec![split, length - split]));
        for chunks in partitions {
            let input = json!({
                "bytes_hex":case["bytes_hex"],
                "max_event_bytes":case["max_event_bytes"],
                "chunks":chunks,
            });
            let actual = family_projection::observe(id, &input).await.unwrap();
            assert_eq!(actual["events"], case["events"], "{id}: {chunks:?}");
            assert_eq!(actual["error"], case["error"], "{id}: {chunks:?}");
        }
    }
    let cancelled = family_projection::observe("sse.cancelled", &json!({}))
        .await
        .unwrap();
    assert_eq!(
        cancelled["error"]["category"],
        "System.Threading.Tasks.TaskCanceledException"
    );
    assert_eq!(cancelled["error"]["message"], "A task was canceled.");
}

#[tokio::test]
async fn bounded_read_regressions() {
    compare_family("bounded.", 7).await;
}
