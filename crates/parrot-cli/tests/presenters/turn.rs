//! Source runtime assertions against actual presenters, not a replacement stream driver.

#[path = "turn_termimad_live.rs"]
mod termimad_live;

use std::sync::Arc;

use parrot_cli::presenters::{EnhancedSessionPresenter, EnhancedTurnRenderer};
use parrot_cli::terminal_frames::{
    LiveBufferItem, LiveBufferRenderContext, ScrollbackItem, ScrollbackRenderContext,
};
use parrot_domain::protocol::messages::parrot::{self as wire, event::Payload};
use protobuf::Message;

pub(crate) fn event(owner: &str, payload: Payload) -> wire::Event {
    wire::Event {
        agent_session_id: owner.into(),
        payload: Some(payload),
        ..Default::default()
    }
}
pub(crate) fn start(owner: &str) -> wire::Event {
    event(owner, Payload::TurnStarted(wire::TurnStarted::default()))
}
pub(crate) fn text(owner: &str, value: &str) -> wire::Event {
    event(
        owner,
        Payload::TextChunk(wire::TextChunk {
            fragment: value.into(),
            ..Default::default()
        }),
    )
}
pub(crate) fn child(root: &str) -> wire::Event {
    event(
        "child",
        Payload::AgentStarted(wire::AgentStarted {
            parent_agent_session_id: root.into(),
            name: "worker".into(),
            ..Default::default()
        }),
    )
}
pub(crate) fn ended(owner: &str) -> wire::Event {
    event(owner, Payload::TurnEnded(wire::TurnEnded::default()))
}
pub(crate) fn completed(owner: &str) -> wire::Event {
    event(
        owner,
        Payload::ModeTurnCompleted(wire::ModeTurnCompleted::default()),
    )
}
pub(crate) fn commits(items: &[Arc<dyn ScrollbackItem>], columns: usize) -> String {
    items
        .iter()
        .flat_map(|item| {
            item.render(ScrollbackRenderContext {
                columns,
                color: false,
                inline_diff: true,
                previous_packing: None,
            })
            .unwrap()
        })
        .fold(String::new(), |mut output, line| {
            output.push_str(&line);
            output.push_str("\r\n");
            output
        })
}
pub(crate) fn live(items: &[Arc<dyn LiveBufferItem>], columns: usize) -> String {
    items
        .iter()
        .flat_map(|item| {
            item.render(LiveBufferRenderContext {
                columns,
                color: false,
            })
            .unwrap()
            .lines
        })
        .fold(String::new(), |mut output, line| {
            output.push_str(line.text());
            output.push('\n');
            output
        })
}

#[test]
fn resize_retains_the_same_live_response_and_commits_at_the_new_width() {
    let mut renderer = EnhancedTurnRenderer::new(80, false);
    renderer.observe(&start("root"), false).unwrap();
    let pending = renderer
        .observe(&text("root", "abcdefghij"), false)
        .unwrap();
    assert!(pending.commits.is_empty());
    assert_eq!(live(&pending.live, 80), "● abcdefghij\n");

    renderer.resize(8);
    assert_eq!(live(&renderer.snapshot(), 8), "● abcdef\n");
    let appended = renderer.observe(&text("root", "klmn"), false).unwrap();
    assert!(appended.commits.is_empty());
    assert_eq!(appended.completion, None);
    let committed = renderer.observe(&completed("root"), false).unwrap();
    assert_eq!(committed.completion, Some(true));
    // Render the real committed item at a wide frame: its narrow rows must come
    // from resizing the still-pending Markdown accumulator, not frame clipping.
    assert_eq!(
        commits(&committed.commits, 80),
        "● abcdef\r\n  ghijkl\r\n  mn\r\n"
    );
    assert!(committed.live.is_empty());
    assert!(renderer.finish().unwrap().commits.is_empty());
}

