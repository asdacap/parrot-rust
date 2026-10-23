//! Scoped persistence of one agent's recoverable state.

use super::async_ops::BoxFuture;
use crate::values::agent_state::{AgentStateSnapshot, AgentStateUpdate};
use std::fmt;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AgentStateErrorKind {
    Io,
    Corrupt,
    UnsupportedVersion,
    OwnerMismatch,
    AlreadyExists,
    NotFound,
    InvalidPath,
    Retired,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AgentStateError {
    pub kind: AgentStateErrorKind,
    pub message: String,
}
impl AgentStateError {
    pub fn new(kind: AgentStateErrorKind, message: impl Into<String>) -> Self {
        Self {
            kind,
            message: message.into(),
        }
    }
}
impl fmt::Display for AgentStateError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}
impl std::error::Error for AgentStateError {}

/// One shared owner per agent scope. A polled operation is admitted after it
/// acquires the scoped gate and submits its blocking worker. Dropping an observer
/// before admission cancels it; after admission the worker completes with the
/// gate held. Saving state does not run work. Operations require a Tokio runtime.
pub trait AgentStateStore: Send + Sync {
    /// Returns None only when the snapshot file is absent, never for damaged state.
    fn load(&self) -> BoxFuture<'_, Result<Option<AgentStateSnapshot>, AgentStateError>>;

    /// Writes the initial snapshot only when absent. Existing state, including
    /// damaged state, is never overwritten by initialization.
    fn initialize(
        &self,
        snapshot: AgentStateSnapshot,
    ) -> BoxFuture<'_, Result<(), AgentStateError>>;

    /// Serializes read-modify-write of one section and increments the revision.
    /// Share this store instance across section owners; separate stores for the
    /// same directory are not a supported multi-writer arrangement. Pre-rename
    /// failures preserve the old snapshot. A directory-sync error after rename
    /// may leave the new snapshot visible; stop and reconcile, do not assume rollback.
    fn update(
        &self,
        update: AgentStateUpdate,
    ) -> BoxFuture<'_, Result<AgentStateSnapshot, AgentStateError>>;

    /// Removes this owner's snapshot and staging file after all writers have
    /// been stopped and joined. The caller must ensure no operation can write
    /// concurrently outside this shared store instance.
    fn retire(&self) -> BoxFuture<'_, Result<(), AgentStateError>>;
}
