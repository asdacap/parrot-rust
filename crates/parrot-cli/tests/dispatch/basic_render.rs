use std::io::{self, Write};
use std::sync::atomic::{AtomicBool, Ordering};

use parrot_cli::dispatch::basic_render::{
    BasicRenderer, RenderOutcome, RenderStreamError, render_turn, write_plan_report,
};
use parrot_domain::contracts::async_ops::{BoxFuture, Cancellation};
use parrot_domain::protocol::messages::parrot as wire;
use protobuf::Message;
use wire::event::Payload;

fn event(payload: Payload) -> wire::Event {
    wire::Event {
        payload: Some(payload),
        ..Default::default()
    }
}

fn text(fragment: &str) -> wire::Event {
    event(Payload::TextChunk(wire::TextChunk {
        fragment: fragment.into(),
        ..Default::default()
    }))
}

fn render(events: Vec<wire::Event>) -> (String, String, Vec<RenderOutcome>) {
    let (mut output, mut error) = (Vec::new(), Vec::new());
    let mut renderer = BasicRenderer::new();
    let outcomes = events
        .iter()
        .map(|event| {
            renderer
                .render_event(event, false, &mut output, &mut error)
                .unwrap()
        })
        .collect();
    (
        String::from_utf8(output).unwrap(),
        String::from_utf8(error).unwrap(),
        outcomes,
    )
}

#[test]
fn encrypted_reasoning_and_provider_phases_do_not_interrupt_text() {
    for agent in ["root", "child"] {
        let mut events = vec![text("before")];
        for _ in 0..2 {
            let mut encrypted = event(Payload::EncryptedReasoningReceived(
                wire::EncryptedReasoningReceivedEvent::new(),
            ));
            encrypted.agent_session_id = agent.into();
            events.push(encrypted);
        }
        for phase in 1..=3 {
            events.push(event(Payload::ProviderRequestPhaseChanged(
                wire::ProviderRequestPhaseChangedEvent {
                    phase: protobuf::EnumOrUnknown::from_i32(phase),
                    ..Default::default()
                },
            )));
        }
        events.push(event(Payload::ReasoningChunk(wire::ReasoningChunk {
            fragment: "hidden".into(),
            ..Default::default()
        })));
        events.push(text("after"));
        let (output, error, _) = render(events);
        assert_eq!(output, "beforeafter");
        assert!(error.is_empty());
    }
}

#[test]
fn tools_child_agents_and_compaction_render_plain_lines() {
    let (output, error, _) = render(vec![
        text("checking"),
        event(Payload::ToolStarted(wire::ToolStarted {
            tool_name: "read".into(),
            ..Default::default()
        })),
        event(Payload::ToolFinished(wire::ToolFinished {
            tool_name: "read".into(),
            result: Some("enhanced-only result".into()),
            ..Default::default()
        })),
        event(Payload::ToolCancelled(wire::ToolCancelled {
            tool_name: "write".into(),
            ..Default::default()
        })),
        event(Payload::ToolError(wire::ToolError {
            tool_name: "shell".into(),
            message: "denied".into(),
            ..Default::default()
        })),
        event(Payload::AgentStarted(wire::AgentStarted {
            name: "main".into(),
            ..Default::default()
        })),
        event(Payload::AgentStarted(wire::AgentStarted {
            name: "explorer".into(),
            parent_agent_session_id: "main".into(),
            ..Default::default()
        })),
        event(Payload::AgentFinished(wire::AgentFinished {
            name: "explorer".into(),
            parent_agent_session_id: "main".into(),
            elapsed_ms: Some(65_000),
            ..Default::default()
        })),
        event(Payload::AgentFailed(wire::AgentFailed {
            name: "reviewer".into(),
            parent_agent_session_id: "main".into(),
            message: "boom".into(),
            ..Default::default()
        })),
        event(Payload::AgentFinished(wire::AgentFinished {
            name: "main".into(),
            ..Default::default()
        })),
        event(Payload::AgentFailed(wire::AgentFailed {
            name: "main".into(),
            message: "root failure".into(),
            ..Default::default()
        })),
        event(Payload::CompactionStarted(wire::CompactionStarted::new())),
        event(Payload::CompactionFinished(wire::CompactionFinished::new())),
        event(Payload::CompactionFailed(wire::CompactionFailed {
            message: "boom".into(),
            ..Default::default()
        })),
    ]);
    assert_eq!(
        output,
        "checking\n  tool started: read\n  tool finished: read\n  tool cancelled: write\n  tool error: shell: denied\n  agent started: explorer\n  agent finished: explorer (1m 05s)\n  agent failed: reviewer: boom\n  compaction started\n  compaction finished\n  compaction failed: boom\n"
    );
    assert!(error.is_empty());
}

