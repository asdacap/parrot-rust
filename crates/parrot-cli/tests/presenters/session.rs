use std::sync::{Arc, Mutex};
use std::time::Duration;

use parrot_cli::presenters::{
    EnhancedSessionPresenter, ImmediateScrollbackValue, SessionReplayContext, ToolPresenterRegistry,
};
use parrot_domain::protocol::messages::{parrot as wire, queue};
use protobuf::Message;
use wire::event::Payload;

use super::turn::{child, commits, completed, ended, event, live, start, text};

fn configured(columns: usize) -> EnhancedSessionPresenter {
    let mut presenter = EnhancedSessionPresenter::new(columns, false, false);
    presenter.set_session("build", "provider/model").unwrap();
    presenter
}
fn summary(owner: &str, fragment: &str, completed: bool) -> wire::Event {
    event(
        owner,
        Payload::ReasoningChunk(wire::ReasoningChunk {
            fragment: fragment.into(),
            kind: wire::ReasoningKind::REASONING_KIND_SUMMARY.into(),
            completed,
            ..Default::default()
        }),
    )
}

#[test]
fn tool_path_aliases_flow_from_events_to_live_and_replayed_labels_per_agent() {
    for replaying in [false, true] {
        let mut presenter = EnhancedSessionPresenter::new(200, false, replaying);
        presenter.set_session("build", "provider/model").unwrap();
        presenter.observe(&start("root")).unwrap();
        presenter.observe(&child("root")).unwrap();
        for (agent, root) in [("root", "/remote/main"), ("child", "/remote/worker")] {
            for name in ["read", "write", "edit"] {
                let arguments = serde_json::json!({"path": format!("{root}/blobs/output.dat"), "offset":1, "limit":90}).to_string();
                presenter
                    .observe(&event(
                        agent,
                        Payload::ToolCallChunk(wire::ToolCallChunk {
                            tool_call_id: name.into(),
                            tool_name: name.into(),
                            arguments_fragment: arguments,
                            ..Default::default()
                        }),
                    ))
                    .unwrap();
                let started = presenter
                    .observe(&event(
                        agent,
                        Payload::ToolStarted(wire::ToolStarted {
                            tool_call_id: name.into(),
                            tool_name: name.into(),
                            path_variables: [("AGENT_HISTORY_DIR".into(), root.into())]
                                .into_iter()
                                .collect(),
                            ..Default::default()
                        }),
                    ))
                    .unwrap();
                let label = if name == "read" { "read L1-L90" } else { name };
                if !replaying {
                    let output = live(&started.live, 200);
                    assert!(
                        output.contains(&format!("{label} $AGENT_HISTORY_DIR/blobs/output.dat")),
                        "{output}"
                    );
                    assert!(!output.contains(root), "{output}");
                }
                let finished = presenter
                    .observe(&event(
                        agent,
                        Payload::ToolFinished(wire::ToolFinished {
                            tool_call_id: name.into(),
                            tool_name: name.into(),
                            result: Some(String::new()),
                            ..Default::default()
                        }),
                    ))
                    .unwrap();
                let output = commits(&finished.commits, 200);
                assert!(
                    output.contains(&format!("{label} $AGENT_HISTORY_DIR/blobs/output.dat")),
                    "{output}"
                );
                assert!(!output.contains(root), "{output}");
            }
        }
    }
}

#[test]
fn reasoning_rows_flush_before_streaming_answers_and_preserve_terminal_order() {
    for complete in [false, true] {
        let mut presenter = configured(80);
        presenter.observe(&start("root")).unwrap();
        let pending = presenter
            .observe(&summary("root", "Planning", complete))
            .unwrap();
        assert!(pending.commits.is_empty());
        assert!(live(&pending.live, 80).contains("✦ Planning"));
        let answer = presenter.observe(&text("root", "answer\n")).unwrap();
        let mut output = commits(&answer.commits, 80);
        assert!(output.starts_with("✦ Planning\r\n"));
        assert!(!live(&answer.live, 80).contains("✦ Planning"));
        output.push_str(&commits(
            &presenter.observe(&ended("root")).unwrap().commits,
            80,
        ));
        assert_eq!(output.matches("✦ Planning").count(), 1);
        assert_eq!(output.matches("● answer").count(), 1);
        assert!(output.find("✦ Planning").unwrap() < output.find("● answer").unwrap());
        assert!(output.find("● answer").unwrap() < output.find("agent finished").unwrap());
    }
}

#[test]
fn reasoning_rows_flush_before_foreground_notices_and_errors() {
    for payload in [
        Payload::AgentNotice(wire::AgentNotice {
            notice: "notice".into(),
            ..Default::default()
        }),
        Payload::RetryNotice(wire::RetryNotice {
            reason: "retry".into(),
            attempt: 1,
            ..Default::default()
        }),
        Payload::SkillLoaded(wire::SkillLoadedEvent {
            path: "skill".into(),
            ..Default::default()
        }),
        Payload::TurnFailed(wire::TurnFailed {
            message: "failure".into(),
            provider_response_body: "provider body".into(),
            ..Default::default()
        }),
    ] {
        let mut presenter = configured(80);
        presenter.observe(&start("root")).unwrap();
        presenter
            .observe(&summary("root", "Planning", false))
            .unwrap();
        let update = presenter.observe(&event("root", payload)).unwrap();
        let output = commits(&update.commits, 80);
        assert!(output.starts_with("✦ Planning\r\n"), "{output:?}");
        assert_eq!(output.matches("✦ Planning").count(), 1);
        assert!(!live(&update.live, 80).contains("✦ Planning"));
        assert!(update.commits.len() > 1 || !update.errors.is_empty());
    }
}

