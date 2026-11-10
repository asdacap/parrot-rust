use super::*;
use serde_json::{Value, json};

fn encode_legacy(snapshot: &AgentStateSnapshot) -> Value {
    let mut value = serde_json::to_value(snapshot).unwrap();
    value["schema_version"] = json!(1);
    value["tasks"].as_object_mut().unwrap().remove("migration");
    for call in value["tasks"]["accepted_calls"].as_array_mut().unwrap() {
        let receipt = call.as_object_mut().unwrap().remove("receipt").unwrap();
        call["accepted_count"] = receipt["accepted_count"].clone();
    }
    for entry in value["tasks"]["entries"].as_array_mut().unwrap() {
        let execution = entry.as_object_mut().unwrap().remove("execution").unwrap();
        assert_eq!(execution["kind"], "dedicated");
        for (name, field) in execution["worker"].as_object().unwrap() {
            entry
                .as_object_mut()
                .unwrap()
                .insert(name.clone(), field.clone());
        }
    }
    value
}

fn populate_legacy_metadata(snapshot: &mut AgentStateSnapshot) {
    snapshot.tasks.notifications.push(TaskNotificationSnapshot {
        message_id: "notice-1".into(),
        target_session_id: "owner".into(),
        text: "saved notice".into(),
        wake: true,
        ready: true,
        owner_assignment: None,
    });
    snapshot.tasks.accepted_calls.push(AcceptedTaskCall {
        identity: parrot_domain::values::agent_tasks::TaskCallIdentity {
            assistant_sequence: 71,
            call_id: "call-1".into(),
        },
        receipt: TaskCallReceipt::LegacyCount { accepted_count: 5 },
    });
    snapshot.execution.as_mut().unwrap().result_claim = Some(TaskResultClaim {
        owner_session_id: "parent".into(),
        claim_id: "prompt-1".into(),
    });
}

#[tokio::test]
async fn version_one_load_preserves_the_file_and_normalizes_its_snapshot() {
    let fixture = Fixture::new();
    let store = fixture.store().await;
    let mut expected = snapshot();
    populate_legacy_metadata(&mut expected);
    let bytes = serde_json::to_vec(&encode_legacy(&expected)).unwrap();
    fs::write(fixture.path(), &bytes).unwrap();
    let actual = store.load().await.unwrap().unwrap();
    assert_eq!(actual.schema_version, AGENT_STATE_VERSION);
    assert_eq!(actual.owner_session_id, expected.owner_session_id);
    assert_eq!(actual.revision, expected.revision);
    assert_eq!(actual.descriptor, expected.descriptor);
    assert_eq!(actual.children, expected.children);
    assert_eq!(actual.execution, expected.execution);
    assert_eq!(actual.tasks.migration, TaskGraphMigrationSnapshot::Pending);
    assert_eq!(actual.tasks.revision, expected.tasks.revision);
    assert_eq!(actual.tasks.complete, expected.tasks.complete);
    assert_eq!(actual.tasks.all_resolved, expected.tasks.all_resolved);
    assert_eq!(
        actual.tasks.completion_notification,
        expected.tasks.completion_notification
    );
    assert_eq!(actual.tasks.notifications, expected.tasks.notifications);
    assert_eq!(actual.tasks.accepted_calls, expected.tasks.accepted_calls);
    for (actual, expected) in actual.tasks.entries.iter().zip(&expected.tasks.entries) {
        assert_eq!(actual.task, expected.task);
        assert_eq!(actual.diagnostic_id, expected.diagnostic_id);
        assert_eq!(actual.selection, expected.selection);
        assert_eq!(actual.fork_boundary, expected.fork_boundary);
        assert_eq!(actual.fork_mode, expected.fork_mode);
        let TaskExecutionSnapshot::Dedicated { worker: expected } = &expected.execution else {
            panic!("fixture must contain dedicated provenance");
        };
        let actual = match &actual.execution {
            TaskExecutionSnapshot::RetiringLegacy { worker }
            | TaskExecutionSnapshot::Dedicated { worker } => worker,
            _ => panic!("legacy attempt or terminal provenance was lost"),
        };
        assert_eq!(actual, expected);
    }
    assert_eq!(fs::read(fixture.path()).unwrap(), bytes);
}

