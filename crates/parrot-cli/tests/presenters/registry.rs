use std::sync::Arc;

use parrot_cli::presenters::{tools::generic::GenericToolPresenter, *};
use parrot_cli::terminal_frames::{
    LiveBufferItem, LiveBufferRenderContext, ScrollbackItem, ScrollbackLayout,
    ScrollbackRenderContext,
};

struct Fixed {
    name: &'static str,
    fail: bool,
    sensitive: bool,
}
impl ToolPresenter for Fixed {
    fn tool_name(&self) -> &str {
        self.name
    }
    fn metadata(&self) -> ToolPresentationMetadata {
        if self.sensitive {
            ToolPresentationMetadata {
                redacted_input_fields: vec!["input".into()],
                suppress_terminal_details: true,
                ..Default::default()
            }
        } else {
            Default::default()
        }
    }
    fn present_live(&self, _: &ToolCallPresentation, _: usize) -> PresenterResult<Presentation> {
        if self.fail {
            Err(PresenterError("Use fallback.".into()))
        } else {
            Ok(Presentation::Lines {
                lines: vec!["selected".into()],
                layout: ScrollbackLayout::Compact,
            })
        }
    }
    fn present_terminal(
        &self,
        _: &ToolCallPresentation,
        _: &ToolTerminalPresentation,
    ) -> PresenterResult<Option<Presentation>> {
        if self.fail {
            Err(PresenterError("Use fallback.".into()))
        } else {
            Ok(None)
        }
    }
}
fn live(item: Presentation) -> String {
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
    .map(|l| l.text().to_owned())
    .collect::<Vec<_>>()
    .join("\n")
}
fn terminal(item: Presentation) -> String {
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
    .join("\n")
}
#[test]
fn registry_selects_ordinally_and_falls_back_for_invalid_or_unknown_presenters() {
    let registry = ToolPresenterRegistry::new(
        vec![
            Arc::new(Fixed {
                name: "read",
                fail: false,
                sensitive: false,
            }),
            Arc::new(Fixed {
                name: "broken",
                fail: true,
                sensitive: false,
            }),
        ],
        Arc::new(GenericToolPresenter),
    )
    .unwrap();
    assert_eq!(
        live(
            registry
                .present_live(&ToolCallPresentation::new("read", "{}"), 0)
                .unwrap()
        ),
        "selected"
    );
    assert!(
        live(
            registry
                .present_live(&ToolCallPresentation::new("Read", "{}"), 0)
                .unwrap()
        )
        .contains("Read")
    );
    assert!(
        live(
            registry
                .present_live(&ToolCallPresentation::new("broken", "{}"), 0)
                .unwrap()
        )
        .contains("broken")
    );
    assert!(
        terminal(
            registry
                .present_terminal(
                    &ToolCallPresentation::new("broken", "{}"),
                    &Default::default()
                )
                .unwrap()
                .unwrap()
        )
        .contains("tool call broken")
    );
    assert!(
        registry
            .present_terminal(
                &ToolCallPresentation::new("read", "{}"),
                &Default::default()
            )
            .unwrap()
            .is_none()
    );
}
#[test]
fn registry_redacts_sensitive_presentations_before_specialized_fallback() {
    let registry = ToolPresenterRegistry::new(
        vec![Arc::new(Fixed {
            name: "sensitive",
            fail: true,
            sensitive: true,
        })],
        Arc::new(GenericToolPresenter),
    )
    .unwrap();
    for (json, marker) in [
        (
            r#"{"name":"worker","input":"secret😀"}"#,
            "<redacted: 7 chars>",
        ),
        (
            r#"{"name":"build","input":"secret 🔒"}"#,
            "<redacted: 8 chars>",
        ),
        (r#"{"input":{"secret":true},"safe":42}"#, "<redacted>"),
        (r#"{"input":"secret"#, "<redacted>"),
    ] {
        let call = ToolCallPresentation::new("sensitive", json);
        let end = ToolTerminalPresentation {
            status: ToolTerminalStatus::Errored,
            result_present: true,
            result: "secret result".into(),
            error: "secret error".into(),
            ..Default::default()
        };
        let rendered = live(registry.present_live(&call, 0).unwrap())
            + &terminal(registry.present_terminal(&call, &end).unwrap().unwrap());
        assert!(rendered.contains(marker), "{rendered}");
        for secret in [
            "secret😀",
            "secret 🔒",
            "secret result",
            "secret error",
            "\"secret\":true",
        ] {
            assert!(!rendered.contains(secret), "{rendered}");
        }
    }
}
#[test]
fn unknown_calls_keep_malformed_input_and_spill_notice_through_real_generic_presenter() {
    let registry = ToolPresenterRegistry::new(vec![], Arc::new(GenericToolPresenter)).unwrap();
    let call = ToolCallPresentation::new("retired_tool", "not json");
    let notice = "Tool output exceeded 64 KiB and was saved to /tmp/output.";
    let end = ToolTerminalPresentation {
        result_present: true,
        result: notice.into(),
        ..Default::default()
    };
    let shown = terminal(registry.present_terminal(&call, &end).unwrap().unwrap());
    assert!(live(registry.present_live(&call, 0).unwrap()).contains("not json"));
    for text in ["tool call retired_tool", "not json", notice] {
        assert!(shown.contains(text));
    }
}
#[test]
fn registry_rejects_duplicate_and_empty_names_without_changing_ordinal_rules() {
    assert!(
        ToolPresenterRegistry::new(
            vec![Arc::new(Fixed {
                name: "",
                fail: false,
                sensitive: false
            })],
            Arc::new(GenericToolPresenter)
        )
        .is_err()
    );
    assert!(
        ToolPresenterRegistry::new(
            vec![
                Arc::new(Fixed {
                    name: "read",
                    fail: false,
                    sensitive: false
                }),
                Arc::new(Fixed {
                    name: "read",
                    fail: false,
                    sensitive: false
                })
            ],
            Arc::new(GenericToolPresenter)
        )
        .is_err()
    );
    assert!(
        ToolPresenterRegistry::new(
            vec![
                Arc::new(Fixed {
                    name: "Read",
                    fail: false,
                    sensitive: false
                }),
                Arc::new(Fixed {
                    name: "read",
                    fail: false,
                    sensitive: false
                })
            ],
            Arc::new(GenericToolPresenter)
        )
        .is_ok()
    );
}
#[test]
fn actual_factory_registers_every_source_presenter_once() {
    let registry = ToolPresenterRegistry::standard(vec!["git status".into()]);
    let names = registry
        .registered_names()
        .collect::<std::collections::BTreeSet<_>>();
    let expected = [
        "answer",
        "agent_send",
        "agent_spawn",
        "agent_status",
        "clear_exit_reminder",
        "edit",
        "exec_command",
        "glob",
        "imagegen",
        "interrupt_process",
        "read_image",
        "read",
        "set_agent_tasks",
        "get_agent_tasks",
        "question",
        "set_checkpoint",
        "set_exit_reminder",
        "status",
        "queue_push",
        "queue_take",
        "wait",
        "web_fetch",
        "write_stdin",
        "write",
    ]
    .into_iter()
    .collect();
    assert_eq!(names, expected);
    assert!(registry.describe("agent_spawn").terminal_only);
    assert!(registry.describe("question").modeline);
    assert_eq!(
        registry.describe("Read"),
        ToolPresentationMetadata::default()
    );
    let item = registry
        .present_live(
            &ToolCallPresentation::new("exec_command", r#"{"command":"git status"}"#),
            0,
        )
        .unwrap();
    let Presentation::Tool(report) = item else {
        panic!("real exec presenter not selected")
    };
    assert_eq!(report.metadata.style, ToolPresentationStyle::Muted);
    assert!(report.timer.is_none());
}