#[test]
fn reasoning_rows_flush_before_explicit_and_promoted_user_commits() {
    let mut presenter = configured(80);
    presenter.observe(&start("root")).unwrap();
    presenter
        .observe(&summary("root", "Planning", false))
        .unwrap();
    let update = presenter
        .commit(Arc::new(ImmediateScrollbackValue::trusted(vec![
            "dialog output".into(),
        ])))
        .unwrap();
    assert_eq!(
        commits(&update.commits, 80),
        "✦ Planning\r\ndialog output\r\n"
    );
    assert!(!live(&update.live, 80).contains("✦ Planning"));
    presenter
        .observe(&event(
            "root",
            Payload::InputAdmitted(wire::InputAdmitted {
                input_id: "steer".into(),
                content: "user input".into(),
                delivery: wire::Delivery::DELIVERY_STEER.into(),
                ..Default::default()
            }),
        ))
        .unwrap();
    presenter
        .observe(&summary("root", "More planning", false))
        .unwrap();
    let update = presenter
        .observe(&event(
            "root",
            Payload::InputPromoted(wire::InputPromoted {
                input_id: "steer".into(),
                ..Default::default()
            }),
        ))
        .unwrap();
    assert_eq!(
        commits(&update.commits, 80),
        "✦ More planning\r\n◆ user input\r\n"
    );
    assert!(!live(&update.live, 80).contains("✦ More planning"));
}

#[test]
fn reasoning_rows_flush_once_on_root_and_child_terminal_boundaries() {
    for owner in ["root", "child"] {
        for payload in [
            Payload::TurnEnded(wire::TurnEnded::default()),
            Payload::TurnFailed(wire::TurnFailed::default()),
            Payload::AgentFinished(wire::AgentFinished {
                parent_agent_session_id: if owner == "child" { "root" } else { "" }.into(),
                ..Default::default()
            }),
            Payload::AgentFailed(wire::AgentFailed {
                parent_agent_session_id: if owner == "child" { "root" } else { "" }.into(),
                ..Default::default()
            }),
            Payload::ModeTurnCompleted(wire::ModeTurnCompleted::default()),
        ] {
            let mut presenter = configured(80);
            presenter.observe(&start("root")).unwrap();
            presenter.observe(&child("root")).unwrap();
            presenter.observe(&start("child")).unwrap();
            presenter
                .observe(&summary(owner, "Planning", false))
                .unwrap();
            let terminal = event(owner, payload);
            let update = presenter.observe(&terminal).unwrap();
            let output = commits(&update.commits, 80);
            assert_eq!(output.matches("Planning").count(), 1, "{owner}: {output:?}");
            assert!(!live(&update.live, 80).contains("Planning"));
            assert!(
                !commits(&presenter.observe(&terminal).unwrap().commits, 80).contains("Planning")
            );
            assert!(!commits(&presenter.finish().unwrap().commits, 80).contains("Planning"));
        }
    }
}

#[test]
fn reasoning_cleanup_drains_unfinished_agents_in_order_and_is_idempotent() {
    for cancel in [false, true] {
        let mut presenter = configured(80);
        presenter.observe(&start("root")).unwrap();
        presenter.observe(&child("root")).unwrap();
        presenter.observe(&start("child")).unwrap();
        presenter
            .observe(&summary("root", "Root planning", false))
            .unwrap();
        presenter
            .observe(&summary("child", "Worker planning", false))
            .unwrap();
        let update = if cancel {
            presenter.cancel().unwrap()
        } else {
            presenter.finish().unwrap()
        };
        assert_eq!(
            commits(&update.commits, 80),
            "✦ Root planning\r\n  ✦ [worker] Worker planning\r\n"
        );
        assert!(!live(&update.live, 80).contains("planning"));
        assert!(presenter.finish().unwrap().commits.is_empty());
        assert!(presenter.cancel().unwrap().commits.is_empty());
    }
}

#[test]
fn reasoning_rows_end_when_another_agent_begins_visible_reasoning() {
    let mut presenter = configured(80);
    presenter.observe(&start("root")).unwrap();
    presenter.observe(&child("root")).unwrap();
    presenter.observe(&start("child")).unwrap();
    presenter
        .observe(&summary("root", "Root planning", true))
        .unwrap();
    let update = presenter
        .observe(&summary("child", "Worker planning", false))
        .unwrap();
    assert_eq!(commits(&update.commits, 80), "✦ Root planning\r\n");
    assert!(!live(&update.live, 80).contains("Root planning"));
    assert!(live(&update.live, 80).contains("✦ [worker] Worker planning"));
    presenter.observe(&summary("child", "", true)).unwrap();
    let update = presenter
        .observe(&summary("root", "Next planning", true))
        .unwrap();
    assert_eq!(
        commits(&update.commits, 80),
        "  ✦ [worker] Worker planning\r\n"
    );
    assert!(live(&update.live, 80).contains("✦ Next planning"));
}

