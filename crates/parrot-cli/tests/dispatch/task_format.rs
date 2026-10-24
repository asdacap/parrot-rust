use parrot_cli::dispatch::task_format::{
    AgentTaskDeclarationFormatter, DurationFormatter, ProgressFormatter,
};
use parrot_cli::terminal_layout::TerminalText;
use parrot_domain::protocol::messages::parrot as wire;
use protobuf::Message;

fn declaration(name: &str, description: &str) -> wire::PlanTaskDeclaration {
    wire::PlanTaskDeclaration {
        name: name.into(),
        description: description.into(),
        acceptance_criteria: "private criteria".into(),
        model: Some("private-model".into()),
        dependencies: vec!["private-dependency".into()],
        payload: Some(wire::plan_task_declaration::Payload::Instruction(
            "private instruction".into(),
        )),
        ..Default::default()
    }
}

#[test]
fn declarations_render_only_names_descriptions_and_children_in_order() {
    let mut composite = declaration("composite", "Composite description");
    composite.payload = Some(wire::plan_task_declaration::Payload::Children(
        wire::PlanTaskDeclarationChildren {
            tasks: vec![
                declaration("child", "Child description"),
                declaration("last child", "Last child description"),
            ],
            ..Default::default()
        },
    ));
    let declarations = vec![
        declaration("first", "First description"),
        declaration("second", "Second description"),
        composite,
    ];
    assert_eq!(
        AgentTaskDeclarationFormatter::format(&declarations).join("\n"),
        "Agent tasks:\n- name: first\n  description: First description\n- name: second\n  description: Second description\n- name: composite\n  description: Composite description\n    - name: child\n      description: Child description\n    - name: last child\n      description: Last child description"
    );
}

#[test]
fn hidden_declarations_hide_their_whole_subtree_and_empty_headings_without_mutation() {
    let mut hidden = declaration("hidden", "Secret");
    hidden.hidden = true;
    hidden.payload = Some(wire::plan_task_declaration::Payload::Children(
        wire::PlanTaskDeclarationChildren {
            tasks: vec![declaration("visible child", "Must stay hidden")],
            ..Default::default()
        },
    ));
    let original = hidden.clone();
    assert!(AgentTaskDeclarationFormatter::format(std::slice::from_ref(&hidden)).is_empty());
    assert!(
        AgentTaskDeclarationFormatter::format_for_width(std::slice::from_ref(&hidden), 10)
            .is_empty()
    );
    assert_eq!(
        AgentTaskDeclarationFormatter::format(&[
            hidden.clone(),
            declaration("shown", "Shown work")
        ])
        .join("\n"),
        "Agent tasks:\n- name: shown\n  description: Shown work"
    );
    assert_eq!(hidden, original);
}

#[test]
fn declarations_sanitize_names_and_multiline_descriptions_without_block_markers() {
    let task = declaration(
        "task\u{1b}[2J\nname\tvalue",
        "first line\nsecond\tline\u{1b}[H",
    );
    assert_eq!(
        AgentTaskDeclarationFormatter::format(&[task]).join("\n"),
        "Agent tasks:\n- name: task[2J name    value\n  description:\n    first line\n    second    line[H"
    );
}

#[test]
fn declaration_scalar_inline_threshold_and_empty_physical_lines() {
    let mut tasks = vec![
        declaration("empty", ""),
        declaration("eighty", &"x".repeat(80)),
        declaration("long", &"x".repeat(81)),
        declaration("empty lines", "\n"),
    ];
    let lines = AgentTaskDeclarationFormatter::format(&tasks);
    assert_eq!(lines[2], "  description:");
    assert_eq!(lines[4], format!("  description: {}", "x".repeat(80)));
    assert_eq!(lines[6], "  description:");
    assert_eq!(lines[7], format!("    {}", "x".repeat(81)));
    assert_eq!(&lines[9..], &["  description:", "    ", "    "]);
    tasks.clear();
    assert!(AgentTaskDeclarationFormatter::format(&tasks).is_empty());
}

#[test]
fn declarations_wrap_unicode_with_hanging_indent_and_minimum_width() {
    let tasks = vec![declaration("日本語-task", "説明文 words\nsecond line")];
    let lines = AgentTaskDeclarationFormatter::format_for_width(&tasks, 16);
    assert_eq!(lines[0], "Agent tasks:");
    let compacted = lines.concat().replace(' ', "");
    assert!(compacted.contains("日本語-task"));
    assert!(lines.join("\n").contains("説明文"));
    assert!(lines.join("\n").contains("second line"));
    assert!(lines.len() > 5);
    for line in lines.iter().skip(1) {
        assert!(TerminalText::width(line) <= 16, "{line:?}");
    }
    let description = lines
        .iter()
        .position(|line| line == "  description:")
        .unwrap();
    assert!(lines[description + 1].starts_with("    "));
    assert!(lines[description + 2].starts_with("    "));
    assert!(!AgentTaskDeclarationFormatter::format_for_width(&tasks, 0).is_empty());
}

