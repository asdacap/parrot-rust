use crate::{
    CancellationToken, ConfigKey, ContextSnapshot, Error, EventPublisher, Provider, States,
    ToolDefinition, add_context_reminder,
    context::{compact_context, prepare_context_reminder},
    rendering::render_instructions,
};

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PreparedContext {
    pub instructions: String,
    pub reduced: bool,
}

pub async fn prepare_context_epoch(
    states: &mut dyn States,
    provider: &dyn Provider,
    publisher: &dyn EventPublisher,
    definitions: &[ToolDefinition],
    cancellation: CancellationToken<'_>,
) -> Result<PreparedContext, Error> {
    if cancellation.is_cancelled() {
        return Err(Error::cancelled());
    }
    let mut instructions = render_instructions(states).await?;
    let mut messages = states.read_provider_messages().await?;
    let mut configuration = states
        .get_config(ConfigKey::Context)
        .await?
        .into_context()?;
    let calibration = states.read_token_calibration().await?;
    let estimate = provider.estimate_input_tokens(&instructions, definitions, &messages)?;
    let mut context = ContextSnapshot::estimate(estimate, &configuration);
    let mut reduced = false;
    if context.requires_compaction(&calibration, &configuration.model.canonical_model) {
        let compacted =
            compact_context(states, provider, publisher, definitions, cancellation).await?;
        instructions = compacted.instructions;
        reduced = compacted.reduced;
        messages = states.read_provider_messages().await?;
        configuration = states
            .get_config(ConfigKey::Context)
            .await?
            .into_context()?;
        let estimate = provider.estimate_input_tokens(&instructions, definitions, &messages)?;
        context = ContextSnapshot::estimate(estimate, &configuration);
        context.ensure_input_fits()?;
    }
    if let Some(reminder) = prepare_context_reminder(states, &context, &configuration).await? {
        let mut candidate = messages.clone();
        candidate.push(reminder.render_message());
        let estimate = provider.estimate_input_tokens(&instructions, definitions, &candidate)?;
        let inserted = ContextSnapshot::estimate(estimate, &configuration);
        if inserted.exceeds_input_limit() || inserted.exceeds_trigger() {
            let compacted =
                compact_context(states, provider, publisher, definitions, cancellation).await?;
            instructions = compacted.instructions;
            reduced |= compacted.reduced;
            messages = states.read_provider_messages().await?;
            configuration = states
                .get_config(ConfigKey::Context)
                .await?
                .into_context()?;
            let estimate = provider.estimate_input_tokens(&instructions, definitions, &messages)?;
            ContextSnapshot::estimate(estimate, &configuration).ensure_input_fits()?;
        } else {
            add_context_reminder(states, &reminder).await?;
        }
    }
    Ok(PreparedContext {
        instructions,
        reduced,
    })
}