#[test]
fn typed_turn_events_render_cumulative_text_and_sanitize_terminal_content() {
    let mut renderer = EnhancedTurnRenderer::new(8, false);
    let mut events = vec![
        event(
            "",
            Payload::InputAdmitted(wire::InputAdmitted {
                content: "already visible".into(),
                ..Default::default()
            }),
        ),
        start(""),
    ];
    for (kind, notice) in [
        ("status", "Status prompt injected"),
        ("active_work_reminder", "Active work reminder injected"),
        (
            "context_reminder",
            "Context reminder injected (27% context used)",
        ),
        (
            "exit_reminder_changed",
            "Exit reminder set: port\x1b[2J: finish the port\x1b[2J",
        ),
        ("exit_reminder_changed", "Exit reminder cleared: port"),
        (
            "final_provider_request_prompt",
            "Final provider request prompt injected",
        ),
        (
            "tool_availability_restored_prompt",
            "Tool availability restored prompt injected",
        ),
    ] {
        events.push(event(
            "",
            Payload::AgentNotice(wire::AgentNotice {
                kind: kind.into(),
                notice: notice.into(),
                ..Default::default()
            }),
        ));
    }
    events.extend([
        event(
            "",
            Payload::SkillLoaded(wire::SkillLoadedEvent {
                path: "/skills/example\x1b[2J/SKILL.md".into(),
                ..Default::default()
            }),
        ),
        event(
            "",
            Payload::InputAdmitted(wire::InputAdmitted {
                content: "next\x1b[2J\tline".into(),
                ..Default::default()
            }),
        ),
        event(
            "",
            Payload::ReasoningChunk(wire::ReasoningChunk {
                fragment: "think".into(),
                ..Default::default()
            }),
        ),
        event(
            "",
            Payload::ReasoningChunk(wire::ReasoningChunk {
                fragment: "\x1b]0;title\n".into(),
                ..Default::default()
            }),
        ),
        text("", "abcdefgh"),
        text("", "ij"),
        event(
            "",
            Payload::ToolCallChunk(wire::ToolCallChunk {
                tool_name: "unrendered".into(),
                ..Default::default()
            }),
        ),
        event(
            "",
            Payload::ToolStarted(wire::ToolStarted {
                tool_call_id: "call-1".into(),
                tool_name: "shell\x1b[31m".into(),
                ..Default::default()
            }),
        ),
        event(
            "",
            Payload::ToolFinished(wire::ToolFinished {
                tool_call_id: "call-1".into(),
                tool_name: "shell\x1b[31m".into(),
                ..Default::default()
            }),
        ),
        event(
            "",
            Payload::ToolCancelled(wire::ToolCancelled {
                tool_call_id: "call-2".into(),
                tool_name: "read\x1b[2J".into(),
                ..Default::default()
            }),
        ),
        event(
            "",
            Payload::ToolError(wire::ToolError {
                tool_call_id: "call-3".into(),
                tool_name: "write\x1b[31m".into(),
                message: "denied\x1b[2J".into(),
                ..Default::default()
            }),
        ),
        event(
            "",
            Payload::AgentStarted(wire::AgentStarted {
                name: "explorer\x1b[31m".into(),
                ..Default::default()
            }),
        ),
        event(
            "",
            Payload::AgentFinished(wire::AgentFinished {
                name: "explorer\x1b[31m".into(),
                elapsed_ms: Some(7000),
                ..Default::default()
            }),
        ),
        event(
            "",
            Payload::AgentFailed(wire::AgentFailed {
                name: "reviewer\x1b[31m".into(),
                message: "boom\x1b[2J".into(),
                ..Default::default()
            }),
        ),
        event(
            "",
            Payload::CompactionStarted(wire::CompactionStarted::default()),
        ),
        event(
            "",
            Payload::CompactionFinished(wire::CompactionFinished::default()),
        ),
        event(
            "",
            Payload::CompactionFailed(wire::CompactionFailed {
                message: "boom\x1b[2J".into(),
                ..Default::default()
            }),
        ),
        text("", "tail"),
        ended(""),
        completed(""),
    ]);
    let mut output = String::new();
    let mut errors = Vec::new();
    let mut completion = None;
    for published in &events {
        let update = renderer.observe(published, false).unwrap();
        output.push_str(&commits(&update.commits, 8));
        errors.extend(update.errors);
        completion = update.completion.or(completion);
    }
    assert_eq!(completion, Some(true));
    assert!(errors.is_empty());
    for notice in [
        "Status prompt injected",
        "Active work reminder injected",
        "Context reminder injected (27% context used)",
        "Exit reminder set: port[2J: finish the port[2J",
        "Exit reminder cleared: port",
        "Final provider request prompt injected",
        "Tool availability restored prompt injected",
        "Skill loaded: /skills/example[2J/SKILL.md",
    ] {
        assert!(output.contains(&format!("↻ {notice}")), "{output:?}");
        assert!(!output.contains(&format!("  ↻ {notice}")));
    }
    assert!(!output.contains("think]0;title"));
    assert!(output.contains("● abcdef\r\n  ghij\r\n"), "{output:?}");
    assert!(output.contains("● tail\r\n"));
    assert_eq!(output.matches("● abcdef\r\n").count(), 1);
    let untrusted = output.replace("\x1b[2m", "").replace("\x1b[0m", "");
    assert!(!untrusted.contains('\x1b'));
    assert!(!output.contains("\x1b[?1049"));
}

