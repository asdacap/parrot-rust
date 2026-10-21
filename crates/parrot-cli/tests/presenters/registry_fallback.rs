use parrot_cli::presenters::{tools::generic::GenericToolPresenter, *};
use parrot_cli::terminal_frames::{
    LiveBufferItem, LiveBufferRenderContext, ScrollbackItem, ScrollbackRenderContext,
};

fn live(item: Presentation) -> Vec<String> {
    LiveBufferItem::render(
        &item,
        LiveBufferRenderContext {
            columns: 32768,
            color: false,
        },
    )
    .unwrap()
    .lines
    .iter()
    .map(|line| line.text().to_owned())
    .collect()
}
fn rows(item: Presentation) -> Vec<String> {
    ScrollbackItem::render(
        &item,
        ScrollbackRenderContext {
            columns: 32768,
            color: false,
            inline_diff: true,
            previous_packing: None,
        },
    )
    .unwrap()
}
fn fallback(registry: &ToolPresenterRegistry, tool: &str, args: &str, result: &str) -> Vec<String> {
    let call = ToolCallPresentation::new(tool, args);
    let terminal = ToolTerminalPresentation {
        result_present: true,
        result: result.into(),
        ..Default::default()
    };
    let actual = rows(
        registry
            .present_terminal(&call, &terminal)
            .unwrap()
            .unwrap(),
    );
    let expected = rows(
        GenericToolPresenter
            .present_terminal(&call, &terminal)
            .unwrap()
            .unwrap(),
    );
    assert_eq!(actual, expected, "{tool}: {args}; {result}");
    assert!(actual[0].starts_with(&format!("✓ tool call {tool}")));
    actual
}

#[test]
fn actual_registry_spawn_preserves_full_prompt_under_nested_activity_decoration() {
    use parrot_domain::protocol::messages::parrot as wire;
    use std::sync::Arc;
    let registry = Arc::new(ToolPresenterRegistry::standard(vec![]));
    let prompt = vec!["one two three four five"; 110].join("\n");
    let call = ToolCallPresentation::new(
        "agent_spawn",
        serde_json::json!({"name":"scout","prompt":prompt}).to_string(),
    );
    let terminal = ToolTerminalPresentation {
        result_present: true,
        result: "{}".into(),
        ..Default::default()
    };
    let context = || ScrollbackRenderContext {
        columns: 24,
        color: false,
        inline_diff: false,
        previous_packing: None,
    };
    let expected = registry
        .present_terminal(&call, &terminal)
        .unwrap()
        .unwrap()
        .render_scrollback_decorated(
            context(),
            &ActivityDecoration::describe(24, 1, Some("child"), ""),
        )
        .unwrap();
    assert_eq!(expected[0], "  ♟ [child] Started");
    assert_eq!(expected[1], "    [child] scout");
    assert!(expected.len() > 110);
    assert!(
        expected
            .iter()
            .all(|line| parrot_cli::terminal_layout::TerminalText::width(line) <= 24)
    );
    assert_eq!(expected.last().unwrap(), "    [child] five");
    assert!(!expected.join("|").contains("truncated"));

    let mut view = RawActivityView::new(registry);
    let event = |owner: &str, payload| wire::Event {
        agent_session_id: owner.into(),
        payload: Some(payload),
        ..Default::default()
    };
    view.observe(&event(
        "root",
        wire::event::Payload::TurnStarted(Default::default()),
    ))
    .unwrap();
    view.observe(&event(
        "child-session",
        wire::event::Payload::AgentStarted(wire::AgentStarted {
            name: "child".into(),
            parent_agent_session_id: "root".into(),
            ..Default::default()
        }),
    ))
    .unwrap();
    view.observe(&event(
        "child-session",
        wire::event::Payload::ToolCallChunk(wire::ToolCallChunk {
            tool_call_id: "spawn".into(),
            tool_name: "agent_spawn".into(),
            arguments_fragment: call.arguments_json,
            ..Default::default()
        }),
    ))
    .unwrap();
    let update = view
        .observe(&event(
            "child-session",
            wire::event::Payload::ToolFinished(wire::ToolFinished {
                tool_call_id: "spawn".into(),
                tool_name: "agent_spawn".into(),
                result: Some("{}".into()),
                ..Default::default()
            }),
        ))
        .unwrap();
    assert_eq!(update.commits.len(), 1);
    assert_eq!(update.commits[0].render(context()).unwrap(), expected);
}

