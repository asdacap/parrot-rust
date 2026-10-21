use parrot_cli::presenters::tools::interrupt_process::InterruptProcessToolPresenter;
use parrot_cli::presenters::*;
#[test]
fn interrupt_presenter_formats_signals_and_process_exit_status_without_sending_signals() {
    for (args, result, label, status) in [
        (
            r#"{"name":"build","signal":15}"#,
            "Signal 15 sent to shell process 'build'.",
            "signal 15 build",
            ToolTerminalStatus::Succeeded,
        ),
        (
            r#"{"name":"build"}"#,
            "Process exited with code 2 after 0.42s",
            "signal 2 build",
            ToolTerminalStatus::ReportedFailure,
        ),
        (
            r#"{"name":"build","signal":null}"#,
            "Signal 2 sent to shell process 'build'.",
            "signal 2 build",
            ToolTerminalStatus::Succeeded,
        ),
    ] {
        let call = ToolCallPresentation::new("interrupt_process", args);
        let Presentation::Tool(live) = InterruptProcessToolPresenter
            .present_live(&call, 0)
            .unwrap()
        else {
            panic!("tool")
        };
        assert_eq!(live.label, label);
        assert_eq!(live.block.kind, ToolBlockKind::None);
        let Presentation::Tool(report) = InterruptProcessToolPresenter
            .present_terminal(
                &call,
                &ToolTerminalPresentation {
                    result_present: true,
                    result: result.into(),
                    ..Default::default()
                },
            )
            .unwrap()
            .unwrap()
        else {
            panic!("tool")
        };
        assert_eq!(report.label, label);
        assert_eq!(report.status, Some(status));
        if status == ToolTerminalStatus::Succeeded {
            assert_eq!(report.block.kind, ToolBlockKind::None);
        } else {
            assert_eq!(report.block.text, result);
            assert_eq!(report.block.kind, ToolBlockKind::Error);
        }
    }
}
#[test]
fn interrupt_presenter_retains_source_integer_validation_not_tool_execution() {
    for args in [
        "{",
        "[]",
        "{}",
        r#"{"name":null}"#,
        r#"{"name":"build","signal":"15"}"#,
        r#"{"name":"build","signal":1.5}"#,
        r#"{"name":"build","signal":2147483648}"#,
    ] {
        assert!(
            InterruptProcessToolPresenter
                .present_live(&ToolCallPresentation::new("interrupt_process", args), 0)
                .is_err()
        );
    }
    let call = ToolCallPresentation::new("interrupt_process", r#"{"name":"build","signal":-5}"#);
    let Presentation::Tool(report) = InterruptProcessToolPresenter
        .present_live(&call, 0)
        .unwrap()
    else {
        panic!("tool")
    };
    assert_eq!(report.label, "signal -5 build");
    let Presentation::Tool(report) = InterruptProcessToolPresenter
        .present_terminal(
            &call,
            &ToolTerminalPresentation {
                status: ToolTerminalStatus::Errored,
                error: "permission denied".into(),
                ..Default::default()
            },
        )
        .unwrap()
        .unwrap()
    else {
        panic!("tool")
    };
    assert_eq!(report.block.text, "permission denied");
}
