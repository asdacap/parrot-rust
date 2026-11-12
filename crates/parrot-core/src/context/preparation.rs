use crate::{
    AgentState, CancellationToken, ConfigKey, ContextCompaction, Error, EventPublisher, Provider,
    ProviderRequest, States, ToolDefinition, TurnOptions,
    compaction::{compact_conversation, prepare_compaction},
};

/// Compacts context, finalizes the result, and resets request statistics after a reduction.
pub async fn compact_context(
    states: &mut dyn States,
    state: &mut AgentState,
    provider: &dyn Provider,
    publisher: &dyn EventPublisher,
    definitions: &[ToolDefinition],
    request: crate::CompactionRequest,
    cancellation: CancellationToken<'_>,
) -> Result<ContextCompaction, Error> {
    let mut configuration = states
        .get_config(ConfigKey::Compaction)
        .await?
        .into_compaction()?;
    if let Some(target) = request.target {
        configuration.target = Some(target);
    }
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
    states.set_agent_state(state.clone()).await?;
    let compacted = match compacted {
        Ok(Some(fitted)) => publisher
            .report_compaction_saving(cancellation)
            .await
            .map(|()| Some(fitted)),
        result => result,
    };
    let reduced = compacted.as_ref().is_ok_and(|fitted| fitted.is_some());
    let recorded = states.finalize_compaction(compacted).await;
    *state = states.get_agent_state().await?;
    if reduced && recorded.is_ok() {
        state.last_request_statistic.reset();
    }
    match recorded {
        Ok(()) => {
            publisher.report_compaction_finished().await?;
            Ok(ContextCompaction {
                instructions: input.instructions,
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
    .await?;
    let messages = states.read_conversation_state().await?.messages;
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
