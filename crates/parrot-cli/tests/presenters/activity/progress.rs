use super::*;
use parrot_domain::values::progress::{PROGRESS_INVENTORY_KIND, Progress, ProgressSnapshot};

fn publish(
    view: &mut RawActivityView,
    owner: &str,
    instance: &str,
    revision: u64,
    progress: Option<Progress>,
) {
    let snapshot = ProgressSnapshot {
        revision,
        progress,
        ..ProgressSnapshot::new(owner.into())
    };
    let update = view
        .observe(&event(
            owner,
            wire::event::Payload::ToolSnapshot(wire::ToolSnapshot {
                kind: PROGRESS_INVENTORY_KIND.into(),
                owner_agent_session_id: owner.into(),
                inventory_instance_id: instance.into(),
                revision,
                final_chunk: true,
                data: serde_json::to_vec(&snapshot).unwrap(),
                ..Default::default()
            }),
        ))
        .unwrap();
    assert!(update.commits.is_empty());
}

#[test]
fn main_and_child_progress_replace_close_and_reopen_independently() {
    let mut view = view();
    view.observe(&turn("root")).unwrap();
    view.observe(&child("root", "captain", "")).unwrap();
    view.observe(&child("worker", "worker", "root")).unwrap();
    publish(
        &mut view,
        "root",
        "root-scope",
        1,
        Some(Progress {
            steps: 2,
            total_steps: 5,
            current_step: Some("Running tests".into()),
        }),
    );
    publish(
        &mut view,
        "worker",
        "child-scope",
        1,
        Some(Progress {
            steps: 0,
            total_steps: 4,
            current_step: Some("Inspecting".into()),
        }),
    );
    let rendered = live(&view);
    assert!(rendered.contains("[captain] 2/5 ["), "{rendered}");
    assert!(rendered.contains("40% Running tests"));
    assert!(rendered.contains("[worker] 0/4 ["));
    assert!(rendered.contains("0% Inspecting"));
    publish(
        &mut view,
        "root",
        "root-scope",
        2,
        Some(Progress {
            steps: 5,
            total_steps: 5,
            current_step: None,
        }),
    );
    let rendered = live(&view);
    assert!(rendered.contains("5/5 [████████████████████████] 100%"));
    assert!(!rendered.contains("Running tests"));
    publish(
        &mut view,
        "root",
        "root-scope",
        3,
        Some(Progress {
            steps: 1,
            total_steps: 3,
            current_step: None,
        }),
    );
    assert!(live(&view).contains("33%"));
    publish(&mut view, "root", "root-scope", 4, None);
    assert!(!live(&view).contains("1/3"));
    assert!(live(&view).contains("0/4"));
    publish(
        &mut view,
        "root",
        "root-scope",
        5,
        Some(Progress {
            steps: 1,
            total_steps: 2,
            current_step: Some("New plan".into()),
        }),
    );
    assert!(live(&view).contains("50% New plan"));
    assert_eq!(live(&view).matches("1/2").count(), 1);
}

#[test]
fn saved_progress_renders_without_a_turn_or_tool_and_survives_turn_failure() {
    let mut view = view();
    view.observe(&child("root", "captain", "")).unwrap();
    view.observe(&child("worker", "worker", "root")).unwrap();
    for owner in ["root", "worker"] {
        publish(
            &mut view,
            owner,
            owner,
            7,
            Some(Progress {
                steps: 2,
                total_steps: 5,
                current_step: Some("Saved work".into()),
            }),
        );
    }
    assert_eq!(live(&view).matches("40% Saved work").count(), 2);
    view.observe(&turn("root")).unwrap();
    view.observe(&turn("worker")).unwrap();
    view.observe(&event(
        "root",
        wire::event::Payload::TurnEnded(wire::TurnEnded {
            finish_reason: "interrupted".into(),
            ..Default::default()
        }),
    ))
    .unwrap();
    view.observe(&event(
        "worker",
        wire::event::Payload::AgentFailed(wire::AgentFailed {
            parent_agent_session_id: "root".into(),
            name: "worker".into(),
            message: "Interrupted".into(),
            ..Default::default()
        }),
    ))
    .unwrap();
    assert_eq!(live(&view).matches("40% Saved work").count(), 2);
}

