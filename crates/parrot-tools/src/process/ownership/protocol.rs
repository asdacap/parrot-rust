//! Emits the unchanged process.proto payload in source-ordered inventory chunks.

use std::io;

use parrot_domain::protocol::messages::parrot::{Event, ShellProcessActivityKind, ToolSnapshot};
use parrot_domain::protocol::messages::process::{
    ActiveShellProcess, CompletedShellProcess, ShellProcessSnapshot,
};
use protobuf::{EnumOrUnknown, Message};

use super::inventory::{ActivityKind, InventorySubscription, ShellProcessInventorySnapshot};

pub const KIND: &str = "shell_process";
pub const MAXIMUM_EVENT_BYTES: u64 = 32 * 1024 * 1024;

/// One record per chunk, active records first and completed records second.
/// Empty and removed inventories still emit one final chunk.
pub fn convert(inventory: &ShellProcessInventorySnapshot) -> io::Result<Vec<Event>> {
    let count = inventory
        .processes
        .len()
        .checked_add(inventory.completed_processes.len())
        .filter(|count| *count <= i32::MAX as usize)
        .ok_or_else(|| io::Error::other("Shell process inventory record count overflow."))?;
    if count == 0 {
        return Ok(vec![build(
            inventory,
            0,
            true,
            ShellProcessSnapshot::new(),
        )?]);
    }
    let mut events = Vec::with_capacity(count);
    for process in &inventory.processes {
        let mut snapshot = ShellProcessSnapshot::new();
        snapshot.processes.push(ActiveShellProcess {
            process_id: process.process_id.clone(),
            name: process.name.clone(),
            command: process.command.clone(),
            origin_tool_call_id: process.origin_tool_call_id.clone(),
            owner_agent_session_id: process.owner_agent_session_id.clone(),
            owner_agent_name: process.owner_agent_name.clone(),
            parent_agent_session_id: process.parent_agent_session_id.clone(),
            parent_agent_name: process.parent_agent_name.clone(),
            depth: process.depth,
            elapsed_ms: process.elapsed_milliseconds(),
            description: process.description.clone(),
            activity_kind: EnumOrUnknown::new(match process.activity_kind {
                ActivityKind::Execution => {
                    ShellProcessActivityKind::SHELL_PROCESS_ACTIVITY_KIND_EXECUTION
                }
                ActivityKind::Monitor => {
                    ShellProcessActivityKind::SHELL_PROCESS_ACTIVITY_KIND_MONITOR
                }
            }),
            ..Default::default()
        });
        events.push(build(
            inventory,
            events.len() as u32,
            events.len() + 1 == count,
            snapshot,
        )?);
    }
    for process in &inventory.completed_processes {
        let mut snapshot = ShellProcessSnapshot::new();
        snapshot.completed_processes.push(CompletedShellProcess {
            process_id: process.process_id.clone(),
            elapsed_ms: process.elapsed_milliseconds,
            ..Default::default()
        });
        events.push(build(
            inventory,
            events.len() as u32,
            events.len() + 1 == count,
            snapshot,
        )?);
    }
    Ok(events)
}

fn build(
    inventory: &ShellProcessInventorySnapshot,
    chunk_index: u32,
    final_chunk: bool,
    snapshot: ShellProcessSnapshot,
) -> io::Result<Event> {
    let mut event = Event::new();
    event.set_tool_snapshot(ToolSnapshot {
        kind: KIND.into(),
        owner_agent_session_id: inventory.owner_agent_session_id.clone(),
        removed: inventory.removed,
        inventory_instance_id: inventory.inventory_instance_id.clone(),
        revision: inventory.revision,
        chunk_index,
        final_chunk,
        data: snapshot
            .write_to_bytes()
            .map_err(|error| io::Error::other(error.to_string()))?,
        ..Default::default()
    });
    check_size(&event)?;
    Ok(event)
}

fn check_size(event: &Event) -> io::Result<()> {
    if event.compute_size() > MAXIMUM_EVENT_BYTES {
        return Err(io::Error::other(format!(
            "Shell process inventory record exceeds {MAXIMUM_EVENT_BYTES} bytes."
        )));
    }
    Ok(())
}

