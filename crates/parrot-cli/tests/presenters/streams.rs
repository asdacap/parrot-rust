use parrot_cli::presenters::streams::{
    QueueSnapshotStreamReader, ShellProcessSnapshotStreamReader, StreamObservation,
};
use parrot_domain::protocol::messages::{parrot as wire, process, queue};
use protobuf::Message;
use tonic::Code;

fn visible(id: &str) -> wire::Event {
    wire::Event {
        id: id.into(),
        payload: Some(wire::event::Payload::TextChunk(wire::TextChunk {
            fragment: "answer".into(),
            ..Default::default()
        })),
        ..Default::default()
    }
}
fn queue_state(owner: &str, name: &str, count: i32) -> queue::QueueState {
    queue::QueueState {
        owner_agent_session_id: owner.into(),
        name: name.into(),
        item_count: count,
        description: format!("{owner} queue"),
        ..Default::default()
    }
}
#[allow(clippy::too_many_arguments)]
fn queue_event(
    owner: &str,
    instance: &str,
    revision: u64,
    chunk: u32,
    final_chunk: bool,
    removed: bool,
    root: &str,
    queues: Vec<queue::QueueState>,
) -> wire::Event {
    let snapshot = queue::QueueSnapshot {
        root_agent_session_id: root.into(),
        queues,
        ..Default::default()
    };
    wire::Event {
        payload: Some(wire::event::Payload::ToolSnapshot(wire::ToolSnapshot {
            kind: "queue".into(),
            owner_agent_session_id: owner.into(),
            inventory_instance_id: instance.into(),
            revision,
            chunk_index: chunk,
            final_chunk,
            removed,
            data: snapshot.write_to_bytes().unwrap(),
            ..Default::default()
        })),
        ..Default::default()
    }
}
fn process_state(owner: &str, id: &str) -> process::ActiveShellProcess {
    process::ActiveShellProcess {
        owner_agent_session_id: owner.into(),
        process_id: id.into(),
        name: id.into(),
        command: "sleep 10".into(),
        ..Default::default()
    }
}
#[allow(clippy::too_many_arguments)]
fn process_event(
    owner: &str,
    instance: &str,
    revision: u64,
    chunk: u32,
    final_chunk: bool,
    removed: bool,
    processes: Vec<process::ActiveShellProcess>,
    completed: Vec<process::CompletedShellProcess>,
) -> wire::Event {
    let snapshot = process::ShellProcessSnapshot {
        processes,
        completed_processes: completed,
        ..Default::default()
    };
    wire::Event {
        payload: Some(wire::event::Payload::ToolSnapshot(wire::ToolSnapshot {
            kind: "shell_process".into(),
            owner_agent_session_id: owner.into(),
            inventory_instance_id: instance.into(),
            revision,
            chunk_index: chunk,
            final_chunk,
            removed,
            data: snapshot.write_to_bytes().unwrap(),
            ..Default::default()
        })),
        ..Default::default()
    }
}
fn queue_result(
    reader: &mut QueueSnapshotStreamReader,
    event: &wire::Event,
) -> Option<(wire::ToolSnapshot, queue::QueueSnapshot)> {
    match reader.observe(event).unwrap() {
        StreamObservation::Consumed(value) => value,
        StreamObservation::Forward => panic!("queue inventory forwarded"),
    }
}
fn process_result(
    reader: &mut ShellProcessSnapshotStreamReader,
    event: &wire::Event,
) -> Option<(wire::ToolSnapshot, process::ShellProcessSnapshot)> {
    match reader.observe(event).unwrap() {
        StreamObservation::Consumed(value) => value,
        StreamObservation::Forward => panic!("process inventory forwarded"),
    }
}

