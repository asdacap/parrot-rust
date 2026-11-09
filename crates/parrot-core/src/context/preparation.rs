use crate::{
    CancellationToken, ContextCompaction, ContextSnapshot, Error, PreparationDependencies,
    ProviderRequest, States, ToolDefinition, TurnOptions,
    budget::TurnFlags,
    compaction::{CompactionDependencies, compact_conversation, prepare_compaction},
};

pub(crate) async fn compact_context(
    states: &mut dyn States,
    compaction: &mut CompactionDependencies<'_>,
    definitions: &[ToolDefinition],
    cancellation: CancellationToken<'_>,
) -> Result<ContextCompaction, Error> {
    let input = prepare_compaction(
        states.read_conversation_state(),
        definitions,
        compaction.configuration,
        compaction.preparation,
        compaction.status,
        |instructions, tools, messages| {
            compaction
                .effects
                .estimate_context_tokens(instructions, tools, messages)
        },
        cancellation,
    )
    .await?;
    let compacted = compact_conversation(
        &input,
        definitions,
        compaction.configuration,
        compaction.effects,
        compaction.provider,
        compaction.status,
        cancellation,
    )
    .await;
    states
        .record_compaction(&input.instructions, definitions, compacted)
        .await
}

pub(crate) async fn prepare_request(
    states: &mut dyn States,
    preparation: &mut PreparationDependencies<'_, '_>,
    definitions: &[ToolDefinition],
    flags: TurnFlags,
    options: &TurnOptions,
    cancellation: CancellationToken<'_>,
) -> Result<ProviderRequest, Error> {
    if flags.final_phase {
        states.record_final_request().await?;
    }
    if cancellation.is_cancelled() {
        return Err(Error::cancelled());
    }
    let mut instructions = preparation.request.render_instructions().await?;
    let mut messages = states.read_messages().await?;
    let mut configuration = states.read_context_configuration().await?;
    let calibration = states.read_token_calibration().await?;
    let estimate = preparation
        .request
        .estimate_input_tokens(&instructions, definitions, &messages)
        .await?;
    let mut context = ContextSnapshot::estimate(estimate, &configuration);
    if context.requires_compaction(&calibration, &configuration.model.canonical_model) {
        instructions = compact_context(states, preparation.compaction, definitions, cancellation)
            .await?
            .instructions;
        messages = states.read_messages().await?;
        configuration = states.read_context_configuration().await?;
        let estimate = preparation
            .request
            .estimate_input_tokens(&instructions, definitions, &messages)
            .await?;
        context = ContextSnapshot::estimate(estimate, &configuration);
        context.ensure_input_fits()?;
    }
    if let Some(prompt) = states.get_pending_prompt(&context).await? {
        let mut candidate = messages.clone();
        candidate.push(prompt);
        let estimate = preparation
            .request
            .estimate_input_tokens(&instructions, definitions, &candidate)
            .await?;
        let inserted = ContextSnapshot::estimate(estimate, &configuration);
        if inserted.exceeds_input_limit() || inserted.exceeds_trigger() {
            instructions =
                compact_context(states, preparation.compaction, definitions, cancellation)
                    .await?
                    .instructions;
            messages = states.read_messages().await?;
            configuration = states.read_context_configuration().await?;
            let estimate = preparation
                .request
                .estimate_input_tokens(&instructions, definitions, &messages)
                .await?;
            ContextSnapshot::estimate(estimate, &configuration).ensure_input_fits()?;
        } else {
            let acknowledged = if states.record_pending_prompt().await? {
                messages = states.read_messages().await?;
                let estimate = preparation
                    .request
                    .estimate_input_tokens(&instructions, definitions, &messages)
                    .await?;
                ContextSnapshot::estimate(estimate, &configuration)
            } else {
                inserted
            };
            states.acknowledge_prompt(&acknowledged).await?;
        }
    }
    let messages = states.read_messages().await?;
    let configuration = states.read_context_configuration().await?;
    let calibration = states.read_token_calibration().await?;
    let estimate = preparation
        .request
        .estimate_input_tokens(&instructions, definitions, &messages)
        .await?;
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
