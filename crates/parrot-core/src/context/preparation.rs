use crate::{
    AgentState, CancellationToken, ConfigKey, ContextCompaction, Error, EventPublisher, Provider,
    ProviderRequest, States, ToolDefinition, TurnOptions,
    compaction::{compact_conversation, prepare_compaction},
};

pub(crate) async fn compact_context(
    states: &mut dyn States,
    state: &mut AgentState,
    provider: &dyn Provider,
    publisher: &dyn EventPublisher,
    definitions: &[ToolDefinition],
    cancellation: CancellationToken<'_>,
) -> Result<ContextCompaction, Error> {
    let configuration = states
        .get_config(ConfigKey::Compaction)
        .await?
        .into_compaction()?;
    let input =
        prepare_compaction(states, provider, publisher, definitions, &configuration).await?;
    let compacted = compact_conversation(
        &input,
        definitions,
        &configuration,
        states,
        provider,
        publisher,
        cancellation,
    )
    .await;
    let reduced = compacted.as_ref().is_ok_and(|fitted| fitted.is_some());
    states.set_agent_state(state.clone()).await?;
    let recorded = states
        .finalize_compaction(&input.instructions, definitions, compacted)
        .await;
    *state = states.get_agent_state().await?;
    if reduced && recorded.is_ok() {
        state.token_calibration.reset();
    }
    match recorded {
        Ok(instructions) => {
            publisher.report_compaction_finished().await?;
            Ok(ContextCompaction {
                instructions,
                reduced,
            })
        }
        Err(error) => {
            publisher
                .report_compaction_failed(error.to_string())
                .await?;
            Err(error)
        }
    }
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
    publisher: &dyn EventPublisher,
    definitions: &[ToolDefinition],
    options: &TurnOptions,
    cancellation: CancellationToken<'_>,
) -> Result<PreparedRequest, Error> {
    let instructions = crate::prepare_context_epoch(
        states,
        state,
        provider,
        publisher,
        definitions,
        cancellation,
    )
    .await?
    .instructions;
    let messages = states.read_provider_messages().await?;
    let configuration = states
        .get_config(ConfigKey::Context)
        .await?
        .into_context()?;
    let estimate = provider.estimate_input_tokens(&instructions, definitions, &messages)?;
    let max_output_tokens = state
        .token_calibration
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
