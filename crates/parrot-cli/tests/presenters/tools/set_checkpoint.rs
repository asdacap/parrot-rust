use parrot_cli::presenters::tools::set_checkpoint::SetCheckpointToolPresenter;
use parrot_cli::presenters::*;
#[test]
fn checkpoint_useful_label_compact_success_and_reported_failure() {
    let call = ToolCallPresentation::new("set_checkpoint", r#"{"title":"before refactor"}"#);
    let Presentation::Tool(live) = SetCheckpointToolPresenter.present_live(&call, 0).unwrap()
    else {
        panic!("tool")
    };
    assert_eq!(live.label, "Set checkpoint before refactor");
    assert_eq!(live.block.kind, ToolBlockKind::None);
    for (result, status, detail) in [
        ("checkpoint set", ToolTerminalStatus::Succeeded, ""),
        (
            "error: unavailable",
            ToolTerminalStatus::ReportedFailure,
            "error: unavailable",
        ),
    ] {
        let Presentation::Tool(report) = SetCheckpointToolPresenter
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
        assert_eq!(report.label, "Set checkpoint before refactor");
        assert_eq!(report.status, Some(status));
        assert_eq!(report.block.text, detail);
    }
}
#[test]
fn checkpoint_requires_a_string_title_on_all_paths() {
    for args in ["{", "[]", "{}", r#"{"title":null}"#, r#"{"title":5}"#] {
        let call = ToolCallPresentation::new("set_checkpoint", args);
        assert!(SetCheckpointToolPresenter.present_live(&call, 0).is_err());
        assert!(
            SetCheckpointToolPresenter
                .present_terminal(&call, &ToolTerminalPresentation::default())
                .is_err()
        );
    }
}
