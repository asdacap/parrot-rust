//! T77 fixed regressions plus bounded seeded format and scheduler campaigns.
//! Each domain runs its fixed regressions before its generated cases; no retries.
mod paths;
mod queue;
mod streams;
mod structured;
mod support;

#[path = "../stress/queue.rs"]
mod queue_stress;
#[path = "../stress/scheduler.rs"]
mod scheduler;
