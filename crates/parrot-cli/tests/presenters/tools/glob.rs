use parrot_cli::presenters::tools::glob::GlobToolPresenter;
use parrot_cli::presenters::*;
#[test]
fn glob_schema_labels_omitted_path_and_reported_failure() {
    for (args, label) in [
        (
            r#"{"pattern":"**/*.cs","path":"src"}"#,
            "glob \"**/*.cs\" in src",
        ),
        (r#"{"pattern":"src/**/*.cs"}"#, "glob \"src/**/*.cs\""),
    ] {
        let call = ToolCallPresentation::new("glob", args);
        let Presentation::Tool(live) = GlobToolPresenter.present_live(&call, 0).unwrap() else {
            panic!("tool")
        };
        assert_eq!(live.label, label);
        assert_eq!(live.metadata.style, ToolPresentationStyle::Muted);
        let Presentation::Tool(failed) = GlobToolPresenter
            .present_terminal(
                &call,
                &ToolTerminalPresentation {
                    result_present: true,
                    result: "error: tool failure".into(),
                    ..Default::default()
                },
            )
            .unwrap()
            .unwrap()
        else {
            panic!("tool")
        };
        assert_eq!(failed.label, label);
        assert_eq!(failed.status, Some(ToolTerminalStatus::ReportedFailure));
        assert_eq!(failed.block.text, "error: tool failure");
    }
}
#[test]
fn glob_counts_only_nonempty_nonbracket_paths_and_omits_success_details() {
    let call = ToolCallPresentation::new("glob", r#"{"pattern":"*"}"#);
    for (result, count, unit) in [
        ("", 0, "paths"),
        ("one\n[2 paths truncated]\n \n", 1, "path"),
        ("one\ntwo", 2, "paths"),
    ] {
        let Presentation::Tool(report) = GlobToolPresenter
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
        assert_eq!(report.label, format!("glob \"*\" · {count} {unit}"));
        assert_eq!(report.block.kind, ToolBlockKind::None);
    }
    let malformed = ToolCallPresentation::new("glob", "{");
    assert!(GlobToolPresenter.present_live(&malformed, 0).is_err());
    assert!(
        GlobToolPresenter
            .present_terminal(&malformed, &ToolTerminalPresentation::default())
            .is_err()
    );
    let Presentation::Tool(report) = GlobToolPresenter
        .present_live(
            &ToolCallPresentation::new("glob", r#"{"pattern":5,"path":false}"#),
            0,
        )
        .unwrap()
    else {
        panic!("tool")
    };
    assert_eq!(report.label, "glob \"\"");
}
