//! Retained child scopes around the real single-drain agent session.
mod completion;
mod operation;
mod policy;
mod ports;
mod registry;
mod resolver;
mod scope;
mod spawner;

pub use completion::ParentScope;
pub use operation::{ChildError, ChildResult, Operation};
pub use policy::{PolicyLineage, SelectionCell};
pub use ports::*;
pub use registry::{AgentRegistry, ChildAdmission, ChildRegistry, RegistryConfig};
pub use resolver::AgentResolver;
pub use scope::{Scope, ScopeConfig};
pub use spawner::{AgentSpawner, HistorySource, LaunchRequest};
