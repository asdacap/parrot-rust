//! Provider-side reasoning classification and stripping of raw provider
//! content, one implementation per wire protocol.

pub(crate) mod anthropic;
pub(crate) mod chat_completions;
#[cfg(test)]
mod filter_tests;
mod registry;
pub(crate) mod responses;

pub use registry::reasoning_filters;