#[test]
fn inventory_revisions_close_and_scope_retirement_prevent_stale_revival() {
    let mut view = view();
    view.observe(&turn("root")).unwrap();
    let initial = Progress {
        steps: 1,
        total_steps: 2,
        current_step: Some("Current".into()),
    };
    publish(&mut view, "root", "first", 5, Some(initial.clone()));
    for revision in [4, 5] {
        publish(&mut view, "root", "first", revision, None);
        assert!(live(&view).contains("50% Current"));
    }
    publish(&mut view, "root", "first", 6, None);
    assert!(!live(&view).contains("1/2"));
    publish(&mut view, "root", "first", 5, Some(initial.clone()));
    assert!(!live(&view).contains("1/2"));
    publish(&mut view, "root", "second", 6, Some(initial.clone()));
    assert!(live(&view).contains("50% Current"));
    publish(&mut view, "root", "first", 100, None);
    assert!(live(&view).contains("50% Current"));
    let state = ProgressSnapshot {
        revision: 6,
        progress: Some(initial.clone()),
        ..ProgressSnapshot::new("root".into())
    };
    view.observe(&event(
        "root",
        wire::event::Payload::ToolSnapshot(wire::ToolSnapshot {
            kind: PROGRESS_INVENTORY_KIND.into(),
            owner_agent_session_id: "root".into(),
            inventory_instance_id: "second".into(),
            revision: 6,
            final_chunk: true,
            removed: true,
            data: serde_json::to_vec(&state).unwrap(),
            ..Default::default()
        }),
    ))
    .unwrap();
    assert!(!live(&view).contains("1/2"));
    publish(&mut view, "root", "second", 7, Some(initial.clone()));
    assert!(!live(&view).contains("1/2"));
    publish(&mut view, "root", "third", 6, Some(initial));
    assert!(live(&view).contains("50% Current"));
}

#[test]
fn incomplete_envelopes_are_ignored_and_invalid_ownership_or_revision_is_an_error() {
    let mut view = view();
    view.observe(&turn("root")).unwrap();
    let snapshot = ProgressSnapshot {
        revision: 1,
        progress: Some(Progress {
            steps: 1,
            total_steps: 2,
            current_step: None,
        }),
        ..ProgressSnapshot::new("root".into())
    };
    let valid = wire::ToolSnapshot {
        kind: PROGRESS_INVENTORY_KIND.into(),
        owner_agent_session_id: "root".into(),
        inventory_instance_id: "scope".into(),
        revision: 1,
        final_chunk: true,
        data: serde_json::to_vec(&snapshot).unwrap(),
        ..Default::default()
    };
    for inventory in [
        wire::ToolSnapshot {
            chunk_index: 1,
            ..valid.clone()
        },
        wire::ToolSnapshot {
            final_chunk: false,
            ..valid.clone()
        },
    ] {
        view.observe(&event(
            "root",
            wire::event::Payload::ToolSnapshot(inventory),
        ))
        .unwrap();
        assert!(!live(&view).contains("1/2"));
    }
    for inventory in [
        wire::ToolSnapshot {
            owner_agent_session_id: "other".into(),
            ..valid.clone()
        },
        wire::ToolSnapshot {
            inventory_instance_id: String::new(),
            ..valid.clone()
        },
        wire::ToolSnapshot {
            revision: 2,
            ..valid.clone()
        },
    ] {
        assert!(
            view.observe(&event(
                "root",
                wire::event::Payload::ToolSnapshot(inventory)
            ))
            .is_err()
        );
        assert!(!live(&view).contains("1/2"));
    }
    assert!(
        view.observe(&event(
            "",
            wire::event::Payload::ToolSnapshot(valid.clone())
        ))
        .is_err()
    );
    assert!(
        view.observe(&event(
            "other",
            wire::event::Payload::ToolSnapshot(valid.clone())
        ))
        .is_err()
    );
    view.observe(&event("root", wire::event::Payload::ToolSnapshot(valid)))
        .unwrap();
    assert!(live(&view).contains("50%"));
}

#[test]
fn malformed_payloads_never_replace_committed_progress() {
    let mut view = view();
    view.observe(&turn("root")).unwrap();
    publish(
        &mut view,
        "root",
        "scope",
        1,
        Some(Progress {
            steps: 1,
            total_steps: 2,
            current_step: Some("Committed".into()),
        }),
    );
    for data in [
        "{",
        r#"{"schema_version":2,"owner_session_id":"root","revision":2,"progress":null}"#,
        r#"{"schema_version":1,"owner_session_id":"other","revision":2,"progress":null}"#,
        r#"{"schema_version":1,"owner_session_id":"root","revision":2}"#,
        r#"{"schema_version":1,"owner_session_id":"root","revision":2,"progress":{"steps":0,"total_steps":0}}"#,
        r#"{"schema_version":1,"owner_session_id":"root","revision":2,"progress":{"steps":3,"total_steps":2}}"#,
        r#"{"schema_version":1,"owner_session_id":"root","revision":2,"progress":{"steps":-1,"total_steps":2}}"#,
        r#"{"schema_version":1,"owner_session_id":"root","revision":2,"progress":{"steps":1.5,"total_steps":2}}"#,
        r#"{"schema_version":1,"owner_session_id":"root","revision":2,"progress":null,"extra":true}"#,
    ] {
        assert!(
            view.observe(&event(
                "root",
                wire::event::Payload::ToolSnapshot(wire::ToolSnapshot {
                    kind: PROGRESS_INVENTORY_KIND.into(),
                    owner_agent_session_id: "root".into(),
                    inventory_instance_id: "scope".into(),
                    revision: 2,
                    final_chunk: true,
                    data: data.as_bytes().to_vec(),
                    ..Default::default()
                })
            ))
            .is_err(),
            "{data}"
        );
        assert!(live(&view).contains("50% Committed"));
    }
}