#[test]
fn omitted_child_elapsed_time_uses_the_protobuf_scalar_default() {
    let (output, error, _) = render(vec![event(Payload::AgentFinished(wire::AgentFinished {
        name: "child".into(),
        parent_agent_session_id: "main".into(),
        elapsed_ms: None,
        ..Default::default()
    }))]);
    assert_eq!(output, "  agent finished: child (0s)\n");
    assert!(error.is_empty());
}

#[test]
fn main_agent_boundary_and_empty_fragment_match_source_state() {
    let (output, _, _) = render(vec![
        text("partial"),
        event(Payload::AgentStarted(wire::AgentStarted::new())),
        text("continued\n"),
        text(""),
        event(Payload::ToolStarted(wire::ToolStarted::new())),
    ]);
    assert_eq!(output, "partial\ncontinued\n\n  tool started: \n");
}

#[test]
fn admissions_only_print_after_start_and_keep_source_line_state() {
    let admitted = || {
        event(Payload::InputAdmitted(wire::InputAdmitted {
            content: "next".into(),
            ..Default::default()
        }))
    };
    let (output, _, _) = render(vec![
        admitted(),
        event(Payload::TurnStarted(wire::TurnStarted::new())),
        text("partial"),
        admitted(),
        event(Payload::AgentNotice(wire::AgentNotice {
            notice: "notice".into(),
            ..Default::default()
        })),
    ]);
    assert_eq!(output, "partial\n  queued: next\n\n↻ notice\n");
}

#[test]
fn notices_and_skill_loaded_are_separate_unindented_lines() {
    let notices = [
        "Status prompt injected",
        "Active work reminder injected",
        "Exit reminder set: port: finish the port",
        "Exit reminder cleared: port",
        "Context reminder injected (27% context used)",
        "Final provider request prompt injected",
        "Tool availability restored prompt injected",
    ];
    for notice in notices {
        let (output, error, _) = render(vec![
            text("draft"),
            event(Payload::AgentNotice(wire::AgentNotice {
                notice: notice.into(),
                ..Default::default()
            })),
            event(Payload::AgentNotice(wire::AgentNotice::new())),
            event(Payload::SkillLoaded(wire::SkillLoadedEvent {
                path: "/skills/example/SKILL.md".into(),
                ..Default::default()
            })),
        ]);
        assert_eq!(
            output,
            format!("draft\n↻ {notice}\n↻ Skill loaded: /skills/example/SKILL.md\n")
        );
        assert!(error.is_empty());
    }
    let (output, _, _) = render(vec![
        text("a"),
        event(Payload::AgentNotice(wire::AgentNotice::new())),
        text("b"),
    ]);
    assert_eq!(output, "ab");
}

#[test]
fn completion_reports_cumulative_totals_but_only_mode_completion_ends_rendering() {
    for (reason, input, output_tokens) in [("stop", 1234, 567), ("length", 100, 1)] {
        let (output, error, outcomes) = render(vec![
            event(Payload::TurnEnded(wire::TurnEnded {
                finish_reason: reason.into(),
                input_tokens: input,
                output_tokens,
                ..Default::default()
            })),
            event(Payload::PlanCompleted(wire::PlanCompleted {
                markdown: "not live output".into(),
                ..Default::default()
            })),
            event(Payload::ModeTurnCompleted(wire::ModeTurnCompleted::new())),
        ]);
        assert_eq!(
            outcomes,
            [
                RenderOutcome::Continue,
                RenderOutcome::Continue,
                RenderOutcome::Completed
            ]
        );
        assert_eq!(
            output,
            format!("\n  turn ended ({reason}, {input} total in / {output_tokens} total out)\n")
        );
        assert!(error.is_empty());
    }
}

