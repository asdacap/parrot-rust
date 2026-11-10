use crate::{
    CancellationToken, ConfigKey, ContextCompaction, ContextConfiguration,
    ContextReminderCheckpoint, ContextSnapshot, Error, PendingPrompt, Provider, ProviderRequest,
    States, ToolDefinition, TurnOptions,
    budget::TurnFlags,
    compaction::{compact_conversation, prepare_compaction},
    rendering::{render_instructions, render_template},
};

pub(crate) async fn compact_context(
    states: &mut dyn States,
    provider: &dyn Provider,
    definitions: &[ToolDefinition],
) -> Result<ContextCompaction, Error> {
    let configuration = states
        .get_config(ConfigKey::Compaction)
        .await?
        .into_compaction()?;
    let input = prepare_compaction(states, provider, definitions, &configuration).await?;
    let compacted =
        compact_conversation(&input, definitions, &configuration, states, provider).await;
    states
        .record_compaction(&input.instructions, definitions, compacted)
        .await
}

async fn prepare_pending_prompt(
    states: &mut dyn States,
    context: &ContextSnapshot,
    configuration: &ContextConfiguration,
) -> Result<Option<PendingPrompt>, Error> {
    if let Some(prompt) = states.read_pending_prompt().await? {
        return Ok(Some(PendingPrompt::Message(prompt)));
    }
    let mut cadence = states.read_context_cadence().await?;
    if !cadence.is_restored() {
        cadence.restore(states.read_context_checkpoint().await?);
    }
    let canonical_model = &configuration.model.canonical_model;
    let observation = cadence.observe(context, canonical_model, states.read_history_size().await?);
    states.write_context_cadence(cadence).await?;
    let Some(percentage) = observation? else {
        return Ok(None);
    };
    let mut arguments = vec![
        ("percentage".into(), percentage.to_string().into()),
        (
            "estimated_tokens".into(),
            context.estimated_tokens.to_string().into(),
        ),
        (
            "context_limit".into(),
            context.context_limit.to_string().into(),
        ),
        (
            "notification_interval".into(),
            crate::ContextCadence::NOTIFICATION_INTERVAL
                .to_string()
                .into(),
        ),
        ("trigger".into(), context.trigger_percent.to_string().into()),
    ];
    let key = if context.has_context_limit_override {
        arguments.push((
            "trigger_tokens".into(),
            context
                .trigger_tokens
                .map_or_else(|| "unavailable".into(), |value| value.to_string())
                .into(),
        ));
        ConfigKey::ContextLimitReminderTemplate
    } else {
        ConfigKey::ContextReminderTemplate
    };
    let content = render_template(states, key, &arguments).await?;
    Ok(Some(PendingPrompt::ContextReminder {
        checkpoint: ContextReminderCheckpoint {
            canonical_model: canonical_model.clone(),
            context_limit: context.context_limit,
            percentage,
        },
        content,
        observed_percentage: context.usage_percent.unwrap_or_default(),
    }))
}

async fn acknowledge_prompt(
    states: &mut dyn States,
    prompt: &PendingPrompt,
    context: &ContextSnapshot,
) -> Result<(), Error> {
    states.acknowledge_pending_prompt(prompt, context).await?;
    if let PendingPrompt::ContextReminder { checkpoint, .. } = prompt {
        let mut cadence = states.read_context_cadence().await?;
        cadence.acknowledge(
            context,
            &checkpoint.canonical_model,
            states.read_history_size().await?,
        )?;
        states.write_context_cadence(cadence).await?;
    }
    Ok(())
}

pub(crate) async fn prepare_request(
    states: &mut dyn States,
    provider: &dyn Provider,
    definitions: &[ToolDefinition],
    flags: TurnFlags,
    options: &TurnOptions,
    cancellation: CancellationToken<'_>,
) -> Result<ProviderRequest, Error> {
    if flags.final_phase {
        let content = render_template(states, ConfigKey::FinalRequestTemplate, &[]).await?;
        states.record_steer(&content).await?;
    }
    if cancellation.is_cancelled() {
        return Err(Error::cancelled());
    }
    let mut instructions = render_instructions(states).await?;
    let mut messages = states.read_messages().await?;
    let mut configuration = states
        .get_config(ConfigKey::Context)
        .await?
        .into_context()?;
    let calibration = states.read_token_calibration().await?;
    let estimate = provider.estimate_input_tokens(&instructions, definitions, &messages)?;
    let mut context = ContextSnapshot::estimate(estimate, &configuration);
    if context.requires_compaction(&calibration, &configuration.model.canonical_model) {
        instructions = compact_context(states, provider, definitions)
            .await?
            .instructions;
        messages = states.read_messages().await?;
        configuration = states
            .get_config(ConfigKey::Context)
            .await?
            .into_context()?;
        let estimate = provider.estimate_input_tokens(&instructions, definitions, &messages)?;
        context = ContextSnapshot::estimate(estimate, &configuration);
        context.ensure_input_fits()?;
    }
    if let Some(prompt) = prepare_pending_prompt(states, &context, &configuration).await? {
        let mut candidate = messages.clone();
        candidate.push(prompt.render_message());
        let estimate = provider.estimate_input_tokens(&instructions, definitions, &candidate)?;
        let inserted = ContextSnapshot::estimate(estimate, &configuration);
        if inserted.exceeds_input_limit() || inserted.exceeds_trigger() {
            instructions = compact_context(states, provider, definitions)
                .await?
                .instructions;
            messages = states.read_messages().await?;
            configuration = states
                .get_config(ConfigKey::Context)
                .await?
                .into_context()?;
            let estimate = provider.estimate_input_tokens(&instructions, definitions, &messages)?;
            ContextSnapshot::estimate(estimate, &configuration).ensure_input_fits()?;
        } else {
            let receipt = states.record_pending_prompt(&prompt).await?;
            let acknowledged = if receipt.is_some() {
                messages = states.read_messages().await?;
                let estimate =
                    provider.estimate_input_tokens(&instructions, definitions, &messages)?;
                ContextSnapshot::estimate(estimate, &configuration)
            } else {
                inserted
            };
            acknowledge_prompt(states, &prompt, &acknowledged).await?;
            if let Some(receipt) = receipt {
                states.publish_pending_prompt(receipt).await?;
            }
        }
    }
    let messages = states.read_messages().await?;
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
