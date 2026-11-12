use crate::{
    AgentState, CancellationToken, ConfigKey, ContextSnapshot, Error, EventPublisher, Provider,
    States, ToolDefinition, context::compact_context,
};

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PreparedContext {
    pub instructions: String,
    pub reduced: bool,
}

pub async fn prepare_context_epoch(
    states: &mut dyn States,
    state: &mut AgentState,
    provider: &dyn Provider,
    publisher: &dyn EventPublisher,
    definitions: &[ToolDefinition],
    cancellation: CancellationToken<'_>,
) -> Result<PreparedContext, Error> {
    if cancellation.is_cancelled() {
        return Err(Error::cancelled());
    }
    let mut instructions = states.capture_system_prompt().await?;
    let mut messages = states.read_conversation_state().await?.messages;
    let mut configuration = states
        .get_config(ConfigKey::Context)
        .await?
        .into_context()?;
    let estimate = provider.estimate_input_tokens(&instructions, definitions, &messages)?;
    let mut context = ContextSnapshot::estimate(estimate, &configuration);
    let mut reduced = false;
    if context.requires_compaction(
        &state.token_calibration,
        &configuration.model.canonical_model,
    ) {
        let compacted = compact_context(
            states,
            state,
            provider,
            publisher,
            definitions,
            cancellation,
        )
        .await?;
        instructions = compacted.instructions;
        reduced = compacted.reduced;
        messages = states.read_conversation_state().await?.messages;
        configuration = states
            .get_config(ConfigKey::Context)
            .await?
            .into_context()?;
        let estimate = provider.estimate_input_tokens(&instructions, definitions, &messages)?;
        context = ContextSnapshot::estimate(estimate, &configuration);
        context.ensure_input_fits()?;
    }
    Ok(PreparedContext {
        instructions,
        reduced,
    })
}
