use super::services::TurnSelection;
use crate::compaction::ContextSnapshot;
use parrot_domain::contracts::security::AccessSnapshot;

/// One source-ordered capture. Later status/statistics observations use this
/// selection, but are intentionally not part of a global atomic snapshot.
pub struct ObservedContext<S: AccessSnapshot> {
    pub selection: TurnSelection<S>,
    pub context: ContextSnapshot,
}
