use parrot_cli::presenters::{tools::file_mutation::FileMutationToolPresenter, *};
use parrot_cli::terminal_frames::{
    LiveBufferItem, LiveBufferRenderContext, ScrollbackItem, ScrollbackRenderContext,
};
fn rows(item: &Presentation) -> Vec<String> {
    ScrollbackItem::render(
        item,
        ScrollbackRenderContext {
            columns: 80,
            color: false,
            inline_diff: true,
            previous_packing: None,
        },
    )
    .unwrap()
}
#[test]
fn mutation_labels_only_path_and_does_not_show_secret_content() {
    for (name, arguments, hidden) in [
        (
            "write",
            r#"{"path":"src/file.txt","content":"secret content"}"#,
            "secret content",
        ),
        (
            "edit",
            r#"{"path":"src/file.txt","old_string":"secret search","new_string":"secret replacement","replace_all":true}"#,
            "secret search",
        ),
    ] {
        let presenter = FileMutationToolPresenter::new(name);
        let call = ToolCallPresentation::new(name, arguments);
        let live = presenter.present_live(&call, 0).unwrap();
        let text = LiveBufferItem::render(
            &live,
            LiveBufferRenderContext {
                columns: 32768,
                color: false,
            },
        )
        .unwrap();
        assert_eq!(text.lines[0].text(), format!("⠋ {name} src/file.txt"));
        assert!(!text.lines.iter().any(|line| line.text().contains(hidden)));
        let item = presenter
            .present_terminal(
                &call,
                &ToolTerminalPresentation {
                    result_present: true,
                    result: "No changes made.".into(),
                    ..Default::default()
                },
            )
            .unwrap()
            .unwrap();
        assert_eq!(rows(&item), vec![format!("✓ {name} src/file.txt")]);
    }
}
#[test]
fn unified_diff_results_and_reported_errors_preserve_source_layout() {
    for name in ["write", "edit"] {
        let presenter = FileMutationToolPresenter::new(name);
        let call = ToolCallPresentation::new(
            name,
            r#"{"path":"src/file.txt","content":"secret content"}"#,
        );
        let diff = "--- a/src/file.txt\n+++ b/src/file.txt\n@@ -1,1 +1,1 @@\n-old\n+new\n";
        let item = presenter
            .present_terminal(
                &call,
                &ToolTerminalPresentation {
                    result_present: true,
                    result: diff.into(),
                    ..Default::default()
                },
            )
            .unwrap()
            .unwrap();
        let text = rows(&item);
        assert!(text[0].contains(&format!("{name} src/file.txt")));
        assert!(text[1].contains("@@ -1,1 +1,1 @@"));
        assert!(text.join("|").contains("1 -old"));
        assert!(text.join("|").contains("1 +new"));
        assert!(!text.join("|").contains("secret content"));
        let item = presenter
            .present_terminal(
                &call,
                &ToolTerminalPresentation {
                    result_present: true,
                    result: "error: operation failed".into(),
                    ..Default::default()
                },
            )
            .unwrap()
            .unwrap();
        assert_eq!(
            rows(&item),
            vec![
                format!("✗ {name} src/file.txt"),
                "  error: operation failed".into()
            ]
        );
        for operation in [
            presenter.present_live(&ToolCallPresentation::new(name, "{"), 0),
            presenter
                .present_terminal(&ToolCallPresentation::new(name, "{"), &Default::default())
                .map(|item| item.unwrap()),
        ] {
            assert!(operation.is_err());
        }
    }
}
#[test]
fn non_diff_success_is_text_not_diff_and_empty_output_stays_compact() {
    let presenter = FileMutationToolPresenter::new("write");
    let call = ToolCallPresentation::new("write", r#"{"path":"a"}"#);
    for (result, kind) in [
        ("", ToolBlockKind::None),
        (
            "--- only one header\nnot a plus header",
            ToolBlockKind::Text,
        ),
    ] {
        let item = presenter
            .present_terminal(
                &call,
                &ToolTerminalPresentation {
                    result_present: true,
                    result: result.into(),
                    ..Default::default()
                },
            )
            .unwrap()
            .unwrap();
        let Presentation::Tool(report) = item else {
            panic!("expected tool");
        };
        assert_eq!(report.block.kind, kind);
    }
}
