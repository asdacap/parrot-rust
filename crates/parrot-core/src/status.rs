use crate::{
    CancellationToken, ConfigKey, ContextSnapshot, ConversationAddition, ConversationEvent,
    ConversationPersistence, Error, Message, Provider, ProviderRequest, Role, States,
    ToolDefinition, compaction::fit_status,
};
use std::{collections::BTreeMap, fmt};

/// The notice text that records one injected status message. Status steering and
/// compaction both persist a status under this notice.
pub(crate) fn status_notice(content: &str) -> String {
    format!("Status prompt injected\n{content}")
}

/// Names one status slot of a conversation. Keys are stable across engine restarts
/// and iterate in string order so status assembly stays deterministic.
#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct StatusKey(String);

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

/// One offered status slot. The source computes the hash over its own inputs, so
/// the core steers without re-rendering anything and a meaning-preserving re-render
/// of unchanged inputs can be suppressed at the source.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct StatusOffer {
    pub hash: String,
    pub text: String,
    pub is_auto_update: bool,
}

/// The status slots offered to the core for one assembly, in fixed slot order.
pub type StatusOffering = BTreeMap<StatusKey, StatusOffer>;

/// Cumulative identities of the status slots delivered to the conversation,
/// read from the latest persisted status message.
pub type StatusStanding = BTreeMap<StatusKey, String>;

/// Returns whether the core should steer a new status assembly at a safe point.
/// Non-auto slots never trigger steering on their own. A new conversation has
/// empty standing state, so the first safe point steers the initial assembly
/// even when every slot is static. An auto-update slot with no standing hash
/// counts as changed, because its content was never injected.
pub fn status_steering_needed(offering: &StatusOffering, standing: &StatusStanding) -> bool {
    if offering.is_empty() {
        return false;
    }
    standing.is_empty()
        || offering
            .iter()
            .filter(|(_, offer)| offer.is_auto_update)
            .any(|(key, offer)| standing.get(key) != Some(&offer.hash))
}

/// Joins every offered slot's rendered text in slot order.
pub fn join_status_offering(offering: &StatusOffering) -> String {
    offering
        .values()
        .map(|offer| offer.text.as_str())
        .collect::<Vec<_>>()
        .join("\n\n")
}

/// Injects full fitted status without a standing baseline. Later auto-update
/// changes inject only auto-update slots with changed identities.
/// Each message atomically records cumulative standing identities so unchanged
/// slots stay known across updates and restarts.
/// Safe points occur between provider requests.
pub async fn steer_status(
    states: &mut dyn States,
    provider: &dyn Provider,
    definitions: &[ToolDefinition],
    cancellation: CancellationToken<'_>,
) -> Result<(), Error> {
    if cancellation.is_cancelled() {
        return Err(Error::cancelled());
    }
    let instructions = states.capture_system_prompt().await?;
    let messages = states.read_provider_messages().await?;
    let configuration = states
        .get_config(ConfigKey::Context)
        .await?
        .into_context()?;
    let tokens = provider.estimate_input_tokens(&instructions, definitions, &messages)?;
    let context = ContextSnapshot::estimate(tokens, &configuration);
    let offering = states.read_status_offering(context).await?;
    let mut standing = states.get_agent_state().await?.standing_statuses;
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
            &configuration,
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
            persistence: ConversationPersistence::StatusSteering {
                standing,
                event: ConversationEvent::Notice {
                    kind: "status".into(),
                    notice,
                },
            },
        })
        .await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{StatusKey, StatusOffer, StatusOffering, StatusStanding, status_steering_needed};

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
        let recorded = standing(&[("selection", "a"), ("time", "b")]);
        assert!(!status_steering_needed(&current, &recorded));
    }

    #[test]
    fn changed_auto_hash_steers() {
        let current = offering(&[("selection", "a2", true), ("time", "b", false)]);
        let recorded = standing(&[("selection", "a"), ("time", "b")]);
        assert!(status_steering_needed(&current, &recorded));
    }

    #[test]
    fn non_auto_change_alone_steers_nothing() {
        let current = offering(&[("selection", "a", true), ("time", "b2", false)]);
        let recorded = standing(&[("selection", "a"), ("time", "b")]);
        assert!(!status_steering_needed(&current, &recorded));
    }

    #[test]
    fn unchanged_after_restart_steers_nothing() {
        let restored = offering(&[("selection", "a2", true), ("time", "b2", false)]);
        let recorded = standing(&[("selection", "a2"), ("time", "b2")]);
        assert!(!status_steering_needed(&restored, &recorded));
    }

    #[test]
    fn auto_slot_without_standing_hash_counts_as_changed() {
        let current = offering(&[("selection", "a", true), ("work", "w", true)]);
        let recorded = standing(&[("selection", "a")]);
        assert!(status_steering_needed(&current, &recorded));
    }

    #[test]
    fn empty_offering_steers_nothing() {
        let current = StatusOffering::default();
        let recorded = standing(&[("selection", "a")]);
        assert!(!status_steering_needed(&current, &recorded));
    }
}
