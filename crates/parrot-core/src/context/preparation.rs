use crate::{
    CancellationToken, Content, ContextCompaction, ContextReminderCheckpoint, ContextSnapshot,
    Error, Message, ProviderRequest, Role, States, ToolDefinition, TurnOptions,
    budget::TurnRequestPhase,
};

pub(crate) async fn compact_context(
    states: &mut dyn States,
    definitions: &[ToolDefinition],
    cancellation: CancellationToken<'_>,
) -> Result<ContextCompaction, Error> {
    let compacted = states
        .prepare_compaction(definitions)
        .await?
        .run(cancellation)
        .await;
    states.record_compaction(compacted).await
}

pub(crate) async fn prepare_request(
    states: &mut dyn States,
    definitions: &[ToolDefinition],
    phase: TurnRequestPhase,
    options: &TurnOptions,
    cancellation: CancellationToken<'_>,
) -> Result<ProviderRequest, Error> {
    if phase.enters_final_phase {
        states.record_final_request().await?;
    }
    if cancellation.is_cancelled() {
        return Err(Error::cancelled());
    }
    let mut instructions = states.render_instructions().await?;
    let mut messages = states.read_messages().await?;
    let mut state = states.read_context_state().await?;
    let estimate = states
        .estimate_input_tokens(&instructions, definitions, &messages)
        .await?;
    let mut context = ContextSnapshot::estimate(estimate, &state.configuration);
    if context.requires_compaction(
        &state.calibration,
        &state.configuration.model.canonical_model,
    ) {
        instructions = compact_context(states, definitions, cancellation)
            .await?
            .instructions;
        messages = states.read_messages().await?;
        state = states.read_context_state().await?;
        let estimate = states
            .estimate_input_tokens(&instructions, definitions, &messages)
            .await?;
        context = ContextSnapshot::estimate(estimate, &state.configuration);
        context.ensure_input_fits()?;
    }
    if !state.cadence.is_restored() {
        state
            .cadence
            .restore(states.read_context_checkpoint().await?);
    }
    let observation = state.cadence.observe(
        &context,
        &state.configuration.model.canonical_model,
        messages.len() as i64,
    );
    states.write_context_cadence(state.cadence.clone()).await?;
    if let Some(percentage) = observation? {
        let reminder = states.render_context_reminder(&context, percentage).await?;
        let mut candidate = messages.clone();
        candidate.push(Message {
            role: Role::System,
            contents: vec![Content::Text(reminder.clone())],
            ..Message::default()
        });
        let estimate = states
            .estimate_input_tokens(&instructions, definitions, &candidate)
            .await?;
        let inserted = ContextSnapshot::estimate(estimate, &state.configuration);
        if inserted.is_available() {
            if inserted.exceeds_input_limit() || inserted.exceeds_trigger() {
                instructions = compact_context(states, definitions, cancellation)
                    .await?
                    .instructions;
                messages = states.read_messages().await?;
                state = states.read_context_state().await?;
                let estimate = states
                    .estimate_input_tokens(&instructions, definitions, &messages)
                    .await?;
                ContextSnapshot::estimate(estimate, &state.configuration).ensure_input_fits()?;
            } else {
                let checkpoint = ContextReminderCheckpoint {
                    canonical_model: state.configuration.model.canonical_model.clone(),
                    context_limit: context.context_limit,
                    percentage,
                };
                let appended = states
                    .append_context_reminder(
                        checkpoint,
                        reminder,
                        context.usage_percent.unwrap_or_default(),
                    )
                    .await?;
                if appended {
                    messages = states.read_messages().await?;
                    let estimate = states
                        .estimate_input_tokens(&instructions, definitions, &messages)
                        .await?;
                    let persisted = ContextSnapshot::estimate(estimate, &state.configuration);
                    state.cadence.acknowledge(
                        &persisted,
                        &state.configuration.model.canonical_model,
                        messages.len() as i64,
                    )?;
                } else {
                    state.cadence.acknowledge(
                        &inserted,
                        &state.configuration.model.canonical_model,
                        messages.len() as i64,
                    )?;
                }
                states.write_context_cadence(state.cadence).await?;
                if appended {
                    states.publish_context_reminder().await?;
                }
            }
        }
    }
    let messages = states.read_messages().await?;
    let state = states.read_context_state().await?;
    let estimate = states
        .estimate_input_tokens(&instructions, definitions, &messages)
        .await?;
    let max_output_tokens = state
        .calibration
        .calculate_maximum_output_tokens(&state.configuration.model, estimate)?;
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
