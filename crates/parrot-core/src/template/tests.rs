use crate::{BoxFuture, CancellationToken, ErrorKind, PromptTemplate};
use serde_json::json;

struct Cancelled;

impl crate::Provider for Cancelled {
    fn is_cancelled(&self) -> bool {
        true
    }

    fn cancelled(&self) -> BoxFuture<'_, ()> {
        Box::pin(async {})
    }
}

#[test]
fn scalar_compilation_keeps_inserted_template_syntax_opaque() {
    let template = PromptTemplate::compile(
        "prompt_templates.scalar",
        "{{literal}} {value}",
        "scalar",
        &["value".into()],
        &["value".into()],
    )
    .unwrap();
    assert_eq!(
        template
            .render(
                &[("value".into(), json!("{value} {{ untouched }}"))],
                CancellationToken::uncancelled()
            )
            .unwrap(),
        "{literal} {value} {{ untouched }}"
    );
}

#[test]
fn ordered_argument_validation_precedes_missing_required_arguments() {
    let template = PromptTemplate::compile(
        "prompt_templates.ordered",
        "{required}",
        "scalar",
        &["required".into(), "other".into()],
        &["required".into()],
    )
    .unwrap();
    let error = template
        .render(
            &[("other".into(), json!("a")), ("other".into(), json!("b"))],
            CancellationToken::uncancelled(),
        )
        .unwrap_err();
    assert_eq!(
        error.message,
        "prompt_templates.ordered received duplicate argument 'other'"
    );
}

#[test]
fn native_template_receives_structured_values_and_preserves_trailing_newlines() {
    let template = PromptTemplate::compile(
        "prompt_templates.native",
        "{% for row in rows %}{{ row.name }}={{ row.value + 1 }}\n{% endfor %}",
        "minijinja",
        &["rows".into()],
        &["rows".into()],
    )
    .unwrap();
    assert_eq!(
        template
            .render(
                &[("rows".into(), json!([{"name": "alpha", "value": 3}]))],
                CancellationToken::uncancelled(),
            )
            .unwrap(),
        "alpha=4\n"
    );
}

#[test]
fn cancellation_precedes_argument_validation_for_shared_templates() {
    let template = PromptTemplate::compile(
        "prompt_templates.cancelled",
        "{required}",
        "scalar",
        &["required".into()],
        &["required".into()],
    )
    .unwrap();
    let error = template
        .render(&[], CancellationToken::from_provider(&Cancelled))
        .unwrap_err();
    assert_eq!(error.kind, ErrorKind::Cancelled);
}