#[test]
fn queue_applies_only_complete_ordered_replacements_and_preserves_owners() {
    let events = [
        queue_event(
            "root",
            "inventory",
            1,
            0,
            false,
            false,
            "root",
            vec![queue_state("child", "work", 1)],
        ),
        queue_event(
            "root",
            "inventory",
            1,
            1,
            true,
            false,
            "root",
            vec![queue_state("root", "work", 2)],
        ),
        visible("visible"),
        queue_event(
            "root",
            "inventory",
            1,
            0,
            true,
            false,
            "root",
            vec![queue_state("ignored", "stale", 1)],
        ),
        queue_event(
            "root",
            "inventory",
            3,
            1,
            true,
            false,
            "root",
            vec![queue_state("ignored", "orphan", 1)],
        ),
        queue_event(
            "root",
            "inventory",
            3,
            0,
            false,
            false,
            "root",
            vec![queue_state("child", "newer", 3)],
        ),
        queue_event("root", "inventory", 2, 0, true, false, "root", vec![]),
        queue_event(
            "root",
            "inventory",
            3,
            1,
            true,
            false,
            "root",
            vec![queue_state("root", "latest", 4)],
        ),
        visible("next"),
    ];
    let mut reader = QueueSnapshotStreamReader::new();
    let mut forwarded = Vec::new();
    let mut values = Vec::new();
    for event in &events {
        match reader.observe(event).unwrap() {
            StreamObservation::Forward => forwarded.push(event.id.as_str()),
            StreamObservation::Consumed(value) => values.extend(value),
        }
    }
    assert_eq!(forwarded, ["visible", "next"]);
    assert_eq!(values.len(), 2);
    assert_eq!(values[0].1.root_agent_session_id, "root");
    let rows = |snapshot: &queue::QueueSnapshot| {
        snapshot
            .queues
            .iter()
            .map(|q| format!("{}:{}:{}", q.owner_agent_session_id, q.name, q.item_count))
            .collect::<Vec<_>>()
            .join(",")
    };
    assert_eq!(rows(&values[0].1), "child:work:1,root:work:2");
    assert_eq!(rows(&values[1].1), "child:newer:3,root:latest:4");
}

#[test]
fn queue_partitions_interleaved_owners_and_rejects_stale_or_removed_generations() {
    for removed in [false, true] {
        let mut reader = QueueSnapshotStreamReader::new();
        let root = queue_state("root", "root-queue", 1);
        let events = [
            queue_event(
                "root",
                "root-one",
                8,
                0,
                false,
                false,
                "",
                vec![root.clone()],
            ),
            queue_event("child", "child-one", 1, 0, false, false, "", vec![]),
            queue_event(
                "root",
                "root-one",
                8,
                1,
                true,
                false,
                "",
                vec![root.clone()],
            ),
            queue_event("child", "child-one", 1, 1, true, false, "", vec![]),
            queue_event("child", "child-one", 2, 0, true, removed, "", vec![]),
            queue_event("child", "child-one", 1, 0, true, false, "", vec![]),
            queue_event(
                "root",
                "root-two",
                0,
                0,
                false,
                false,
                "",
                vec![root.clone()],
            ),
            queue_event(
                "root",
                "root-one",
                99,
                0,
                true,
                false,
                "",
                vec![root.clone()],
            ),
            queue_event(
                "root",
                "root-two",
                0,
                1,
                true,
                false,
                "",
                vec![root.clone()],
            ),
        ];
        let mut applied = events
            .iter()
            .filter_map(|event| queue_result(&mut reader, event))
            .collect::<Vec<_>>();
        assert_eq!(
            applied
                .iter()
                .map(|(i, _)| format!(
                    "{}:{}:{}",
                    i.owner_agent_session_id, i.inventory_instance_id, i.revision
                ))
                .collect::<Vec<_>>()
                .join(","),
            "root:root-one:8,child:child-one:1,child:child-one:2,root:root-two:0"
        );
        assert_eq!(applied[2].0.removed, removed);
        assert_eq!(applied[0].1.queues, vec![root.clone(), root.clone()]);
        let mut reconnected = QueueSnapshotStreamReader::new();
        applied.push(
            queue_result(
                &mut reconnected,
                &queue_event("root", "root-two", 0, 0, true, false, "", vec![root]),
            )
            .unwrap(),
        );
        assert_eq!(applied.len(), 5);
        assert_eq!(applied.last().unwrap().0.inventory_instance_id, "root-two");
    }
}

