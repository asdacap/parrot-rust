use crate::{
    Content, ContextConfiguration, ContextSnapshot, Error, Message, Provider, ProviderRequest,
    Role, States, join_status_offering,
};
use std::future::Future;

/// Fits a new system status message after the request's existing messages.
pub async fn fit_status(
    request: &ProviderRequest,
    configuration: &ContextConfiguration,
    states: &dyn States,
    provider: &dyn Provider,
) -> Result<String, Error> {
    fit_status_with(
        |content: &str| {
            let mut messages = request.messages.clone();
            messages.push(Message {
                role: Role::System,
                contents: vec![Content::Text(content.into())],
                ..Message::default()
            });
            let tokens =
                provider.estimate_input_tokens(&request.instructions, &request.tools, &messages)?;
            Ok(ContextSnapshot::estimate(tokens, configuration))
        },
        |context| async move {
            let offering = states.read_status_offering(context).await?;
            Ok(join_status_offering(&offering))
        },
    )
    .await
}

async fn fit_status_with<S, E, F>(
    mut estimate: impl FnMut(&str) -> Result<S, E>,
    mut render: impl FnMut(S) -> F,
) -> Result<String, E>
where
    S: Clone + Eq,
    F: Future<Output = Result<String, E>>,
{
    let mut context = estimate("")?;
    let mut content = render(context.clone()).await?;
    for _ in 0..8 {
        let rendered = estimate(&content)?;
        if rendered == context {
            break;
        }
        context = rendered;
        content = render(context.clone()).await?;
    }
    Ok(content)
}

#[cfg(test)]
mod tests;
