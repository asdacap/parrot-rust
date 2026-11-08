use parrot_cli::presenters::{
    tools::read::{ReadToolPresenter, SkillReadPathFormatter},
    *,
};
use parrot_cli::terminal_frames::{
    LiveBufferItem, LiveBufferRenderContext, ScrollbackItem, ScrollbackRenderContext,
};
use parrot_domain::protocol::messages::parrot::{ListedSkillScope, SkillRootLocation};
use std::sync::Arc;
fn rendered(
    call: &ToolCallPresentation,
    terminal: &ToolTerminalPresentation,
    color: bool,
) -> Vec<String> {
    rendered_with(&ReadToolPresenter::default(), call, terminal, color)
}
fn rendered_with(
    presenter: &ReadToolPresenter,
    call: &ToolCallPresentation,
    terminal: &ToolTerminalPresentation,
    color: bool,
) -> Vec<String> {
    let item = presenter.present_terminal(call, terminal).unwrap().unwrap();
    ScrollbackItem::render(
        &item,
        ScrollbackRenderContext {
            columns: 32768,
            color,
            inline_diff: true,
            previous_packing: None,
        },
    )
    .unwrap()
}
#[test]
fn missing_read_is_compact_and_keeps_failure_status() {
    for (arguments, label) in [
        (
            r#"{"path":"crates/parrot-domain/tests/protocol/fixtures/README.md"}"#,
            "read crates/parrot-domain/tests/protocol/fixtures/README.md",
        ),
        (
            r#"{"path":"missing-directory","offset":12,"limit":3}"#,
            "read L12-L14 missing-directory",
        ),
    ] {
        let call = ToolCallPresentation::new("read", arguments);
        let terminal = ToolTerminalPresentation {
            result_present: true,
            result: "error: no such file or directory".into(),
            ..Default::default()
        };
        assert_eq!(
            rendered(&call, &terminal, false),
            vec![format!("✗ {label} · not found")]
        );
        let Presentation::Tool(report) = ReadToolPresenter::default()
            .present_terminal(&call, &terminal)
            .unwrap()
            .unwrap()
        else {
            panic!("tool")
        };
        assert_eq!(report.status, Some(ToolTerminalStatus::ReportedFailure));
        assert!(report.block.text.is_empty());
    }
}
#[test]
fn other_read_failures_show_full_request_yaml_not_raw_json() {
    let call = ToolCallPresentation::new("read", r#"{"path":"src/App.cs","offset":12,"limit":3}"#);
    for terminal in [
        ToolTerminalPresentation {
            result_present: true,
            result: "error: permission denied".into(),
            ..Default::default()
        },
        ToolTerminalPresentation {
            result_present: true,
            result: "error: no such file or directory\nadditional details".into(),
            ..Default::default()
        },
        ToolTerminalPresentation {
            status: ToolTerminalStatus::Errored,
            error: "unexpected failure".into(),
            ..Default::default()
        },
    ] {
        let expected = if terminal.result_present {
            &terminal.result
        } else {
            &terminal.error
        };
        let expected = expected.replace('\n', "\n  ");
        assert_eq!(
            rendered(&call, &terminal, false).join("\n"),
            format!(
                "✗ read L12-L14 src/App.cs\n  \"path\": \"src/App.cs\"\n  \"offset\": 12\n  \"limit\": 3\n  ---\n  {expected}"
            )
        );
        let live = ReadToolPresenter::default().present_live(&call, 0).unwrap();
        assert_eq!(
            LiveBufferItem::render(
                &live,
                LiveBufferRenderContext {
                    columns: 32768,
                    color: false
                }
            )
            .unwrap()
            .lines[0]
                .text(),
            "⠋ read L12-L14 src/App.cs"
        );
    }
}
#[test]
fn non_failed_read_is_compact_and_muted() {
    let call = ToolCallPresentation::new("read", r#"{"path":"README.md","offset":2,"limit":4}"#);
    assert_eq!(
        rendered(
            &call,
            &ToolTerminalPresentation {
                result_present: true,
                result: "1: heading\n2: body".into(),
                ..Default::default()
            },
            false
        ),
        vec!["✓ read L2-L5 README.md"]
    );
    assert_eq!(
        rendered(
            &call,
            &ToolTerminalPresentation {
                status: ToolTerminalStatus::Cancelled,
                ..Default::default()
            },
            false
        ),
        vec!["■ read L2-L5 README.md"]
    );
    assert_eq!(
        ReadToolPresenter::default().metadata().style,
        ToolPresentationStyle::Muted
    );
    let colored = rendered(&call, &Default::default(), true);
    assert!(colored[0].contains('\u{1b}'));
}
#[test]
fn read_range_labels_use_inclusive_bounds_and_tool_defaults() {
    for (arguments, label) in [
        (
            r#"{"path":"README.md","offset":12,"limit":3}"#,
            "read L12-L14 README.md",
        ),
        (
            r#"{"path":"README.md","offset":12}"#,
            "read L12-L2011 README.md",
        ),
        (r#"{"path":"README.md","limit":3}"#, "read L1-L3 README.md"),
        (
            r#"{"path":"README.md","offset":12,"limit":1}"#,
            "read L12-L12 README.md",
        ),
        (
            r#"{"path":"README.md","offset":2147483647,"limit":2000}"#,
            "read L2147483647-L2147485646 README.md",
        ),
        (r#"{"path":"src"}"#, "read src"),
        (r#"{"path":"src","offset":null,"limit":null}"#, "read src"),
    ] {
        let call = ToolCallPresentation::new("read", arguments);
        let Presentation::Tool(live) = ReadToolPresenter::default().present_live(&call, 0).unwrap()
        else {
            panic!("tool")
        };
        assert_eq!(live.label, label);
        assert!(live.block.text.is_empty());
        let Presentation::Tool(terminal) = ReadToolPresenter::default()
            .present_terminal(&call, &Default::default())
            .unwrap()
            .unwrap()
        else {
            panic!("tool")
        };
        assert_eq!(terminal.label, label);
        assert!(terminal.block.text.is_empty());
    }
}
#[test]
fn skill_reads_alias_every_scope_instructions_supporting_files_and_directories() {
    for (scope, alias) in [
        (ListedSkillScope::SKILL_SCOPE_USER, "$USER_SKILLS"),
        (ListedSkillScope::SKILL_SCOPE_REPO, "$REPO_SKILLS"),
        (ListedSkillScope::SKILL_SCOPE_SYSTEM, "$SYSTEM_SKILLS"),
    ] {
        let presenter = ReadToolPresenter::new(Arc::new(SkillReadPathFormatter::new(
            "/unrelated/client/workspace".into(),
            vec![SkillRootLocation {
                path: "/server/skills".into(),
                scope: scope.into(),
                ..Default::default()
            }],
        )));
        for (path, suffix) in [
            ("/server/skills/solidifier/SKILL.md", "/solidifier/SKILL.md"),
            (
                "/server/skills/solidifier/references/example.md",
                "/solidifier/references/example.md",
            ),
            ("/server/skills/整理/参照.md", "/整理/参照.md"),
            ("/server/skills/solidifier", "/solidifier"),
            ("/server/skills", ""),
        ] {
            let call = ToolCallPresentation::new(
                "read",
                serde_json::json!({"path":path, "offset":57, "limit":30}).to_string(),
            );
            let expected = format!("read L57-L86 {alias}{suffix}");
            let Presentation::Tool(live) = presenter.present_live(&call, 0).unwrap() else {
                panic!("tool")
            };
            let Presentation::Tool(terminal) = presenter
                .present_terminal(&call, &Default::default())
                .unwrap()
                .unwrap()
            else {
                panic!("tool")
            };
            for report in [live, terminal] {
                assert_eq!(report.label, expected);
                assert_eq!(report.metadata.style, ToolPresentationStyle::Default);
                assert!(report.block.text.is_empty());
            }
            let no_range =
                ToolCallPresentation::new("read", serde_json::json!({"path":path}).to_string());
            assert_eq!(
                rendered_with(&presenter, &no_range, &Default::default(), false),
                [format!("✓ read {alias}{suffix}")]
            );
        }
    }
}

#[test]
fn skill_read_matching_is_lexical_component_safe_and_stable_on_ties() {
    let presenter = ReadToolPresenter::new(Arc::new(SkillReadPathFormatter::new(
        "/server/work".into(),
        [
            ("/server/skills", ListedSkillScope::SKILL_SCOPE_USER as i32),
            (
                "/server/skills/nested",
                ListedSkillScope::SKILL_SCOPE_REPO as i32,
            ),
            (
                "/server/skills/nested/",
                ListedSkillScope::SKILL_SCOPE_SYSTEM as i32,
            ),
            ("/server/skills/nested/deep", 99),
            ("/unknown", 99),
            (
                "/unspecified",
                ListedSkillScope::SKILL_SCOPE_UNSPECIFIED as i32,
            ),
            ("", ListedSkillScope::SKILL_SCOPE_USER as i32),
            ("relative", ListedSkillScope::SKILL_SCOPE_USER as i32),
        ]
        .into_iter()
        .map(|(path, scope)| SkillRootLocation {
            path: path.into(),
            scope: protobuf::EnumOrUnknown::from_i32(scope),
            ..Default::default()
        })
        .collect(),
    )));
    for (path, display, matched) in [
        (
            "/server/skills/nested/SKILL.md",
            "$REPO_SKILLS/SKILL.md",
            true,
        ),
        ("../skills/nested/SKILL.md", "$REPO_SKILLS/SKILL.md", true),
        (
            "/server/skills/./nested/deep/参照.md",
            "$REPO_SKILLS/deep/参照.md",
            true,
        ),
        (
            "/server/skills/nestedness/file",
            "$USER_SKILLS/nestedness/file",
            true,
        ),
        (
            "/server/skills-extra/SKILL.md",
            "/server/skills-extra/SKILL.md",
            false,
        ),
        (
            "/server/skills/../../outside/SKILL.md",
            "/server/skills/../../outside/SKILL.md",
            false,
        ),
        ("/SERVER/skills/SKILL.md", "/SERVER/skills/SKILL.md", false),
        ("/unknown/SKILL.md", "/unknown/SKILL.md", false),
        ("/unspecified/SKILL.md", "/unspecified/SKILL.md", false),
        ("relative/SKILL.md", "relative/SKILL.md", false),
        ("ordinary.md", "ordinary.md", false),
        ("", "", false),
    ] {
        let call = ToolCallPresentation::new("read", serde_json::json!({"path":path}).to_string());
        let Presentation::Tool(live) = presenter.present_live(&call, 0).unwrap() else {
            panic!("tool")
        };
        let Presentation::Tool(terminal) = presenter
            .present_terminal(&call, &Default::default())
            .unwrap()
            .unwrap()
        else {
            panic!("tool")
        };
        for report in [live, terminal] {
            assert_eq!(report.label, format!("read {display}"));
            assert_eq!(
                report.metadata.style,
                if matched {
                    ToolPresentationStyle::Default
                } else {
                    ToolPresentationStyle::Muted
                }
            );
        }
    }
}

#[test]
fn skill_reads_use_normal_rendered_styles_and_preserve_original_failure_details() {
    let path = "/server/home/.agents/skills/solidifier/SKILL.md";
    let arguments = format!(r#"{{"path":"{path}","offset":57,"limit":30}}"#);
    let call = ToolCallPresentation::new("read", &arguments);
    let presenter = ReadToolPresenter::new(Arc::new(SkillReadPathFormatter::new(
        "/server/work".into(),
        vec![SkillRootLocation {
            path: "/server/home/.agents/skills".into(),
            scope: ListedSkillScope::SKILL_SCOPE_USER.into(),
            ..Default::default()
        }],
    )));
    let label = "read L57-L86 $USER_SKILLS/solidifier/SKILL.md";
    let live = presenter.present_live(&call, 0).unwrap();
    for color in [false, true] {
        let rendered = LiveBufferItem::render(
            &live,
            LiveBufferRenderContext {
                columns: 32768,
                color,
            },
        )
        .unwrap();
        assert_eq!(rendered.lines[0].text(), format!("⠋ {label}"));
        assert_eq!(
            rendered.lines[0].style().start(),
            if color { "\x1b[48;5;236m\x1b[36m" } else { "" }
        );
    }
    assert_eq!(
        rendered_with(&presenter, &call, &Default::default(), true),
        [format!("\x1b[32m✓ {label}\x1b[0m")]
    );
    assert_eq!(
        rendered_with(&presenter, &call, &Default::default(), false),
        [format!("✓ {label}")]
    );
    for terminal in [
        ToolTerminalPresentation {
            result_present: true,
            result: "error: denied".into(),
            ..Default::default()
        },
        ToolTerminalPresentation {
            status: ToolTerminalStatus::Errored,
            error: "error: denied".into(),
            ..Default::default()
        },
    ] {
        assert_eq!(
            rendered_with(&presenter, &call, &terminal, false).join("\n"),
            format!(
                "✗ {label}\n  \"path\": \"{path}\"\n  \"offset\": 57\n  \"limit\": 30\n  ---\n  error: denied"
            )
        );
    }
    assert_eq!(
        rendered_with(
            &presenter,
            &call,
            &ToolTerminalPresentation {
                result_present: true,
                result: "error: no such file or directory".into(),
                ..Default::default()
            },
            false
        ),
        [format!("✗ {label} · not found")]
    );
    assert_eq!(call.arguments_json, arguments);
    assert_eq!(
        rendered_with(
            &presenter,
            &call,
            &ToolTerminalPresentation {
                status: ToolTerminalStatus::Cancelled,
                ..Default::default()
            },
            false
        ),
        [format!("■ {label}")]
    );
}

#[test]
fn registry_injects_session_skill_paths_and_replacement_clears_stale_aliases() {
    let skill_paths = Arc::new(SkillReadPathFormatter::new(
        "/server/work".into(),
        vec![SkillRootLocation {
            path: "/nonexistent/skills".into(),
            scope: ListedSkillScope::SKILL_SCOPE_USER.into(),
            ..Default::default()
        }],
    ));
    let registry =
        ToolPresenterRegistry::standard_with_skill_read_paths(Vec::new(), skill_paths.clone());
    let call = ToolCallPresentation::new(
        "read",
        r#"{"path":"/nonexistent/skills/solidifier/SKILL.md"}"#,
    );
    let Presentation::Tool(report) = registry.present_live(&call, 0).unwrap() else {
        panic!("tool")
    };
    assert_eq!(report.label, "read $USER_SKILLS/solidifier/SKILL.md");
    assert_eq!(report.metadata.style, ToolPresentationStyle::Default);
    skill_paths.replace_root_locations(Vec::new());
    let Presentation::Tool(report) = registry.present_live(&call, 0).unwrap() else {
        panic!("tool")
    };
    assert_eq!(report.label, "read /nonexistent/skills/solidifier/SKILL.md");
    assert_eq!(report.metadata.style, ToolPresentationStyle::Muted);
    let unknown = ToolCallPresentation::new("read", r#"{"path":"ordinary.md"}"#);
    let Presentation::Tool(report) = registry.present_live(&unknown, 0).unwrap() else {
        panic!("tool")
    };
    assert_eq!(report.label, "read ordinary.md");
    assert_eq!(report.metadata.style, ToolPresentationStyle::Muted);
}

#[test]
fn malformed_arguments_return_errors() {
    let call = ToolCallPresentation::new("read", "{");
    assert!(ReadToolPresenter::default().present_live(&call, 0).is_err());
    assert!(
        ReadToolPresenter::default()
            .present_terminal(&call, &Default::default())
            .is_err()
    );
}