#[test]
fn split_markdown_chunks_are_promoted_to_scrollback_once() {
    let mut renderer = EnhancedTurnRenderer::new(80, false);
    let mut output = String::new();
    let mut completion = None;
    for published in [
        start(""),
        text("", "# Head"),
        text("", "ing\n**bo"),
        text("", "ld**"),
        ended(""),
        completed(""),
    ] {
        let update = renderer.observe(&published, false).unwrap();
        output.push_str(&commits(&update.commits, 80));
        completion = update.completion.or(completion);
        assert!(update.errors.is_empty());
    }
    assert_eq!(completion, Some(true));
    assert!(output.contains("Heading\r\n"));
    assert!(output.contains("bold\r\n"));
    assert_eq!(output.matches("Heading\r\n").count(), 1);
    assert_eq!(output.matches("bold\r\n").count(), 1);
}

#[test]
fn session_turn_renders_main_notices_and_leaves_child_notices_to_activity() {
    let mut renderer = EnhancedTurnRenderer::new(80, false);
    renderer.observe(&start("main"), false).unwrap();
    renderer.observe(&child("main"), false).unwrap();
    let mut output = String::new();
    let mut lines = 0;
    for owner in ["child", "main"] {
        let notices = [
            event(
                owner,
                Payload::RetryNotice(wire::RetryNotice {
                    attempt: 2,
                    retry_after_ms: 2000,
                    reason: "Provider timeout\x1b[2J".into(),
                    ..Default::default()
                }),
            ),
            event(
                owner,
                Payload::AgentNotice(wire::AgentNotice {
                    notice: "Retrying after plan validation failure: Invalid plan\x1b[2J".into(),
                    ..Default::default()
                }),
            ),
            event(
                owner,
                Payload::AgentNotice(wire::AgentNotice {
                    notice: "Retrying with pending child question reminder".into(),
                    ..Default::default()
                }),
            ),
            event(
                owner,
                Payload::AgentNotice(wire::AgentNotice {
                    notice: "Status prompt injected".into(),
                    ..Default::default()
                }),
            ),
            event(
                owner,
                Payload::AgentNotice(wire::AgentNotice {
                    notice: "Final provider request prompt injected".into(),
                    ..Default::default()
                }),
            ),
            event(
                owner,
                Payload::AgentNotice(wire::AgentNotice {
                    notice: "Tool availability restored prompt injected".into(),
                    ..Default::default()
                }),
            ),
        ];
        for notice in notices {
            let update = renderer.observe(&notice, false).unwrap();
            lines += update.commits.len();
            output.push_str(&commits(&update.commits, 80));
        }
    }
    assert_eq!(
        renderer
            .observe(&completed("main"), false)
            .unwrap()
            .completion,
        Some(true)
    );
    assert_eq!(lines, 6);
    for value in [
        "retry 2 in 2000 ms: Provider timeout[2J",
        "Retrying after plan validation failure: Invalid plan[2J",
        "Retrying with pending child question reminder",
        "Status prompt injected",
        "Final provider request prompt injected",
        "Tool availability restored prompt injected",
    ] {
        assert!(output.contains(value));
    }
    assert!(!output.contains("\x1b[2J"));
}

