use parrot_cli::presenters::tools::set_exit_reminder::SetExitReminderToolPresenter;
use parrot_cli::presenters::*;
#[test]
fn set_reminder_leaves_success_to_change_notice_and_renders_failures() {
    let call = ToolCallPresentation::new(
        "set_exit_reminder",
        r#"{"title":"tests","description":"run the suite"}"#,
    );
    let Presentation::Tool(live) = SetExitReminderToolPresenter.present_live(&call, 0).unwrap()
    else {
        panic!("tool")
    };
    assert_eq!(live.label, "Set exit reminder · tests · run the suite");
    assert_eq!(live.block.kind, ToolBlockKind::None);
    assert!(
        SetExitReminderToolPresenter
            .present_terminal(
                &call,
                &ToolTerminalPresentation {
                    result_present: true,
                    result: "exit reminder changed".into(),
                    ..Default::default()
                }
            )
            .unwrap()
            .is_none()
    );
    let Presentation::Tool(report) = SetExitReminderToolPresenter
        .present_terminal(
            &call,
            &ToolTerminalPresentation {
                result_present: true,
                result: "error: unavailable".into(),
                ..Default::default()
            },
        )
        .unwrap()
        .unwrap()
    else {
        panic!("tool")
    };
    assert_eq!(report.label, live.label);
    assert_eq!(report.status, Some(ToolTerminalStatus::ReportedFailure));
    assert_eq!(report.block.text, "error: unavailable");
}
#[test]
fn set_reminder_requires_string_title_description_except_suppressed_success() {
    for args in [
        "{",
        "[]",
        "{}",
        r#"{"title":"t"}"#,
        r#"{"title":3,"description":"d"}"#,
        r#"{"title":"t","description":null}"#,
    ] {
        let call = ToolCallPresentation::new("set_exit_reminder", args);
        assert!(SetExitReminderToolPresenter.present_live(&call, 0).is_err());
        assert!(
            SetExitReminderToolPresenter
                .present_terminal(&call, &ToolTerminalPresentation::default())
                .unwrap()
                .is_none()
        );
        assert!(
            SetExitReminderToolPresenter
                .present_terminal(
                    &call,
                    &ToolTerminalPresentation {
                        status: ToolTerminalStatus::Errored,
                        error: "failed".into(),
                        ..Default::default()
                    }
                )
                .is_err()
        );
    }
}