#[test]
fn process_applies_complete_ordered_snapshots_and_bypasses_activity_stream() {
    let completed = process::CompletedShellProcess {
        process_id: "done".into(),
        elapsed_ms: Some(7123),
        ..Default::default()
    };
    let events = [
        process_event(
            "root",
            "one",
            1,
            0,
            false,
            false,
            vec![process_state("", "first")],
            vec![],
        ),
        process_event(
            "root",
            "one",
            1,
            1,
            true,
            false,
            vec![process_state("", "second")],
            vec![completed.clone()],
        ),
        visible("visible"),
        process_event(
            "root",
            "one",
            1,
            0,
            true,
            false,
            vec![process_state("", "stale")],
            vec![],
        ),
        process_event(
            "root",
            "one",
            3,
            1,
            true,
            false,
            vec![process_state("", "orphan")],
            vec![process::CompletedShellProcess {
                process_id: "orphan".into(),
                ..Default::default()
            }],
        ),
        process_event(
            "root",
            "one",
            2,
            0,
            true,
            false,
            vec![],
            vec![completed.clone()],
        ),
        visible("next"),
    ];
    let mut reader = ShellProcessSnapshotStreamReader::new();
    let mut forwarded = Vec::new();
    let mut values = Vec::new();
    for event in &events {
        match reader.observe(event).unwrap() {
            StreamObservation::Forward => forwarded.push(event.id.as_str()),
            StreamObservation::Consumed(value) => values.extend(value),
        }
    }
    assert_eq!(forwarded, ["visible", "next"]);
    assert_eq!(values.len(), 2);
    assert_eq!(
        values[0]
            .1
            .processes
            .iter()
            .map(|p| p.process_id.as_str())
            .collect::<Vec<_>>(),
        vec!["first", "second"]
    );
    assert_eq!(values[0].1.completed_processes, vec![completed.clone()]);
    assert_eq!(values[1].0.revision, 2);
    assert!(values[1].1.processes.is_empty());
    assert_eq!(values[1].1.completed_processes, vec![completed]);
}

#[test]
fn process_rejects_delayed_chunks_from_retired_inventory_instance() {
    let mut reader = ShellProcessSnapshotStreamReader::new();
    let mut applied = Vec::new();
    for (instance, revision, id) in [
        ("old", 4, "old"),
        ("new", 1, "new"),
        ("old", 5, "resurrected"),
    ] {
        if let Some(value) = process_result(
            &mut reader,
            &process_event(
                "root",
                instance,
                revision,
                0,
                true,
                false,
                vec![process_state("", id)],
                vec![],
            ),
        ) {
            applied.push(value);
        }
    }
    assert_eq!(applied.len(), 2);
    assert_eq!(applied[1].0.inventory_instance_id, "new");
    assert_eq!(applied[1].1.processes[0].process_id, "new");
}

#[test]
fn process_partitions_interleaved_owners_and_rejects_stale_or_removed_generations() {
    for removed in [false, true] {
        let mut reader = ShellProcessSnapshotStreamReader::new();
        let root = process_state("root", "root-process");
        let events = [
            process_event(
                "root",
                "root-one",
                8,
                0,
                false,
                false,
                vec![root.clone()],
                vec![],
            ),
            process_event("child", "child-one", 1, 0, false, false, vec![], vec![]),
            process_event(
                "root",
                "root-one",
                8,
                1,
                true,
                false,
                vec![root.clone()],
                vec![],
            ),
            process_event("child", "child-one", 1, 1, true, false, vec![], vec![]),
            process_event("child", "child-one", 2, 0, true, removed, vec![], vec![]),
            process_event("child", "child-one", 1, 0, true, false, vec![], vec![]),
            process_event(
                "root",
                "root-two",
                0,
                0,
                false,
                false,
                vec![root.clone()],
                vec![],
            ),
            process_event(
                "root",
                "root-one",
                99,
                0,
                true,
                false,
                vec![root.clone()],
                vec![],
            ),
            process_event(
                "root",
                "root-two",
                0,
                1,
                true,
                false,
                vec![root.clone()],
                vec![],
            ),
        ];
        let mut applied = events
            .iter()
            .filter_map(|event| process_result(&mut reader, event))
            .collect::<Vec<_>>();
        assert_eq!(
            applied
                .iter()
                .map(|(i, _)| format!(
                    "{}:{}:{}",
                    i.owner_agent_session_id, i.inventory_instance_id, i.revision
                ))
                .collect::<Vec<_>>()
                .join(","),
            "root:root-one:8,child:child-one:1,child:child-one:2,root:root-two:0"
        );
        assert_eq!(applied[2].0.removed, removed);
        assert_eq!(applied[0].1.processes, vec![root.clone(), root.clone()]);
        let mut reconnected = ShellProcessSnapshotStreamReader::new();
        applied.push(
            process_result(
                &mut reconnected,
                &process_event("root", "root-two", 0, 0, true, false, vec![root], vec![]),
            )
            .unwrap(),
        );
        assert_eq!(applied.len(), 5);
        assert_eq!(applied.last().unwrap().0.inventory_instance_id, "root-two");
    }
}

