//! T77 fixed regressions plus bounded seeded format and scheduler campaigns.
//! Each domain runs its fixed regressions before its generated cases; no retries.
mod paths;
mod streams;
mod structured;
mod support;

#[path = "../../crates/parrot-runtime/tests/memory_agent_state_support.rs"]
mod agent_state_support;
#[path = "../stress/scheduler.rs"]
mod scheduler;
