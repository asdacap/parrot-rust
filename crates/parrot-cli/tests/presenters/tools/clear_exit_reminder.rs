use parrot_cli::presenters::tools::clear_exit_reminder::ClearExitReminderToolPresenter;
use parrot_cli::presenters::*;
#[test]
fn clear_reminder_leaves_success_to_change_notice_but_reports_failure() {
    let call = ToolCallPresentation::new("clear_exit_reminder", r#"{"title":"tests"}"#);
    let Presentation::Tool(live) = ClearExitReminderToolPresenter
        .present_live(&call, 0)
        .unwrap()
    else {
        panic!("expected tool")
    };
    assert_eq!(live.label, "Clear exit reminder · tests");
    assert_eq!(live.block.kind, ToolBlockKind::None);
    assert!(
        ClearExitReminderToolPresenter
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
    let Presentation::Tool(failed) = ClearExitReminderToolPresenter
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
        panic!("expected tool")
    };
    assert_eq!(failed.label, "Clear exit reminder · tests");
    assert_eq!(failed.status, Some(ToolTerminalStatus::ReportedFailure));
    assert_eq!(failed.block.text, "error: unavailable");
}
#[test]
fn clear_reminder_requires_string_title_only_on_rendered_paths() {
    for input in ["{", "[]", "{}", r#"{"title":null}"#, r#"{"title":5}"#] {
        let call = ToolCallPresentation::new("clear_exit_reminder", input);
        assert!(
            ClearExitReminderToolPresenter
                .present_live(&call, 0)
                .is_err()
        );
        assert!(
            ClearExitReminderToolPresenter
                .present_terminal(&call, &ToolTerminalPresentation::default())
                .unwrap()
                .is_none()
        );
        assert!(
            ClearExitReminderToolPresenter
                .present_terminal(
                    &call,
                    &ToolTerminalPresentation {
                        status: ToolTerminalStatus::Errored,
                        error: "failure".into(),
                        ..Default::default()
                    }
                )
                .is_err()
        );
    }
}
