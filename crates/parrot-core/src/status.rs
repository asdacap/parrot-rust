use crate::{
    AgentState, CancellationToken, ConfigKey, ContextSnapshot, ConversationAddition,
    ConversationEvent, ConversationOrigin, ConversationPersistence, Error, Message, Provider,
    ProviderRequest, Role, States, StatusKey, ToolDefinition, context::fit_status,
    status_steering_needed,
};
use std::fmt;

/// The notice text that records one injected status message. Status steering and
/// compaction both persist a status under this notice.
pub(crate) fn status_notice(content: &str) -> String {
    format!("Status prompt injected\n{content}")
}

impl StatusKey {
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for StatusKey {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

/// Injects full fitted status without a standing baseline. Later auto-update
/// changes inject only auto-update slots with changed identities.
/// After each message, the agent state records cumulative standing identities so
/// unchanged slots stay known across updates and restarts.
/// Safe points occur between provider requests.
pub(crate) async fn steer_status(
    states: &mut dyn States,
    agent_state: &mut AgentState,
    provider: &dyn Provider,
    definitions: &[ToolDefinition],
    cancellation: CancellationToken<'_>,
) -> Result<(), Error> {
    if cancellation.is_cancelled() {
        return Err(Error::cancelled());
    }
    let instructions = states.capture_system_prompt().await?;
    let messages = states.read_conversation_state().await?.assemble_messages();
    let context_configuration = states
        .get_config(ConfigKey::Context)
        .await?
        .into_context()?;
    let tokens = provider.estimate_input_tokens(&instructions, definitions, &messages)?;
    let snapshot = ContextSnapshot::estimate(tokens, &context_configuration);
    let offering = states.read_status_offering(snapshot).await?;
    let mut standing = agent_state.standing_statuses.clone();
    if !status_steering_needed(&offering, &standing) {
        return Ok(());
    }
    let updates = offering
        .iter()
        .filter(|(key, offer)| {
            standing.is_empty() || (offer.is_auto_update && standing.get(key) != Some(&offer.hash))
        })
        .collect::<Vec<_>>();
    let content = if standing.is_empty() {
        fit_status(
            &ProviderRequest {
                instructions,
                messages,
                tools: definitions.to_vec(),
                ..ProviderRequest::default()
            },
            &context_configuration,
            states,
            provider,
        )
        .await?
    } else {
        updates
            .iter()
            .map(|(_, offer)| offer.text.as_str())
            .collect::<Vec<_>>()
            .join("\n\n")
    };
    for (key, offer) in updates {
        standing.insert(key.clone(), offer.hash.clone());
    }
    let notice = status_notice(&content);
    states
        .add_conversation_messages(ConversationAddition {
            messages: vec![Message::text(Role::System, content)],
            persistence: ConversationPersistence::Append {
                origin: ConversationOrigin::System,
                event: ConversationEvent::Notice {
                    kind: "status".into(),
                    notice,
                },
            },
        })
        .await?;
    agent_state.standing_statuses = standing;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{StatusKey, status_steering_needed};
    use crate::{StatusOffer, StatusOffering, StatusStanding};

    fn offering(entries: &[(&str, &str, bool)]) -> StatusOffering {
        entries
            .iter()
            .map(|(key, hash, is_auto_update)| {
                (
                    StatusKey::new(*key),
                    StatusOffer {
                        hash: (*hash).into(),
                        text: (*key).into(),
                        is_auto_update: *is_auto_update,
                    },
                )
            })
            .collect()
    }

    fn standing(entries: &[(&str, &str)]) -> StatusStanding {
        entries
            .iter()
            .map(|(key, hash)| (StatusKey::new(*key), (*hash).into()))
            .collect()
    }

    #[test]
    fn empty_standing_steers_initial_assembly() {
        let current = offering(&[("selection", "a", true), ("time", "b", false)]);
        assert!(status_steering_needed(&current, &StatusStanding::default()));
    }

    #[test]
    fn matching_standing_steers_nothing() {
        let current = offering(&[("selection", "a", true), ("time", "b", false)]);
        let known = standing(&[("selection", "a"), ("time", "b")]);
        assert!(!status_steering_needed(&current, &known));
    }

    #[test]
    fn changed_auto_hash_steers() {
        let current = offering(&[("selection", "a2", true), ("time", "b", false)]);
        let known = standing(&[("selection", "a"), ("time", "b")]);
        assert!(status_steering_needed(&current, &known));
    }

    #[test]
    fn non_auto_change_alone_steers_nothing() {
        let current = offering(&[("selection", "a", true), ("time", "b2", false)]);
        let known = standing(&[("selection", "a"), ("time", "b")]);
        assert!(!status_steering_needed(&current, &known));
    }

    #[test]
    fn unchanged_after_restart_steers_nothing() {
        let restored = offering(&[("selection", "a2", true), ("time", "b2", false)]);
        let known = standing(&[("selection", "a2"), ("time", "b2")]);
        assert!(!status_steering_needed(&restored, &known));
    }

    #[test]
    fn auto_slot_without_standing_hash_counts_as_changed() {
        let current = offering(&[("selection", "a", true), ("work", "w", true)]);
        let known = standing(&[("selection", "a")]);
        assert!(status_steering_needed(&current, &known));
    }

    #[test]
    fn empty_offering_steers_nothing() {
        let current = StatusOffering::default();
        let known = standing(&[("selection", "a")]);
        assert!(!status_steering_needed(&current, &known));
    }
}
