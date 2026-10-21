use parrot_cli::presenters::{tools::set_agent_tasks::SetAgentTasksToolPresenter, *};
use std::sync::{Arc, Mutex};
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