#[tokio::test]
async fn legacy_execution_classification_keeps_stop_correlations_and_malformed_running_state() {
    let fixture = Fixture::new();
    let store = fixture.store().await;
    let mut legacy = encode_legacy(&snapshot());
    let entries = legacy["tasks"]["entries"].as_array_mut().unwrap();
    entries[1]["runner"] = Value::Null;
    let mut pending = entries[0].clone();
    pending["task"]["name"] = json!("not-started");
    pending["runner"] = Value::Null;
    pending["worker_session_id"] = Value::Null;
    pending["terminal_notification"] = Value::Null;
    entries.push(pending);
    let mut unassigned_terminal = entries[2].clone();
    unassigned_terminal["task"]["name"] = json!("accepted-without-execution");
    unassigned_terminal["runner"] = Value::Null;
    unassigned_terminal["worker_session_id"] = Value::Null;
    unassigned_terminal["terminal_notification"] = Value::Null;
    entries.push(unassigned_terminal);
    let mut stopping = entries[4].clone();
    stopping["task"]["name"] = json!("canceling");
    stopping["runner"] = Value::Null;
    let stop = TaskStopCheckpoint {
        generation_id: "old-generation".into(),
        prompt_message_id: "old-prompt".into(),
        worker_session_id: "child".into(),
        notice_message_id: "old-stop-notice".into(),
        stopped: true,
    };
    stopping["pending_stop"] = serde_json::to_value(&stop).unwrap();
    entries.push(stopping);
    fs::write(fixture.path(), serde_json::to_vec(&legacy).unwrap()).unwrap();
    let actual = store.load().await.unwrap().unwrap();
    let entries = &actual.tasks.entries;
    assert!(matches!(
        entries[0].execution,
        TaskExecutionSnapshot::RetiringLegacy { .. }
    ));
    let TaskExecutionSnapshot::RetiringLegacy { worker } = &entries[1].execution else {
        panic!("a malformed running legacy task must remain retirement work");
    };
    assert_eq!(entries[1].task.state, ExecutionStatus::Running);
    assert_eq!(worker.runner, None);
    assert!(
        entries[2..5]
            .iter()
            .all(|entry| matches!(entry.execution, TaskExecutionSnapshot::Dedicated { .. }))
    );
    assert!(matches!(
        entries[5].execution,
        TaskExecutionSnapshot::Unassigned
    ));
    assert!(matches!(
        entries[6].execution,
        TaskExecutionSnapshot::Unassigned
    ));
    let TaskExecutionSnapshot::RetiringLegacy { worker } = &entries[7].execution else {
        panic!("a terminal task with a pending stop must preserve retirement work");
    };
    assert_eq!(entries[7].task.state, ExecutionStatus::Canceled);
    assert_eq!(worker.pending_stop, Some(stop));
}

#[tokio::test]
async fn unrelated_section_saves_preserve_pending_legacy_migration_on_reopen() {
    for update in [
        AgentStateUpdate::Descriptor(snapshot().descriptor),
        AgentStateUpdate::Children(vec![]),
        AgentStateUpdate::Execution(None),
    ] {
        let fixture = Fixture::new();
        let store = fixture.store().await;
        let legacy = encode_legacy(&snapshot());
        fs::write(fixture.path(), serde_json::to_vec(&legacy).unwrap()).unwrap();
        let original = store.load().await.unwrap().unwrap();
        let saved = store.update(update).await.unwrap();
        assert_eq!(saved.tasks, original.tasks);
        assert_eq!(saved.tasks.migration, TaskGraphMigrationSnapshot::Pending);
        assert_eq!(saved.revision, original.revision + 1);
        let bytes: Value = serde_json::from_slice(&fs::read(fixture.path()).unwrap()).unwrap();
        assert_eq!(bytes["schema_version"], AGENT_STATE_VERSION);
        assert_eq!(fixture.store().await.load().await.unwrap(), Some(saved));
    }
}

#[tokio::test]
async fn existing_version_two_snapshots_default_to_current_migration() {
    let fixture = Fixture::new();
    let expected = snapshot();
    let mut encoded = serde_json::to_value(&expected).unwrap();
    encoded["tasks"]
        .as_object_mut()
        .unwrap()
        .remove("migration");
    let bytes = serde_json::to_vec(&encoded).unwrap();
    fs::write(fixture.path(), &bytes).unwrap();
    assert_eq!(fixture.store().await.load().await.unwrap(), Some(expected));
    assert_eq!(fs::read(fixture.path()).unwrap(), bytes);
}

