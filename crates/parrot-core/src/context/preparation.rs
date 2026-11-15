use crate::{
    AgentState, CancellationToken, ContextCompaction, Error, Provider, States, ToolDefinition,
};

/// Requests provider-owned compaction and reloads its state effects before returning.
pub(crate) async fn compact_context(
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

#[cfg(test)]
mod tests;
