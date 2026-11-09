use super::{
    CompactionConfiguration, CompactionEffects, CompactionFailure, CompactionInput, CompactionPlan,
    CompactionResult, FittedCompaction, PlanningError, StatusRenderer, SummaryProvider, compact,
};
use crate::{CancellationToken, Content, ContextSnapshot, Message, Role, ToolDefinition};
use std::future::Future;

pub async fn fit_status<S, E, F>(
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

pub async fn fit_compaction<E: From<CompactionFailure> + Send>(
    input: &CompactionInput,
    definitions: &[ToolDefinition],
    configuration: &CompactionConfiguration,
    result: Option<CompactionResult>,
    effects: &dyn CompactionEffects<Error = E>,
    status: &dyn StatusRenderer<Error = E>,
    cancellation: CancellationToken<'_>,
) -> Result<Option<FittedCompaction>, E> {
    let result = result.filter(|result| result.watermark > input.base_watermark);
    let Some(result) = result else {
        let tokens =
            effects.estimate_context_tokens(&input.instructions, definitions, &input.messages)?;
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
        let tokens = effects.estimate_context_tokens(&input.instructions, definitions, &history)?;
        Ok(ContextSnapshot::estimate(tokens, &configuration.context))
    };
    let fitted = fit_status(estimate, |context| status.render(context, cancellation)).await?;
    if estimate(&fitted)?.exceeds_input_limit() {
        return Err(CompactionFailure::FittedInputLimitExceeded.into());
    }
    Ok(Some(FittedCompaction {
        result,
        status: fitted,
    }))
}

pub async fn compact_conversation<E>(
    input: &CompactionInput,
    definitions: &[ToolDefinition],
    configuration: &CompactionConfiguration,
    effects: &mut dyn CompactionEffects<Error = E>,
    provider: &dyn SummaryProvider<Error = E>,
    status: &dyn StatusRenderer<Error = E>,
    cancellation: CancellationToken<'_>,
) -> Result<Option<FittedCompaction>, E>
where
    E: From<PlanningError> + From<CompactionFailure> + Send,
{
    let plan = CompactionPlan::select(
        input,
        definitions,
        configuration,
        |messages| effects.estimate_tokens(messages),
        |instructions, definitions, messages| {
            effects.estimate_context_tokens(instructions, definitions, messages)
        },
    )?;
    let result = compact(plan, effects, provider, cancellation).await?;
    fit_compaction(
        input,
        definitions,
        configuration,
        result,
        effects,
        status,
        cancellation,
    )
    .await
}