#[tokio::test]
async fn legacy_changed_boundaries_reject_unknown_fields_including_new_notification_metadata() {
    let mut expected = snapshot();
    populate_legacy_metadata(&mut expected);
    for pointer in [
        "/unexpected",
        "/tasks/migration",
        "/tasks/entries/0/execution",
        "/tasks/entries/0/task/unexpected",
        "/tasks/entries/0/runner/unexpected",
        "/tasks/notifications/0/owner_assignment",
        "/descriptor/unexpected",
    ] {
        let fixture = Fixture::new();
        let store = fixture.store().await;
        let mut value = encode_legacy(&expected);
        let (parent, name) = pointer.rsplit_once('/').unwrap();
        value
            .pointer_mut(parent)
            .unwrap()
            .as_object_mut()
            .unwrap()
            .insert(name.into(), Value::Null);
        let bytes = serde_json::to_vec(&value).unwrap();
        fs::write(fixture.path(), &bytes).unwrap();
        assert_eq!(
            store.load().await.unwrap_err().kind,
            AgentStateErrorKind::Corrupt,
            "{pointer}"
        );
        assert_eq!(fs::read(fixture.path()).unwrap(), bytes);
    }
}

#[tokio::test]
async fn legacy_versions_and_owner_mismatches_keep_distinct_errors() {
    for (version, owner, kind) in [
        (0, "owner", AgentStateErrorKind::UnsupportedVersion),
        (99, "owner", AgentStateErrorKind::UnsupportedVersion),
        (1, "another-owner", AgentStateErrorKind::OwnerMismatch),
    ] {
        let fixture = Fixture::new();
        let store = fixture.store().await;
        let mut value = encode_legacy(&snapshot());
        value["schema_version"] = json!(version);
        value["owner_session_id"] = json!(owner);
        fs::write(fixture.path(), serde_json::to_vec(&value).unwrap()).unwrap();
        assert_eq!(store.load().await.unwrap_err().kind, kind);
    }
}

#[tokio::test]
async fn retiring_migration_progress_round_trips_across_other_section_updates() {
    for disposition in [
        TaskMigrationDisposition::Reassign,
        TaskMigrationDisposition::CancelObsolete,
    ] {
        let fixture = Fixture::new();
        let store = fixture.store().await;
        let mut expected = snapshot();
        expected.tasks.migration = TaskGraphMigrationSnapshot::Retiring {
            disposition,
            excluded_session_ids: vec!["old-worker".into(), "old-grandchild".into()],
        };
        store.initialize(expected.clone()).await.unwrap();
        store
            .update(AgentStateUpdate::Execution(None))
            .await
            .unwrap();
        expected.revision += 1;
        expected.execution = None;
        assert_eq!(fixture.store().await.load().await.unwrap(), Some(expected));
    }
}

#[tokio::test]
async fn current_migration_descriptors_reject_unknown_fields_and_invalid_dispositions() {
    for migration in [
        json!({"kind":"current","unexpected":true}),
        json!({"kind":"pending","unexpected":true}),
        json!({"kind":"retiring","disposition":"reassign","excluded_session_ids":[],"unexpected":true}),
        json!({"kind":"retiring","disposition":"invalid","excluded_session_ids":[]}),
        Value::Null,
    ] {
        let fixture = Fixture::new();
        let mut value = serde_json::to_value(snapshot()).unwrap();
        value["tasks"]["migration"] = migration;
        fs::write(fixture.path(), serde_json::to_vec(&value).unwrap()).unwrap();
        assert_eq!(
            fixture.store().await.load().await.unwrap_err().kind,
            AgentStateErrorKind::Corrupt
        );
    }
}

#[test]
fn receipt_decode_preserves_rich_response_and_normalizes_old_counts() {
    let identity = json!({"assistant_sequence": 17, "call_id": "call-1"});
    for receipt in [
        json!({"kind": "legacy_count", "accepted_count": 4}),
        json!({"kind": "preparing", "accepted_count": 4}),
        json!({"kind": "accepted", "accepted_count": 4, "response_text": "exact\nresponse"}),
    ] {
        let encoded = json!({"identity": identity, "receipt": receipt});
        let decoded: AcceptedTaskCall = serde_json::from_value(encoded.clone()).unwrap();
        assert_eq!(serde_json::to_value(decoded).unwrap(), encoded);
    }
    let old = json!({"identity": identity, "accepted_count": 4});
    let decoded: AcceptedTaskCall = serde_json::from_value(old).unwrap();
    assert_eq!(
        serde_json::to_value(decoded).unwrap(),
        json!({
            "identity": identity,
            "receipt": {"kind": "legacy_count", "accepted_count": 4}
        })
    );
}

