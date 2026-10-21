use std::collections::{BTreeMap, BTreeSet};

use parrot_cli::presenters::{
    AgentTaskDeclarationFormatter, AgentTaskProgressFormatter, LiveModelAliasIcon, TaskAgentLine,
};
use parrot_domain::protocol::messages::parrot as wire;
use protobuf::{EnumOrUnknown, Message};

fn node(name: &str, description: &str, status: i32, agent: &str) -> wire::AgentTaskProgressNode {
    wire::AgentTaskProgressNode {
        name: name.into(),
        description: description.into(),
        status: EnumOrUnknown::from_i32(status),
        agent_session_id: agent.into(),
        ..Default::default()
    }
}

fn snapshot() -> wire::AgentTaskProgressSnapshot {
    let mut child = node("child", "child description", 1, "");
    child
        .children
        .push(node("grandchild", "日本語 説明", 3, ""));
    let mut root = node("root", "root description", 2, "");
    root.children.push(child);
    wire::AgentTaskProgressSnapshot {
        root_nodes: vec![root, node("last root", "", 4, "")],
        ..Default::default()
    }
}

fn row_text(
    snapshot: &wire::AgentTaskProgressSnapshot,
    lines: Option<&BTreeMap<String, TaskAgentLine>>,
    active: Option<&BTreeSet<String>>,
) -> String {
    AgentTaskProgressFormatter::format_rows(snapshot, lines, active)
        .iter()
        .map(|row| row.text.as_str())
        .collect::<Vec<_>>()
        .join("\n")
}

#[test]
fn format_stays_byte_identical_while_rows_carry_the_same_text() {
    let snapshot = snapshot();
    let formatted = AgentTaskProgressFormatter::format(&snapshot).join("\n");
    assert_eq!(
        formatted,
        "Agent tasks:\n◐ root description\n└── ○ child description\n    └── ✓ 日本語 説明\n✗ last root"
    );
    assert_eq!(row_text(&snapshot, None, None), formatted);
    assert_eq!(
        parrot_cli::dispatch::task_format::ProgressFormatter::format(&snapshot).join("\n"),
        formatted
    );
}

#[test]
fn known_child_graphs_nest_without_changing_the_original() {
    let owner = wire::AgentTaskProgressSnapshot {
        revision: 3,
        root_nodes: vec![
            node("composite", "", 2, "composite-agent"),
            node("leaf", "", 1, "leaf-agent"),
        ],
        ..Default::default()
    };
    let trees = BTreeMap::from([
        (
            "composite-agent".into(),
            wire::AgentTaskProgressSnapshot {
                root_nodes: vec![
                    node("inner", "", 2, "inner-agent"),
                    node("after", "", 1, ""),
                ],
                ..Default::default()
            },
        ),
        (
            "inner-agent".into(),
            wire::AgentTaskProgressSnapshot {
                root_nodes: vec![node("deepest", "", 3, "")],
                ..Default::default()
            },
        ),
    ]);
    let before = owner.clone();
    let trees_before = trees.clone();
    let nested = AgentTaskProgressFormatter::nest(&owner, &trees);
    assert_eq!(
        AgentTaskProgressFormatter::format(&nested).join("\n"),
        "Agent tasks:\n◐ composite\n├── ◐ inner\n│   └── ✓ deepest\n└── ○ after\n○ leaf"
    );
    assert_eq!(nested.revision, 3);
    assert_eq!(owner, before);
    assert_eq!(trees, trees_before);
}

#[test]
fn pending_and_starting_tasks_have_no_unrelated_agent_links() {
    let snapshot = wire::AgentTaskProgressSnapshot {
        root_nodes: vec![node("pending", "", 1, ""), node("starting", "", 2, "")],
        ..Default::default()
    };
    let trees = BTreeMap::from([(
        "other-agent".into(),
        wire::AgentTaskProgressSnapshot {
            root_nodes: vec![node("unrelated", "", 0, "")],
            ..Default::default()
        },
    )]);
    let lines = BTreeMap::from([(
        "other-agent".into(),
        TaskAgentLine::new("unrelated agent line", None),
    )]);
    let active = lines.keys().cloned().collect();
    let nested = AgentTaskProgressFormatter::nest(&snapshot, &trees);
    assert!(
        AgentTaskProgressFormatter::get_embedded_agent_session_ids(&nested, &lines, &active)
            .is_empty()
    );
    assert_eq!(
        row_text(&nested, Some(&lines), Some(&active)),
        "Agent tasks:\n○ pending\n◐ starting"
    );
}

