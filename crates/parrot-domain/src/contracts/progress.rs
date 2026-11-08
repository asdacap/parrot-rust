use super::async_ops::BoxFuture;
use crate::values::progress::ProgressSnapshot;
use std::fmt;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ProgressStoreErrorKind {
    Io,
    Corrupt,
    UnsupportedVersion,
    OwnerMismatch,
    InvalidPath,
    UncertainCommit,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProgressStoreError {
    pub kind: ProgressStoreErrorKind,
    pub message: String,
}
impl ProgressStoreError {
    pub fn new(kind: ProgressStoreErrorKind, message: impl Into<String>) -> Self {
        Self {
            kind,
            message: message.into(),
        }
    }
}
impl fmt::Display for ProgressStoreError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}
impl std::error::Error for ProgressStoreError {}

/// Persistence for one agent's current progress, independent of its conversation.
/// Share one store instance within the owning agent scope. Operations require a
/// Tokio runtime; admitted blocking work retains its serialization gate even if
/// its observer is dropped. This store does not activate or close agent work.
pub trait ProgressStore: Send + Sync {
    /// Returns None only for an absent file. Invalid state is an error and must
    /// never be interpreted as an implicitly closed report.
    fn load(&self) -> BoxFuture<'_, Result<Option<ProgressSnapshot>, ProgressStoreError>>;

    /// Atomically replaces the owner's snapshot after validating existing state.
    /// The owner assigns revisions. Pre-commit errors preserve the previous file;
    /// UncertainCommit means rename succeeded but durability could not be confirmed.
    /// Reconcile from load before updating memory or assuming rollback in that case.
    fn replace(&self, snapshot: ProgressSnapshot) -> BoxFuture<'_, Result<(), ProgressStoreError>>;
}