#[test]
fn turn_stream_commits_only_monitor_receipts_without_contents_or_promotion_duplicates() {
    let mut presenter = EnhancedSessionPresenter::new(120, false, false);
    let mut output = String::new();
    let mut events = vec![
        start("root"),
        child("root"),
        event(
            "root",
            Payload::InputAdmitted(wire::InputAdmitted {
                delivery: wire::Delivery::DELIVERY_STEER.into(),
                content: "old untagged notification".into(),
                ..Default::default()
            }),
        ),
    ];
    for (owner, id, content) in [
        (
            "root",
            "root-batch",
            "Monitor watch (ticks):\nfirst\nsecond",
        ),
        (
            "child",
            "child-batch",
            "Monitor child-watch (ticks):\nthird",
        ),
    ] {
        events.push(event(
            owner,
            Payload::InputAdmitted(wire::InputAdmitted {
                input_id: id.into(),
                delivery: wire::Delivery::DELIVERY_STEER.into(),
                source: wire::InputSource::INPUT_SOURCE_MONITOR.into(),
                content: content.into(),
                ..Default::default()
            }),
        ));
    }
    events.extend([
        event(
            "root",
            Payload::InputPromoted(wire::InputPromoted {
                input_id: "root-batch".into(),
                ..Default::default()
            }),
        ),
        event(
            "child",
            Payload::InputPromoted(wire::InputPromoted {
                input_id: "child-batch".into(),
                ..Default::default()
            }),
        ),
        ended("root"),
        completed("root"),
    ]);
    let mut completion = None;
    for published in events {
        let update = presenter.observe(&published).unwrap();
        output.push_str(&commits(&update.commits, 120));
        assert!(update.errors.is_empty());
        completion = update.completion.or(completion);
    }
    assert_eq!(completion, Some(true));
    let notices: Vec<_> = output
        .lines()
        .filter(|s| s.contains("Monitor notification received"))
        .collect();
    assert_eq!(
        notices,
        [
            "↻ Monitor notification received",
            "  ↻ [worker] Monitor notification received"
        ]
    );
    for hidden in [
        "old untagged notification",
        "watch",
        "first",
        "second",
        "third",
    ] {
        assert!(!output.contains(hidden));
    }
}

#[test]
fn enhanced_turn_ignores_agent_statistics_events() {
    let mut renderer = EnhancedTurnRenderer::new(8, false);
    renderer.observe(&start("main"), false).unwrap();
    let stats = event(
        "main",
        Payload::AgentStatisticsUpdated(wire::AgentStatisticsUpdatedEvent {
            input_tokens: 10,
            cached_input_tokens: 5,
            output_tokens: 2,
            context_size: 12,
            context_limit: 100,
            ..Default::default()
        }),
    );
    let update = renderer.observe(&stats, false).unwrap();
    assert!(update.commits.is_empty());
    assert!(update.errors.is_empty());
    renderer.observe(&ended("main"), false).unwrap();
    assert_eq!(
        renderer
            .observe(&completed("main"), false)
            .unwrap()
            .completion,
        Some(true)
    );
}