/// The publication loop owns its subscription and ends when inventory disposal
/// closes it. Each callback receives a complete contiguous chunk batch.
pub async fn publish_snapshots(
    mut subscription: InventorySubscription,
    mut publish: impl FnMut(Vec<Event>) -> io::Result<()>,
) -> io::Result<()> {
    while let Some(snapshot) = subscription.recv().await {
        publish(convert(&snapshot)?)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::process::ownership::inventory::{
        ActiveShellProcessState, CompletedShellProcessState, ShellProcessInventory,
    };
    use std::time::{Duration, Instant};

    fn active(activity_kind: ActivityKind) -> ActiveShellProcessState {
        ActiveShellProcessState {
            process_id: "active-b".into(),
            name: "build".into(),
            command: "dotnet build".into(),
            description: "description".into(),
            origin_tool_call_id: "call-id".into(),
            owner_agent_session_id: "agent-id".into(),
            owner_agent_name: "main".into(),
            parent_agent_session_id: "parent-id".into(),
            parent_agent_name: "parent".into(),
            depth: 2,
            started: Instant::now() - Duration::from_millis(5_001),
            activity_kind,
        }
    }

    fn inventory(activity_kind: ActivityKind) -> ShellProcessInventorySnapshot {
        ShellProcessInventorySnapshot {
            owner_agent_session_id: "agent-id".into(),
            inventory_instance_id: "inventory".into(),
            revision: 7,
            removed: false,
            processes: vec![active(activity_kind)],
            completed_processes: vec![
                CompletedShellProcessState {
                    process_id: "completed-a".into(),
                    elapsed_milliseconds: Some(5_001),
                },
                CompletedShellProcessState {
                    process_id: "completed-c".into(),
                    elapsed_milliseconds: None,
                },
            ],
        }
    }

    #[test]
    fn original_protocol_chunks_preserve_fields_order_kind_and_optional_elapsed() {
        for kind in [ActivityKind::Execution, ActivityKind::Monitor] {
            let events = convert(&inventory(kind)).unwrap();
            assert_eq!(events.len(), 3);
            for (index, event) in events.iter().enumerate() {
                let encoded = event.write_to_bytes().unwrap();
                let decoded = Event::parse_from_bytes(&encoded).unwrap();
                assert_eq!(&decoded, event);
                let chunk = decoded.tool_snapshot();
                assert_eq!(chunk.kind, "shell_process");
                assert_eq!(chunk.owner_agent_session_id, "agent-id");
                assert_eq!(chunk.inventory_instance_id, "inventory");
                assert_eq!(chunk.revision, 7);
                assert_eq!(chunk.chunk_index, index as u32);
                assert_eq!(chunk.final_chunk, index == 2);
                assert!(!chunk.removed);
            }
            let first =
                ShellProcessSnapshot::parse_from_bytes(&events[0].tool_snapshot().data).unwrap();
            assert_eq!(first.processes.len(), 1);
            assert!(first.completed_processes.is_empty());
            let process = &first.processes[0];
            assert_eq!(process.process_id, "active-b");
            assert_eq!(process.name, "build");
            assert_eq!(process.command, "dotnet build");
            assert_eq!(process.description, "description");
            assert_eq!(process.origin_tool_call_id, "call-id");
            assert_eq!(process.owner_agent_session_id, "agent-id");
            assert_eq!(process.owner_agent_name, "main");
            assert_eq!(process.parent_agent_session_id, "parent-id");
            assert_eq!(process.parent_agent_name, "parent");
            assert_eq!(process.depth, 2);
            assert!(process.elapsed_ms >= 5_001);
            assert_eq!(
                process.activity_kind.value(),
                if kind == ActivityKind::Execution {
                    0
                } else {
                    1
                }
            );
            let second =
                ShellProcessSnapshot::parse_from_bytes(&events[1].tool_snapshot().data).unwrap();
            assert!(second.processes.is_empty());
            assert_eq!(second.completed_processes.len(), 1);
            assert_eq!(second.completed_processes[0].process_id, "completed-a");
            assert_eq!(second.completed_processes[0].elapsed_ms, Some(5_001));
            let third =
                ShellProcessSnapshot::parse_from_bytes(&events[2].tool_snapshot().data).unwrap();
            assert_eq!(third.completed_processes[0].process_id, "completed-c");
            assert_eq!(third.completed_processes[0].elapsed_ms, None);
        }
    }

    #[test]
    fn empty_and_removed_inventory_emit_one_final_empty_chunk() {
        let inventory = ShellProcessInventory::with_instance_id("agent-id", "empty");
        for removed in [false, true] {
            if removed {
                inventory.dispose();
            }
            let events = convert(&inventory.capture()).unwrap();
            assert_eq!(events.len(), 1);
            let chunk = events[0].tool_snapshot();
            assert_eq!(chunk.chunk_index, 0);
            assert!(chunk.final_chunk);
            assert_eq!(chunk.removed, removed);
            let data = ShellProcessSnapshot::parse_from_bytes(&chunk.data).unwrap();
            assert!(data.processes.is_empty());
            assert!(data.completed_processes.is_empty());
        }
    }

    #[test]
    fn whole_event_exact_limit_is_allowed_and_one_byte_above_is_rejected() {
        let mut event = Event::new();
        event.set_tool_snapshot(ToolSnapshot {
            data: vec![0; MAXIMUM_EVENT_BYTES as usize],
            ..Default::default()
        });
        let excess = event.compute_size() - MAXIMUM_EVENT_BYTES;
        event
            .mut_tool_snapshot()
            .data
            .truncate((MAXIMUM_EVENT_BYTES - excess) as usize);
        assert_eq!(event.compute_size(), MAXIMUM_EVENT_BYTES);
        check_size(&event).unwrap();
        event.mut_tool_snapshot().data.push(0);
        assert_eq!(event.compute_size(), MAXIMUM_EVENT_BYTES + 1);
        assert!(check_size(&event).is_err());
    }

    #[test]
    fn oversized_actual_record_and_outer_metadata_are_not_silently_split_or_omitted() {
        let mut oversized = inventory(ActivityKind::Monitor);
        oversized.processes[0].command = "x".repeat(MAXIMUM_EVENT_BYTES as usize);
        assert!(convert(&oversized).is_err());
        let mut metadata = ShellProcessInventory::with_instance_id("owner", "empty").capture();
        metadata.owner_agent_session_id = "x".repeat(MAXIMUM_EVENT_BYTES as usize);
        assert!(convert(&metadata).is_err());
    }

    #[tokio::test]
    async fn publisher_delivers_current_and_removed_batches_then_joins() {
        let inventory = ShellProcessInventory::with_instance_id("agent-id", "inventory");
        let subscription = inventory.subscribe();
        let mut batches = Vec::new();
        publish_snapshots(subscription, |events| {
            batches.push(events);
            // Synchronous publication may cause a new snapshot, just as a broker
            // callback can; the inventory lock is not held across this callback.
            if batches.len() == 1 {
                inventory.dispose();
            }
            Ok(())
        })
        .await
        .unwrap();
        assert_eq!(batches.len(), 2);
        assert!(!batches[0][0].tool_snapshot().removed);
        assert!(batches[1][0].tool_snapshot().removed);
    }
}
