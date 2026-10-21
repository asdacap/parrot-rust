use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use futures_util::stream;
use parrot_cli::presenters::streams::{
    ProviderCallUsageStreamReader, QueueSnapshotStreamReader, SessionUsageSnapshotStreamReader,
    ShellProcessSnapshotStreamReader, StreamObservation,
};
use parrot_domain::contracts::async_ops::{BoxFuture, Cancellation};
use parrot_domain::protocol::messages::{parrot as wire, process, queue};
use protobuf::Message;
use tonic::{Code, Status};

#[derive(Default)]
struct TestCancellation {
    cancelled: AtomicBool,
    notify: tokio::sync::Notify,
}
impl TestCancellation {
    fn cancel(&self) {
        self.cancelled.store(true, Ordering::SeqCst);
        self.notify.notify_waiters();
    }
}
impl Cancellation for TestCancellation {
    fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::SeqCst)
    }
    fn cancelled(&self) -> BoxFuture<'_, ()> {
        Box::pin(async move {
            loop {
                let notified = self.notify.notified();
                tokio::pin!(notified);
                notified.as_mut().enable();
                if self.is_cancelled() {
                    return;
                }
                notified.await;
            }
        })
    }
}

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

#[tokio::test]
async fn usage_is_consumed_and_never_forwarded() {
    let usage = wire::ProviderCallUsage {
        input_tokens: 1,
        output_tokens: 2,
        ..Default::default()
    };
    let mut source = stream::iter([
        Ok(wire::Event {
            agent_session_id: "child".into(),
            payload: Some(wire::event::Payload::ProviderCallUsage(usage.clone())),
            ..Default::default()
        }),
        Ok(wire::Event {
            payload: Some(wire::event::Payload::TurnStarted(
                wire::TurnStarted::default(),
            )),
            ..Default::default()
        }),
    ]);
    let received = Arc::new(Mutex::new(Vec::new()));
    let mut reader = ProviderCallUsageStreamReader::new();
    let cancellation = TestCancellation::default();
    let callback = received.clone();
    let forwarded = reader
        .next(
            &mut source,
            move |value, _| {
                callback.lock().unwrap().push(value);
                Box::pin(async { Ok(()) })
            },
            &cancellation,
        )
        .await
        .unwrap()
        .unwrap();
    assert!(matches!(
        forwarded.payload,
        Some(wire::event::Payload::TurnStarted(_))
    ));
    assert_eq!(*received.lock().unwrap(), vec![usage]);
    assert!(
        reader
            .next(
                &mut source,
                |_, _| Box::pin(async { panic!("unexpected callback") }),
                &cancellation
            )
            .await
            .unwrap()
            .is_none()
    );
}

#[tokio::test]
async fn snapshots_update_modeline_usage_and_are_not_rendered() {
    let snapshot = wire::SessionUsageSnapshot {
        revision: 3,
        input_tokens: 2400,
        ..Default::default()
    };
    let expected = wire::Event {
        agent_session_id: "root".into(),
        payload: Some(wire::event::Payload::TurnStarted(
            wire::TurnStarted::default(),
        )),
        ..Default::default()
    };
    let mut source = stream::iter([
        Ok(wire::Event {
            payload: Some(wire::event::Payload::SessionUsageSnapshot(snapshot.clone())),
            ..Default::default()
        }),
        Ok(expected.clone()),
    ]);
    let received = Arc::new(Mutex::new(Vec::new()));
    let mut reader = SessionUsageSnapshotStreamReader::new();
    let callback = received.clone();
    assert_eq!(
        reader
            .next(
                &mut source,
                move |value, _| {
                    callback.lock().unwrap().push(value);
                    Box::pin(async { Ok(()) })
                },
                &TestCancellation::default()
            )
            .await
            .unwrap(),
        Some(expected)
    );
    assert_eq!(*received.lock().unwrap(), vec![snapshot]);
    assert!(
        reader
            .next(
                &mut source,
                |_, _| Box::pin(async { panic!("unexpected callback") }),
                &TestCancellation::default()
            )
            .await
            .unwrap()
            .is_none()
    );
}

