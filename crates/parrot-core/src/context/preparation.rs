use crate::{
    CancellationToken, ConfigKey, ContextCompaction, Error, EventPublisher, Provider,
    ProviderRequest, States, ToolDefinition, TurnOptions,
    budget::TurnFlags,
    compaction::{compact_conversation, prepare_compaction},
    rendering::render_template,
};

pub(crate) async fn compact_context(
    states: &mut dyn States,
    provider: &dyn Provider,
    publisher: &dyn EventPublisher,
    definitions: &[ToolDefinition],
    cancellation: CancellationToken<'_>,
) -> Result<ContextCompaction, Error> {
    let configuration = states
        .get_config(ConfigKey::Compaction)
        .await?
        .into_compaction()?;
    let input = prepare_compaction(
        states,
        provider,
        publisher,
        definitions,
        &configuration,
        cancellation,
    )
    .await?;
    let compacted = compact_conversation(
        &input,
        definitions,
        &configuration,
        states,
        provider,
        cancellation,
    )
    .await;
    states
        .finalize_compaction(&input.instructions, definitions, compacted)
        .await
}

pub(crate) async fn prepare_request(
    states: &mut dyn States,
    provider: &dyn Provider,
    publisher: &dyn EventPublisher,
    definitions: &[ToolDefinition],
    flags: TurnFlags,
    options: &TurnOptions,
    cancellation: CancellationToken<'_>,
) -> Result<ProviderRequest, Error> {
    if flags.final_phase {
        let content = render_template(states, ConfigKey::FinalRequestTemplate, &[]).await?;
        crate::add_phase_prompt(states, &content, true).await?;
    }
    let instructions =
        crate::prepare_context_epoch(states, provider, publisher, definitions, cancellation)
            .await?
            .instructions;
    let messages = states.read_provider_messages().await?;
    let configuration = states
        .get_config(ConfigKey::Context)
        .await?
        .into_context()?;
    let calibration = states.read_token_calibration().await?;
    let estimate = provider.estimate_input_tokens(&instructions, definitions, &messages)?;
    let max_output_tokens =
        calibration.calculate_maximum_output_tokens(&configuration.model, estimate)?;
    Ok(ProviderRequest {
        model: options.model.clone(),
        instructions,
        messages,
        tools: definitions.to_vec(),
        max_output_tokens,
        reasoning: options.reasoning.clone(),
        provider_preferences: options.provider_preferences.clone(),
        include_router_metadata: options.include_router_metadata,
    })
}
