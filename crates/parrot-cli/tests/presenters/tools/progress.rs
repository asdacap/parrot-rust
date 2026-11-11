use parrot_cli::presenters::{tools::progress::ProgressToolPresenter, *};

#[test]
fn progress_success_is_quiet_and_errors_remain_visible_even_with_invalid_arguments() {
    let presenter = ProgressToolPresenter;
    for arguments in [r#"{"steps":2,"total_steps":5}"#, r#"{"close":true}"#, "{"] {
        let call = ToolCallPresentation::new("progress", arguments);
        let Presentation::Tool(live) = presenter.present_live(&call, 0).unwrap() else {
            panic!("tool report")
        };
        assert_eq!(live.label, "progress");
        assert!(!live.label.contains('█'));
        assert!(
            presenter
                .present_terminal(
                    &call,
                    &ToolTerminalPresentation {
                        result_present: true,
                        result: "Progress updated".into(),
                        ..Default::default()
                    }
                )
                .unwrap()
                .is_none()
        );
        for terminal in [
            ToolTerminalPresentation {
                result_present: true,
                result: "error: invalid counts".into(),
                ..Default::default()
            },
            ToolTerminalPresentation {
                status: ToolTerminalStatus::Errored,
                error: "Persistence failed".into(),
                ..Default::default()
            },
            ToolTerminalPresentation {
                status: ToolTerminalStatus::Cancelled,
                ..Default::default()
            },
        ] {
            let Presentation::Tool(report) = presenter
                .present_terminal(&call, &terminal)
                .unwrap()
                .unwrap()
            else {
                panic!("terminal report")
            };
            assert_eq!(report.label, "progress");
            assert_eq!(report.status, Some(terminal.resolve_status()));
            if terminal.resolve_status() != ToolTerminalStatus::Cancelled {
                assert_eq!(report.block.kind, ToolBlockKind::Error);
                assert!(!report.block.text.is_empty());
            }
        }
    }
}

#[test]
fn standard_registry_uses_the_quiet_progress_presenter() {
    let registry = ToolPresenterRegistry::standard_with_skill_read_paths(
        Vec::new(),
        std::sync::Arc::new(tools::read::SkillReadPathFormatter::default()),
    );
    assert!(
        registry
            .present_terminal(
                &ToolCallPresentation::new("progress", "{}"),
                &ToolTerminalPresentation::default()
            )
            .unwrap()
            .is_none()
    );
    let failure = registry
        .present_terminal(
            &ToolCallPresentation::new("progress", "{"),
            &ToolTerminalPresentation {
                status: ToolTerminalStatus::Errored,
                error: "Invalid JSON".into(),
                ..Default::default()
            },
        )
        .unwrap()
        .unwrap();
    let Presentation::Tool(report) = failure else {
        panic!("tool report")
    };
    assert_eq!(report.block.text, "Invalid JSON");
}
