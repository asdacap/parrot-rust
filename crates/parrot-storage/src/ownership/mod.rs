//! Versioned session ownership and selected-root admission.
//!
//! Owner records are append-only, and a released lease may only retire its own
//! generation. Root selection precedes construction of internal resources.

mod claim;
mod runtime;
mod selected;
mod session_store;
mod types;

pub use claim::{WorkingDirectoryClaim, serialize_owner};
pub use runtime::{capture_runtime_identity, fingerprint_host, host_key, inspect_process};
pub use selected::{OwnershipRoots, SessionAdmission, SessionAdmissions};
pub use session_store::*;
pub use types::*;

#[cfg(test)]
mod claim_tests;
#[cfg(test)]
mod record_read_tests;
#[cfg(test)]
mod source_comparison_tests;