#[test]
fn receipt_decode_rejects_unknown_fields_and_malformed_variants() {
    let identity = json!({"assistant_sequence": 17, "call_id": "call-1"});
    for invalid in [
        json!({"identity": identity, "accepted_count": 4, "extra": true}),
        json!({"identity": identity, "accepted_count": 4, "receipt": {"kind": "preparing", "accepted_count": 4}}),
        json!({"identity": identity, "receipt": {"kind": "preparing", "accepted_count": 4, "response_text": "not sealed"}}),
        json!({"identity": identity, "receipt": {"kind": "accepted", "accepted_count": 4}}),
        json!({"identity": identity, "receipt": {"kind": "unknown", "accepted_count": 4}}),
        json!({"identity": identity, "receipt": {"kind": "legacy_count", "accepted_count": -1}}),
    ] {
        assert!(serde_json::from_value::<AcceptedTaskCall>(invalid).is_err());
    }
}

#[tokio::test]
async fn version_one_receipts_reject_new_response_metadata() {
    let fixture = Fixture::new();
    let store = fixture.store().await;
    let mut legacy = encode_legacy(&snapshot());
    legacy["tasks"]["accepted_calls"] = json!([{
        "identity": {"assistant_sequence": 17, "call_id": "call-1"},
        "receipt": {"kind": "accepted", "accepted_count": 4, "response_text": "exact response"}
    }]);
    let bytes = serde_json::to_vec(&legacy).unwrap();
    fs::write(fixture.path(), &bytes).unwrap();
    assert_eq!(
        store.load().await.unwrap_err().kind,
        AgentStateErrorKind::Corrupt
    );
    assert_eq!(fs::read(fixture.path()).unwrap(), bytes);
}

#[tokio::test]
async fn version_two_count_receipt_load_is_read_only_and_section_save_uses_new_shape() {
    let fixture = Fixture::new();
    let store = fixture.store().await;
    let mut old = serde_json::to_value(snapshot()).unwrap();
    old["tasks"]["accepted_calls"] = json!([{
        "identity": {"assistant_sequence": 17, "call_id": "call-1"},
        "accepted_count": 4
    }]);
    let bytes = serde_json::to_vec(&old).unwrap();
    fs::write(fixture.path(), &bytes).unwrap();
    let loaded = store.load().await.unwrap().unwrap();
    assert_eq!(
        loaded.tasks.accepted_calls[0].receipt,
        TaskCallReceipt::LegacyCount { accepted_count: 4 }
    );
    assert_eq!(fs::read(fixture.path()).unwrap(), bytes);
    store
        .update(AgentStateUpdate::Children(loaded.children))
        .await
        .unwrap();
    let saved: Value = serde_json::from_slice(&fs::read(fixture.path()).unwrap()).unwrap();
    assert_eq!(saved["schema_version"], AGENT_STATE_VERSION);
    assert_eq!(
        saved["tasks"]["accepted_calls"][0]["receipt"],
        json!({"kind": "legacy_count", "accepted_count": 4})
    );
    assert!(
        saved["tasks"]["accepted_calls"][0]
            .get("accepted_count")
            .is_none()
    );
}

#[tokio::test]
async fn current_rich_receipts_survive_storage_reopen_and_unrelated_section_saves() {
    let fixture = Fixture::new();
    let store = fixture.store().await;
    let mut expected = snapshot();
    for (index, receipt) in [
        TaskCallReceipt::LegacyCount { accepted_count: 4 },
        TaskCallReceipt::Preparing { accepted_count: 5 },
        TaskCallReceipt::Accepted {
            accepted_count: 6,
            response_text: "exact\nresponse with unicode 🦜".into(),
        },
    ]
    .into_iter()
    .enumerate()
    {
        expected.tasks.accepted_calls.push(AcceptedTaskCall {
            identity: parrot_domain::values::agent_tasks::TaskCallIdentity {
                assistant_sequence: 17,
                call_id: format!("call-{index}"),
            },
            receipt,
        });
    }
    store.initialize(expected.clone()).await.unwrap();
    let reopened = fixture.store().await;
    assert_eq!(reopened.load().await.unwrap(), Some(expected.clone()));
    let saved = reopened
        .update(AgentStateUpdate::Execution(None))
        .await
        .unwrap();
    assert_eq!(saved.tasks.accepted_calls, expected.tasks.accepted_calls);
    assert_eq!(fixture.store().await.load().await.unwrap(), Some(saved));
}