#[test]
#[ignore = "Legacy word wrapping; foreground/child lifecycle is covered by termimad_live"]
fn child_turn_completion_does_not_complete_the_foreground_turn() {
    let mut renderer = EnhancedTurnRenderer::new(8, false);
    let mut output = String::new();
    for published in [
        start("main-session"),
        event(
            "child-session",
            Payload::AgentStarted(wire::AgentStarted {
                parent_agent_session_id: "main-session".into(),
                name: "explorer".into(),
                ..Default::default()
            }),
        ),
        start("child-session"),
        event(
            "child-session",
            Payload::TurnEnded(wire::TurnEnded {
                finish_reason: "child-stop".into(),
                ..Default::default()
            }),
        ),
    ] {
        assert_eq!(
            renderer.observe(&published, false).unwrap().completion,
            None
        );
    }
    renderer
        .observe(&text("main-session", "main continues"), false)
        .unwrap();
    output.push_str(&commits(
        &renderer
            .observe(
                &event(
                    "main-session",
                    Payload::TurnEnded(wire::TurnEnded {
                        finish_reason: "main-stop".into(),
                        ..Default::default()
                    }),
                ),
                false,
            )
            .unwrap()
            .commits,
        8,
    ));
    let done = renderer.observe(&completed("main-session"), false).unwrap();
    output.push_str(&commits(&done.commits, 8));
    assert_eq!(done.completion, Some(true));
    assert!(done.errors.is_empty());
    assert!(!output.contains("child-stop"));
    assert!(output.contains("● main c"));
}

#[test]
fn failure_commits_live_suffix_and_reports_sanitized_error() {
    let mut renderer = EnhancedTurnRenderer::new(8, false);
    renderer.observe(&start(""), false).unwrap();
    renderer.observe(&text("", "partial"), false).unwrap();
    let update = renderer
        .observe(
            &event(
                "",
                Payload::TurnFailed(wire::TurnFailed {
                    message: "bad\x1b[2J\trequest".into(),
                    provider_response_body: "{\"error\":\"broken\x1b[2J\"}\nsecond line".into(),
                    ..Default::default()
                }),
            ),
            false,
        )
        .unwrap();
    assert_eq!(update.completion, Some(false));
    assert!(commits(&update.commits, 8).contains("● partia\r\n  l\r\n"));
    let error = update.errors.join("\n");
    assert!(error.contains("  bad[2J    request"));
    assert!(error.contains("  provider response:"));
    assert!(error.contains("{\"error\":\"broken[2J\"}\nsecond line"));
    assert!(!error.contains("\x1b[2J"));
}

#[test]
fn child_provider_failure_reports_its_response_body() {
    let mut renderer = EnhancedTurnRenderer::new(8, false);
    for published in [start("main"), child("main"), start("child")] {
        renderer.observe(&published, false).unwrap();
    }
    let update = renderer
        .observe(
            &event(
                "child",
                Payload::TurnFailed(wire::TurnFailed {
                    message: "child failed".into(),
                    provider_response_body: "{\"child\":true}".into(),
                    ..Default::default()
                }),
            ),
            false,
        )
        .unwrap();
    assert_eq!(update.completion, None);
    assert!(update.errors.join("\n").contains("  provider response:"));
    assert!(update.errors.join("\n").contains("{\"child\":true}"));
    renderer.observe(&ended("main"), false).unwrap();
    assert_eq!(
        renderer
            .observe(&completed("main"), false)
            .unwrap()
            .completion,
        Some(true)
    );
}

#[test]
fn task_progress_is_not_committed_by_standalone_turn_renderer() {
    let mut renderer = EnhancedTurnRenderer::new(8, false);
    for status in [
        wire::AgentTaskProgressStatus::AGENT_TASK_PROGRESS_STATUS_RUNNING,
        wire::AgentTaskProgressStatus::AGENT_TASK_PROGRESS_STATUS_SUCCEEDED,
    ] {
        let snapshot = wire::AgentTaskProgressSnapshot {
            revision: 1,
            root_nodes: vec![wire::AgentTaskProgressNode {
                name: "work\x1b[2J\t日本".into(),
                status: status.into(),
                children: [
                    (
                        "pending",
                        wire::AgentTaskProgressStatus::AGENT_TASK_PROGRESS_STATUS_PENDING,
                    ),
                    (
                        "succeeded",
                        wire::AgentTaskProgressStatus::AGENT_TASK_PROGRESS_STATUS_SUCCEEDED,
                    ),
                    (
                        "failed",
                        wire::AgentTaskProgressStatus::AGENT_TASK_PROGRESS_STATUS_FAILED,
                    ),
                    (
                        "canceled",
                        wire::AgentTaskProgressStatus::AGENT_TASK_PROGRESS_STATUS_CANCELED,
                    ),
                ]
                .into_iter()
                .map(|(name, status)| wire::AgentTaskProgressNode {
                    name: name.into(),
                    status: status.into(),
                    ..Default::default()
                })
                .collect(),
                ..Default::default()
            }],
            ..Default::default()
        };
        let update = renderer
            .observe(
                &event(
                    "",
                    Payload::ToolSnapshot(wire::ToolSnapshot {
                        kind: "agent_task_progress".into(),
                        data: snapshot.write_to_bytes().unwrap(),
                        ..Default::default()
                    }),
                ),
                false,
            )
            .unwrap();
        assert!(update.commits.is_empty());
        assert!(update.errors.is_empty());
        assert_eq!(update.completion, None);
    }
    assert!(renderer.finish().unwrap().commits.is_empty());
}

