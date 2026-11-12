use super::support::*;
use parrot_core::{CompactionRequest, ContextLimit};
use parrot_domain::contracts::async_ops::OperationError;
use parrot_tools::session::Kind;

#[test]
fn compact_arguments_preserve_optional_targets_and_last_valid_duplicate() {
    for (json, target) in [
        ("{}", None),
        (r#"{"target_context_size":null}"#, None),
        (
            r#"{"target_context_size":"20%"}"#,
            Some(ContextLimit::parse("20%").unwrap()),
        ),
        (
            r#"{"target_context_size":"2k"}"#,
            Some(ContextLimit::parse("2k").unwrap()),
        ),
        (
            r#"{"target_context_size":"10%","target_context_size":"20%"}"#,
            Some(ContextLimit::parse("20%").unwrap()),
        ),
    ] {
        assert_eq!(CompactionRequest::parse(json).unwrap().target, target);
    }
}

#[tokio::test]
async fn valid_compaction_requests_only_acknowledge_without_service_calls() {
    let fixture = Fixture::new();
    for json in [
        "{}",
        r#"{"target_context_size":null}"#,
        r#"{"target_context_size":"20%"}"#,
        r#"{"target_context_size":"2k"}"#,
        r#"{"target_context_size":"10%","target_context_size":"20%"}"#,
    ] {
        assert_eq!(
            text(fixture.invoke(Kind::CompactContext, json).await),
            "Context compaction requested."
        );
        assert!(fixture.calls().is_empty());
    }
}

#[tokio::test]
async fn invalid_compaction_input_never_calls_services() {
    let fixture = Fixture::new();
    for json in [
        "",
        "{",
        "null",
        "[]",
        "1",
        r#"{"extra":true}"#,
        r#"{"target_context_size":""}"#,
        r#"{"target_context_size":" "}"#,
        r#"{"target_context_size":20}"#,
        r#"{"target_context_size":[]}"#,
        r#"{"target_context_size":"0"}"#,
        r#"{"target_context_size":"101%"}"#,
        r#"{"target_context_size":"invalid"}"#,
        r#"{"target_context_size":20,"target_context_size":"20%"}"#,
    ] {
        let output = text(
            fixture
                .invoke_with(
                    Kind::CompactContext,
                    json,
                    formatter_catalog(),
                    &Cancel(false),
                )
                .await,
        );
        assert!(output.starts_with("ERROR<"), "{json}: {output}");
    }
    assert!(fixture.calls().is_empty());
    assert_eq!(
        text(
            fixture
                .invoke(Kind::CompactContext, r#"{"target_context_size":""}"#)
                .await
        ),
        "error: target_context_size must not be empty."
    );
}

#[tokio::test]
async fn compaction_acknowledgement_uses_the_configured_template() {
    let mut fixture = Fixture::new();
    fixture.templates = catalog(
        "prompt_templates:\n  compact-context-tool.requested:\n    template: 'Compaction will follow tool settlement.'\n",
    );
    assert_eq!(
        text(fixture.invoke(Kind::CompactContext, "{}").await),
        "Compaction will follow tool settlement."
    );
    assert!(fixture.calls().is_empty());
}

#[tokio::test]
async fn cancelled_compaction_request_does_not_acknowledge_or_call_services() {
    let fixture = Fixture::new();
    assert_eq!(
        fixture
            .invoke_with(
                Kind::CompactContext,
                "{}",
                formatter_catalog(),
                &Cancel(true),
            )
            .await,
        Err(OperationError::Cancelled)
    );
    assert!(fixture.calls().is_empty());
}