#[test]
fn progress_adapts_to_width_color_unicode_and_large_counts() {
    let mut view = view();
    view.observe(&turn("root")).unwrap();
    publish(
        &mut view,
        "root",
        "scope",
        1,
        Some(Progress {
            steps: 2,
            total_steps: 5,
            current_step: Some("\u{1b}[31m测试\n🦜\twork\0".into()),
        }),
    );
    for columns in [24, 40, 80, 160, 24] {
        for color in [false, true] {
            let rows: Vec<_> = view
                .snapshot()
                .unwrap()
                .iter()
                .flat_map(|item| {
                    item.render(LiveBufferRenderContext { columns, color })
                        .unwrap()
                        .lines
                })
                .collect();
            assert_eq!(rows.len(), 1);
            let row = &rows[0];
            assert!(row.text().contains("2/5"));
            assert!(row.text().contains("40%"));
            assert!(
                parrot_cli::terminal_layout::TerminalText::width(row.text()) <= columns,
                "{}",
                row.text()
            );
            assert!(row.text().chars().all(|c| !c.is_control()));
            assert_eq!(row.style().start().is_empty(), !color);
            if columns == 160 {
                assert!(row.text().contains("测试 🦜    work"));
            }
        }
    }
    publish(
        &mut view,
        "root",
        "scope",
        2,
        Some(Progress {
            steps: u64::MAX - 1,
            total_steps: u64::MAX,
            current_step: None,
        }),
    );
    let rendered = live(&view);
    assert!(rendered.contains("18446744073709551614/18446744073709551615"));
    assert!(rendered.contains("99%"));
    publish(
        &mut view,
        "root",
        "scope",
        3,
        Some(Progress {
            steps: u64::MAX,
            total_steps: u64::MAX,
            current_step: None,
        }),
    );
    assert!(live(&view).contains("100%"));
}

#[test]
fn task_embedded_worker_keeps_exactly_one_independent_progress_bar() {
    let mut view = view();
    view.observe(&turn("root")).unwrap();
    view.observe(&child("worker", "worker", "root")).unwrap();
    view.observe(&turn("worker")).unwrap();
    let tasks = wire::AgentTaskProgressSnapshot {
        revision: 1,
        root_nodes: vec![wire::AgentTaskProgressNode {
            name: "implementation".into(),
            status: wire::AgentTaskProgressStatus::AGENT_TASK_PROGRESS_STATUS_RUNNING.into(),
            agent_session_id: "worker".into(),
            ..Default::default()
        }],
        ..Default::default()
    };
    view.observe(&event(
        "root",
        wire::event::Payload::ToolSnapshot(wire::ToolSnapshot {
            kind: "agent_task_progress".into(),
            revision: 1,
            final_chunk: true,
            data: tasks.write_to_bytes().unwrap(),
            ..Default::default()
        }),
    ))
    .unwrap();
    publish(
        &mut view,
        "worker",
        "scope",
        1,
        Some(Progress {
            steps: 1,
            total_steps: 4,
            current_step: Some("Worker report".into()),
        }),
    );
    let rendered = live(&view);
    assert!(rendered.contains("implementation"));
    assert!(rendered.contains("[worker] 1/4 ["));
    assert_eq!(rendered.matches("25% Worker report").count(), 1);
}

#[test]
fn tool_arguments_and_terminal_success_do_not_drive_the_progress_bar() {
    let mut view = RawActivityView::new(160, Arc::new(ToolPresenterRegistry::standard(Vec::new())));
    view.observe(&turn("root")).unwrap();
    view.observe(&event(
        "root",
        wire::event::Payload::ToolCallChunk(wire::ToolCallChunk {
            tool_call_id: "call".into(),
            tool_name: "progress".into(),
            arguments_fragment: r#"{"steps":3,"total_steps":5}"#.into(),
            ..Default::default()
        }),
    ))
    .unwrap();
    view.observe(&event(
        "root",
        wire::event::Payload::ToolStarted(wire::ToolStarted {
            tool_call_id: "call".into(),
            tool_name: "progress".into(),
            ..Default::default()
        }),
    ))
    .unwrap();
    assert!(!live(&view).contains("3/5"));
    assert!(!live(&view).contains('█'));
    publish(
        &mut view,
        "root",
        "scope",
        1,
        Some(Progress {
            steps: 2,
            total_steps: 5,
            current_step: Some("Committed".into()),
        }),
    );
    let finished = view
        .observe(&event(
            "root",
            wire::event::Payload::ToolFinished(wire::ToolFinished {
                tool_call_id: "call".into(),
                tool_name: "progress".into(),
                result: Some("Progress updated".into()),
                ..Default::default()
            }),
        ))
        .unwrap();
    assert!(finished.commits.is_empty());
    assert!(live(&view).contains("40% Committed"));
    assert!(!live(&view).contains("3/5"));
}