#[test]
fn failure_hides_provider_body_and_replay_keeps_processing_plan_and_completion() {
    let failed = event(Payload::TurnFailed(wire::TurnFailed {
        message: "request failed".into(),
        provider_response_body: "provider body".into(),
        ..Default::default()
    }));
    let (output, error, outcomes) = render(vec![failed.clone()]);
    assert!(output.is_empty());
    assert_eq!(error, "parrot: request failed\n");
    assert_eq!(outcomes, [RenderOutcome::Failed]);
    let mut renderer = BasicRenderer::new();
    let (mut output, mut error) = (Vec::new(), Vec::new());
    for event in [
        failed,
        event(Payload::PlanCompleted(wire::PlanCompleted {
            markdown: "# Replay plan".into(),
            ..Default::default()
        })),
        event(Payload::ModeTurnCompleted(wire::ModeTurnCompleted::new())),
    ] {
        assert_eq!(
            renderer
                .render_event(&event, true, &mut output, &mut error)
                .unwrap(),
            RenderOutcome::Continue
        );
    }
    assert_eq!(output, b"# Replay plan\n");
    assert_eq!(error, b"parrot: request failed\n");
}

#[derive(Default)]
struct FlushWriter {
    bytes: Vec<u8>,
    flushes: Vec<Vec<u8>>,
}
impl Write for FlushWriter {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.bytes.extend_from_slice(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        self.flushes.push(self.bytes.clone());
        Ok(())
    }
}

fn snapshot_event(hidden: bool, status: i32) -> wire::Event {
    let snapshot = wire::AgentTaskProgressSnapshot {
        root_nodes: vec![wire::AgentTaskProgressNode {
            name: "root\u{1b}[2J\t日本".into(),
            hidden,
            status: protobuf::EnumOrUnknown::from_i32(status),
            children: (1..=6)
                .filter(|status| *status != 5)
                .map(|status| wire::AgentTaskProgressNode {
                    name: status.to_string(),
                    status: protobuf::EnumOrUnknown::from_i32(status),
                    ..Default::default()
                })
                .collect(),
            ..Default::default()
        }],
        ..Default::default()
    };
    event(Payload::ToolSnapshot(wire::ToolSnapshot {
        kind: "agent_task_progress".into(),
        data: snapshot.write_to_bytes().unwrap(),
        ..Default::default()
    }))
}

#[test]
fn progress_snapshots_append_complete_trees_after_partial_text_and_flush() {
    let (mut output, mut error) = (FlushWriter::default(), Vec::new());
    let mut renderer = BasicRenderer::new();
    for event in [
        text("partial"),
        snapshot_event(false, 2),
        snapshot_event(false, 3),
    ] {
        renderer
            .render_event(&event, false, &mut output, &mut error)
            .unwrap();
    }
    let first =
        "partial\nAgent tasks:\n◐ root[2J    日本\n├── ○ 1\n├── ◐ 2\n├── ✓ 3\n├── ✗ 4\n└── ■ 6\n";
    assert_eq!(output.flushes.len(), 2);
    assert_eq!(output.flushes[0], first.as_bytes());
    assert_eq!(
        output.flushes[1],
        format!(
            "{first}{}",
            first.trim_start_matches("partial\n").replacen('◐', "✓", 1)
        )
        .as_bytes()
    );
    assert!(error.is_empty());
}

#[test]
fn hidden_snapshots_still_flush_and_declarations_take_precedence_over_tree() {
    let (mut output, mut error) = (FlushWriter::default(), Vec::new());
    BasicRenderer::new()
        .render_event(&snapshot_event(true, 2), false, &mut output, &mut error)
        .unwrap();
    assert!(output.bytes.is_empty());
    assert_eq!(output.flushes, [Vec::<u8>::new()]);
    let plan = wire::PlanCompleted {
        markdown: "Approved plan".into(),
        task_tree: Some(wire::AgentTaskProgressSnapshot {
            root_nodes: vec![wire::AgentTaskProgressNode {
                name: "fallback must not show".into(),
                ..Default::default()
            }],
            ..Default::default()
        })
        .into(),
        task_declarations: vec![wire::PlanTaskDeclaration {
            name: "hidden".into(),
            hidden: true,
            ..Default::default()
        }],
        ..Default::default()
    };
    write_plan_report(&plan, &mut output).unwrap();
    assert_eq!(output.bytes, b"Approved plan\n");
}