#[tokio::test]
async fn queue_applies_only_complete_ordered_replacements_and_preserves_owners() {
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
    let mut source = stream::iter(events.into_iter().map(Ok));
    let mut reader = QueueSnapshotStreamReader::new();
    let received = Arc::new(Mutex::new(Vec::new()));
    for id in ["visible", "next"] {
        let callback = received.clone();
        assert_eq!(
            reader
                .next(
                    &mut source,
                    move |value, _| {
                        callback.lock().unwrap().push(value);
                        Box::pin(async { Ok(()) })
                    },
                    &TestCancellation::default()
                )
                .await
                .unwrap()
                .unwrap()
                .id,
            id
        );
    }
    let values = received.lock().unwrap();
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

#[tokio::test]
async fn process_applies_complete_ordered_snapshots_and_bypasses_activity_stream() {
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
    let mut source = stream::iter(events.into_iter().map(Ok));
    let mut reader = ShellProcessSnapshotStreamReader::new();
    let received = Arc::new(Mutex::new(Vec::new()));
    for id in ["visible", "next"] {
        let callback = received.clone();
        assert_eq!(
            reader
                .next(
                    &mut source,
                    move |value, _| {
                        callback.lock().unwrap().push(value);
                        Box::pin(async { Ok(()) })
                    },
                    &TestCancellation::default()
                )
                .await
                .unwrap()
                .unwrap()
                .id,
            id
        );
    }
    let values = received.lock().unwrap();
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

#[tokio::test]
async fn stream_filters_propagate_transport_and_observer_failures_without_forwarding() {
    let mut reader = ProviderCallUsageStreamReader::new();
    let mut source = stream::iter([Err(Status::unavailable("source failure"))]);
    let error = reader
        .next(
            &mut source,
            |_, _| Box::pin(async { panic!("observer called") }),
            &TestCancellation::default(),
        )
        .await
        .unwrap_err();
    assert_eq!(error.code(), Code::Unavailable);
    assert_eq!(error.message(), "source failure");
    let mut source = stream::iter([
        Ok(wire::Event {
            payload: Some(wire::event::Payload::ProviderCallUsage(
                wire::ProviderCallUsage::default(),
            )),
            ..Default::default()
        }),
        Ok(visible("not yet forwarded")),
    ]);
    let error = reader
        .next(
            &mut source,
            |_, _| Box::pin(async { Err(Status::aborted("observer failure")) }),
            &TestCancellation::default(),
        )
        .await
        .unwrap_err();
    assert_eq!(error.code(), Code::Aborted);
    assert_eq!(error.message(), "observer failure");
    assert_eq!(
        reader
            .next(
                &mut source,
                |_, _| Box::pin(async { Ok(()) }),
                &TestCancellation::default()
            )
            .await
            .unwrap()
            .unwrap()
            .id,
        "not yet forwarded"
    );
}

#[tokio::test]
async fn cancellation_is_observed_before_read_and_while_waiting_without_signals() {
    let mut reader = ProviderCallUsageStreamReader::new();
    let cancellation = Arc::new(TestCancellation::default());
    cancellation.cancel();
    let mut source = stream::iter([Ok(visible("not consumed"))]);
    assert_eq!(
        reader
            .next(
                &mut source,
                |_, _| Box::pin(async { Ok(()) }),
                cancellation.as_ref()
            )
            .await
            .unwrap_err()
            .code(),
        Code::Cancelled
    );
    assert_eq!(
        reader
            .next(
                &mut source,
                |_, _| Box::pin(async { Ok(()) }),
                &TestCancellation::default()
            )
            .await
            .unwrap()
            .unwrap()
            .id,
        "not consumed"
    );
    let cancellation = Arc::new(TestCancellation::default());
    let cancel = cancellation.clone();
    let cancelling = async move {
        tokio::task::yield_now().await;
        cancel.cancel();
    };
    let reading = async {
        let mut pending = stream::pending::<Result<wire::Event, Status>>();
        reader
            .next(
                &mut pending,
                |_, _| Box::pin(async { Ok(()) }),
                cancellation.as_ref(),
            )
            .await
    };
    let (result, ()) = tokio::join!(reading, cancelling);
    assert_eq!(result.unwrap_err().code(), Code::Cancelled);
}
