//! Selected-session metadata and recoverable JSONL history projections.
//!
//! Session `meta.json` is a listing index, not database authority. `history.jsonl`
//! is a replay-friendly derived projection of the authoritative conversation DB.
mod agent_history;
mod catalog;
mod error;
mod index;
pub mod json;
mod metadata;

#[cfg(test)]
mod exact24_tests;
#[cfg(test)]
mod malformed_request_tests;

#[cfg(test)]
mod tests;

pub use agent_history::AgentHistoryFile;
pub use catalog::{SessionCatalog, SessionCatalogEntry, SessionCatalogState};
pub use error::{Error, ErrorKind, Result};
pub use index::SessionIndex;
pub use metadata::SessionMeta;
