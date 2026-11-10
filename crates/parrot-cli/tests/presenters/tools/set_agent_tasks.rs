use parrot_cli::presenters::{tools::set_agent_tasks::SetAgentTasksToolPresenter, *};
use parrot_cli::terminal_frames::{ScrollbackItem, ScrollbackRenderContext};
use parrot_cli::terminal_layout::TerminalText;
use std::sync::{Arc, Mutex};

#[test]
fn successful_assignment_output_preserves_full_instructions_and_acceptance_criteria() {
    let instruction = (1..=16)
        .map(|index| format!("Instruction {index}. Read and verify the assigned work."))
        .collect::<Vec<_>>()
        .join("\n");
    let result = format!(
        "Updated 2 task(s).\nAssigned to: self\nInstruction:\n{instruction}\nAcceptance criteria: Final evidence is complete.\nLiteral text: [self] {{ready}}\nControls: safe\u{1b}"
    );
    let item = SetAgentTasksToolPresenter::default()
        .present_terminal(
            &ToolCallPresentation::new("set_agent_tasks", "private malformed input"),
            &ToolTerminalPresentation {
                result_present: true,
                result: result.clone(),
                ..Default::default()
            },
        )
        .unwrap()
        .unwrap();
    for columns in [32, 512] {
        let actual = ScrollbackItem::render(
            &item,
            ScrollbackRenderContext {
                columns,
                color: false,
                inline_diff: false,
                previous_packing: None,
            },
        )
        .unwrap();
        let sanitized = result.replace('\u{1b}', "");
        assert_eq!(actual[0], "✓ tool call set_agent_tasks");
        assert_eq!(
            actual[1..].join(" ").split_whitespace().collect::<Vec<_>>(),
            sanitized.split_whitespace().collect::<Vec<_>>()
        );
        if columns == 512 {
            assert_eq!(
                actual[1..],
                sanitized
                    .lines()
                    .map(|line| format!("  {line}"))
                    .collect::<Vec<_>>()
            );
        }
        assert!(
            actual
                .iter()
                .all(|line| TerminalText::width(line) <= columns)
        );
        assert!(actual.len() > 10);
        assert!(!actual.join("\n").contains("private"));
    }
}

#[test]
fn legacy_count_output_and_unsuccessful_outcomes_preserve_existing_rendering() {
    for terminal in [
        ToolTerminalPresentation {
            result_present: true,
            result: "Updated 2 task(s).".into(),
            ..Default::default()
        },
        ToolTerminalPresentation {
            result_present: true,
            result: "error: invalid task".into(),
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
        let call = ToolCallPresentation::new("set_agent_tasks", "private input");
        let actual = SetAgentTasksToolPresenter::default()
            .present_terminal(&call, &terminal)
            .unwrap()
            .unwrap();
        let expected = tools::generic::GenericToolPresenter
            .present_terminal(&ToolCallPresentation::new("set_agent_tasks", ""), &terminal)
            .unwrap()
            .unwrap();
        let context = ScrollbackRenderContext {
            columns: 120,
            color: false,
            inline_diff: false,
            previous_packing: None,
        };
        assert_eq!(
            ScrollbackItem::render(&actual, context.clone()).unwrap(),
            ScrollbackItem::render(&expected, context).unwrap()
        );
    }
}
struct CapturingPresenter {
    calls: Arc<Mutex<Vec<String>>>,
}
impl ToolPresenter for CapturingPresenter {
    fn tool_name(&self) -> &str {
        "capturing"
    }
    fn present_live(
        &self,
        _call: &ToolCallPresentation,
        _frame: usize,
    ) -> PresenterResult<Presentation> {
        unreachable!("not used by terminal path")
    }
    fn present_terminal(
        &self,
        call: &ToolCallPresentation,
        _terminal: &ToolTerminalPresentation,
    ) -> PresenterResult<Option<Presentation>> {
        self.calls.lock().unwrap().push(call.arguments_json.clone());
        Ok(None)
    }
}
#[test]
fn metadata_redacts_path_tasks_and_terminal_clears_arguments() {
    let calls = Arc::new(Mutex::new(Vec::new()));
    let presenter = SetAgentTasksToolPresenter::new(Arc::new(CapturingPresenter {
        calls: Arc::clone(&calls),
    }));
    assert_eq!(
        presenter.metadata().redacted_input_fields,
        vec!["path", "tasks"]
    );
    let call = ToolCallPresentation::new(
        "set_agent_tasks",
        r#"{"path":"private/tasks.json","tasks":[{"payload":"secret"}]}"#,
    );
    assert!(
        presenter
            .present_terminal(&call, &Default::default())
            .unwrap()
            .is_none()
    );
    assert_eq!(*calls.lock().unwrap(), vec![""]);
    let Presentation::Tool(report) = presenter.present_live(&call, 4).unwrap() else {
        panic!("expected live tool");
    };
    assert_eq!(report.label, "setting agent tasks");
    assert_eq!(report.block.kind, ToolBlockKind::None);
    assert_eq!(report.frame, 4);
}
#[test]
fn default_generic_renders_only_terminal_result() {
    let presenter = SetAgentTasksToolPresenter::default();
    let item = presenter
        .present_terminal(
            &ToolCallPresentation::new("set_agent_tasks", "secret malformed input"),
            &ToolTerminalPresentation {
                result_present: true,
                result: "declarations accepted".into(),
                ..Default::default()
            },
        )
        .unwrap()
        .unwrap();
    let Presentation::Tool(report) = item else {
        panic!("expected generic terminal tool");
    };
    assert_eq!(report.label, "tool call set_agent_tasks");
    assert_eq!(report.block.text, "declarations accepted");
    assert!(!report.block.text.contains("secret"));
}
