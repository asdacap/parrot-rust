use std::collections::BTreeMap;

/// Names one status slot of a conversation. Keys are stable across engine restarts
/// and iterate in string order so status assembly stays deterministic.
#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd, serde::Serialize, serde::Deserialize)]
pub struct StatusKey(pub(crate) String);

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
