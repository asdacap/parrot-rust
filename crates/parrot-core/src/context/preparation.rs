use crate::{
    AgentState, CancellationToken, ConfigKey, ContextCompaction, ContextSnapshot, Error, Provider,
    ProviderRequest, States, ToolDefinition, TurnOptions,
};

/// Requests provider-owned compaction and reloads its state effects before returning.
pub async fn compact_context(
    states: &mut dyn States,
    state: &mut AgentState,
    provider: &dyn Provider,
    definitions: &[ToolDefinition],
    mut request: crate::CompactionRequest,
    cancellation: CancellationToken<'_>,
) -> Result<ContextCompaction, Error> {
    request.tools = definitions.to_vec();
    states.set_agent_state(state.clone()).await?;
    let compacted = provider.compact(request, cancellation).await;
    *state = states.get_agent_state().await?;
    let reduced = compacted?.is_some();
    Ok(ContextCompaction {
        instructions: states.capture_system_prompt().await?,
        reduced,
    })
}

pub(crate) struct PreparedRequest {
    pub request: ProviderRequest,
    pub estimated_input_tokens: i64,
    pub model_selector: String,
}

pub(crate) async fn prepare_request(
    states: &mut dyn States,
    state: &mut AgentState,
    provider: &dyn Provider,
    definitions: &[ToolDefinition],
    options: &TurnOptions,
    cancellation: CancellationToken<'_>,
) -> Result<PreparedRequest, Error> {
    let instructions =
        prepare_context_epoch(states, state, provider, definitions, cancellation).await?;
    let messages = states.read_conversation_state().await?.assemble_messages();
    let configuration = states
        .get_config(ConfigKey::Context)
        .await?
        .into_context()?;
    let estimate = provider.estimate_input_tokens(&instructions, definitions, &messages)?;
    let max_output_tokens = state
        .last_request_statistic
        .calculate_maximum_output_tokens(&configuration.model, estimate)?;
    Ok(PreparedRequest {
        estimated_input_tokens: estimate,
        model_selector: configuration.model.canonical_model,
        request: ProviderRequest {
            model: options.model.clone(),
            instructions,
            messages,
            tools: definitions.to_vec(),
            max_output_tokens,
            reasoning: options.reasoning.clone(),
            provider_preferences: options.provider_preferences.clone(),
            include_router_metadata: options.include_router_metadata,
        },
    })
}

async fn prepare_context_epoch(
    states: &mut dyn States,
    state: &mut AgentState,
    provider: &dyn Provider,
    definitions: &[ToolDefinition],
    cancellation: CancellationToken<'_>,
) -> Result<String, Error> {
    if cancellation.is_cancelled() {
        return Err(Error::cancelled());
    }
    let mut instructions = states.capture_system_prompt().await?;
    let mut messages = states.read_conversation_state().await?.assemble_messages();
    let mut configuration = states
        .get_config(ConfigKey::Context)
        .await?
        .into_context()?;
    let estimate = provider.estimate_input_tokens(&instructions, definitions, &messages)?;
    let mut context = ContextSnapshot::estimate(estimate, &configuration);
    if context.requires_compaction(
        &state.last_request_statistic,
        &configuration.model.canonical_model,
    ) {
        let compacted = compact_context(
            states,
            state,
            provider,
            definitions,
            crate::CompactionRequest::default(),
            cancellation,
        )
        .await?;
        instructions = compacted.instructions;
        messages = states.read_conversation_state().await?.assemble_messages();
        configuration = states
            .get_config(ConfigKey::Context)
            .await?
            .into_context()?;
        let estimate = provider.estimate_input_tokens(&instructions, definitions, &messages)?;
        context = ContextSnapshot::estimate(estimate, &configuration);
        context.ensure_input_fits()?;
    }
    Ok(instructions)
}
