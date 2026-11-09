use super::support::*;
use parrot_tools::session::Kind;

#[tokio::test]
async fn checkpoint_returns_verbatim_title_without_service_mutation_or_text_formatter() {
    let fixture = Fixture::new();
    let title = "  handoff {title}\n😀  ";
    let json = serde_json::json!({"title": title}).to_string();
    assert_eq!(
        text(
            fixture
                .invoke_with(
                    Kind::SetCheckpoint,
                    &json,
                    Some(formatter_catalog()),
                    &Cancel(false)
                )
                .await
        ),
        title
    );
    assert!(fixture.calls().is_empty());
}

#[tokio::test]
async fn checkpoint_argument_failures_use_the_configured_error_formatter() {
    let fixture = Fixture::new();
    assert_eq!(
        text(
            fixture
                .invoke_with(
                    Kind::SetCheckpoint,
                    r#"{}"#,
                    Some(formatter_catalog()),
                    &Cancel(false),
                )
                .await
        ),
        "ERROR<Tool arguments require a string 'title'.>"
    );
    assert!(fixture.calls().is_empty());
}

#[tokio::test]
async fn invalid_checkpoint_arguments_return_errors_without_service_mutation() {
    let fixture = Fixture::new();
    for json in [
        "",
        "{",
        "null",
        "[]",
        "1",
        "{}",
        r#"{"title":null}"#,
        r#"{"title":1}"#,
        r#"{"title":[]}"#,
        r#"{"title":"handoff","extra":true}"#,
    ] {
        assert!(
            text(fixture.invoke(Kind::SetCheckpoint, json).await).starts_with("error:"),
            "{json}"
        );
    }
    assert!(fixture.calls().is_empty());
}