#[test]
fn rows_hang_at_description_start_for_all_four_source_cases() {
    let rows = AgentTaskProgressFormatter::format_rows(&snapshot(), None, None);
    for (index, indent) in ["", "  ", "      ", "          "].into_iter().enumerate() {
        assert_eq!(
            rows[index].hanging_indent, indent,
            "source argument {index}"
        );
    }
}

#[test]
fn display_text_sanitizes_controls_and_flattens_names() {
    let snapshot = wire::AgentTaskProgressSnapshot {
        root_nodes: vec![node("safe\u{1b}[2J\tnode", "", 1, "")],
        ..Default::default()
    };
    let rows = AgentTaskProgressFormatter::format_rows(&snapshot, None, None);
    assert_eq!(rows[1].text, "○ safe[2J    node");
    assert_eq!(rows[1].hanging_indent, "  ");
    assert_eq!(
        parrot_cli::terminal_layout::TerminalText::width(&rows[1].hanging_indent),
        2
    );
}

#[test]
fn descriptions_follow_running_agent_lines_with_source_indentation() {
    let mut root = node("root", "root description", 2, "root-agent");
    root.children = vec![
        node("first", "first description", 2, "first-agent"),
        node("unseen", "unseen description", 2, "unseen-agent"),
        node("done", "done description", 3, "done-agent"),
    ];
    let snapshot = wire::AgentTaskProgressSnapshot {
        root_nodes: vec![root],
        ..Default::default()
    };
    let lines: BTreeMap<String, TaskAgentLine> = ["root", "first", "done"]
        .into_iter()
        .map(|name| {
            (
                format!("{name}-agent"),
                TaskAgentLine::new(format!("⠋ [{name}] "), None),
            )
        })
        .collect();
    let active = lines.keys().cloned().collect();
    let rows = AgentTaskProgressFormatter::format_rows(&snapshot, Some(&lines), Some(&active));
    let actual = rows
        .iter()
        .map(|row| format!("{}:{}", row.hanging_indent.len(), row.text))
        .collect::<Vec<_>>()
        .join("\n");
    assert_eq!(
        actual,
        "0:Agent tasks:\n0:⠋ [root] root description\n4:├── ⠋ [first] first description\n6:├── ◐ unseen description\n4:└── ⠋ [done] done description"
    );
    assert_eq!(rows[1].status.unwrap().value(), 2);
    assert_eq!(rows[4].status.unwrap().value(), 3);
}

#[test]
fn hidden_branches_keep_active_paths_without_mutating_flags() {
    let mut snapshot = snapshot();
    let root = &mut snapshot.root_nodes[0];
    root.hidden = true;
    root.agent_session_id = "root-agent".into();
    root.children[0].agent_session_id = "child-agent".into();
    root.children[0].children[0].agent_session_id = "grandchild-agent".into();
    root.children
        .push(node("idle sibling", "", 0, "idle-agent"));
    let original = snapshot.clone();
    let active = BTreeSet::from(["grandchild-agent".into()]);
    let lines = BTreeMap::from([(
        "grandchild-agent".into(),
        TaskAgentLine::new("working grandchild", None),
    )]);
    assert_eq!(
        row_text(&snapshot, Some(&lines), Some(&active)),
        "Agent tasks:\n◐ root description\n└── ○ child description\n    └── working grandchild 日本語 説明\n✗ last root"
    );
    assert_eq!(snapshot, original);
    assert_eq!(
        AgentTaskProgressFormatter::get_embedded_agent_session_ids(&snapshot, &lines, &active),
        ["grandchild-agent"]
    );
    assert_eq!(
        AgentTaskProgressFormatter::get_visible_child_graph_owner_session_ids(&snapshot, &active),
        ["root-agent", "child-agent"]
    );
    assert!(
        AgentTaskProgressFormatter::get_visible_child_graph_owner_session_ids(
            &snapshot,
            &BTreeSet::new()
        )
        .is_empty()
    );
    assert_eq!(
        AgentTaskProgressFormatter::format(&snapshot).join("\n"),
        "Agent tasks:\n✗ last root"
    );
}

