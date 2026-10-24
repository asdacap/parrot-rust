use super::*;

#[tokio::test]
async fn actual_pinned_text_observer_rows_match_native_fetch_processing() {
    let rows = include_str!("fixtures/text-observer/raw/observations.jsonl");
    let mut compared = 0;
    for line in rows.lines() {
        let row: Vec<String> = serde_json::from_str(line).unwrap();
        for (mime, input, expected) in [
            (
                "text/html",
                if row[0] == "named" {
                    format!("L{}R", row[2])
                } else {
                    row[2].clone()
                },
                &row[3],
            ),
            ("text/plain", row[2].clone(), &row[4]),
        ] {
            let transport = Arc::new(ScriptedTransport::new(vec![response(
                200,
                None,
                Some(mime),
                &[],
                input.as_bytes(),
            )]));
            let fetcher = fetcher(
                Arc::new(PrivateAddressPolicy),
                Arc::new(FixedResolver::default()),
                transport,
                Options {
                    max_body_bytes: 4096,
                    ..options()
                },
            );
            let result = fetcher
                .fetch(get("http://127.0.0.1/"), &NeverCancel)
                .await
                .unwrap();
            assert_eq!(&result.text, expected, "{} {} {mime}", row[0], row[1]);
            assert!(!result.truncated);
            compared += 1;
        }
    }
    assert_eq!(compared, 712);
}

#[tokio::test]
async fn actual_pinned_content_type_observer_rows_match_native_fetch_processing() {
    let mut mismatches = Vec::new();
    let mut compared = 0;
    for line in [
        include_str!("fixtures/text-observer/content-type/raw/observations.jsonl"),
        include_str!("fixtures/text-observer/content-type/edges/raw/observations.jsonl"),
        include_str!("fixtures/text-observer/content-type/quote-boundaries/raw/observations.jsonl"),
    ]
    .into_iter()
    .flat_map(str::lines)
    {
        let row: Vec<String> = serde_json::from_str(line).unwrap();
        let transport = Arc::new(ScriptedTransport::new(vec![response(
            200,
            None,
            Some(&row[1]),
            &[],
            b"content",
        )]));
        let fetcher = fetcher(
            Arc::new(PrivateAddressPolicy),
            Arc::new(FixedResolver::default()),
            transport,
            options(),
        );
        let result = fetcher.fetch(get("http://127.0.0.1/"), &NeverCancel).await;
        let actual = match result {
            Ok(result) => ("result".to_owned(), result.content_type),
            Err(OperationError::Failed(error)) => ("error".to_owned(), error.to_string()),
            Err(OperationError::Cancelled) => panic!("unexpected cancellation"),
        };
        let expected = (row[3].clone(), row[4].clone());
        if actual != expected {
            mismatches.push((row[0].clone(), row[1].clone(), actual, expected));
        }
        compared += 1;
    }
    assert_eq!(compared, 136);
    assert!(
        mismatches.is_empty(),
        "actual source/native MIME differences: {mismatches:#?}"
    );
}
