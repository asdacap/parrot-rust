//! T51 scoped queues over real retained sessions, storage, registry admission and event broker.
// Reuse the accepted real-session fixture; these modules never implement a replacement session.
mod admission;
#[path = "../children/admission_publisher.rs"]
mod admission_publisher;
mod binding;
mod dropped_parent;
mod errors;
mod harness;
mod inventory;
mod lifetime;
mod protocol;
mod replay_flow;
mod routing;
mod source_predicates;
#[allow(dead_code)] // Shared real-session fixture also serves dependency tests outside this target.
#[path = "../children/support.rs"]
mod support;
