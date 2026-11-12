use crate::{
    CancellationToken, ConfigKey, ContextCompaction, Error, EventPublisher, Provider,
    ProviderRequest, States, ToolDefinition, TurnFlags, TurnOptions,
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
    let recorded = states
        .finalize_compaction(&input.instructions, definitions, compacted)
        .await;
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
    let messages = states.read_conversation_state().await?.messages;
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