#[test]
fn hidden_running_nodes_require_activity_and_filtered_connectors_are_correct() {
    let mut snapshot = snapshot();
    snapshot.root_nodes[1].hidden = true;
    let root = &mut snapshot.root_nodes[0];
    root.children[0].hidden = true;
    root.children.push(node("kept", "", 1, ""));
    let mut omitted = node("omitted", "", 0, "");
    omitted.hidden = true;
    root.children.push(omitted);
    assert_eq!(
        AgentTaskProgressFormatter::format(&snapshot).join("\n"),
        "Agent tasks:\n◐ root description\n└── ○ kept"
    );
    snapshot.root_nodes[0].hidden = true;
    snapshot.root_nodes[0].agent_session_id = "worker".into();
    assert!(AgentTaskProgressFormatter::format(&snapshot).is_empty());
    assert_eq!(
        row_text(&snapshot, None, Some(&BTreeSet::from(["worker".into()]))),
        "Agent tasks:\n◐ root description"
    );
}

#[test]
fn nested_hidden_child_graphs_wait_for_child_worker_activity() {
    let owner = wire::AgentTaskProgressSnapshot {
        root_nodes: vec![node("parent", "", 1, "parent-agent")],
        ..Default::default()
    };
    let mut child = node("finished child", "", 3, "child-agent");
    child.hidden = true;
    let children = wire::AgentTaskProgressSnapshot {
        root_nodes: vec![child],
        ..Default::default()
    };
    let nested = AgentTaskProgressFormatter::nest(
        &owner,
        &BTreeMap::from([("parent-agent".into(), children.clone())]),
    );
    assert_eq!(
        AgentTaskProgressFormatter::format(&nested).join("\n"),
        "Agent tasks:\n○ parent"
    );
    assert_eq!(
        row_text(&nested, None, Some(&BTreeSet::from(["child-agent".into()]))),
        "Agent tasks:\n○ parent\n└── ✓ finished child"
    );
    assert!(children.root_nodes[0].hidden);
}

#[test]
fn task_read_delegates_real_protocol_parser_and_keeps_failures() {
    let snapshot = snapshot();
    let event = wire::Event {
        payload: Some(wire::event::Payload::ToolSnapshot(wire::ToolSnapshot {
            kind: "agent_task_progress".into(),
            data: snapshot.write_to_bytes().unwrap(),
            ..Default::default()
        })),
        ..Default::default()
    };
    assert_eq!(
        AgentTaskProgressFormatter::try_read(&event).unwrap(),
        Some(snapshot)
    );
    assert!(
        AgentTaskProgressFormatter::try_read(&wire::Event::default())
            .unwrap()
            .is_none()
    );
    let bad = wire::Event {
        payload: Some(wire::event::Payload::ToolSnapshot(wire::ToolSnapshot {
            kind: "agent_task_progress".into(),
            data: vec![0xff],
            ..Default::default()
        })),
        ..Default::default()
    };
    assert!(AgentTaskProgressFormatter::try_read(&bad).is_err());
}

#[test]
fn task_declaration_consumer_is_the_same_real_shared_helper() {
    let declarations = [wire::PlanTaskDeclaration {
        name: "same helper".into(),
        description: "no duplicate implementation".into(),
        ..Default::default()
    }];
    assert_eq!(
        AgentTaskDeclarationFormatter::format(&declarations),
        parrot_cli::dispatch::task_format::AgentTaskDeclarationFormatter::format(&declarations)
    );
}