#[test]
fn queue_rejects_mismatched_roots_and_removed_chunk_flags_without_committing_partial_rows() {
    let mut reader = QueueSnapshotStreamReader::new();
    assert!(
        queue_result(
            &mut reader,
            &queue_event(
                "root",
                "one",
                0,
                0,
                false,
                false,
                "root",
                vec![queue_state("child", "first", 1)]
            )
        )
        .is_none()
    );
    assert!(
        queue_result(
            &mut reader,
            &queue_event(
                "root",
                "one",
                0,
                1,
                true,
                false,
                "other",
                vec![queue_state("root", "wrong-root", 2)]
            )
        )
        .is_none()
    );
    assert!(
        queue_result(
            &mut reader,
            &queue_event(
                "root",
                "one",
                0,
                1,
                true,
                true,
                "root",
                vec![queue_state("root", "wrong-removal", 2)]
            )
        )
        .is_none()
    );
    let (_, snapshot) = queue_result(
        &mut reader,
        &queue_event(
            "root",
            "one",
            0,
            1,
            true,
            false,
            "root",
            vec![queue_state("root", "last", 2)],
        ),
    )
    .unwrap();
    assert_eq!(
        snapshot
            .queues
            .iter()
            .map(|q| q.name.as_str())
            .collect::<Vec<_>>(),
        vec!["first", "last"]
    );
    assert!(
        queue_result(
            &mut reader,
            &queue_event("root", "one", 1, 0, true, true, "root", vec![])
        )
        .is_some()
    );
    assert!(
        queue_result(
            &mut reader,
            &queue_event("root", "one", 99, 0, true, false, "root", vec![])
        )
        .is_none()
    );
    assert!(
        queue_result(
            &mut reader,
            &queue_event("root", "two", 0, 0, true, false, "root", vec![])
        )
        .is_some()
    );
}

#[test]
fn both_inventory_filters_consume_invalid_identity_forward_other_kinds_and_reject_corrupt_data() {
    let mut queue_reader = QueueSnapshotStreamReader::new();
    let mut process_reader = ShellProcessSnapshotStreamReader::new();
    assert_eq!(
        queue_reader.observe(&visible("visible")).unwrap(),
        StreamObservation::Forward
    );
    assert_eq!(
        process_reader.observe(&visible("visible")).unwrap(),
        StreamObservation::Forward
    );
    let mut event = queue_event("", "one", 1, 0, true, false, "root", vec![]);
    let Some(wire::event::Payload::ToolSnapshot(inventory)) = event.payload.as_mut() else {
        unreachable!()
    };
    inventory.data = vec![0xff];
    assert_eq!(
        queue_reader.observe(&event).unwrap(),
        StreamObservation::Consumed(None)
    );
    let Some(wire::event::Payload::ToolSnapshot(inventory)) = event.payload.as_mut() else {
        unreachable!()
    };
    inventory.owner_agent_session_id = "root".into();
    assert_eq!(
        queue_reader.observe(&event).unwrap_err().code(),
        Code::Internal
    );
    assert_eq!(
        process_reader.observe(&event).unwrap(),
        StreamObservation::Forward
    );
    let Some(wire::event::Payload::ToolSnapshot(inventory)) = event.payload.as_mut() else {
        unreachable!()
    };
    inventory.kind = "shell_process".into();
    inventory.inventory_instance_id.clear();
    assert_eq!(
        process_reader.observe(&event).unwrap(),
        StreamObservation::Consumed(None)
    );
    let Some(wire::event::Payload::ToolSnapshot(inventory)) = event.payload.as_mut() else {
        unreachable!()
    };
    inventory.inventory_instance_id = "one".into();
    assert_eq!(
        process_reader.observe(&event).unwrap_err().code(),
        Code::Internal
    );
    assert_eq!(
        queue_reader.observe(&event).unwrap(),
        StreamObservation::Forward
    );
}
