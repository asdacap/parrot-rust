use super::{AgentQueues, QueueError, QueueInventorySnapshot};
use crate::{
    children::{ChildError, ChildResult, Operation, SnapshotPublisher},
    event_broker::EventBroker,
};
use parrot_domain::{
    contracts::security::RestrictableAccessSnapshot,
    protocol::messages::{
        parrot::{Event, ToolSnapshot},
        queue::{QueueSnapshot, QueueState},
    },
};
use parrot_storage::queue_files::{Error, ErrorKind, Result};
use protobuf::Message;
use std::sync::Arc;

pub const MAXIMUM_EVENT_BYTES: u64 = 32 * 1024 * 1024;
/// One complete inventory batch: one queue per chunk, or one empty final chunk.
pub fn inventory_events(
    inventory: &QueueInventorySnapshot,
    root_agent_session_id: &str,
) -> Result<Vec<Event>> {
    if root_agent_session_id.is_empty() {
        return Err(Error::new(
            ErrorKind::Argument,
            "root agent session id must not be empty",
        ));
    }
    let count = inventory.queues.len().max(1);
    let mut events = Vec::with_capacity(count);
    for index in 0..count {
        let mut data = QueueSnapshot::new();
        data.root_agent_session_id = root_agent_session_id.to_owned();
        if let Some(state) = inventory.queues.get(index) {
            data.queues.push(QueueState {
                name: state.name.clone(),
                description: state.description.clone(),
                item_count: i32::try_from(state.item_count).map_err(|_| {
                    Error::new(ErrorKind::Queue, "queue: item count exceeds protocol range")
                })?,
                owner_agent_session_id: state.owner.session_id.clone(),
                owner_agent_name: state.owner.name.clone(),
                parent_agent_session_id: state.owner.parent_session_id.clone(),
                parent_agent_name: state.owner.parent_name.clone(),
                ..QueueState::new()
            });
        }
        let tool = ToolSnapshot {
            kind: "queue".into(),
            owner_agent_session_id: inventory.owner_agent_session_id.clone(),
            inventory_instance_id: inventory.inventory_instance_id.clone(),
            revision: inventory.revision,
            chunk_index: u32::try_from(index).map_err(|_| {
                Error::new(
                    ErrorKind::Queue,
                    "queue: inventory chunk index exceeds protocol range",
                )
            })?,
            final_chunk: index == count - 1,
            removed: inventory.removed,
            data: data
                .write_to_bytes()
                .map_err(|error| Error::new(ErrorKind::Queue, error.to_string()))?,
            ..ToolSnapshot::new()
        };
        let mut event = Event::new();
        event.set_tool_snapshot(tool);
        if event.compute_size() > MAXIMUM_EVENT_BYTES {
            return Err(Error::new(
                ErrorKind::Queue,
                format!("queue: inventory record exceeds {MAXIMUM_EVENT_BYTES} bytes"),
            ));
        }
        events.push(event);
    }
    Ok(events)
}

pub struct QueueSnapshotPublisher<S: RestrictableAccessSnapshot + 'static> {
    queues: Arc<AgentQueues<S>>,
    events: Arc<EventBroker>,
    root_agent_session_id: String,
}
impl<S: RestrictableAccessSnapshot + 'static> QueueSnapshotPublisher<S> {
    pub fn new(
        queues: Arc<AgentQueues<S>>,
        events: Arc<EventBroker>,
        root_agent_session_id: String,
    ) -> Self {
        Self {
            queues,
            events,
            root_agent_session_id,
        }
    }
}
impl<S: RestrictableAccessSnapshot + 'static> SnapshotPublisher for QueueSnapshotPublisher<S> {
    fn capture(&self) -> ChildResult<Vec<Event>> {
        inventory_events(
            &self.queues.capture_inventory(),
            &self.root_agent_session_id,
        )
        .map_err(|error| QueueError::from(error).into())
    }
    fn start(&self) -> ChildResult<Operation<()>> {
        // C# Run subscribes synchronously before its first await. Admission followed
        // immediately by close must retain the terminal snapshot before first poll.
        let mut subscription = self
            .queues
            .subscribe_inventory()
            .map_err(|error| ChildError::from(QueueError::from(error)))?;
        let events = self.events.clone();
        let root = self.root_agent_session_id.clone();
        Ok(Operation::start(async move {
            while let Some(snapshot) = subscription.next().await {
                let batch = inventory_events(&snapshot, &root)
                    .map_err(|error| ChildError::from(QueueError::from(error)))?;
                events
                    .publish_inventory(&batch)
                    .map_err(|error| ChildError::failure("invalid_operation", error.to_string()))?;
            }
            Ok(())
        }))
    }
}