#[test]
fn reasoning_rows_survive_silent_events_and_folded_tools() {
    let mut presenter = configured(80);
    presenter.observe(&start("root")).unwrap();
    presenter.observe(&child("root")).unwrap();
    presenter.observe(&start("child")).unwrap();
    presenter
        .observe(&summary("root", "Planning", true))
        .unwrap();
    for published in [
        phase(
            "root",
            wire::ProviderRequestPhase::PROVIDER_REQUEST_PHASE_REQUESTING,
            1,
        ),
        event(
            "root",
            Payload::ProviderCallUsage(wire::ProviderCallUsage::default()),
        ),
        event(
            "root",
            Payload::AgentStatisticsUpdated(wire::AgentStatisticsUpdatedEvent::default()),
        ),
        event(
            "",
            Payload::SessionUsageSnapshot(wire::SessionUsageSnapshot::default()),
        ),
        event(
            "root",
            Payload::ToolStarted(wire::ToolStarted {
                tool_call_id: "wait".into(),
                tool_name: "wait".into(),
                ..Default::default()
            }),
        ),
        summary("child", "", true),
    ] {
        let update = presenter.observe(&published).unwrap();
        assert!(update.commits.is_empty());
        assert!(live(&update.live, 80).contains("✦ Planning"));
    }
    let tick = presenter.tick(Duration::from_millis(80)).unwrap();
    assert!(tick.commits.is_empty());
    assert!(live(&tick.live, 80).contains("✦ Planning"));
}

#[test]
fn reasoning_rows_resize_without_losing_entries_or_rewriting_committed_history() {
    for tiny in [0, 1] {
        let mut presenter = configured(7);
        presenter.observe(&start("root")).unwrap();
        presenter.observe(&summary("root", "A", true)).unwrap();
        presenter.observe(&summary("root", "B", true)).unwrap();
        let resized = presenter.resize(3).unwrap();
        assert_eq!(commits(&resized.commits, 80), "✦ A\r\n");
        assert!(live(&resized.live, 3).contains("✦ B\n"));
        assert!(presenter.resize(7).unwrap().commits.is_empty());
        let pending = presenter.observe(&summary("root", "C", true)).unwrap();
        assert!(pending.commits.is_empty());
        assert!(live(&pending.live, 7).contains("✦ B ✦ C\n"));
        let resized = presenter.resize(tiny).unwrap();
        assert_eq!(commits(&resized.commits, 80), "✦ B\r\n✦ C\r\n");
        assert!(!live(&resized.live, 80).contains('✦'));
        assert_eq!(resized.completion, None);
        assert!(presenter.finish().unwrap().commits.is_empty());
    }
}

#[test]
fn reasoning_replay_and_deferred_publication_share_the_same_rows() {
    let mut traces = Vec::new();
    for deferred in [false, true] {
        let mut presenter = EnhancedSessionPresenter::new(80, false, true);
        let mut trace = Vec::new();
        for published in [
            start("root"),
            summary("root", "A", true),
            summary("root", "B", true),
            event(
                "",
                Payload::SessionUsageSnapshot(wire::SessionUsageSnapshot::default()),
            ),
            summary("root", "C", true),
            text("root", "answer"),
            ended("root"),
            completed("root"),
        ] {
            let update = if deferred {
                presenter.apply_event(&published).unwrap()
            } else {
                presenter.observe(&published).unwrap()
            };
            if deferred {
                assert!(update.live.is_empty());
            }
            trace.push((
                commits(&update.commits, 80),
                live(&presenter.snapshot().unwrap(), 80),
            ));
        }
        assert!(trace[2].0.is_empty());
        assert!(trace[2].1.contains("✦ A ✦ B\n"));
        assert_eq!(trace[3].0, "✦ A ✦ B\r\n");
        assert!(!trace[3].1.contains('✦'));
        assert!(trace[4].1.contains("✦ C\n"));
        assert_eq!(trace[5].0, "✦ C\r\n");
        assert!(presenter.finish().unwrap().commits.is_empty());
        traces.push(trace);
    }
    assert_eq!(traces[0], traces[1]);
}

#[test]
fn reasoning_rows_precede_replayed_and_live_plan_output() {
    for replaying in [false, true] {
        let replay = Arc::new(Mutex::new(SessionReplayContext::new(replaying)));
        let mut presenter = EnhancedSessionPresenter::with_replay_context(80, false, replay);
        presenter.observe(&start("root")).unwrap();
        presenter
            .observe(&summary("root", "Planning", false))
            .unwrap();
        let planned = presenter
            .observe(&event(
                "root",
                Payload::PlanCompleted(wire::PlanCompleted {
                    markdown: "Plan body".into(),
                    ..Default::default()
                }),
            ))
            .unwrap();
        let output = if replaying {
            commits(&planned.commits, 80)
        } else {
            assert!(planned.commits.is_empty());
            commits(&presenter.observe(&completed("root")).unwrap().commits, 80)
        };
        assert!(output.starts_with("✦ Planning\r\n"));
        assert!(output.contains("Plan body"));
        assert_eq!(output.matches("Planning").count(), 1);
        assert!(!live(&presenter.snapshot().unwrap(), 80).contains("Planning"));
    }
}

