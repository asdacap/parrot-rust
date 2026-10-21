use parrot_cli::presenters::{tools::write_stdin::WriteStdinToolPresenter, *};
use parrot_cli::terminal_frames::{
    LiveBufferItem, LiveBufferRenderContext, ScrollbackItem, ScrollbackRenderContext,
};
#[test]
fn only_process_character_count_and_status_are_shown_after_redaction() {
    let presenter = WriteStdinToolPresenter;
    let metadata = presenter.metadata();
    assert_eq!(metadata.redacted_input_fields, vec!["input"]);
    assert!(metadata.suppress_terminal_details);
    let call = redact_call(
        &ToolCallPresentation::new(
            "write_stdin",
            r#"{"name":"build","input":"secret 🔒","yield_after_ms":0}"#,
        ),
        &metadata,
    );
    let terminal = redact_terminal(
        &ToolTerminalPresentation {
            result_present: true,
            result: "private process output".into(),
            ..Default::default()
        },
        &metadata,
    );
    let live = presenter.present_live(&call, 0).unwrap();
    let live_text = LiveBufferItem::render(
        &live,
        LiveBufferRenderContext {
            columns: 32768,
            color: false,
        },
    )
    .unwrap()
    .lines
    .iter()
    .map(|line| line.text().to_owned())
    .collect::<Vec<_>>()
    .join("|");
    let finished = presenter
        .present_terminal(&call, &terminal)
        .unwrap()
        .unwrap();
    let text = ScrollbackItem::render(
        &finished,
        ScrollbackRenderContext {
            columns: 32768,
            color: false,
            inline_diff: true,
            previous_packing: None,
        },
    )
    .unwrap()
    .join("|");
    assert!(live_text.contains("write 8 chars to build"));
    assert!(text.contains("write 8 chars to build"));
    for secret in ["secret 🔒", "private process output"] {
        assert!(!live_text.contains(secret));
        assert!(!text.contains(secret));
    }
}
#[test]
fn partial_arguments_are_nonthrowing_and_do_not_show_secret_errors() {
    let call = ToolCallPresentation::new("write_stdin", r#"{"name":"build","input":"secret"#);
    let live = WriteStdinToolPresenter.present_live(&call, 0).unwrap();
    let Presentation::Tool(report) = live else {
        panic!("expected tool");
    };
    assert_eq!(report.label, "write input to process");
    assert!(report.block.text.is_empty());
    let terminal = WriteStdinToolPresenter
        .present_terminal(
            &call,
            &ToolTerminalPresentation {
                status: ToolTerminalStatus::Errored,
                error: "secret error".into(),
                ..Default::default()
            },
        )
        .unwrap()
        .unwrap();
    let Presentation::Tool(report) = terminal else {
        panic!("expected tool");
    };
    assert_eq!(report.label, "write input to process");
    assert_eq!(report.block.kind, ToolBlockKind::None);
}
#[test]
fn counts_unicode_scalars_and_only_valid_nonnegative_redacted_counts() {
    for (input, expected) in [
        ("😀x", 2),
        ("<redacted: 12 chars>", 12),
        ("<redacted: +1 chars>", 20),
        ("<redacted: -1 chars>", 20),
        ("", 0),
    ] {
        let call = ToolCallPresentation::new(
            "write_stdin",
            serde_json::json!({"name":"build","input":input}).to_string(),
        );
        let Presentation::Tool(report) = WriteStdinToolPresenter.present_live(&call, 0).unwrap()
        else {
            panic!("expected tool");
        };
        assert_eq!(report.label, format!("write {expected} chars to build"));
    }
}
