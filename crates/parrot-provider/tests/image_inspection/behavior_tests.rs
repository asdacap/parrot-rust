use super::source_cases::{arguments, cases_for, hex_decode};
use parrot_domain::values::tools::ToolCycleImageBudget;
use parrot_provider::image_inspection::{
    FALLBACK_IMAGE_TOKENS, HEADER_LENGTH, calculate_image_tokens, detect_media_type,
    image_reference, inspect,
};
use serde_json::Value;

#[test]
fn source_image_format_and_truncation_cases() {
    let inspection = cases_for("Inspects_format_dimensions_and_frames");
    assert_eq!(inspection.len(), 8);
    for case in inspection {
        let args = arguments(&case).expect("source test input");
        let bytes = hex_decode(args[0].as_str().unwrap());
        let expected_media = args[1].as_str().unwrap();
        let expected_frames = args[2].as_i64().unwrap() as i32;
        assert_eq!(
            detect_media_type(&bytes[..HEADER_LENGTH]),
            Some(expected_media)
        );
        let image = inspect(&bytes).unwrap();
        assert_eq!(image.media_type, expected_media);
        assert_eq!(image.width, 3);
        assert_eq!(image.height, 2);
        assert_eq!(image.frame_count, expected_frames);
    }

    let truncated = cases_for("Rejects_truncated_and_unknown_images");
    assert_eq!(truncated.len(), 9);
    for case in truncated {
        let input = arguments(&case).expect("source test input");
        let mut bytes = hex_decode(input[0].as_str().unwrap());
        bytes.truncate(bytes.len() - 1);
        assert!(inspect(&bytes).is_err(), "{}", case["coverage_id"]);
    }
}

#[test]
fn source_image_token_cost_cases() {
    let oracle: Value =
        serde_json::from_str(include_str!("fixtures/oracle/expected.json")).unwrap();
    let rows: Vec<_> = oracle["cases"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|case| {
            case["case_id"]
                .as_str()
                .unwrap()
                .contains("Calculates_documented_auto_detail")
        })
        .collect();
    assert_eq!(rows.len(), 30);
    for case in rows {
        let input = &case["input"];
        assert_eq!(
            calculate_image_tokens(
                input["model_id"].as_str().unwrap(),
                input["width"].as_i64().unwrap() as i32,
                input["height"].as_i64().unwrap() as i32,
            ),
            case["observed"]["tokens"].as_i64().unwrap(),
            "{}",
            case["case_id"]
        );
    }
    assert_eq!(
        calculate_image_tokens("gpt-6-astra", 0, 1),
        FALLBACK_IMAGE_TOKENS
    );
    assert_eq!(
        calculate_image_tokens("gpt-6-astra", 400, 0),
        FALLBACK_IMAGE_TOKENS
    );
    assert_eq!(
        calculate_image_tokens("unknown-model", 400, 400),
        FALLBACK_IMAGE_TOKENS
    );
}

#[test]
fn image_reference_cases_preserve_link_and_lazy_inline_read() {
    let rows = cases_for("Chat_completions_encode_ordered_text_and_image_reference");
    assert_eq!(rows.len(), 2);
    let inline = image_reference(None, "image/png", || Ok::<_, ()>([0, 1, 2].to_vec())).unwrap();
    assert_eq!(inline, "data:image/png;base64,AAEC");

    let read_called = std::cell::Cell::new(false);
    let url = "https://minio.example.com/parrot-images/parrot/abc.png";
    let linked = image_reference(Some(url), "image/png", || {
        read_called.set(true);
        Ok::<_, ()>(Vec::new())
    })
    .unwrap();
    assert_eq!(linked, url);
    assert!(
        !read_called.get(),
        "linked image must not load inline bytes"
    );

    // The Responses and ChatCompletions source rows both assert the same
    // protocol-neutral image reference; message/part order belongs to T26/T27.
    assert_eq!(
        cases_for("Responses_encode_ordered_text_and_image_reference").len(),
        2
    );
}

#[test]
fn source_budget_cases_reuse_domain_implementation() {
    let cases = cases_for("Restores_usage_and_overflow_without_sharing_state");
    assert_eq!(cases.len(), 2);
    for case in cases {
        let restore_overflow = case["display_name"].as_str().unwrap().contains("(True)");
        let budget = ToolCycleImageBudget::new(10);
        budget.restore_accepted(6).unwrap();
        if restore_overflow {
            budget.restore_exceeded();
        }
        assert_eq!(budget.try_accept(4).unwrap(), !restore_overflow);
        assert!(!budget.try_accept(5).unwrap());
        assert!(!budget.try_accept(1).unwrap());
        assert!(budget.is_exceeded());
        let independent = ToolCycleImageBudget::new(10);
        assert!(independent.try_accept(10).unwrap());
        assert!(!independent.is_exceeded());
    }
    assert!(ToolCycleImageBudget::new(10).try_accept(-1).is_err());
}
