use parrot_cli::presenters::{tools::question::QuestionToolPresenter, *};
#[test]
fn question_counts_first_prompt_and_child_option_labels() {
    let call = ToolCallPresentation::new(
        "question",
        r#"{"questions":[{"prompt":"Choose storage","options":[{"label":"Postgres"},{"label":"SQLite"}]},{"prompt":"Keep logs?"}]}"#,
    );
    assert!(QuestionToolPresenter.metadata().modeline);
    let Presentation::Tool(live) = QuestionToolPresenter.present_live(&call, 3).unwrap() else {
        panic!("expected tool");
    };
    assert_eq!(live.label, "Question · 2 items");
    assert_eq!(live.block.text, "Choose storage");
    assert_eq!(live.frame, 3);
    let Presentation::Tool(started) = QuestionToolPresenter
        .present_child_started(&call)
        .unwrap()
        .unwrap()
    else {
        panic!("expected child question");
    };
    assert_eq!(
        started.block.text,
        "Choose storage\n  - Postgres\n  - SQLite\nKeep logs?"
    );
    assert_eq!(started.metadata.style, ToolPresentationStyle::Muted);
    assert_eq!(started.metadata.success_icon, "○");
    let Presentation::Tool(terminal) = QuestionToolPresenter
        .present_terminal(
            &call,
            &ToolTerminalPresentation {
                result_present: true,
                result: "answered".into(),
                ..Default::default()
            },
        )
        .unwrap()
        .unwrap()
    else {
        panic!("expected tool");
    };
    assert_eq!(terminal.block.text, "answered");
    assert_eq!(terminal.label, "Question · 2 items");
}
#[test]
fn empty_null_prompt_error_and_invalid_schemas() {
    for (arguments, label, prompt) in [
        (r#"{"questions":[]}"#, "Question · 0 items", "Question"),
        (
            r#"{"questions":[{"prompt":null}]}"#,
            "Question · 1 item",
            "Question",
        ),
    ] {
        let Presentation::Tool(report) = QuestionToolPresenter
            .present_live(&ToolCallPresentation::new("question", arguments), 0)
            .unwrap()
        else {
            panic!("expected tool");
        };
        assert_eq!(report.label, label);
        assert_eq!(report.block.text, prompt);
    }
    let call = ToolCallPresentation::new("question", r#"{"questions":[{"prompt":"x"}]}"#);
    let Presentation::Tool(report) = QuestionToolPresenter
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
        panic!("expected tool");
    };
    assert_eq!(report.status, Some(ToolTerminalStatus::ReportedFailure));
    assert_eq!(report.block.kind, ToolBlockKind::Error);
    for arguments in [
        "{",
        "[]",
        "{}",
        r#"{"questions":null}"#,
        r#"{"questions":[{"prompt":3}]}"#,
    ] {
        assert!(
            QuestionToolPresenter
                .present_live(&ToolCallPresentation::new("question", arguments), 0)
                .is_err()
        );
    }
    assert!(
        QuestionToolPresenter
            .present_child_started(&ToolCallPresentation::new(
                "question",
                r#"{"questions":[{"prompt":"x","options":[{}]}]}"#
            ))
            .is_err()
    );
}