#[test]
fn live_skill_context_refresh_recomputes_reads_without_resetting_other_tool_timers() {
    use parrot_cli::presenters::{
        ToolPresenter,
        tools::{
            exec_command::ExecCommandToolPresenter,
            generic::GenericToolPresenter,
            read::{ReadToolPresenter, SkillReadPathFormatter},
        },
    };
    use parrot_cli::terminal_frames::LiveBufferRenderContext;
    use std::sync::atomic::{AtomicU64, Ordering};
    let clock = Arc::new(AtomicU64::new(0));
    let paths = Arc::new(SkillReadPathFormatter::new(
        "/server/work".into(),
        vec![wire::SkillRootLocation {
            path: "/server/skills".into(),
            scope: wire::ListedSkillScope::SKILL_SCOPE_USER.into(),
            ..Default::default()
        }],
    ));
    let presenters: Vec<Arc<dyn ToolPresenter>> = vec![
        Arc::new(ReadToolPresenter::new(paths.clone())),
        Arc::new(ExecCommandToolPresenter::with_clock(
            Vec::new(),
            Arc::new({
                let clock = clock.clone();
                move || Duration::from_secs(clock.load(Ordering::Acquire))
            }),
        )),
    ];
    let registry =
        Arc::new(ToolPresenterRegistry::new(presenters, Arc::new(GenericToolPresenter)).unwrap());
    let mut presenter = EnhancedSessionPresenter::with_registry(
        200,
        true,
        Arc::new(Mutex::new(SessionReplayContext::new(false))),
        registry,
    );
    presenter.set_session("build", "provider/model").unwrap();
    presenter.observe(&start("root")).unwrap();
    for (id, name, arguments) in [
        (
            "read-call",
            "read",
            r#"{"path":"/server/skills/solidifier/SKILL.md"}"#,
        ),
        ("exec-call", "exec_command", r#"{"command":"perform-work"}"#),
    ] {
        presenter
            .observe(&event(
                "root",
                Payload::ToolCallChunk(wire::ToolCallChunk {
                    tool_call_id: id.into(),
                    tool_name: name.into(),
                    arguments_fragment: arguments.into(),
                    ..Default::default()
                }),
            ))
            .unwrap();
        presenter
            .observe(&event(
                "root",
                Payload::ToolStarted(wire::ToolStarted {
                    tool_call_id: id.into(),
                    tool_name: name.into(),
                    ..Default::default()
                }),
            ))
            .unwrap();
    }
    let before = presenter.snapshot().unwrap();
    assert!(live(&before, 200).contains("running 0s"));
    let before_lines = before
        .iter()
        .flat_map(|item| {
            item.render(LiveBufferRenderContext {
                columns: 200,
                color: true,
            })
            .unwrap()
            .lines
        })
        .collect::<Vec<_>>();
    let read = before_lines
        .iter()
        .find(|line| line.text().contains("$USER_SKILLS/solidifier/SKILL.md"))
        .unwrap();
    assert_eq!(read.style().start(), "\x1b[48;5;236m\x1b[36m");
    clock.store(12, Ordering::Release);
    paths.replace_root_locations(Vec::new());
    let after = presenter.snapshot().unwrap();
    assert!(
        live(&after, 200).contains("running 12s"),
        "unrelated cached timer origin must survive context refresh"
    );
    let after_lines = after
        .iter()
        .flat_map(|item| {
            item.render(LiveBufferRenderContext {
                columns: 200,
                color: true,
            })
            .unwrap()
            .lines
        })
        .collect::<Vec<_>>();
    let read = after_lines
        .iter()
        .find(|line| line.text().contains("/server/skills/solidifier/SKILL.md"))
        .unwrap();
    assert_eq!(read.style().start(), "\x1b[48;5;236m\x1b[38;5;245m");
    assert!(!live(&after, 200).contains("$USER_SKILLS"));
}

#[test]
fn steer_remains_pending_across_turn_boundaries_until_its_own_promotion() {
    let mut presenter = configured(120);
    presenter.observe(&start("root")).unwrap();
    presenter.observe(&child("root")).unwrap();
    for (id, content) in [("first", "first steer"), ("second", "second steer")] {
        let admitted = presenter
            .observe(&event(
                "root",
                Payload::InputAdmitted(wire::InputAdmitted {
                    input_id: id.into(),
                    content: content.into(),
                    delivery: wire::Delivery::DELIVERY_STEER.into(),
                    ..Default::default()
                }),
            ))
            .unwrap();
        assert!(admitted.commits.is_empty());
        assert!(live(&admitted.live, 120).contains(&format!("◇ {content}")));
    }
    for published in [
        text("root", "still working"),
        ended("root"),
        start("root"),
        event(
            "child",
            Payload::InputPromoted(wire::InputPromoted {
                input_id: "first".into(),
                ..Default::default()
            }),
        ),
    ] {
        let update = presenter.observe(&published).unwrap();
        let pending = live(&update.live, 120);
        assert!(pending.contains("◇ first steer"));
        assert!(pending.contains("◇ second steer"));
        assert!(!commits(&update.commits, 120).contains("◆ first steer"));
    }
    let promotion = event(
        "root",
        Payload::InputPromoted(wire::InputPromoted {
            input_id: "first".into(),
            ..Default::default()
        }),
    );
    let promoted = presenter.observe(&promotion).unwrap();
    assert_eq!(commits(&promoted.commits, 120), "◆ first steer\r\n");
    assert!(!live(&promoted.live, 120).contains("first steer"));
    assert!(live(&promoted.live, 120).contains("◇ second steer"));
    assert!(presenter.observe(&promotion).unwrap().commits.is_empty());
    let canceled = presenter
        .observe(&event(
            "root",
            Payload::InputCanceled(wire::InputCanceled {
                input_id: "second".into(),
                ..Default::default()
            }),
        ))
        .unwrap();
    assert!(canceled.commits.is_empty());
    assert!(!live(&canceled.live, 120).contains("second steer"));
}

#[test]
fn replay_restores_pending_steers_and_commits_only_promoted_ones() {
    let mut presenter = EnhancedSessionPresenter::new(120, false, true);
    for (id, content) in [("taken", "taken steer"), ("pending", "pending steer")] {
        let update = presenter
            .observe(&event(
                "root",
                Payload::InputAdmitted(wire::InputAdmitted {
                    input_id: id.into(),
                    content: content.into(),
                    delivery: wire::Delivery::DELIVERY_STEER.into(),
                    ..Default::default()
                }),
            ))
            .unwrap();
        assert!(update.commits.is_empty());
        assert!(live(&update.live, 120).contains(&format!("◇ {content}")));
    }
    presenter.observe(&start("root")).unwrap();
    let promoted = presenter
        .observe(&event(
            "root",
            Payload::InputPromoted(wire::InputPromoted {
                input_id: "taken".into(),
                ..Default::default()
            }),
        ))
        .unwrap();
    assert_eq!(commits(&promoted.commits, 120), "◆ taken steer\r\n");
    assert!(!live(&promoted.live, 120).contains("taken steer"));
    assert!(live(&promoted.live, 120).contains("◇ pending steer"));
    let resized = presenter.resize(20).unwrap();
    assert!(live(&resized.live, 20).contains("pending steer"));
}

#[test]
fn deferred_events_preserve_text_and_completion_without_live_materialization() {
    let mut presenter = configured(2048);
    let started = presenter.apply_event(&start("root")).unwrap();
    assert!(started.started_main_turn);
    assert!(started.live.is_empty());
    for _ in 0..1024 {
        let update = presenter.apply_event(&text("root", "x")).unwrap();
        assert!(update.live.is_empty());
        assert!(update.commits.is_empty());
    }
    let tick = presenter.advance_time(Duration::from_millis(80)).unwrap();
    assert!(tick.live.is_empty());
    assert!(tick.commits.is_empty());
    assert!(live(&presenter.snapshot().unwrap(), 2048).contains(&"x".repeat(1024)));
    let finished = presenter.apply_event(&completed("root")).unwrap();
    assert!(finished.live.is_empty());
    assert!(finished.finished_main_turn);
    assert_eq!(finished.completion, Some(true));
    assert_eq!(commits(&finished.commits, 2048).matches('x').count(), 1024);
    assert!(presenter.finish().unwrap().commits.is_empty());
}

#[test]
fn resize_preserves_pending_live_text_and_zero_width_clamps_like_one() {
    let mut outputs = Vec::new();
    for columns in [8, 0, 1] {
        let mut presenter = configured(80);
        presenter.observe(&start("root")).unwrap();
        let pending = presenter.observe(&text("root", "abcdefghij")).unwrap();
        assert!(pending.commits.is_empty());
        assert!(live(&pending.live, 80).contains("● abcdefghij\n"));

        let resized = presenter.resize(columns).unwrap();
        assert!(resized.commits.is_empty());
        assert_eq!(resized.completion, None);
        assert!(!resized.finished_main_turn);
        assert!(!live(&resized.live, columns.max(1)).contains("abcdefghij"));
        let appended = presenter.observe(&text("root", "klmn")).unwrap();
        assert!(appended.commits.is_empty());
        assert!(live(&appended.live, 80).contains("● abcdefghijklmn\n"));
        let committed = presenter.observe(&completed("root")).unwrap();
        assert_eq!(committed.completion, Some(true));
        assert!(committed.finished_main_turn);
        let output = commits(&committed.commits, 80);
        assert_eq!(
            output
                .chars()
                .filter(char::is_ascii_alphabetic)
                .collect::<String>(),
            "abcdefghijklmn"
        );
        assert!(!output.contains("● abcdefghijklmn\r\n"));
        assert!(presenter.finish().unwrap().commits.is_empty());
        outputs.push(output);
    }
    assert_eq!(outputs[0], "● abcdef\r\n  ghijkl\r\n  mn\r\n");
    assert_eq!(outputs[1], outputs[2]);
}

fn phase(owner: &str, phase: wire::ProviderRequestPhase, attempt: u32) -> wire::Event {
    event(
        owner,
        Payload::ProviderRequestPhaseChanged(wire::ProviderRequestPhaseChangedEvent {
            phase: phase.into(),
            attempt,
            ..Default::default()
        }),
    )
}
fn request_phase_case(ending: &str) {
    use wire::ProviderRequestPhase::*;
    let mut presenter = configured(160);
    presenter.observe(&start("root")).unwrap();
    for (owner, phase_value, attempt, label) in [
        ("root", PROVIDER_REQUEST_PHASE_REQUESTING, 1, "Requesting…"),
        (
            "child",
            PROVIDER_REQUEST_PHASE_HEADERS_RECEIVED,
            2,
            "Requesting…",
        ),
        (
            "root",
            PROVIDER_REQUEST_PHASE_HEADERS_RECEIVED,
            1,
            "Waiting for first token…",
        ),
        (
            "child",
            PROVIDER_REQUEST_PHASE_IDLE,
            2,
            "Waiting for first token…",
        ),
        ("root", PROVIDER_REQUEST_PHASE_IDLE, 1, ""),
        (
            "root",
            PROVIDER_REQUEST_PHASE_REQUESTING,
            2,
            "Requesting (attempt 2)…",
        ),
        ("root", PROVIDER_REQUEST_PHASE_IDLE, 2, ""),
        ("root", PROVIDER_REQUEST_PHASE_REQUESTING, 1, "Requesting…"),
        (
            "root",
            PROVIDER_REQUEST_PHASE_HEADERS_RECEIVED,
            1,
            "Waiting for first token…",
        ),
        ("root", PROVIDER_REQUEST_PHASE_UNSPECIFIED, 1, ""),
        (
            "root",
            PROVIDER_REQUEST_PHASE_HEADERS_RECEIVED,
            1,
            "Waiting for first token…",
        ),
    ] {
        let update = presenter
            .observe(&phase(owner, phase_value, attempt))
            .unwrap();
        assert!(update.commits.is_empty());
        let frame = live(&update.live, 160);
        assert_eq!(
            frame.contains("Requesting"),
            label.starts_with("Requesting"),
            "{frame:?}"
        );
        assert_eq!(
            frame.contains("Waiting for first token…"),
            label == "Waiting for first token…"
        );
        if !label.is_empty() {
            assert!(frame.contains(&format!("{label} (running")));
        }
        assert!(frame.contains("provider/model"));
        assert!(frame.contains("mode: build"));
    }
    let child_ended = presenter.observe(&ended("child")).unwrap();
    assert_eq!(child_ended.completion, None);
    assert!(!child_ended.finished_main_turn);
    assert!(live(&child_ended.live, 160).contains("Waiting for first token… (running"));
    if ending == "ended" || ending == "failed" {
        let terminal = if ending == "ended" {
            ended("root")
        } else {
            event(
                "root",
                Payload::TurnFailed(wire::TurnFailed {
                    message: "failed".into(),
                    ..Default::default()
                }),
            )
        };
        let update = presenter.observe(&terminal).unwrap();
        if ending == "ended" {
            assert_eq!(update.completion, None);
            assert!(!update.finished_main_turn);
        }
        let frame = live(&update.live, 160);
        assert!(!frame.contains("Requesting…"));
        assert!(!frame.contains("Waiting for first token…"));
    }
    let update = if ending == "cancel" {
        presenter.cancel().unwrap()
    } else {
        presenter.finish().unwrap()
    };
    let frame = live(&update.live, 160);
    assert!(!frame.contains("Requesting…"));
    assert!(!frame.contains("Waiting for first token…"));
}
macro_rules! request_cases {($($name:ident:$value:literal),*$(,)?)=>{$(#[test] fn $name(){request_phase_case($value);})*}}
request_cases! { request_phase_eof:"eof", request_phase_cancel:"cancel", request_phase_ended:"ended", request_phase_failed:"failed" }

#[test]
fn live_usage_is_modeline_only_aggregates_all_agents_expires_and_resets() {
    let mut presenter = configured(120);
    let update = presenter
        .observe(&event(
            "",
            Payload::SessionUsageSnapshot(wire::SessionUsageSnapshot {
                revision: 1,
                input_tokens: 100,
                cached_input_tokens: 25,
                output_tokens: 20,
                context_size: 50,
                context_limit: 1000,
                input_cost: 0.5,
                output_cost: 0.25,
                ..Default::default()
            }),
        ))
        .unwrap();
    assert!(update.commits.is_empty());
    for owner in ["root", "child"] {
        let update = presenter
            .observe(&event(
                owner,
                Payload::ProviderCallUsage(wire::ProviderCallUsage {
                    input_tokens: 30,
                    output_tokens: 30,
                    ..Default::default()
                }),
            ))
            .unwrap();
        assert!(!update.live_event);
        assert!(update.commits.is_empty());
    }
    for published in [start("root"), ended("root"), completed("root")] {
        presenter.observe(&published).unwrap();
    }
    let frame = live(&presenter.snapshot().unwrap(), 120);
    for value in ["+100i +20o (+25.00% cache)", "50/1k", "2i/s 2o/s", "$0.75"] {
        assert!(frame.contains(value), "missing {value} in {frame:?}");
    }
    for hidden in [
        "Working:",
        "Thinking…",
        "ProviderCallUsage",
        "InputTokens",
        "OutputTokens",
    ] {
        assert!(!frame.contains(hidden));
    }
    let committed = presenter
        .commit(Arc::new(ImmediateScrollbackValue::trusted(vec![
            "committed sentinel".into(),
        ])))
        .unwrap();
    assert_eq!(committed.commits.len(), 1);
    let output = commits(&committed.commits, 120);
    assert_eq!(output.matches("committed sentinel").count(), 1);
    for hidden in ["ProviderCallUsage", "InputTokens", "OutputTokens"] {
        assert!(!output.contains(hidden));
    }
    let expired = presenter.tick(Duration::from_secs(30)).unwrap();
    assert!(!live(&expired.live, 120).contains("i/s"));
    let replacement = configured(120);
    let reset = live(&replacement.snapshot().unwrap(), 120);
    assert!(!reset.contains("i/s"));
    assert!(!reset.contains("+100i"));
}

fn retry_case(maximum_retries: i32) {
    let mut presenter = configured(140);
    presenter.observe(&start("root")).unwrap();
    let mut output = String::new();
    for attempt in 1..=maximum_retries {
        let update = presenter
            .observe(&event(
                "root",
                Payload::RetryNotice(wire::RetryNotice {
                    attempt,
                    retry_after_ms: 2000,
                    reason: "Provider response headers timed out. Retrying.".into(),
                    ..Default::default()
                }),
            ))
            .unwrap();
        output.push_str(&commits(&update.commits, 140));
    }
    let message = format!(
        "Provider header timeout retry limit exceeded ({maximum_retries} retries, {} attempts).",
        maximum_retries + 1
    );
    let failed = presenter
        .observe(&event(
            "root",
            Payload::TurnFailed(wire::TurnFailed {
                message: message.clone(),
                ..Default::default()
            }),
        ))
        .unwrap();
    assert_eq!(failed.completion, Some(false));
    assert!(failed.finished_main_turn);
    assert!((output + &failed.errors.join("\n")).contains(&message));
    let frame = live(&failed.live, 140);
    assert!(!frame.contains("running"));
    assert!(!frame.contains("Retrying."));
}
macro_rules! retry_cases {($($name:ident:$value:literal),*$(,)?)=>{$(#[test] fn $name(){retry_case($value);})*}}
retry_cases! { retry_exhaustion_zero:0, retry_exhaustion_one:1, retry_exhaustion_five:5 }

#[test]
fn root_turn_duration_advances_and_respects_lifecycle_boundaries() {
    let mut presenter = configured(120);
    let first = presenter.observe(&start("root")).unwrap();
    assert!(live(&first.live, 120).contains("agent main (running 0s)"));
    let elapsed = presenter.tick(Duration::from_secs(65)).unwrap();
    assert!(live(&elapsed.live, 120).contains("agent main (running 1m 05s)"));
    let working = presenter
        .observe(&event(
            "root",
            Payload::ToolStarted(wire::ToolStarted {
                tool_call_id: "wait-call".into(),
                tool_name: "wait".into(),
                ..Default::default()
            }),
        ))
        .unwrap();
    assert!(live(&working.live, 120).contains("Working: wait (running 1m 05s)"));
    let elapsed = presenter.tick(Duration::from_secs(7384)).unwrap();
    assert!(live(&elapsed.live, 120).contains("Working: wait (running 2h 03m 04s)"));
    for published in [child("root"), start("child"), ended("child")] {
        let update = presenter.observe(&published).unwrap();
        assert_eq!(update.completion, None);
        assert!(!update.finished_main_turn);
        assert!(live(&update.live, 120).contains("Working: wait (running 2h 03m 04s)"));
    }
    assert!(
        live(&presenter.snapshot().unwrap(), 120).contains("Working: wait (running 2h 03m 04s)")
    );
    let done = presenter.observe(&ended("root")).unwrap();
    assert_eq!(done.completion, None);
    assert!(!done.finished_main_turn);
    assert!(!live(&done.live, 120).contains("running"));
    let again = presenter.observe(&start("root")).unwrap();
    assert!(live(&again.live, 120).contains("agent main (running 0s)"));
    let failed = presenter
        .observe(&event(
            "root",
            Payload::TurnFailed(wire::TurnFailed {
                message: "failed".into(),
                ..Default::default()
            }),
        ))
        .unwrap();
    assert!(!live(&failed.live, 120).contains("running"));
    presenter.observe(&start("root")).unwrap();
    let later = presenter.tick(Duration::from_secs(7396)).unwrap();
    assert!(live(&later.live, 120).contains("agent main (running 12s)"));
    let replacement = configured(120);
    assert!(!live(&replacement.snapshot().unwrap(), 120).contains("running"));
}

#[test]
fn older_stream_without_live_usage_keeps_rates_out_of_modeline() {
    let mut presenter = configured(120);
    presenter
        .observe(&event(
            "",
            Payload::SessionUsageSnapshot(wire::SessionUsageSnapshot {
                revision: 1,
                input_tokens: 100,
                output_tokens: 20,
                context_size: 50,
                context_limit: 1000,
                ..Default::default()
            }),
        ))
        .unwrap();
    for published in [start("root"), ended("root"), completed("root")] {
        presenter.observe(&published).unwrap();
    }
    let frame = live(&presenter.snapshot().unwrap(), 120);
    assert!(frame.contains("+100i +20o"));
    assert!(frame.contains("50/1k"));
    assert!(!frame.contains("i/s"));
    assert!(!frame.contains("o/s"));
}

pub(crate) fn queue_event(owner: &str, queues: Vec<queue::QueueState>) -> wire::Event {
    let snapshot = queue::QueueSnapshot {
        root_agent_session_id: "root".into(),
        queues,
        ..Default::default()
    };
    event(
        "",
        Payload::ToolSnapshot(wire::ToolSnapshot {
            kind: "queue".into(),
            owner_agent_session_id: owner.into(),
            inventory_instance_id: "inventory".into(),
            revision: 1,
            final_chunk: true,
            data: snapshot.write_to_bytes().unwrap(),
            ..Default::default()
        }),
    )
}

fn boundary_case(kind: &str, owner: &str, replaying: bool) {
    // This is the source's constant RunWithReplay predicate, not prepared-stream state.
    let context = Arc::new(Mutex::new(SessionReplayContext::new(replaying)));
    let mut presenter = EnhancedSessionPresenter::with_registry(
        120,
        false,
        context,
        Arc::new(ToolPresenterRegistry::standard(Vec::new())),
    );
    presenter.set_session("build", "provider/model").unwrap();
    let boundary = match kind {
        "queue" => queue_event(owner, Vec::new()),
        "reasoning" => event(
            owner,
            Payload::EncryptedReasoningReceived(wire::EncryptedReasoningReceivedEvent::default()),
        ),
        _ => event(
            owner,
            Payload::AgentFinished(wire::AgentFinished::default()),
        ),
    };
    let events = [
        start("root"),
        child("root"),
        start("child"),
        text("root", "root before"),
        text("child", "child before"),
        event(
            owner,
            Payload::ProviderCallUsage(wire::ProviderCallUsage {
                input_tokens: 1,
                output_tokens: 1,
                ..Default::default()
            }),
        ),
        event(
            "",
            Payload::SessionUsageSnapshot(wire::SessionUsageSnapshot {
                revision: 1,
                ..Default::default()
            }),
        ),
        boundary,
        text("root", "root after"),
        text("child", "child after"),
        ended("child"),
        ended("root"),
        completed("root"),
    ];
    let mut output = String::new();
    let mut main_turns = 0;
    for published in events {
        let update = presenter.observe(&published).unwrap();
        output.push_str(&commits(&update.commits, 120));
        main_turns += usize::from(update.started_main_turn);
        assert!(update.errors.is_empty());
        if matches!(
            published.payload,
            Some(
                Payload::ProviderCallUsage(_)
                    | Payload::ToolSnapshot(_)
                    | Payload::AgentFinished(_)
            )
        ) {
            assert!(!update.live_event);
        }
    }
    let prefix = if owner == "root" {
        "● "
    } else {
        "  ● [worker] "
    };
    assert!(
        output.contains(&format!("{prefix}{owner} after\r\n")),
        "{output:?}"
    );
    assert!(
        !output.contains(&format!("{owner} before{owner} after")),
        "{output:?}"
    );
    let other = if owner == "root" { "child" } else { "root" };
    assert!(
        output.contains(&format!("{other} before{other} after")),
        "{output:?}"
    );
    assert_eq!(main_turns, usize::from(!replaying));
}
macro_rules! boundaries {($($name:ident:($kind:literal,$owner:literal,$replaying:literal)),*$(,)?)=>{$(#[test]fn $name(){boundary_case($kind,$owner,$replaying);})*}}
boundaries! {
    queue_root_live:("queue","root",false), queue_child_live:("queue","child",false), queue_root_replay:("queue","root",true), queue_child_replay:("queue","child",true),
    reasoning_root_live:("reasoning","root",false), reasoning_child_live:("reasoning","child",false), reasoning_root_replay:("reasoning","root",true), reasoning_child_replay:("reasoning","child",true),
    lifecycle_root_live:("lifecycle","root",false), lifecycle_root_replay:("lifecycle","root",true),
}

#[test]
fn orders_and_deduplicates_queue_snapshot_and_starts_only_main_turn() {
    let mut presenter = configured(100);
    let queues = [
        ("zeta", "last queue", 1),
        ("alpha", "stale queue", 2),
        ("alpha", "first queue", 3),
        ("empty", "hidden queue", 0),
    ]
    .into_iter()
    .map(|(name, description, item_count)| queue::QueueState {
        owner_agent_session_id: "root".into(),
        name: name.into(),
        description: description.into(),
        item_count,
        ..Default::default()
    })
    .collect();
    presenter.observe(&queue_event("root", queues)).unwrap();
    let mut main_turns = 0;
    let mut completion = None;
    for published in [
        start("root"),
        child("root"),
        start("child"),
        ended("root"),
        completed("root"),
    ] {
        let update = presenter.observe(&published).unwrap();
        main_turns += usize::from(update.started_main_turn);
        completion = update.completion.or(completion);
    }
    let frame = live(&presenter.snapshot().unwrap(), 100);
    let alpha = frame
        .find("queue: alpha · 3 items — first queue")
        .expect(&frame);
    let zeta = frame
        .find("queue: zeta · 1 item — last queue")
        .expect(&frame);
    assert_eq!(completion, Some(true));
    assert_eq!(main_turns, 1);
    assert!(zeta > alpha);
    assert!(!frame.contains("stale queue"));
    assert!(!frame.contains("hidden queue"));
}
