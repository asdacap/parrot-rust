use parrot_cli::presenters::{tools::agent_spawn::AgentSpawnToolPresenter, *};
use parrot_cli::terminal_frames::{
    LiveBufferItem, LiveBufferRenderContext, ScrollbackItem, ScrollbackRenderContext,
};

fn rendered(
    arguments: &str,
    terminal: ToolTerminalPresentation,
    columns: usize,
    color: bool,
) -> Vec<String> {
    let item = AgentSpawnToolPresenter
        .present_terminal(
            &ToolCallPresentation::new("agent_spawn", arguments),
            &terminal,
        )
        .unwrap()
        .unwrap();
    ScrollbackItem::render(
        &item,
        ScrollbackRenderContext {
            columns,
            color,
            inline_diff: true,
            previous_packing: None,
        },
    )
    .unwrap()
}
fn succeeded() -> ToolTerminalPresentation {
    ToolTerminalPresentation {
        result_present: true,
        result: "{}".into(),
        ..Default::default()
    }
}
#[test]
fn useful_live_and_terminal_details() {
    let call = ToolCallPresentation::new(
        "agent_spawn",
        r#"{"prompt":"inspect logs","name":"scout","scope":"storage layer"}"#,
    );
    let live = AgentSpawnToolPresenter.present_live(&call, 0).unwrap();
    assert_eq!(
        LiveBufferItem::render(
            &live,
            LiveBufferRenderContext {
                columns: 512,
                color: false
            }
        )
        .unwrap()
        .lines[0]
            .text(),
        "⠋ Start agent scout"
    );
    assert_eq!(
        rendered(&call.arguments_json, succeeded(), 512, false).join("|"),
        "♟ Started scout|  scope: storage layer||  inspect logs"
    );
    let metadata = AgentSpawnToolPresenter.metadata();
    assert!(metadata.terminal_only);
    assert_eq!(metadata.success_icon, "♟");
}
#[test]
fn omits_absent_forks_and_renders_full_and_checkpoint_forks() {
    for (fork, expected) in [
        (None, None),
        (Some(""), None),
        (Some("full"), Some("fork: full")),
        (Some("before refactor"), Some("fork: before refactor")),
    ] {
        let mut arguments = serde_json::json!({"prompt":"inspect","agent":"worker"});
        if let Some(fork) = fork {
            arguments["fork"] = fork.into();
        }
        let text = rendered(&arguments.to_string(), succeeded(), 512, false).join("|");
        if let Some(expected) = expected {
            assert!(text.contains(expected), "{text}");
        } else {
            assert!(!text.contains("fork:"), "{text}");
        }
    }
}
#[test]
fn compact_metadata_preserves_prompt_paragraphs() {
    assert_eq!(rendered(r#"{"name":"scout","agent":"explorer","model":"low_llm","fork":"full","prompt":"Read only.\n\nKeep wording.\nNext line."}"#, succeeded(), 512, false).join("|"), "♟ Started scout|  explorer · low_llm · fork: full||  Read only.||  Keep wording.|  Next line.");
}
#[test]
fn keeps_full_prompt_and_wraps_without_truncation() {
    let prompt = vec!["one two three four five"; 110].join("\n");
    let rows = rendered(
        &serde_json::json!({"name":"scout","prompt":prompt}).to_string(),
        succeeded(),
        24,
        false,
    );
    assert!(rows.len() > 110);
    assert!(
        rows.iter()
            .all(|line| parrot_cli::terminal_layout::TerminalText::width(line) <= 24)
    );
    assert!(!rows.join("|").contains("truncated"));
    assert!(rows.last().unwrap().ends_with("five"));
}
#[test]
fn styles_only_name_and_metadata() {
    let rows = rendered(
        r#"{"name":"scout","agent":"explorer","prompt":"plain **prompt**"}"#,
        succeeded(),
        80,
        true,
    );
    assert_eq!(rows[0], "♟ Started \u{1b}[1mscout\u{1b}[0m");
    assert!(rows[1].contains("explorer"));
    assert!(rows[1].contains('\u{1b}'));
    assert_eq!(rows[2], "");
    assert_eq!(rows[3], "  plain **prompt**");
}
#[test]
fn reported_error_and_cancelled_input_are_structured() {
    let arguments = r#"{"name":"scout","prompt":"one\ntwo"}"#;
    let failure = rendered(
        arguments,
        ToolTerminalPresentation {
            result_present: true,
            result: "error: unavailable".into(),
            ..Default::default()
        },
        512,
        false,
    );
    assert_eq!(failure[0], "✗ Start agent scout");
    assert!(failure.join("|").contains("error: unavailable"));
    let presentation = AgentSpawnToolPresenter
        .present_terminal(
            &ToolCallPresentation::new("agent_spawn", arguments),
            &ToolTerminalPresentation {
                status: ToolTerminalStatus::Cancelled,
                ..Default::default()
            },
        )
        .unwrap()
        .unwrap();
    let Presentation::Tool(report) = presentation else {
        panic!("expected completed input");
    };
    assert_eq!(report.block.kind, ToolBlockKind::CompletedInput);
    assert_eq!(
        report.block.text,
        "name: scout\nfork: empty\nprompt: |-\n  one\n  two"
    );
    assert!(
        AgentSpawnToolPresenter
            .present_terminal(
                &ToolCallPresentation::new("agent_spawn", r#"{"prompt":"x","fork":1}"#),
                &ToolTerminalPresentation {
                    status: ToolTerminalStatus::Cancelled,
                    ..Default::default()
                }
            )
            .is_err()
    );
    for json in ["{", "[]", r#"{"name":3}"#] {
        assert!(
            AgentSpawnToolPresenter
                .present_live(&ToolCallPresentation::new("agent_spawn", json), 0)
                .is_err()
        );
    }
}
