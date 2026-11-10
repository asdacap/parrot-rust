use super::{
    CompactionConfiguration, CompactionFailure, CompactionInput, CompactionPlan, CompactionResult,
    FittedCompaction, compact,
};
use crate::{
    CancellationToken, Content, ContextSnapshot, Error, Message, Provider, Role, States,
    ToolDefinition,
};
use std::future::Future;

pub async fn fit_status(
    states: &dyn States,
    provider: &dyn Provider,
    cancellation: CancellationToken<'_>,
) -> Result<String, Error> {
    fit_status_with(
        |content| provider.estimate_status_context(content),
        |context| states.render_status(context, cancellation),
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

pub async fn fit_compaction(
    input: &CompactionInput,
    definitions: &[ToolDefinition],
    configuration: &CompactionConfiguration,
    result: Option<CompactionResult>,
    states: &dyn States,
    provider: &dyn Provider,
    cancellation: CancellationToken<'_>,
) -> Result<Option<FittedCompaction>, Error> {
    let result = result.filter(|result| result.watermark > input.base_watermark);
    let Some(result) = result else {
        let tokens =
            provider.estimate_input_tokens(&input.instructions, definitions, &input.messages)?;
        if tokens > i64::from(configuration.context.model.calculate_input_limit()) {
            return Err(CompactionFailure::NoSafeBoundary.into());
        }
        return Ok(None);
    };
    let estimate = |content: &str| {
        let mut history = result.history.clone();
        let message = Message {
            role: Role::System,
            contents: vec![Content::Text(content.into())],
            ..Message::default()
        };
        if let Some(status) = history
            .iter_mut()
            .skip(1)
            .find(|message| message.role == Role::System)
        {
            *status = message;
        }
        let tokens = provider.estimate_input_tokens(&input.instructions, definitions, &history)?;
        Ok(ContextSnapshot::estimate(tokens, &configuration.context))
    };
    let fitted = fit_status_with(estimate, |context| {
        states.render_status(context, cancellation)
    })
    .await?;
    if estimate(&fitted)?.exceeds_input_limit() {
        return Err(CompactionFailure::FittedInputLimitExceeded.into());
    }
    Ok(Some(FittedCompaction {
        result,
        status: fitted,
    }))
}

pub async fn compact_conversation(
    input: &CompactionInput,
    definitions: &[ToolDefinition],
    configuration: &CompactionConfiguration,
    states: &mut dyn States,
    provider: &dyn Provider,
    cancellation: CancellationToken<'_>,
) -> Result<Option<FittedCompaction>, Error> {
    let plan = CompactionPlan::select(input, definitions, configuration, provider)?;
    let result = compact(plan, states, provider, cancellation).await?;
    fit_compaction(
        input,
        definitions,
        configuration,
        result,
        states,
        provider,
        cancellation,
    )
    .await
}
