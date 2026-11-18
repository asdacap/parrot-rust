use super::{Failure, FailureKind, Invocation, Output};
use parrot_core::NeverCancelled;
use parrot_domain::contracts::async_ops::OperationError;
use parrot_foundation::template_catalog::PromptTemplateCatalog;
use serde_json::Value;

fn render(
    templates: &PromptTemplateCatalog,
    id: &str,
    arguments: &[(&str, String)],
) -> Result<String, OperationError<Failure>> {
    let arguments = arguments
        .iter()
        .map(|(name, value)| ((*name).to_owned(), Value::String(value.clone())))
        .collect::<Vec<_>>();
    templates
        .render(id, &arguments, &NeverCancelled)
        .map_err(|error| match error {
            OperationError::Cancelled => OperationError::Cancelled,
            OperationError::Failed(error) => {
                OperationError::Failed(Failure::new(FailureKind::Argument, error.to_string()))
            }
        })
}

pub(super) fn text_output(
    invocation: &Invocation,
    text: &str,
) -> Result<Output, OperationError<Failure>> {
    render(
        &invocation.prompt_templates,
        "tool-result.text",
        &[("value", text.to_owned())],
    )
    .map(Into::into)
}

pub(super) fn error_output(
    invocation: &Invocation,
    message: &str,
) -> Result<Output, OperationError<Failure>> {
    render(
        &invocation.prompt_templates,
        "tool-result.error",
        &[("message", message.to_owned())],
    )
    .map(Into::into)
}