#[test]
fn plan_reports_markdown_then_sanitized_tree_or_markdown_alone() {
    let mut output = Vec::new();
    let plan = wire::PlanCompleted {
        markdown: "# Plan markdown".into(),
        task_tree: Some(wire::AgentTaskProgressSnapshot {
            root_nodes: vec![wire::AgentTaskProgressNode {
                name: "root\u{1b}[2J\tnode".into(),
                status: protobuf::EnumOrUnknown::from_i32(1),
                children: vec![
                    wire::AgentTaskProgressNode {
                        name: "first".into(),
                        status: protobuf::EnumOrUnknown::from_i32(1),
                        children: vec![wire::AgentTaskProgressNode {
                            name: "nested".into(),
                            status: protobuf::EnumOrUnknown::from_i32(1),
                            ..Default::default()
                        }],
                        ..Default::default()
                    },
                    wire::AgentTaskProgressNode {
                        name: "last".into(),
                        status: protobuf::EnumOrUnknown::from_i32(1),
                        ..Default::default()
                    },
                ],
                ..Default::default()
            }],
            ..Default::default()
        })
        .into(),
        ..Default::default()
    };
    write_plan_report(&plan, &mut output).unwrap();
    assert_eq!(output, "# Plan markdown\nAgent tasks:\n○ root[2J    node\n├── ○ first\n│   └── ○ nested\n└── ○ last\n".as_bytes());
    output.clear();
    write_plan_report(
        &wire::PlanCompleted {
            markdown: "# Plan".into(),
            ..Default::default()
        },
        &mut output,
    )
    .unwrap();
    assert_eq!(output, b"# Plan\n");
}

struct BrokenWriter;
impl Write for BrokenWriter {
    fn write(&mut self, _: &[u8]) -> io::Result<usize> {
        Err(io::Error::other("output failed"))
    }
    fn flush(&mut self) -> io::Result<()> {
        Err(io::Error::other("flush failed"))
    }
}

#[test]
fn malformed_task_snapshots_and_output_failures_are_not_swallowed() {
    let mut renderer = BasicRenderer::new();
    let malformed = event(Payload::ToolSnapshot(wire::ToolSnapshot {
        kind: "agent_task_progress".into(),
        data: vec![0xff],
        ..Default::default()
    }));
    assert_eq!(
        renderer
            .render_event(&malformed, false, &mut Vec::new(), &mut Vec::new())
            .unwrap_err()
            .kind(),
        io::ErrorKind::InvalidData
    );
    for event in [
        text("text"),
        event(Payload::TurnEnded(wire::TurnEnded::new())),
        event(Payload::PlanCompleted(wire::PlanCompleted::new())),
    ] {
        assert_eq!(
            renderer
                .render_event(&event, true, &mut BrokenWriter, &mut Vec::new())
                .unwrap_err()
                .to_string(),
            "output failed"
        );
    }
    assert_eq!(
        renderer
            .render_event(
                &snapshot_event(true, 2),
                false,
                &mut BrokenWriter,
                &mut Vec::new()
            )
            .unwrap_err()
            .to_string(),
        "flush failed"
    );
    assert!(
        renderer
            .render_event(
                &event(Payload::TurnFailed(wire::TurnFailed::new())),
                false,
                &mut Vec::new(),
                &mut BrokenWriter
            )
            .is_err()
    );
}

struct TestCancellation(AtomicBool);
impl Cancellation for TestCancellation {
    fn is_cancelled(&self) -> bool {
        self.0.load(Ordering::SeqCst)
    }
    fn cancelled(&self) -> BoxFuture<'_, ()> {
        Box::pin(futures_util::future::poll_fn(|_| {
            if self.is_cancelled() {
                std::task::Poll::Ready(())
            } else {
                std::task::Poll::Pending
            }
        }))
    }
}