fn node(
    name: &str,
    description: &str,
    status: i32,
    children: Vec<wire::AgentTaskProgressNode>,
) -> wire::AgentTaskProgressNode {
    wire::AgentTaskProgressNode {
        name: name.into(),
        description: description.into(),
        status: protobuf::EnumOrUnknown::from_i32(status),
        children,
        ..Default::default()
    }
}

#[test]
fn progress_format_is_exact_and_sanitizes_description_fallback() {
    let snapshot = wire::AgentTaskProgressSnapshot {
        root_nodes: vec![
            node(
                "root",
                "root description",
                2,
                vec![node(
                    "child",
                    "child description",
                    1,
                    vec![node("grandchild", "日本語 説明", 3, vec![])],
                )],
            ),
            node("last root", "", 4, vec![]),
        ],
        ..Default::default()
    };
    assert_eq!(
        ProgressFormatter::format(&snapshot).join("\n"),
        "Agent tasks:\n◐ root description\n└── ○ child description\n    └── ✓ 日本語 説明\n✗ last root"
    );
    let snapshot = wire::AgentTaskProgressSnapshot {
        root_nodes: vec![
            node("safe\u{1b}[2J\tnode", "", 1, vec![]),
            node("fallback", "multi\nline", 6, vec![]),
            node("unknown", "", 99, vec![]),
        ],
        ..Default::default()
    };
    assert_eq!(
        ProgressFormatter::format(&snapshot).join("\n"),
        "Agent tasks:\n○ safe[2J    node\n■ multi line\n? unknown"
    );
}

#[test]
fn hidden_progress_branches_filter_connectors_and_never_mutate_flags() {
    let mut hidden = node(
        "hidden",
        "",
        2,
        vec![node("must remain hidden", "", 1, vec![])],
    );
    hidden.hidden = true;
    let snapshot = wire::AgentTaskProgressSnapshot {
        root_nodes: vec![
            node(
                "root",
                "",
                2,
                vec![hidden.clone(), node("kept", "", 1, vec![]), hidden.clone()],
            ),
            hidden.clone(),
        ],
        ..Default::default()
    };
    let original = snapshot.clone();
    assert_eq!(
        ProgressFormatter::format(&snapshot).join("\n"),
        "Agent tasks:\n◐ root\n└── ○ kept"
    );
    assert_eq!(snapshot, original);
    assert!(
        ProgressFormatter::format(&wire::AgentTaskProgressSnapshot {
            root_nodes: vec![hidden],
            ..Default::default()
        })
        .is_empty()
    );
}

#[test]
fn progress_try_read_uses_exact_kind_and_protobuf_not_json() {
    let snapshot = wire::AgentTaskProgressSnapshot {
        revision: 7,
        root_nodes: vec![node("root", "", 1, vec![])],
        ..Default::default()
    };
    let mut event = wire::Event {
        payload: Some(wire::event::Payload::ToolSnapshot(wire::ToolSnapshot {
            kind: ProgressFormatter::KIND.into(),
            data: snapshot.write_to_bytes().unwrap(),
            ..Default::default()
        })),
        ..Default::default()
    };
    assert_eq!(ProgressFormatter::try_read(&event).unwrap(), Some(snapshot));
    if let Some(wire::event::Payload::ToolSnapshot(tool)) = &mut event.payload {
        tool.kind = "unrelated".into();
        tool.data = vec![0xff];
    }
    assert!(ProgressFormatter::try_read(&event).unwrap().is_none());
    if let Some(wire::event::Payload::ToolSnapshot(tool)) = &mut event.payload {
        tool.kind = ProgressFormatter::KIND.into();
    }
    assert!(ProgressFormatter::try_read(&event).is_err());
    assert!(
        ProgressFormatter::try_read(&wire::Event::new())
            .unwrap()
            .is_none()
    );
}

#[test]
#[ignore = "Rust duration rounding no longer reproduces C# unchecked overflow"]
fn duration_maximum_preserves_dotnet_unchecked_overflow() {
    assert_eq!(DurationFormatter::format(i64::MAX), "0s");
}

#[test]
fn duration_rounding_stays_monotonic_at_integer_limits() {
    assert_eq!(DurationFormatter::format(i64::MIN), "0s");
    assert_eq!(
        DurationFormatter::format(i64::MAX - 1000),
        "2562047788015h 12m 55s"
    );
    assert_eq!(
        DurationFormatter::format(i64::MAX - 500),
        "2562047788015h 12m 55s"
    );
    assert_eq!(
        DurationFormatter::format(i64::MAX),
        "2562047788015h 12m 56s"
    );
}

#[test]
fn duration_rounds_clamps_and_uses_fixed_minute_hour_fields() {
    for (milliseconds, expected) in [
        (-1000, "0s"),
        (-500, "0s"),
        (0, "0s"),
        (499, "0s"),
        (500, "1s"),
        (59_499, "59s"),
        (59_500, "1m 00s"),
        (65_000, "1m 05s"),
        (3_599_500, "1h 00m 00s"),
        (3_661_000, "1h 01m 01s"),
    ] {
        assert_eq!(
            DurationFormatter::format(milliseconds),
            expected,
            "{milliseconds}"
        );
    }
}