#[derive(Default)]
struct RuntimeCancellation {
    cancelled: std::sync::atomic::AtomicBool,
    notify: tokio::sync::Notify,
}
impl RuntimeCancellation {
    fn cancel(&self) {
        self.cancelled
            .store(true, std::sync::atomic::Ordering::SeqCst);
        self.notify.notify_waiters();
    }
}
impl parrot_domain::contracts::async_ops::Cancellation for RuntimeCancellation {
    fn is_cancelled(&self) -> bool {
        self.cancelled.load(std::sync::atomic::Ordering::SeqCst)
    }
    fn cancelled(&self) -> parrot_domain::contracts::async_ops::BoxFuture<'_, ()> {
        Box::pin(async move {
            loop {
                let notified = self.notify.notified();
                tokio::pin!(notified);
                notified.as_mut().enable();
                if self.cancelled.load(std::sync::atomic::Ordering::SeqCst) {
                    return;
                }
                notified.await;
            }
        })
    }
}

#[tokio::test(flavor = "current_thread")]
async fn before_render_runs_before_each_event_on_actual_stream_renderer() {
    let mut renderer = EnhancedTurnRenderer::new(80, false);
    let events = [
        ("none", wire::Event::default()),
        (
            "admitted-before-turn",
            event(
                "",
                Payload::InputAdmitted(wire::InputAdmitted {
                    content: "sent".into(),
                    ..Default::default()
                }),
            ),
        ),
        (
            "promoted",
            event(
                "",
                Payload::InputPromoted(wire::InputPromoted {
                    input_id: "input".into(),
                    ..Default::default()
                }),
            ),
        ),
        (
            "retry",
            event(
                "",
                Payload::RetryNotice(wire::RetryNotice {
                    attempt: 1,
                    ..Default::default()
                }),
            ),
        ),
        ("started", start("")),
        (
            "tool-chunk",
            event(
                "",
                Payload::ToolCallChunk(wire::ToolCallChunk {
                    tool_name: "shell".into(),
                    ..Default::default()
                }),
            ),
        ),
        ("first-visible", text("", "answer")),
        (
            "later-visible",
            event(
                "",
                Payload::ToolStarted(wire::ToolStarted {
                    tool_name: "shell".into(),
                    ..Default::default()
                }),
            ),
        ),
        ("ended", ended("")),
        ("ended-mode", completed("")),
    ]
    .into_iter()
    .map(|(id, mut event)| {
        event.id = id.into();
        Ok(event)
    });
    let mut stream = futures_util::stream::iter(events);
    let ids = Arc::new(std::sync::Mutex::new(Vec::new()));
    let output = Arc::new(std::sync::Mutex::new(String::new()));
    let before_ids = ids.clone();
    let before_output = output.clone();
    let commit_output = output.clone();
    let completed = renderer
        .render_session_turn(
            &mut stream,
            move |published, _| {
                let id = published.id.clone();
                before_ids.lock().unwrap().push(id.clone());
                before_output
                    .lock()
                    .unwrap()
                    .push_str(&format!("before:{id}|"));
                Box::pin(async { Ok(()) })
            },
            |_, _| Box::pin(async { Ok(()) }),
            |_, _| Box::pin(async { Ok(()) }),
            move |item, _, _| {
                commit_output
                    .lock()
                    .unwrap()
                    .push_str(&commits(&[item], 80));
                Box::pin(async { Ok(()) })
            },
            |errors, _| {
                assert!(errors.is_empty());
                Box::pin(async { Ok(()) })
            },
            || false,
            &RuntimeCancellation::default(),
        )
        .await
        .unwrap();
    assert!(completed);
    assert_eq!(
        ids.lock().unwrap().join(","),
        "none,admitted-before-turn,promoted,retry,started,tool-chunk,first-visible,later-visible,ended,ended-mode"
    );
    let rendered = output.lock().unwrap();
    assert!(rendered.starts_with("before:none|"));
    assert!(rendered.contains("retry 1 in 0 ms:"));
}