#[test]
fn task_status_icons_include_canceled_and_unknown_values() {
    let snapshot = wire::AgentTaskProgressSnapshot {
        root_nodes: vec![node("canceled", "", 6, ""), node("unknown", "", 91, "")],
        ..Default::default()
    };
    assert_eq!(
        AgentTaskProgressFormatter::format(&snapshot),
        ["Agent tasks:", "■ canceled", "? unknown"]
    );
}

#[test]
fn nesting_cycle_is_bounded_without_mutating_source_graphs() {
    let owner = wire::AgentTaskProgressSnapshot {
        root_nodes: vec![node("owner", "", 2, "a")],
        ..Default::default()
    };
    let trees = BTreeMap::from([
        (
            "a".into(),
            wire::AgentTaskProgressSnapshot {
                root_nodes: vec![node("a-child", "", 2, "b")],
                ..Default::default()
            },
        ),
        (
            "b".into(),
            wire::AgentTaskProgressSnapshot {
                root_nodes: vec![node("b-child", "", 2, "a")],
                ..Default::default()
            },
        ),
    ]);
    let before = trees.clone();
    assert_eq!(
        AgentTaskProgressFormatter::format(&AgentTaskProgressFormatter::nest(&owner, &trees)),
        [
            "Agent tasks:",
            "◐ owner",
            "└── ◐ a-child",
            "    └── ◐ b-child"
        ]
    );
    assert_eq!(trees, before);
}

#[test]
fn embedded_icon_metadata_preserves_utf16_offsets_and_known_colors() {
    let icon = wire::TurnModelAliasIcon {
        glyph: "🦜".into(),
        color: EnumOrUnknown::new(wire::TurnModelAliasIconColor::TURN_MODEL_ALIAS_ICON_COLOR_CYAN),
        ..Default::default()
    };
    let converted = LiveModelAliasIcon::convert(Some(&icon)).unwrap().unwrap();
    let mut line = TaskAgentLine::new("⠋ [worker] 🦜 ", Some(converted.clone()));
    line.glyph_start_index = "⠋ [worker] ".encode_utf16().count();
    let mut root = node("root", "", 2, "");
    root.children
        .push(node("child", "description", 2, "worker"));
    let snapshot = wire::AgentTaskProgressSnapshot {
        root_nodes: vec![root],
        ..Default::default()
    };
    let rows = AgentTaskProgressFormatter::format_rows(
        &snapshot,
        Some(&BTreeMap::from([("worker".into(), line.clone())])),
        None,
    );
    assert_eq!(rows[2].model_alias_icon, Some(converted));
    assert_eq!(rows[2].glyph_start_index, 4 + line.glyph_start_index);
    assert_eq!(rows[2].hanging_indent, "    ");
    assert!(LiveModelAliasIcon::convert(None).unwrap().is_none());
    let invalid = wire::TurnModelAliasIcon {
        color: EnumOrUnknown::from_i32(999),
        ..Default::default()
    };
    assert_eq!(
        LiveModelAliasIcon::convert(Some(&invalid))
            .unwrap_err()
            .to_string(),
        "The model alias icon color is invalid."
    );
}

#[test]
fn hidden_activity_projection_changes_without_new_snapshot_revision() {
    for status in [2, 3] {
        let mut worker = node("hidden task", "", status, "worker");
        worker.hidden = true;
        let snapshot = wire::AgentTaskProgressSnapshot {
            revision: 1,
            root_nodes: vec![worker],
            ..Default::default()
        };
        let before = snapshot.clone();
        assert!(AgentTaskProgressFormatter::format(&snapshot).is_empty());
        let lines = BTreeMap::from([("worker".into(), TaskAgentLine::new("⠋ [worker] ", None))]);
        let live = row_text(
            &snapshot,
            Some(&lines),
            Some(&BTreeSet::from(["worker".into()])),
        );
        assert_eq!(live, "Agent tasks:\n⠋ [worker] hidden task");
        assert_eq!(live.matches("[worker]").count(), 1);
        assert!(AgentTaskProgressFormatter::format(&snapshot).is_empty());
        assert_eq!(snapshot, before);
    }
}
