//! Immutable per-invocation policy captures and explicit selected values.

use std::path::Path;

use super::async_ops::Cancellation;
use super::paths::ResolvedRead;

/// An owned policy capture. Interior mutation that changes the captured policy
/// violates this contract, even if the implementation is otherwise thread safe.
pub trait AccessSnapshot: Send + Sync {
    type Error: Send + Sync + 'static;

    /// Authorizes the actual resolved target, never just the requested path or
    /// an absent primary before fallback. Physical target resolution is T10/T36.
    fn check_read(&self, read: &ResolvedRead) -> Result<(), Self::Error>;
    fn check_write(&self, actual_target: &Path) -> Result<(), Self::Error>;
}

/// Restriction of already captured immutable policies, without policy lookup.
/// Lineage callers capture ancestors' current selected policies when resolving,
/// recursively restrict the ancestor result with the child, and return the child
/// unchanged at the root where there is no ancestor.
pub trait RestrictableAccessSnapshot: AccessSnapshot + Sized {
    /// Returns a new snapshot without changing either input or elevating access:
    /// permissions intersect, and read-only restrictions combine with logical OR.
    fn restrict_with(&self, child: &Self) -> Self;
}

pub trait SecurityProvider: Send + Sync {
    type Snapshot: AccessSnapshot;

    /// Captures a new owned immutable snapshot at each invocation start.
    fn capture(&self) -> Self::Snapshot;
}

/// Owns invocation-start policy and selection, borrowing only cancellation.
pub struct InvocationContext<'a, Security, Selection> {
    security: Security,
    selection: Selection,
    cancellation: &'a dyn Cancellation,
}

impl<'a, Security, Selection> InvocationContext<'a, Security, Selection> {
    pub fn new(
        security: Security,
        selection: Selection,
        cancellation: &'a dyn Cancellation,
    ) -> Self {
        Self {
            security,
            selection,
            cancellation,
        }
    }

    pub fn security(&self) -> &Security {
        &self.security
    }

    pub fn selection(&self) -> &Selection {
        &self.selection
    }

    pub fn cancellation(&self) -> &'a dyn Cancellation {
        self.cancellation
    }
}
