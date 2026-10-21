use parrot_cli::presenters::tools::get_agent_tasks::GetAgentTasksToolPresenter;
use parrot_cli::presenters::*;
use parrot_cli::terminal_frames::{ScrollbackItem, ScrollbackRenderContext};
const LEAF: &str = r#"{"name":"task","description":"Do work","dependencies":[],"acceptance_criteria":"Work done","payload":"Run work","state":"succeeded"}"#;
fn item(args: &str, result: &str) -> PresenterResult<Option<Presentation>> {
    GetAgentTasksToolPresenter::default().present_terminal(
        &ToolCallPresentation::new("get_agent_tasks", args),
        &ToolTerminalPresentation {
            result_present: true,
            result: result.into(),
            ..Default::default()
        },
    )
}
fn render(args: &str, result: &str, width: usize) -> Vec<String> {
    ScrollbackItem::render(
        &item(args, result).unwrap().unwrap(),
        ScrollbackRenderContext {
            columns: width,
            color: false,
            inline_diff: false,
            previous_packing: None,
        },
    )
    .unwrap()
}
#[test]
fn live_labels_select_query_form() {
    for (args, label) in [
        ("{}", "Agent tasks"),
        (r#"{"name":"task"}"#, "Agent task · task"),
    ] {
        let Presentation::Tool(report) = GetAgentTasksToolPresenter::default()
            .present_live(&ToolCallPresentation::new("get_agent_tasks", args), 0)
            .unwrap()
        else {
            panic!("tool")
        };
        assert_eq!(report.label, label);
        assert_eq!(report.block.kind, ToolBlockKind::None);
    }
}
#[test]
fn summary_displays_all_explicit_state_icons_and_empty_list() {
    for (state, icon) in [
        ("pending", "○"),
        ("running", "◐"),
        ("succeeded", "✓"),
        ("failed", "✗"),
        ("canceled", "■"),
    ] {
        let result=serde_json::json!([{"name":"first","state":state,"description":"First work"},{"name":"second","state":"pending","description":"Second work"}]).to_string();
        assert_eq!(
            render("{}", &result, 32768).join("\n"),
            format!(
                "✓ Agent tasks\n  {icon} {state} · first · First work\n  ○ pending · second · Second work"
            )
        );
    }
    assert_eq!(
        render("{}", "[]", 32768).join("\n"),
        "✓ Agent tasks\n  No agent tasks."
    );
}
#[test]
fn leaf_definition_and_optional_outcomes_preserve_order_without_invented_fields() {
    let text = render(r#"{"name":"task"}"#, LEAF, 32768).join("\n");
    assert_eq!(
        text,
        "✓ Agent task · task\n  task · succeeded\n  Description: Do work\n  Acceptance criteria: Work done\n  Instruction: Run work"
    );
    for key in ["Dependencies:", "Model:", "Result:", "Failure:"] {
        assert!(!text.contains(key));
    }
    let result=LEAF.replace(r#""dependencies":[]"#,r#""dependencies":["prepare","review"]"#).replace(r#""state":"succeeded""#,r#""state":"failed","model":"medium_llm","result":"Partial work","failure":"Missing input""#);
    let lines = render(r#"{"name":"task"}"#, &result, 32768);
    let text = lines.join("\n");
    assert!(text.contains("Dependencies: prepare, review\n  Model: medium_llm"));
    assert!(
        text.contains("Result: Partial work\n  Failure: Missing input\n  Instruction: Run work")
    );
    assert_eq!(lines[0], "✓ Agent task · task");
    assert_eq!(lines[1], "  task · failed");
}
#[test]
fn nested_definitions_preserve_repeated_names_and_reject_execution_state() {
    let result = r#"{"name":"task","description":"Root","dependencies":[],"acceptance_criteria":"Done","state":"running","payload":[{"name":"repeat","description":"Child","dependencies":[],"acceptance_criteria":"Done","payload":[{"name":"repeat","description":"Grandchild","dependencies":[],"acceptance_criteria":"Done","payload":"Run"}]}]}"#;
    let text = render(r#"{"name":"task"}"#, result, 32768).join("\n");
    assert!(text.contains("Declared child definitions:"));
    assert!(text.contains("    repeat · Child"));
    assert!(text.contains("        repeat · Grandchild"));
    for value in ["pending", "unknown", "○"] {
        assert!(!text.contains(value));
    }
    assert!(text.contains("lines truncated."));
    let invalid = result.replace(
        r#""payload":"Run""#,
        r#""payload":"Run","state":"succeeded""#,
    );
    assert!(item(r#"{"name":"task"}"#, &invalid).is_err());
}
#[test]
fn unicode_multiline_controls_and_long_instructions_are_safe_and_bounded() {
    let result =
        r#"[{"name":"界😀","state":"running","description":"First line\nSecond line\u001b[2J"}]"#;
    let wide = render("{}", result, 32768).join("\n");
    assert!(wide.contains("界😀"));
    assert!(wide.contains("Second line"));
    assert!(!wide.contains('\u{1b}'));
    assert!(
        render("{}", result, 18)
            .iter()
            .all(|line| parrot_cli::terminal_layout::TerminalText::width(line) <= 18)
    );
    let long = LEAF.replace(
        r#""payload":"Run work""#,
        &format!(r#""result":"Completed","payload":"{}""#, "x".repeat(400)),
    );
    let lines = render(r#"{"name":"task"}"#, &long, 40);
    assert!(lines.join("\n").contains("Result: Completed"));
    assert!(lines.last().unwrap().contains("lines truncated."));
    assert!(lines.len() <= 10);
}
#[test]
fn invalid_arguments_and_output_return_errors_for_real_registry_fallback() {
    for (args, result) in [
        ("{}", "not json"),
        ("{}", "{}"),
        ("{}", "[{}]"),
        ("{}", r#"[{"name":"a","state":"other","description":"b"}]"#),
        ("{}", r#"[{"name":42,"state":"pending","description":"b"}]"#),
        (r#"{"name":"task"}"#, "[]"),
        ("not json", "[]"),
        (r#"{"name":null}"#, "[]"),
    ] {
        assert!(item(args, result).is_err(), "args={args},result={result}");
    }
}
#[test]
fn non_task_outcomes_preserve_generic_status_and_diagnostics() {
    for terminal in [
        ToolTerminalPresentation {
            result_present: true,
            result: "error: missing task".into(),
            ..Default::default()
        },
        ToolTerminalPresentation {
            status: ToolTerminalStatus::Cancelled,
            ..Default::default()
        },
        ToolTerminalPresentation {
            status: ToolTerminalStatus::Errored,
            error: "transport failed".into(),
            ..Default::default()
        },
        ToolTerminalPresentation::default(),
    ] {
        let Presentation::Tool(report) = GetAgentTasksToolPresenter::default()
            .present_terminal(
                &ToolCallPresentation::new("get_agent_tasks", "{}"),
                &terminal,
            )
            .unwrap()
            .unwrap()
        else {
            panic!("tool")
        };
        assert!(report.label.starts_with("tool call get_agent_tasks"));
        assert_eq!(report.status, Some(terminal.resolve_status()));
        if terminal.result_present {
            assert!(report.block.text.contains(&terminal.result));
        }
        if !terminal.error.is_empty() {
            assert!(report.block.text.contains(&terminal.error));
        }
    }
}
#[test]
fn hidden_tasks_are_omitted_but_legacy_and_explicit_visible_tasks_remain() {
    for (args, result) in [
        (
            "{}",
            r#"[{"name":"task","state":"running","description":"Work","hidden":true}]"#,
        ),
        (
            r#"{"name":"task"}"#,
            r#"{"name":"task","state":"succeeded","hidden":true}"#,
        ),
    ] {
        assert!(item(args, result).unwrap().is_none());
    }
    let result = r#"[{"name":"hidden","state":"running","description":"Hidden work","hidden":true},{"name":"legacy","state":"pending","description":"Legacy work"},{"name":"shown","state":"failed","description":"Shown work","hidden":false}]"#;
    assert_eq!(
        render("{}", result, 32768).join("\n"),
        "✓ Agent tasks\n  ○ pending · legacy · Legacy work\n  ✗ failed · shown · Shown work"
    );
    let result = r#"{"name":"task","description":"Root","dependencies":[],"acceptance_criteria":"Done","state":"running","agent_name":"communicate-with-me","payload":[{"name":"hidden child","hidden":true,"payload":[{"name":"hidden grandchild"}]}]}"#;
    let text = render(r#"{"name":"task"}"#, result, 32768).join("\n");
    assert!(text.contains("Agent: communicate-with-me"));
    for value in [
        "hidden child",
        "hidden grandchild",
        "Declared child definitions:",
    ] {
        assert!(!text.contains(value));
    }
}