#[test]
fn actual_registry_queue_push_malformed_sources_preserve_generic_yaml() {
    let registry = ToolPresenterRegistry::standard(vec![]);
    for args in [
        r#"{"name":"work"}"#,
        r#"{"name":"work","items":[],"source_file":"items.txt"}"#,
        r#"{"name":"work","source_file":42}"#,
    ] {
        let call = ToolCallPresentation::new("queue_push", args);
        assert_eq!(
            live(registry.present_live(&call, 0).unwrap()),
            live(GenericToolPresenter.present_live(&call, 0).unwrap())
        );
        assert!(
            fallback(&registry, "queue_push", args, "{}")
                .join("\n")
                .contains("name: \"work\"")
        );
    }
}

#[test]
fn actual_registry_queue_take_invalid_counts_and_malformed_schemas_fall_back() {
    let registry = ToolPresenterRegistry::standard(vec![]);
    for count in [
        serde_json::json!(0),
        serde_json::json!(-1),
        serde_json::json!(1.5),
        serde_json::json!("2"),
        serde_json::json!(2147483648_i64),
    ] {
        let args = serde_json::json!({"name":"work","count":count}).to_string();
        let call = ToolCallPresentation::new("queue_take", &args);
        assert_eq!(
            live(registry.present_live(&call, 0).unwrap()),
            live(GenericToolPresenter.present_live(&call, 0).unwrap())
        );
        fallback(&registry, "queue_take", &args, "{}");
    }
    for (args, result) in [
        (r#"{"name":false}"#, ""),
        (r#"{"name":"work"}"#, r#"{"name":"work"}"#),
        (
            r#"{"name":"work"}"#,
            r#"{"name":"work","size":0,"closed":false,"items":[3]}"#,
        ),
        (
            r#"{"name":"work"}"#,
            r#"{"name":"work","size":0,"closed":false,"items":[],"description":3}"#,
        ),
    ] {
        fallback(&registry, "queue_take", args, result);
    }
}

#[test]
fn actual_registry_get_agent_tasks_invalid_and_nested_execution_state_fall_back() {
    let registry = ToolPresenterRegistry::standard(vec![]);
    for (args, result) in [
        ("{}", "not json"),
        ("{}", "{}"),
        ("{}", "[{}]"),
        ("{}", r#"[{"name":"a","state":"other","description":"b"}]"#),
        ("{}", r#"[{"name":42,"state":"pending","description":"b"}]"#),
        (r#"{"name":"task"}"#, "[]"),
        ("not json", "[]"),
        (r#"{"name":null}"#, "[]"),
        (
            r#"{"name":"task"}"#,
            r#"{"name":"task","description":"Root","dependencies":[],"acceptance_criteria":"Done","state":"running","payload":[{"name":"repeat","description":"Child","dependencies":[],"acceptance_criteria":"Done","payload":[{"name":"repeat","description":"Grandchild","dependencies":[],"acceptance_criteria":"Done","payload":"Run","state":"succeeded"}]}]}"#,
        ),
    ] {
        fallback(&registry, "get_agent_tasks", args, result);
    }
}

#[test]
fn actual_registry_write_stdin_specialization_never_echoes_sensitive_input_or_results() {
    let registry = ToolPresenterRegistry::standard(vec![]);
    for (args, expected) in [
        (
            r#"{"name":"build","input":"secret 🔒","yield_after_ms":0}"#,
            "write 8 chars to build",
        ),
        (
            r#"{"name":"build","input":"secret😀"}"#,
            "write 7 chars to build",
        ),
        (
            r#"{"name":"build","input":"secret"#,
            "write input to process",
        ),
    ] {
        let call = ToolCallPresentation::new("write_stdin", args);
        let terminal = ToolTerminalPresentation {
            status: ToolTerminalStatus::Errored,
            result_present: true,
            result: "private process output".into(),
            error: "secret error".into(),
            ..Default::default()
        };
        let shown = live(registry.present_live(&call, 0).unwrap()).join("\n")
            + &rows(
                registry
                    .present_terminal(&call, &terminal)
                    .unwrap()
                    .unwrap(),
            )
            .join("\n");
        assert!(shown.contains(expected), "{shown}");
        for secret in ["secret", "private process output"] {
            assert!(!shown.contains(secret), "{shown}");
        }
    }
}
