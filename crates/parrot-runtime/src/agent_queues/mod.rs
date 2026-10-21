//! Scoped persistent queues and revisioned inventory over retained child scopes.
mod inventory;
mod publisher;
mod queues;

pub use inventory::{
    QueueInventory, QueueInventoryFeed, QueueInventorySnapshot, QueueInventorySubscription,
};
pub use publisher::{MAXIMUM_EVENT_BYTES, QueueSnapshotPublisher, inventory_events};
pub use queues::{
    AgentQueues, QueueChildAdmissionValidator, QueueDiagnostic, QueueDiagnostics, QueueError,
    QueueOwnerSnapshot, QueueResult,
};
