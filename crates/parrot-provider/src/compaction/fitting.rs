use super::{
    Execution,
    model::{CompactionConfiguration, CompactionInput, CompactionResult, FittedCompaction},
};
use parrot_core::{
    CompactionFailure, Content, ContextSnapshot, Error, Message, Role, ToolDefinition,
    join_status_offering,
};
use std::future::Future;

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

pub(super) async fn fit_compaction(
    input: &CompactionInput,
    definitions: &[ToolDefinition],
    configuration: &CompactionConfiguration,
    result: Option<CompactionResult>,
    execution: &Execution<'_>,
) -> Result<Option<FittedCompaction>, Error> {
    let result = result.filter(|result| result.watermark > input.base_watermark);
    let Some(result) = result else {
        let tokens = execution.estimates.estimate_input_tokens(
            &input.instructions,
            definitions,
            &input.messages,
        )?;
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
        let tokens = execution.estimates.estimate_input_tokens(
            &input.instructions,
            definitions,
            &history,
        )?;
        Ok(ContextSnapshot::estimate(tokens, &configuration.context))
    };
    let fitted = fit_status_with(estimate, |context| async move {
        let offering = execution
            .effects
            .offer_status(context, execution.cancellation)
            .await?;
        Ok::<_, Error>(join_status_offering(&offering))
    })
    .await?;
    if estimate(&fitted)?.exceeds_input_limit() {
        return Err(CompactionFailure::FittedInputLimitExceeded.into());
    }
    Ok(Some(FittedCompaction {
        result,
        status: fitted.clone(),
        notice: format!("Status prompt injected\n{fitted}"),
    }))
}
