use crate::{
    CancellationToken, ConfigKey, ContextCompaction, ContextConfiguration, ContextReminder,
    ContextReminderCheckpoint, ContextSnapshot, ConversationAddition, ConversationEvent,
    ConversationPersistence, Error, EventPublisher, Message, Provider, ProviderRequest, Role,
    States, ToolDefinition, TurnOptions,
    budget::TurnFlags,
    compaction::{compact_conversation, prepare_compaction},
    rendering::{render_instructions, render_template},
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
        .record_compaction(&input.instructions, definitions, compacted)
        .await
}

async fn prepare_context_reminder(
    states: &mut dyn States,
    context: &ContextSnapshot,
    configuration: &ContextConfiguration,
) -> Result<Option<ContextReminder>, Error> {
    let last_reminder = states.get_last_context_reminder().await?;
    let canonical_model = &configuration.model.canonical_model;
    let Some(percentage) =
        context.select_reminder_percentage(canonical_model, last_reminder.as_ref())
    else {
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
            ContextReminderCheckpoint::NOTIFICATION_INTERVAL
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
    Ok(Some(ContextReminder {
        checkpoint: ContextReminderCheckpoint {
            canonical_model: canonical_model.clone(),
            context_limit: context.context_limit,
            percentage,
        },
        content,
        observed_percentage: context.usage_percent.unwrap_or_default(),
    }))
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
        states
            .add_conversation(ConversationAddition {
                messages: vec![Message::text(Role::System, content)],
                persistence: ConversationPersistence::Phase {
                    final_phase: true,
                    event: ConversationEvent::Notice {
                        kind: "final_provider_request_prompt".into(),
                        notice: "Final provider request prompt injected".into(),
                    },
                },
            })
            .await?;
    }
    if cancellation.is_cancelled() {
        return Err(Error::cancelled());
    }
    let mut instructions = render_instructions(states, provider).await?;
    let mut messages = states.read_messages().await?;
    let mut configuration = states
        .get_config(ConfigKey::Context)
        .await?
        .into_context()?;
    let calibration = states.read_token_calibration().await?;
    let estimate = provider.estimate_input_tokens(&instructions, definitions, &messages)?;
    let mut context = ContextSnapshot::estimate(estimate, &configuration);
    if context.requires_compaction(&calibration, &configuration.model.canonical_model) {
        instructions = compact_context(states, provider, publisher, definitions, cancellation)
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
    if let Some(reminder) = prepare_context_reminder(states, &context, &configuration).await? {
        let mut candidate = messages.clone();
        candidate.push(reminder.render_message());
        let estimate = provider.estimate_input_tokens(&instructions, definitions, &candidate)?;
        let inserted = ContextSnapshot::estimate(estimate, &configuration);
        if inserted.exceeds_input_limit() || inserted.exceeds_trigger() {
            instructions = compact_context(states, provider, publisher, definitions, cancellation)
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
            states
                .add_conversation(ConversationAddition {
                    messages: vec![reminder.render_message()],
                    persistence: ConversationPersistence::Reminder {
                        checkpoint: reminder.checkpoint,
                        event: ConversationEvent::Notice {
                            kind: "context_reminder".into(),
                            notice: format!(
                                "Context reminder injected ({}% context used)",
                                reminder.observed_percentage
                            ),
                        },
                    },
                })
                .await?;
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