#[tokio::test]
async fn stream_callback_observes_plan_before_mode_completion_and_stops_without_overread() {
    let cancellation = TestCancellation(AtomicBool::new(false));
    let events = vec![
        Ok(event(Payload::TurnStarted(wire::TurnStarted::new()))),
        Ok(event(Payload::TurnEnded(wire::TurnEnded::new()))),
        Ok(event(Payload::PlanCompleted(wire::PlanCompleted::new()))),
        Ok(event(Payload::ModeTurnCompleted(
            wire::ModeTurnCompleted::new(),
        ))),
        Ok(text("not consumed")),
    ];
    let mut stream = futures_util::stream::iter(events);
    let (mut output, mut error, mut observed) = (Vec::new(), Vec::new(), Vec::new());
    let result = render_turn(
        &mut stream,
        &mut output,
        &mut error,
        |event, _| {
            observed.push(event.payload.clone());
            Box::pin(async { Ok(()) })
        },
        || false,
        &cancellation,
    )
    .await
    .unwrap();
    assert!(result);
    assert_eq!(observed.len(), 4);
    assert!(matches!(observed[2], Some(Payload::PlanCompleted(_))));
    assert_eq!(
        futures_util::StreamExt::next(&mut stream)
            .await
            .unwrap()
            .unwrap(),
        text("not consumed")
    );
}

#[tokio::test]
async fn stream_cancellation_ends_only_reader_wait_while_callback_rpc_and_output_errors_propagate()
{
    let cancellation = TestCancellation(AtomicBool::new(true));
    let mut stream = futures_util::stream::pending();
    assert!(
        !render_turn(
            &mut stream,
            &mut Vec::new(),
            &mut Vec::new(),
            |_, _| Box::pin(async { panic!("callback after cancelled read") }),
            || false,
            &cancellation
        )
        .await
        .unwrap()
    );
    let cancellation = TestCancellation(AtomicBool::new(false));
    let mut stream = futures_util::stream::poll_fn(|context| {
        cancellation.0.store(true, Ordering::SeqCst);
        context.waker().wake_by_ref();
        std::task::Poll::Pending
    });
    assert!(
        !render_turn(
            &mut stream,
            &mut Vec::new(),
            &mut Vec::new(),
            |_, _| Box::pin(async { panic!("callback during cancelled read") }),
            || false,
            &cancellation,
        )
        .await
        .unwrap()
    );
    let cancellation = TestCancellation(AtomicBool::new(false));
    let mut stream = futures_util::stream::iter(vec![Ok(text("x"))]);
    assert!(matches!(
        render_turn(
            &mut stream,
            &mut Vec::new(),
            &mut Vec::new(),
            |_, _| Box::pin(async { Err(RenderStreamError::Cancelled) }),
            || false,
            &cancellation
        )
        .await,
        Err(RenderStreamError::Cancelled)
    ));
    let mut stream =
        futures_util::stream::iter(vec![Err(tonic::Status::cancelled("rpc cancelled"))]);
    assert!(
        matches!(render_turn(&mut stream, &mut Vec::new(), &mut Vec::new(), |_, _| Box::pin(async { Ok(()) }), || false, &cancellation).await, Err(RenderStreamError::Rpc(status)) if status.code() == tonic::Code::Cancelled)
    );
    let mut stream = futures_util::stream::iter(vec![Ok(text("x"))]);
    assert!(matches!(
        render_turn(
            &mut stream,
            &mut BrokenWriter,
            &mut Vec::new(),
            |_, _| Box::pin(async { Ok(()) }),
            || false,
            &cancellation
        )
        .await,
        Err(RenderStreamError::Output(_))
    ));
    let mut stream = futures_util::stream::empty();
    assert!(
        !render_turn(
            &mut stream,
            &mut Vec::new(),
            &mut Vec::new(),
            |_, _| Box::pin(async { Ok(()) }),
            || false,
            &cancellation
        )
        .await
        .unwrap()
    );
}
