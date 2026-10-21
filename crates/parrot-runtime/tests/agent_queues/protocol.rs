use super::inventory::{inventory, state};
use parrot_domain::protocol::messages::{
    parrot::{Event, ToolSnapshot},
    queue::{QueueSnapshot, QueueState},
};
use parrot_runtime::agent_queues::*;
use parrot_storage::queue_files::Inventory;
use protobuf::Message;

#[test]
fn protocol_emits_one_queue_per_chunk_with_exact_owner_metadata_and_empty_terminal_chunk() {
    let inventory = inventory();
    inventory.register_owner(&[]).unwrap();
    inventory.update(state("alpha", 1)).unwrap();
    inventory.update(state("zebra", 2)).unwrap();
    let snapshot = inventory.capture();
    let events = inventory_events(&snapshot, "root-session").unwrap();
    assert_eq!(events.len(), 2);
    for (index, event) in events.iter().enumerate() {
        let envelope = event.tool_snapshot();
        assert_eq!(envelope.kind, "queue");
        assert_eq!(envelope.owner_agent_session_id, "child");
        assert_eq!(envelope.inventory_instance_id, "queue-inventory-test");
        assert_eq!(envelope.revision, 2);
        assert_eq!(envelope.chunk_index, index as u32);
        assert_eq!(envelope.final_chunk, index == 1);
        assert!(!envelope.removed);
        let body = QueueSnapshot::parse_from_bytes(&envelope.data).unwrap();
        assert_eq!(body.root_agent_session_id, "root-session");
        assert_eq!(body.queues.len(), 1);
        let queue = &body.queues[0];
        let expected = &snapshot.queues[index];
        assert_eq!(queue.name, expected.name);
        assert_eq!(queue.description, expected.description);
        assert_eq!(queue.item_count, expected.item_count as i32);
        assert_eq!(queue.owner_agent_session_id, "child");
        assert_eq!(queue.owner_agent_name, "child-name");
        assert_eq!(queue.parent_agent_session_id, "parent-id");
        assert_eq!(queue.parent_agent_name, "parent-name");
    }
    inventory.dispose();
    let events = inventory_events(&inventory.capture(), "root-session").unwrap();
    assert_eq!(events.len(), 1);
    let envelope = events[0].tool_snapshot();
    assert!(envelope.removed);
    assert!(envelope.final_chunk);
    assert_eq!(envelope.chunk_index, 0);
    assert!(
        QueueSnapshot::parse_from_bytes(&envelope.data)
            .unwrap()
            .queues
            .is_empty()
    );
    assert!(inventory_events(&snapshot, "").is_err());
}

fn independent_event(snapshot: &QueueInventorySnapshot) -> Event {
    let state = &snapshot.queues[0];
    let mut data = QueueSnapshot::new();
    data.root_agent_session_id = "root".into();
    data.queues.push(QueueState {
        name: state.name.clone(),
        description: state.description.clone(),
        item_count: state.item_count as i32,
        owner_agent_session_id: state.owner.session_id.clone(),
        owner_agent_name: state.owner.name.clone(),
        parent_agent_session_id: state.owner.parent_session_id.clone(),
        parent_agent_name: state.owner.parent_name.clone(),
        ..Default::default()
    });
    let mut event = Event::new();
    event.set_tool_snapshot(ToolSnapshot {
        kind: "queue".into(),
        owner_agent_session_id: snapshot.owner_agent_session_id.clone(),
        inventory_instance_id: snapshot.inventory_instance_id.clone(),
        revision: snapshot.revision,
        chunk_index: 0,
        final_chunk: true,
        removed: snapshot.removed,
        data: data.write_to_bytes().unwrap(),
        ..Default::default()
    });
    event
}

#[test]
fn protocol_limit_is_exact_total_protobuf_event_bytes_not_only_payload_size() {
    let inventory = inventory();
    inventory.register_owner(&[state("work", 1)]).unwrap();
    let mut snapshot = inventory.capture();
    let mut length = MAXIMUM_EVENT_BYTES as usize;
    for _ in 0..4 {
        snapshot.queues[0].description = "x".repeat(length);
        let size = independent_event(&snapshot).compute_size() as usize;
        if size == MAXIMUM_EVENT_BYTES as usize {
            break;
        }
        length = (length as isize + MAXIMUM_EVENT_BYTES as isize - size as isize) as usize;
    }
    snapshot.queues[0].description = "x".repeat(length);
    let expected = independent_event(&snapshot);
    assert_eq!(expected.compute_size(), MAXIMUM_EVENT_BYTES);
    assert!((expected.tool_snapshot().data.len() as u64) < MAXIMUM_EVENT_BYTES);
    let actual = inventory_events(&snapshot, "root").unwrap();
    assert_eq!(actual.len(), 1);
    assert_eq!(actual[0].compute_size(), MAXIMUM_EVENT_BYTES);
    assert_eq!(actual[0], expected);
    snapshot.queues[0].description.push('x');
    assert_eq!(
        independent_event(&snapshot).compute_size(),
        MAXIMUM_EVENT_BYTES + 1
    );
    assert_eq!(
        inventory_events(&snapshot, "root").unwrap_err().to_string(),
        format!("queue: inventory record exceeds {MAXIMUM_EVENT_BYTES} bytes")
    );
}