async fn interrupted_stream_case(cancel: bool) {
    let mut renderer = EnhancedTurnRenderer::new(80, false);
    let token = Arc::new(RuntimeCancellation::default());
    let after_token = token.clone();
    let mut fragment = text("root", "partial");
    fragment.id = "fragment".into();
    let mut events = vec![Ok(start("root")), Ok(fragment)];
    if !cancel {
        events.push(Err(tonic::Status::aborted("stream failed")));
    }
    let mut stream = futures_util::stream::iter(events);
    let output = Arc::new(std::sync::Mutex::new(String::new()));
    let captured = output.clone();
    let result = renderer
        .render_session_turn(
            &mut stream,
            |_, _| Box::pin(async { Ok(()) }),
            move |published, _| {
                if cancel && published.id == "fragment" {
                    after_token.cancel();
                }
                Box::pin(async { Ok(()) })
            },
            |_, _| Box::pin(async { Ok(()) }),
            move |item, _, cancellation| {
                assert!(!cancellation.is_cancelled());
                captured.lock().unwrap().push_str(&commits(&[item], 80));
                Box::pin(async { Ok(()) })
            },
            |errors, _| {
                assert!(errors.is_empty());
                Box::pin(async { Ok(()) })
            },
            || false,
            token.as_ref(),
        )
        .await;
    if cancel {
        assert!(!result.unwrap());
    } else {
        let error = result.unwrap_err();
        assert_eq!(error.code(), tonic::Code::Aborted);
        assert_eq!(error.message(), "stream failed");
    }
    let rendered = output.lock().unwrap();
    assert!(rendered.contains("● partial\r\n"));
    assert_eq!(rendered.matches("partial").count(), 1);
    assert!(renderer.finish().unwrap().commits.is_empty());
}
#[tokio::test(flavor = "current_thread")]
async fn actual_stream_cancellation_flushes_suffix_with_uncancelled_cleanup() {
    interrupted_stream_case(true).await;
}
#[tokio::test(flavor = "current_thread")]
async fn actual_stream_error_flushes_suffix_and_propagates_original_status() {
    interrupted_stream_case(false).await;
}

#[test]
fn actual_live_commit_routes_json_through_existing_final_yaml_formatter_once() {
    let mut presenter = EnhancedSessionPresenter::new(80, false, false);
    presenter.observe(&start("root")).unwrap();
    let preview = presenter
        .observe(&text("root", "{\"path\":\"src/App.cs\",\"enabled\":true}"))
        .unwrap();
    assert!(preview.commits.is_empty());
    assert!(live(&preview.live, 80).contains("enabled"));
    let update = presenter.observe(&completed("root")).unwrap();
    let output = commits(&update.commits, 80);
    assert_eq!(update.completion, Some(true));
    assert!(output.contains("\"path\": \"src/App.cs\""));
    assert!(output.contains("\"enabled\": true"));
    assert!(!output.contains("{\"path\""));
    assert_eq!(output.matches("\"enabled\": true").count(), 1);
    assert!(presenter.finish().unwrap().commits.is_empty());
}
