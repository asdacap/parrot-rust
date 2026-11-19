//! Provider composition, scoped sessions, retry policy and request diagnostics.
mod core_protocol;
pub mod decorators;
pub mod diagnostic_session;
pub mod diagnostics;
mod fast_mode;
pub mod http;
pub mod model;
pub mod pool;
pub mod provider;
pub mod retry;
mod runtime;
pub mod session_id;
pub mod socket_session;

pub use fast_mode::FastMode;
pub use model::*;
pub use pool::ProviderSessions;
pub use runtime::*;
pub use session_id::{ProviderSessionId, ProviderSessionIds};
