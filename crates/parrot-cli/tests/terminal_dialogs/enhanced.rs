//! Actual enhanced-loop observations over a controlled native UDS service.
//! Fresh .NET execution is NOT_RUN; fixtures establish orchestration, not security.

#[path = "enhanced_fixture.rs"]
mod fixture;

use fixture::{Fixture, TerminalFixture, event, question, wait_until};
use parrot_domain::{contracts::async_ops::Cancellation, protocol::messages::parrot as wire};
use std::{
    sync::{Arc, atomic::Ordering},
    time::{Duration, Instant},
};
use tonic::{Code, Status};

#[tokio::test]
async fn quiet_submit_is_one_hundred_ms_and_fast_lines_preserve_blank_lines() {
    for (typed, expected) in [
        ("hello\r", "hello"),
        ("first\rsecond\r", "first\nsecond"),
        ("first\r\rsecond\r", "first\n\nsecond"),
    ] {
        let mut rpc = Fixture::start().await;
        let mut terminal = TerminalFixture::new();
        let cli = rpc.cli(terminal.take(), "", None);
        let stop = rpc.stopping.clone();
        let result = fixture::run_flow(cli, stop.clone(), terminal.end_input(), async {
            terminal.output_contains("❯ ").await;
            let started = Instant::now();
            terminal.input.type_text(typed);
            tokio::time::sleep(Duration::from_millis(40)).await;
            assert!(
                rpc.state.lock().unwrap().sent.is_empty(),
                "submit must wait for quiet"
            );
            rpc.wait(|state| state.sent.len() == 1).await;
            assert!(started.elapsed() >= Duration::from_millis(100));
            assert_eq!(rpc.state.lock().unwrap().sent[0].text, expected);
            terminal.input.end();
        })
        .await;
        assert_eq!(result.unwrap(), 0);
        assert!(terminal.restored.load(Ordering::Acquire));
        assert_eq!(rpc.state.lock().unwrap().sent.len(), 1);
        rpc.close().await;
    }
}

#[tokio::test]
async fn busy_input_and_escape_or_control_c_preserve_draft_and_interrupt_once() {
    for interrupt in ["\x1b", "\x03"] {
        let mut rpc = Fixture::start().await;
        let mut terminal = TerminalFixture::new();
        let cli = rpc.cli(terminal.take(), "", None);
        let stop = rpc.stopping.clone();
        let result = fixture::run_flow(cli, stop.clone(), terminal.end_input(), async {
            terminal.output_contains("❯ ").await;
            terminal.input.type_text("first prompt\r");
            rpc.wait(|state| state.sent.len() == 1).await;
            rpc.publish(
                "session-1",
                event(
                    "started",
                    wire::event::Payload::TurnStarted(wire::TurnStarted {
                        model: "model".into(),
                        ..Default::default()
                    }),
                ),
            )
            .await;
            terminal.input.type_text("draft prompt");
            terminal.output_contains("draft prompt").await;
            terminal.input.type_text(interrupt);
            rpc.wait(|state| state.interrupted.len() == 1).await;
            assert!(!stop.is_cancelled());
            terminal.input.type_text("\r");
            rpc.wait(|state| state.sent.len() == 2).await;
            let state = rpc.state.lock().unwrap();
            assert_eq!(state.interrupted, ["session-1"]);
            assert_eq!(
                state
                    .sent
                    .iter()
                    .map(|request| request.text.as_str())
                    .collect::<Vec<_>>(),
                ["first prompt", "draft prompt"]
            );
            assert_eq!(
                state.sent[1].delivery.enum_value().unwrap(),
                wire::Delivery::DELIVERY_STEER
            );
            drop(state);
            terminal.input.end();
        })
        .await;
        assert_eq!(result.unwrap(), 0);
        assert!(terminal.restored.load(Ordering::Acquire));
        rpc.close().await;
    }
}

#[tokio::test]
async fn history_up_recalls_last_prompt_and_down_discards_recall() {
    let mut rpc = Fixture::start().await;
    let mut terminal = TerminalFixture::new();
    let cli = rpc.cli(terminal.take(), "", None);
    let stop = rpc.stopping.clone();
    let result = fixture::run_flow(cli, stop.clone(), terminal.end_input(), async {
        terminal.output_contains("❯ ").await;
        terminal.input.type_text("first prompt\r");
        rpc.wait(|state| state.sent.len() == 1).await;
        terminal.input.type_text("second prompt\r");
        rpc.wait(|state| state.sent.len() == 2).await;
        terminal.input.type_text("\x1b[A");
        tokio::time::sleep(Duration::from_millis(30)).await;
        terminal.input.type_text("\x1b[B");
        tokio::time::sleep(Duration::from_millis(30)).await;
        assert_eq!(rpc.state.lock().unwrap().sent.len(), 2);
        terminal.input.type_text("\x1b[A\r");
        rpc.wait(|state| state.sent.len() == 3).await;
        assert_eq!(rpc.state.lock().unwrap().sent[2].text, "second prompt");
        terminal.input.end();
    })
    .await;
    assert_eq!(result.unwrap(), 0);
    rpc.close().await;
}

#[tokio::test]
async fn actual_slash_registry_completes_filters_and_dispatches_mode_and_exit() {
    let mut rpc = Fixture::start().await;
    let mut terminal = TerminalFixture::new();
    let cli = rpc.cli(terminal.take(), "", None);
    let stop = rpc.stopping.clone();
    let result = fixture::run_flow(cli, stop.clone(), terminal.end_input(), async {
        terminal.output_contains("❯ ").await;
        terminal.input.type_text("/m");
        terminal.output_contains("❯ /m").await;
        let filtered = terminal.output.text().len();
        terminal.columns.store(81, Ordering::Release);
        wait_until(|| terminal.output.text()[filtered..].contains("❯ /m")).await;
        let filtered_frame = terminal.output.text()[filtered..].to_owned();
        assert!(filtered_frame.contains("Switch the mode for this session"));
        assert!(filtered_frame.contains("Switch the model for this session"));
        assert!(!filtered_frame.contains("Leave the session"));
        terminal.input.type_text("\x1b[B\t");
        terminal.output_contains("❯ /model").await;
        assert!(rpc.state.lock().unwrap().sent.is_empty());
        terminal.input.type_text("\x01\x0b/mo\r");
        terminal.output_contains("Select a mode").await;
        terminal.input.type_text("plan\r");
        terminal.output_contains("mode is now plan").await;
        terminal.input.type_text("\r");
        rpc.wait(|state| state.updated.len() == 1).await;
        assert_eq!(rpc.state.lock().unwrap().updated[0].mode, "plan");
        tokio::time::sleep(Duration::from_millis(30)).await;
        terminal.input.type_text("plain text\r");
        rpc.wait(|state| state.sent.len() == 1).await;
        assert_eq!(rpc.state.lock().unwrap().sent[0].text, "plain text");
        terminal.input.type_text("/exit\r");
    })
    .await;
    assert_eq!(result.unwrap(), 0);
    assert!(terminal.restored.load(Ordering::Acquire));
    rpc.close().await;
}

#[tokio::test]
async fn skill_enter_and_shift_tab_use_actual_catalog_and_cycle_modes() {
    let mut rpc = Fixture::start().await;
    rpc.state.lock().unwrap().skills.insert(
        "session-1".into(),
        vec![wire::Skill {
            name: "greet".into(),
            path: "/greet".into(),
            enabled: true,
            description: "greet description".into(),
            ..Default::default()
        }],
    );
    let mut terminal = TerminalFixture::new();
    let cli = rpc.cli(terminal.take(), "", None);
    let stop = rpc.stopping.clone();
    let result = fixture::run_flow(cli, stop.clone(), terminal.end_input(), async {
        terminal.output_contains("❯ ").await;
        terminal.input.type_text("$gre");
        terminal.output_contains("greet description").await;
        terminal.input.type_text("\r");
        rpc.wait(|state| state.sent.len() == 1).await;
        assert_eq!(rpc.state.lock().unwrap().sent[0].text, "$greet");
        terminal.input.type_text("\x1b[Z\x1b[Z\x1b[Z");
        rpc.wait(|state| state.updated.len() == 3).await;
        assert_eq!(
            rpc.state
                .lock()
                .unwrap()
                .updated
                .iter()
                .map(|request| request.mode.as_str())
                .collect::<Vec<_>>(),
            ["plan", "query", "build"]
        );
        terminal.input.end();
    })
    .await;
    assert_eq!(result.unwrap(), 0);
    rpc.close().await;
}

#[tokio::test]
async fn question_arrival_wakes_input_and_remote_close_prevents_late_reply() {
    let mut rpc = Fixture::start().await;
    let mut terminal = TerminalFixture::new();
    let cli = rpc.cli(terminal.take(), "", None);
    let stop = rpc.stopping.clone();
    let result = fixture::run_flow(cli, stop.clone(), terminal.end_input(), async {
        terminal.output_contains("❯ ").await;
        terminal.input.type_text("ask me\r");
        rpc.wait(|state| state.sent.len() == 1).await;
        rpc.wait(|state| state.question_lists > 0).await;
        let mut pending = question("answer-me", "First answer");
        pending.questions[0].custom = false;
        pending.questions[0].options = vec![wire::QuestionOption {
            label: "Blue".into(),
            ..Default::default()
        }];
        rpc.state
            .lock()
            .unwrap()
            .questions
            .insert("session-1".into(), vec![pending]);
        terminal.output_contains("First answer").await;
        terminal.input.type_text("blue\r");
        rpc.wait(|state| state.question_replies.len() == 1).await;
        {
            let state = rpc.state.lock().unwrap();
            assert_eq!(state.question_replies[0].user_session_id, "session-1");
            assert_eq!(state.question_replies[0].question_request_id, "answer-me");
            assert_eq!(state.question_replies[0].answers[0].text, "Blue");
        }
        rpc.state.lock().unwrap().questions.insert(
            "session-1".into(),
            vec![question("closed-remotely", "Do not answer late")],
        );
        terminal.output_contains("Do not answer late").await;
        terminal.input.type_text("private draft");
        terminal.output_contains("private draft").await;
        rpc.state.lock().unwrap().questions.clear();
        tokio::time::sleep(Duration::from_millis(600)).await;
        terminal.input.type_text("\r");
        tokio::time::sleep(Duration::from_millis(150)).await;
        assert_eq!(rpc.state.lock().unwrap().question_replies.len(), 1);
        assert!(rpc.state.lock().unwrap().question_rejections.is_empty());
        terminal.input.end();
    })
    .await;
    assert_eq!(result.unwrap(), 0);
    rpc.close().await;
}

#[tokio::test]
async fn countdown_updates_without_keys_and_retains_typed_answer() {
    for columns in [20, 80] {
        let mut rpc = Fixture::start().await;
        let mut terminal = TerminalFixture::new();
        terminal.columns.store(columns, Ordering::Release);
        let cli = rpc.cli(terminal.take(), "", None);
        let stop = rpc.stopping.clone();
        let result = fixture::run_flow(cli, stop.clone(), terminal.end_input(), async {
            terminal.output_contains("❯ ").await;
            let mut pending = question("timed", "Timed answer");
            pending.remaining_timeout_ms = Some(61_000);
            rpc.state
                .lock()
                .unwrap()
                .questions
                .insert("session-1".into(), vec![pending]);
            terminal.output_contains("Timed answer").await;
            terminal.input.type_text("typed answer");
            terminal.output_contains("typed answer").await;
            terminal.output_contains("Auto-return in 1:01").await;
            rpc.state
                .lock()
                .unwrap()
                .questions
                .get_mut("session-1")
                .unwrap()[0]
                .remaining_timeout_ms = Some(60_000);
            terminal.output_contains("Auto-return in 1:00").await;
            let before = terminal.output.text().len();
            terminal.columns.store(columns + 1, Ordering::Release);
            wait_until(|| terminal.output.text()[before..].contains("typed answer")).await;
            rpc.state
                .lock()
                .unwrap()
                .questions
                .get_mut("session-1")
                .unwrap()[0]
                .remaining_timeout_ms = None;
            tokio::time::sleep(Duration::from_millis(350)).await;
            let cleared = terminal.output.text().len();
            terminal.columns.store(columns + 2, Ordering::Release);
            wait_until(|| terminal.output.text()[cleared..].contains("typed answer")).await;
            assert!(
                !terminal.output.text()[cleared..].contains("Auto-return"),
                "cleared countdown must disappear on idle redraw without another key"
            );
            terminal.input.type_text("\r");
            rpc.wait(|state| state.question_replies.len() == 1).await;
            assert_eq!(
                rpc.state.lock().unwrap().question_replies[0].answers[0].text,
                "typed answer"
            );
            terminal.input.end();
        })
        .await;
        assert_eq!(result.unwrap(), 0);
        rpc.close().await;
    }
}

#[tokio::test]
async fn transient_question_discovery_failures_keep_listen_and_timed_question_alive_unavailable() {
    run_transient_question_discovery_failure_scenario(Code::Unavailable).await;
}

#[tokio::test]
async fn transient_question_discovery_failures_keep_listen_and_timed_question_alive_deadline_exceeded()
 {
    run_transient_question_discovery_failure_scenario(Code::DeadlineExceeded).await;
}

async fn run_transient_question_discovery_failure_scenario(code: Code) {
    let mut rpc = Fixture::start().await;
    let mut terminal = TerminalFixture::new();
    let cli = rpc.cli(terminal.take(), "", None);
    let stop = rpc.stopping.clone();
    let result = fixture::run_flow(cli, stop.clone(), terminal.end_input(), async {
        terminal.output_contains("❯ ").await;
        rpc.wait(|state| state.question_lists > 0 && state.listens.len() == 1)
            .await;
        let session_id = rpc.state.lock().unwrap().listens[0].user_session_id.clone();
        let assert_running_without_reply = || {
            let state = rpc.state.lock().unwrap();
            assert_eq!(state.listens.len(), 1, "must retain the healthy Listen");
            let streams = &state.streams[&session_id];
            assert_eq!(streams.len(), 1);
            assert!(
                !streams[0].is_closed(),
                "{code:?} question discovery failure must not close healthy Listen"
            );
            assert!(state.question_replies.is_empty());
            assert!(state.question_rejections.is_empty());
            assert!(state.permission_replies.is_empty());
            assert!(state.sent.is_empty());
            assert!(state.interrupted.is_empty());
            assert!(!stop.is_cancelled());
            assert!(!terminal.restored.load(Ordering::Acquire));
        };

        let before_question = {
            let mut state = rpc.state.lock().unwrap();
            state.question_list_failure = Some(Status::new(code, "transient question discovery"));
            state.question_lists
        };
        rpc.wait(|state| {
            state.question_lists >= before_question + 2 || state.streams[&session_id][0].is_closed()
        })
        .await;
        assert_running_without_reply();
        assert!(rpc.state.lock().unwrap().question_lists >= before_question + 2);

        let mut pending = question("recovered-timed", "Answer after discovery recovers");
        let request_id = pending.id.clone();
        pending.remaining_timeout_ms = Some(61_000);
        let recovered = {
            let mut state = rpc.state.lock().unwrap();
            state.question_list_failure = None;
            state.questions.insert(session_id.clone(), vec![pending]);
            state.question_lists
        };
        terminal
            .output_contains("Answer after discovery recovers")
            .await;
        terminal.output_contains("Auto-return in 1:01").await;
        assert!(rpc.state.lock().unwrap().question_lists > recovered);
        terminal.input.type_text("retained typed answer");
        terminal.output_contains("retained typed answer").await;

        let before_failure_output = terminal.output.text().len();
        let while_active = {
            let mut state = rpc.state.lock().unwrap();
            state.question_list_failure = Some(Status::new(code, "transient active question poll"));
            state.question_lists
        };
        rpc.wait(|state| {
            state.question_lists >= while_active + 3 || state.streams[&session_id][0].is_closed()
        })
        .await;
        assert_running_without_reply();
        wait_until(|| {
            terminal.output.text()[before_failure_output..].contains("Auto-return in 1:00")
                || rpc.state.lock().unwrap().streams[&session_id][0].is_closed()
        })
        .await;
        assert_running_without_reply();
        let before_resize = terminal.output.text().len();
        terminal.columns.store(81, Ordering::Release);
        wait_until(|| {
            terminal.output.text()[before_resize..].contains("retained typed answer")
                || rpc.state.lock().unwrap().streams[&session_id][0].is_closed()
        })
        .await;
        assert_running_without_reply();
        let failed_poll_output = terminal.output.text()[before_failure_output..].to_owned();
        assert!(
            failed_poll_output.contains("Auto-return in 1:00"),
            "countdown must extrapolate its last successful sample during {code:?}"
        );
        assert!(failed_poll_output.contains("retained typed answer"));
        assert!(rpc.state.lock().unwrap().question_list_failure.is_some());

        let before_recovery_output = terminal.output.text().len();
        let recovered = {
            let mut state = rpc.state.lock().unwrap();
            let pending = &mut state.questions.get_mut(&session_id).unwrap()[0];
            assert_eq!(pending.id, request_id);
            pending.remaining_timeout_ms = Some(90_000);
            state.question_list_failure = None;
            state.question_lists
        };
        wait_until(|| {
            terminal.output.text()[before_recovery_output..].contains("Auto-return in 1:30")
                || rpc.state.lock().unwrap().streams[&session_id][0].is_closed()
        })
        .await;
        assert_running_without_reply();
        let before_resize = terminal.output.text().len();
        terminal.columns.store(82, Ordering::Release);
        wait_until(|| {
            terminal.output.text()[before_resize..].contains("retained typed answer")
                || rpc.state.lock().unwrap().streams[&session_id][0].is_closed()
        })
        .await;
        assert_running_without_reply();
        let recovered_output = terminal.output.text()[before_recovery_output..].to_owned();
        assert!(recovered_output.contains("Auto-return in 1:30"));
        assert!(recovered_output.contains("retained typed answer"));
        assert!(rpc.state.lock().unwrap().question_lists > recovered);
        terminal.input.type_text("\r");
        rpc.wait(|state| state.question_replies.len() == 1).await;
        {
            let state = rpc.state.lock().unwrap();
            let reply = &state.question_replies[0];
            assert_eq!(reply.user_session_id, session_id);
            assert_eq!(reply.question_request_id, request_id);
            assert_eq!(reply.answers[0].text, "retained typed answer");
            assert!(state.question_rejections.is_empty());
            assert!(state.sent.is_empty());
            assert_eq!(state.listens.len(), 1);
            assert!(!state.streams[&session_id][0].is_closed());
        }
        terminal.input.end();
    })
    .await;
    assert_eq!(
        result.unwrap(),
        0,
        "recovered {code:?} must exit normally at EOF"
    );
    assert!(terminal.restored.load(Ordering::Acquire));
    assert_eq!(rpc.state.lock().unwrap().question_replies.len(), 1);
    rpc.close().await;
}

#[tokio::test]
async fn workspace_switch_prepares_stream_rolls_back_failure_and_fences_buffered_keys() {
    for deferred_first_event in [false, true] {
        let mut rpc = Fixture::start().await;
        let mut terminal = TerminalFixture::new();
        let second = wire::UserSession {
            id: "session-2".into(),
            model: "provider/model".into(),
            mode: "build".into(),
            working_directory: rpc.root.to_string_lossy().into_owned(),
            ..Default::default()
        };
        rpc.state
            .lock()
            .unwrap()
            .sessions
            .insert(second.id.clone(), second);
        {
            let mut state = rpc.state.lock().unwrap();
            let failures = if deferred_first_event {
                &mut state.first_event_failures
            } else {
                &mut state.listen_failures
            };
            failures.insert(
                "session-2".into(),
                Status::unavailable("selected readiness failed"),
            );
        }
        let cli = rpc.cli(terminal.take(), "", None);
        let stop = rpc.stopping.clone();
        let result = fixture::run_flow(cli, stop.clone(), terminal.end_input(), async {
            terminal.output_contains("❯ ").await;
            terminal.input.type_text("/workspace-sessions\r");
            terminal.output_contains("Workspace sessions").await;
            terminal.input.type_text("session-2\r");
            terminal.output_contains("selected readiness failed").await;
            if deferred_first_event {
                rpc.wait(|state| {
                    state.streams["session-2"]
                        .iter()
                        .all(|sender| sender.is_closed())
                })
                .await;
            } else {
                assert!(
                    !rpc.state.lock().unwrap().streams.contains_key("session-2"),
                    "header failure cannot leave a candidate receiver"
                );
            }
            assert!(
                rpc.state.lock().unwrap().streams["session-1"]
                    .iter()
                    .any(|sender| !sender.is_closed()),
                "failed selected candidate must not close old stream"
            );
            terminal.input.type_text("\r");
            tokio::time::sleep(Duration::from_millis(30)).await;
            terminal.input.type_text("still old\r");
            rpc.wait(|state| state.sent.len() == 1).await;
            assert_eq!(
                rpc.state.lock().unwrap().sent[0].user_session_id,
                "session-1"
            );
            rpc.complete_turn("session-1", "old").await;
            {
                let mut state = rpc.state.lock().unwrap();
                state.listen_failures.remove("session-2");
                state.first_event_failures.remove("session-2");
            }
            rpc.state
                .lock()
                .unwrap()
                .silent_listens
                .insert("session-2".into());
            let before = terminal.output.text().len();
            terminal.input.type_text("/workspace-sessions\r");
            wait_until(|| terminal.output.text()[before..].contains("Workspace sessions")).await;
            rpc.state.lock().unwrap().questions.insert(
                "session-1".into(),
                vec![question("old-question", "Old question must not return")],
            );
            terminal.input.type_text("session-2\rshould-not-cross\r");
            rpc.wait(|state| {
                state
                    .listens
                    .iter()
                    .filter(|request| request.user_session_id == "session-2" && request.replay)
                    .count()
                    == 2
            })
            .await;
            tokio::time::sleep(Duration::from_millis(250)).await;
            {
                let state = rpc.state.lock().unwrap();
                assert_eq!(
                    state.sent.len(),
                    1,
                    "no submission before the first prepared event"
                );
                assert!(
                    state.streams["session-1"]
                        .iter()
                        .any(|sender| !sender.is_closed()),
                    "old stream remains owned until readiness commits"
                );
            }
            rpc.publish(
                "session-2",
                event(
                    "ready",
                    wire::event::Payload::SessionUsageSnapshot(wire::SessionUsageSnapshot::new()),
                ),
            )
            .await;
            rpc.wait(|state| {
                state.streams["session-1"]
                    .iter()
                    .all(|sender| sender.is_closed())
            })
            .await;
            assert_eq!(
                rpc.state.lock().unwrap().sent.len(),
                1,
                "buffered old-scope keys must not submit to target"
            );
            terminal.input.type_text("fresh target\r");
            rpc.wait(|state| state.sent.len() == 2).await;
            {
                let state = rpc.state.lock().unwrap();
                assert_eq!(state.sent[1].user_session_id, "session-2");
                assert_eq!(state.sent[1].text, "fresh target");
                assert!(state.question_replies.is_empty());
            }
            rpc.state.lock().unwrap().questions.insert(
                "session-2".into(),
                vec![question("new-question", "New selected question")],
            );
            terminal.output_contains("New selected question").await;
            terminal.input.type_text("new answer\r");
            rpc.wait(|state| state.question_replies.len() == 1).await;
            {
                let state = rpc.state.lock().unwrap();
                assert_eq!(state.question_replies[0].user_session_id, "session-2");
                assert_eq!(
                    state.question_replies[0].question_request_id,
                    "new-question"
                );
                assert_eq!(state.question_replies[0].answers[0].text, "new answer");
                assert!(state.question_rejections.is_empty());
            }
            terminal.input.end();
        })
        .await;
        assert_eq!(result.unwrap(), 0);
        rpc.close().await;
    }
}

#[tokio::test]
async fn cancellation_during_selected_first_event_preparation_joins_both_streams() {
    let mut rpc = Fixture::start().await;
    let selected = wire::UserSession {
        id: "session-2".into(),
        model: "provider/model".into(),
        mode: "build".into(),
        working_directory: rpc.root.display().to_string(),
        ..Default::default()
    };
    {
        let mut state = rpc.state.lock().unwrap();
        state.sessions.insert(selected.id.clone(), selected);
        state.silent_listens.insert("session-2".into());
    }
    let mut terminal = TerminalFixture::new();
    let cli = rpc.cli(terminal.take(), "", None);
    let stop = rpc.stopping.clone();
    let result = fixture::run_flow(cli, stop.clone(), terminal.end_input(), async {
        terminal.output_contains("❯ ").await;
        terminal.input.type_text("/workspace-sessions\r");
        terminal.output_contains("Workspace sessions").await;
        terminal.input.type_text("session-2\r");
        rpc.wait(|state| state.streams.contains_key("session-2"))
            .await;
        assert!(rpc.state.lock().unwrap().sent.is_empty());
        assert!(
            rpc.state.lock().unwrap().streams["session-1"]
                .iter()
                .any(|sender| !sender.is_closed())
        );
        stop.cancel();
    })
    .await;
    assert!(
        matches!(result, Err(parrot_cli::terminal_dialogs::EnhancedFailure::Rpc(status)) if status.code() == Code::Cancelled)
    );
    assert!(terminal.restored.load(Ordering::Acquire));
    rpc.wait(|state| {
        state
            .streams
            .values()
            .flatten()
            .all(|sender| sender.is_closed())
    })
    .await;
    assert!(rpc.state.lock().unwrap().sent.is_empty());
    rpc.close().await;
}

#[tokio::test]
async fn actual_two_server_switch_rebinds_unary_listen_upload_and_retains_old_inflight_stream() {
    let mut old = Fixture::start().await;
    let mut new = Fixture::start().await;
    let selected = wire::UserSession {
        id: "selected-on-new-host".into(),
        model: "provider/model".into(),
        mode: "build".into(),
        working_directory: new.root.display().to_string(),
        ..Default::default()
    };
    {
        let mut state = new.state.lock().unwrap();
        state.sessions.insert(selected.id.clone(), selected.clone());
        state.skills.insert(
            selected.id.clone(),
            vec![wire::Skill {
                name: "new-host-skill".into(),
                path: "/new-host-skill".into(),
                enabled: true,
                description: "skill from new host".into(),
                ..Default::default()
            }],
        );
        state.silent_listens.insert(selected.id.clone());
    }
    let mut captured_old_client = old.client();
    let mut old_inflight = captured_old_client
        .listen(wire::ListenRequest {
            user_session_id: "captured-old-stream".into(),
            ..Default::default()
        })
        .await
        .unwrap()
        .into_inner();
    assert_eq!(
        old_inflight.message().await.unwrap().unwrap().id,
        "snapshot"
    );
    let navigation = Arc::new(fixture::SuppliedNavigation {
        client: new.client(),
        selected: selected.clone(),
    });
    let prepare: parrot_cli::terminal_dialogs::PreparePrompt = Arc::new(
        |mut client, session, _, entered, delivery, _, cancellation| {
            Box::pin(async move {
                let mut parts = Vec::new();
                if entered == "upload synthetic" {
                    let mut source = &b"synthetic upload bytes"[..];
                    let artifact = parrot_cli::attachments::uploader::send(
                        &mut client,
                        session,
                        &mut source,
                        "synthetic.bin",
                        "application/octet-stream",
                        cancellation,
                    )
                    .await?;
                    parts.push(wire::MessageContentPart {
                        content: Some(wire::message_content_part::Content::ArtifactId(
                            artifact.artifact_id,
                        )),
                        ..Default::default()
                    });
                }
                Ok(Some(wire::SendMessageRequest {
                    user_session_id: session.into(),
                    text: entered.into(),
                    delivery: delivery.into(),
                    parts,
                    ..Default::default()
                }))
            })
        },
    );
    let mut terminal = TerminalFixture::new();
    let cli = old.cli_with(terminal.take(), "", None, Some(navigation), prepare);
    let stop = old.stopping.clone();
    let result = fixture::run_flow(cli, stop.clone(), terminal.end_input(), async {
        terminal.output_contains("❯ ").await;
        terminal.input.type_text("old prompt\r");
        old.wait(|state| state.sent.len() == 1).await;
        old.complete_turn("session-1", "old-host").await;
        terminal.input.type_text("/workspace-sessions\r");
        terminal.output_contains("Workspace sessions").await;
        terminal
            .input
            .type_text("selected-on-new-host\rbuffered-old-prompt\r");
        new.wait(|state| state.streams.contains_key("selected-on-new-host"))
            .await;
        tokio::time::sleep(Duration::from_millis(150)).await;
        assert!(
            new.state.lock().unwrap().sent.is_empty(),
            "selected host must wait for a first event"
        );
        assert!(
            old.state.lock().unwrap().streams["session-1"]
                .iter()
                .any(|sender| !sender.is_closed())
        );
        new.publish(
            "selected-on-new-host",
            event(
                "new-ready",
                wire::event::Payload::SessionUsageSnapshot(wire::SessionUsageSnapshot::new()),
            ),
        )
        .await;
        old.wait(|state| {
            state.streams["session-1"]
                .iter()
                .all(|sender| sender.is_closed())
        })
        .await;
        assert!(
            new.state.lock().unwrap().sent.is_empty(),
            "buffered old-generation key tail must not cross hosts"
        );
        old.publish(
            "captured-old-stream",
            event(
                "old-inflight-after-commit",
                wire::event::Payload::SessionUsageSnapshot(wire::SessionUsageSnapshot::new()),
            ),
        )
        .await;
        let old_event = tokio::time::timeout(Duration::from_secs(8), old_inflight.message())
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        assert_eq!(
            old_event.id, "old-inflight-after-commit",
            "captured generated client stream stays on its original host after commit"
        );
        terminal.input.type_text("$new-host\r");
        terminal.output_contains("skill from new host").await;
        new.wait(|state| state.sent.len() == 1).await;
        assert_eq!(new.state.lock().unwrap().sent[0].text, "$new-host-skill");
        new.complete_turn("selected-on-new-host", "selected-skill").await;
        terminal.input.type_text("/mo\r");
        terminal.output_contains("Select a mode").await;
        terminal.input.type_text("query\r");
        terminal.output_contains("mode is now query").await;
        terminal.input.type_text("\r");
        new.wait(|state| state.updated.len() == 1).await;
        tokio::time::sleep(Duration::from_millis(30)).await;
        terminal.input.type_text("upload synthetic\r");
        new.wait(|state| state.sent.len() == 2 && state.uploads.len() == 1)
            .await;
        {
            let state = new.state.lock().unwrap();
            assert!(
                state
                    .sent
                    .iter()
                    .all(|request| request.user_session_id == "selected-on-new-host")
            );
            assert_eq!(state.updated[0].user_session_id, "selected-on-new-host");
            assert_eq!(state.updated[0].mode, "query");
            assert!(
                state
                    .listens
                    .iter()
                    .any(|request| request.user_session_id == "selected-on-new-host"
                        && request.replay)
            );
            let frames = &state.uploads[0];
            assert_eq!(frames.len(), 3);
            assert!(
                matches!(&frames[0].payload, Some(wire::attachment_upload_frame::Payload::Header(header)) if header.user_session_id == "selected-on-new-host")
            );
            assert!(
                matches!(&frames[1].payload, Some(wire::attachment_upload_frame::Payload::Chunk(bytes)) if bytes == b"synthetic upload bytes")
            );
            assert!(
                matches!(&frames[2].payload, Some(wire::attachment_upload_frame::Payload::Description(description)) if description.display_name == "synthetic.bin" && description.byte_length == 22)
            );
            assert!(
                matches!(&state.sent[1].parts[0].content, Some(wire::message_content_part::Content::ArtifactId(id)) if id == "synthetic-upload")
            );
        }
        {
            let state = old.state.lock().unwrap();
            assert_eq!(state.sent.len(), 1);
            assert_eq!(state.sent[0].text, "old prompt");
            assert!(state.updated.is_empty());
            assert!(state.uploads.is_empty());
            assert!(
                state
                    .listens
                    .iter()
                    .all(|request| request.user_session_id != "selected-on-new-host")
            );
        }
        terminal.input.end();
    }).await;
    assert_eq!(result.unwrap(), 0);
    assert!(terminal.restored.load(Ordering::Acquire));
    drop(old_inflight);
    drop(captured_old_client);
    old.close().await;
    new.close().await;
}

#[tokio::test]
async fn replayed_interactions_do_not_act_but_live_permission_and_plan_use_selected_session() {
    let mut rpc = Fixture::start().await;
    let mut terminal = TerminalFixture::new();
    let second = wire::UserSession {
        id: "session-2".into(),
        model: "provider/model".into(),
        mode: "plan".into(),
        working_directory: rpc.root.to_string_lossy().into_owned(),
        ..Default::default()
    };
    let permission = |id: &str, reason: &str| wire::PendingPermission {
        id: id.into(),
        reason: reason.into(),
        agent_session_id: "root".into(),
        choices: vec![
            wire::PermissionChoice {
                value: "allow".into(),
                label: "Allow".into(),
                action: wire::PermissionAction::PERMISSION_ACTION_ALLOW.into(),
                ..Default::default()
            },
            wire::PermissionChoice {
                value: "deny".into(),
                label: "Deny".into(),
                action: wire::PermissionAction::PERMISSION_ACTION_DENY.into(),
                ..Default::default()
            },
        ],
        ..Default::default()
    };
    let plan = |markdown: &str, prompt: &str| wire::PlanCompleted {
        markdown: markdown.into(),
        dialog: Some(wire::TurnCompleteDialog {
            prompt: prompt.into(),
            choices: vec![wire::DialogChoice {
                value: "yes".into(),
                description: "Implement it".into(),
                action: Some(wire::ChoiceAction {
                    mode: "build".into(),
                    prompt: "Implement approved plan.".into(),
                    ..Default::default()
                })
                .into(),
                ..Default::default()
            }],
            ..Default::default()
        })
        .into(),
        ..Default::default()
    };
    {
        let mut state = rpc.state.lock().unwrap();
        state.sessions.insert(second.id.clone(), second);
        state.replay.insert(
            "session-2".into(),
            vec![
                event(
                    "historical-started",
                    wire::event::Payload::TurnStarted(wire::TurnStarted::new()),
                ),
                event(
                    "historical-permission",
                    wire::event::Payload::PermissionPending(permission(
                        "historical",
                        "Historical permission must not open",
                    )),
                ),
                event(
                    "historical-plan",
                    wire::event::Payload::PlanCompleted(plan(
                        "# Historical report",
                        "Historical picker must not open",
                    )),
                ),
                event(
                    "historical-terminal",
                    wire::event::Payload::ModeTurnCompleted(wire::ModeTurnCompleted::new()),
                ),
            ],
        );
    }
    let cli = rpc.cli(terminal.take(), "", None);
    let stop = rpc.stopping.clone();
    let result = fixture::run_flow(cli, stop.clone(), terminal.end_input(), async {
        terminal.output_contains("❯ ").await;
        terminal.input.type_text("/workspace-sessions\r");
        terminal.output_contains("Workspace sessions").await;
        terminal.input.type_text("session-2\r");
        terminal.output_contains("Historical report").await;
        tokio::time::sleep(Duration::from_millis(150)).await;
        assert!(
            !terminal
                .output
                .text()
                .contains("Historical picker must not open")
        );
        assert!(
            !terminal
                .output
                .text()
                .contains("Historical permission must not open")
        );
        assert!(rpc.state.lock().unwrap().permission_replies.is_empty());
        assert!(rpc.state.lock().unwrap().sent.is_empty());
        rpc.publish(
            "session-2",
            event(
                "live-permission",
                wire::event::Payload::PermissionPending(permission(
                    "live",
                    "Live permission reason",
                )),
            ),
        )
        .await;
        terminal.output_contains("Live permission reason").await;
        terminal.input.type_text("Allow\r");
        rpc.wait(|state| state.permission_replies.len() == 1).await;
        {
            let state = rpc.state.lock().unwrap();
            assert_eq!(state.permission_replies[0].user_session_id, "session-2");
            assert_eq!(state.permission_replies[0].permission_request_id, "live");
            assert_eq!(state.permission_replies[0].choice_value, "allow");
        }
        rpc.publish(
            "session-2",
            event(
                "live-started",
                wire::event::Payload::TurnStarted(wire::TurnStarted {
                    model: "model".into(),
                    ..Default::default()
                }),
            ),
        )
        .await;
        rpc.publish(
            "session-2",
            event(
                "live-plan",
                wire::event::Payload::PlanCompleted(plan("# Written live plan", "Plan complete:")),
            ),
        )
        .await;
        rpc.publish(
            "session-2",
            event(
                "live-terminal",
                wire::event::Payload::ModeTurnCompleted(wire::ModeTurnCompleted::new()),
            ),
        )
        .await;
        terminal.output_contains("Plan complete:").await;
        let output = terminal.output.text();
        assert!(output.find("Written live plan").unwrap() < output.find("Plan complete:").unwrap());
        terminal.input.type_text("yes\r");
        rpc.wait(|state| state.sent.len() == 1).await;
        let state = rpc.state.lock().unwrap();
        assert_eq!(state.sent[0].user_session_id, "session-2");
        assert_eq!(state.sent[0].text, "Implement approved plan.");
        assert_eq!(
            state.sent[0].delivery.enum_value().unwrap(),
            wire::Delivery::DELIVERY_QUEUE
        );
        assert_eq!(state.updated.last().unwrap().mode, "build");
        drop(state);
        terminal.input.end();
    })
    .await;
    assert_eq!(result.unwrap(), 0);
    rpc.close().await;
}

#[tokio::test]
async fn actual_initial_and_interactive_plans_approve_after_markdown_and_optional_task_tree() {
    for (initial_prompt, with_tree) in [(false, false), (false, true), (true, false)] {
        let mut rpc = Fixture::start().await;
        rpc.initial_mode = "plan".into();
        let mut terminal = TerminalFixture::new();
        let cli = rpc.cli(
            terminal.take(),
            if initial_prompt { "draft plan" } else { "" },
            None,
        );
        let stop = rpc.stopping.clone();
        let result = fixture::run_flow(cli, stop.clone(), terminal.end_input(), async {
            if !initial_prompt {
                terminal.output_contains("❯ ").await;
                terminal.input.type_text("draft plan\r");
            }
            rpc.wait(|state| state.sent.len() == 1).await;
            {
                let state = rpc.state.lock().unwrap();
                assert_eq!(state.created.len(), 1);
                assert_eq!(state.created[0].mode, "plan");
                assert_eq!(state.created[0].interactive_permissions, !initial_prompt);
                assert_eq!(state.sent[0].text, "draft plan");
            }
            rpc.publish("session-1", event("plan-started", wire::event::Payload::TurnStarted(wire::TurnStarted {
                model: "model".into(), ..Default::default()
            }))).await;
            rpc.publish("session-1", event("ordinary-plan-ended", wire::event::Payload::TurnEnded(wire::TurnEnded {
                finish_reason: "stop".into(), ..Default::default()
            }))).await;
            let task_tree = with_tree.then(|| wire::AgentTaskProgressSnapshot {
                revision: 1,
                root_nodes: vec![
                    wire::AgentTaskProgressNode {
                        name: "first root".into(),
                        status: wire::AgentTaskProgressStatus::AGENT_TASK_PROGRESS_STATUS_PENDING.into(),
                        children: vec![wire::AgentTaskProgressNode {
                            name: "nested task".into(),
                            status: wire::AgentTaskProgressStatus::AGENT_TASK_PROGRESS_STATUS_PENDING.into(),
                            ..Default::default()
                        }],
                        ..Default::default()
                    },
                    wire::AgentTaskProgressNode {
                        name: "second root".into(),
                        status: wire::AgentTaskProgressStatus::AGENT_TASK_PROGRESS_STATUS_PENDING.into(),
                        ..Default::default()
                    },
                ],
                ..Default::default()
            });
            rpc.publish("session-1", event("written-plan", wire::event::Payload::PlanCompleted(wire::PlanCompleted {
                markdown: if with_tree { "# Written plan" } else { "# Written plan\n\n- change code" }.into(),
                task_tree: task_tree.into(),
                dialog: Some(wire::TurnCompleteDialog {
                    prompt: "Plan complete: ".into(),
                    choices: vec![wire::DialogChoice {
                        value: "yes".into(),
                        description: "Implement it".into(),
                        action: Some(wire::ChoiceAction {
                            mode: "build".into(),
                            prompt: "Implement the approved plan.".into(),
                            ..Default::default()
                        }).into(),
                        ..Default::default()
                    }],
                    ..Default::default()
                }).into(),
                ..Default::default()
            }))).await;
            rpc.publish("session-1", event("plan-mode-completed", wire::event::Payload::ModeTurnCompleted(wire::ModeTurnCompleted::new()))).await;
            terminal.output_contains("Written plan").await;
            terminal.output_contains("Plan complete:").await;
            let output = terminal.output.text();
            let markdown = output.find("Written plan").unwrap();
            let picker = output.find("Plan complete:").unwrap();
            if with_tree {
                let tree = output.find("Agent tasks:").expect("integrated plan must render task tree");
                let first = output.find("○ first root").expect("first root must render pending status");
                let child = output.find("└── ○ nested task").expect("nested child must render with tree connector");
                let second = output.find("○ second root").expect("second root must render pending status");
                assert!(markdown < tree && tree < first && first < child && child < second && second < picker,
                    "actual Enhanced plan order must be markdown < tree < first root < nested task < second root < picker");
            } else {
                assert!(!output.contains("Agent tasks:"));
                assert!(markdown < picker);
            }
            assert_eq!(rpc.state.lock().unwrap().sent.len(), 1, "initial/interactive plan must wait for actual picker approval");
            let approved = terminal.output.text().len();
            terminal.input.type_text("yes\r");
            rpc.wait(|state| state.sent.len() == 2).await;
            wait_until(|| terminal.output.text()[approved..].contains("\x1b[2K❯ ")).await;
            {
                let state = rpc.state.lock().unwrap();
                assert_eq!(state.updated.len(), 1);
                assert_eq!(state.updated[0].user_session_id, "session-1");
                assert_eq!(state.updated[0].mode, "build");
                assert_eq!(state.sent[1].text, "Implement the approved plan.");
                assert!(state.sent.iter().all(|request| request.user_session_id == "session-1" && request.delivery.enum_value().unwrap() == wire::Delivery::DELIVERY_QUEUE));
            }
            terminal.input.end();
        }).await;
        assert_eq!(result.unwrap(), 0);
        assert!(terminal.restored.load(Ordering::Acquire));
        rpc.close().await;
    }
}

#[tokio::test]
async fn plan_send_terminal_before_unary_response_does_not_restore_busy() {
    let mut rpc = Fixture::start().await;
    let initial = wire::UserSession {
        id: "supplied-plan-owner".into(),
        model: "provider/model".into(),
        mode: "plan".into(),
        working_directory: rpc.root.display().to_string(),
        ..Default::default()
    };
    rpc.state
        .lock()
        .unwrap()
        .sessions
        .insert(initial.id.clone(), initial.clone());
    let mut terminal = TerminalFixture::new();
    let cli = rpc.cli(terminal.take(), "", Some(initial.clone()));
    let stop = rpc.stopping.clone();
    let result = fixture::run_flow(cli, stop.clone(), terminal.end_input(), async {
        terminal.output_contains("mode: plan").await;
        terminal.output_contains("❯ ").await;
        rpc.wait(|state| state.listens.len() == 1).await;
        for published in [
            event("plan-started", wire::event::Payload::TurnStarted(wire::TurnStarted::new())),
            event("written-plan", wire::event::Payload::PlanCompleted(wire::PlanCompleted {
                markdown: "# Approve this plan".into(),
                dialog: Some(wire::TurnCompleteDialog {
                    prompt: "Approve before fast continuation:".into(),
                    choices: vec![wire::DialogChoice {
                        value: "yes".into(),
                        description: "Implement it".into(),
                        action: Some(wire::ChoiceAction {
                            mode: "build".into(),
                            prompt: "Implement this approved plan.".into(),
                            ..Default::default()
                        }).into(),
                        ..Default::default()
                    }],
                    ..Default::default()
                }).into(),
                ..Default::default()
            })),
            event("plan-completed", wire::event::Payload::ModeTurnCompleted(wire::ModeTurnCompleted::new())),
        ] {
            assert_eq!(published.agent_session_id, "root");
            rpc.publish(&initial.id, published).await;
        }
        terminal.output_contains("Approve before fast continuation:").await;
        let output = terminal.output.text();
        assert!(output.find("Approve this plan").unwrap() < output.find("Approve before fast continuation:").unwrap());
        {
            let mut state = rpc.state.lock().unwrap();
            assert!(state.sent.is_empty());
            assert!(state.updated.is_empty());
            state.send_reply_blocked = true;
        }
        terminal.input.type_text("yes\r");
        rpc.wait(|state| state.sent.len() == 1 && state.updated.len() == 1).await;
        {
            let state = rpc.state.lock().unwrap();
            assert!(state.send_reply_blocked);
            assert_eq!(state.sent[0].user_session_id, initial.id);
            assert_eq!(state.sent[0].text, "Implement this approved plan.");
            assert_eq!(state.sent[0].delivery.enum_value().unwrap(), wire::Delivery::DELIVERY_QUEUE);
            assert_eq!(state.updated[0].user_session_id, initial.id);
            assert_eq!(state.updated[0].mode, "build");
            assert_eq!(state.sessions[&initial.id].mode, "build");
        }
        // This plan's Markdown is committed only by its ModeTurnCompleted, not publication.
        for published in [
            event("fast-continuation-started", wire::event::Payload::TurnStarted(wire::TurnStarted::new())),
            event("fast-continuation-plan", wire::event::Payload::PlanCompleted(wire::PlanCompleted {
                markdown: "# Fast continuation finished".into(),
                ..Default::default()
            })),
        ] {
            assert_eq!(published.agent_session_id, "root");
            rpc.publish(&initial.id, published).await;
        }
        assert!(!terminal.output.text().contains("Fast continuation finished"));
        let completed = event("fast-continuation-completed", wire::event::Payload::ModeTurnCompleted(wire::ModeTurnCompleted::new()));
        assert_eq!(completed.agent_session_id, "root");
        rpc.publish(&initial.id, completed).await;
        terminal.output_contains("Fast continuation finished").await;
        {
            let state = rpc.state.lock().unwrap();
            assert!(state.send_reply_blocked, "actual unary reply must still be held when terminal Markdown is observed");
            assert_eq!(state.sent.len(), 1);
            assert_eq!(state.listens.len(), 1);
            assert_eq!(state.listens[0].user_session_id, initial.id);
            assert!(!state.listens[0].replay);
            assert_eq!(state.streams[&initial.id].len(), 1);
            assert!(!state.streams[&initial.id][0].is_closed());
            assert!(state.created.is_empty());
            assert!(state.interrupted.is_empty());
        }
        assert!(!stop.is_cancelled());
        assert!(!terminal.restored.load(Ordering::Acquire));
        let released = terminal.output.text().len();
        rpc.release_send_reply();
        wait_until(|| terminal.output.text()[released..].contains("\x1b[2K❯ ")).await;
        terminal.input.type_text("next plain prompt\r");
        rpc.wait(|state| state.sent.len() == 2).await;
        {
            let state = rpc.state.lock().unwrap();
            assert_eq!(state.sent[1].delivery.enum_value().unwrap(), wire::Delivery::DELIVERY_QUEUE,
                "terminal Listen completion before the approval unary reply must not be overwritten by a late busy=true");
            assert_eq!(state.sent[1].user_session_id, initial.id);
            assert_eq!(state.sent[1].text, "next plain prompt");
            assert!(!state.send_reply_blocked);
        }
        let fresh = terminal.output.text().len();
        terminal.columns.store(81, Ordering::Release);
        wait_until(|| {
            let output = terminal.output.text();
            output[fresh..].contains("mode: ") && output[fresh..].contains("\x1b[2K❯ ")
        }).await;
        let frame = terminal.output.text()[fresh..].to_owned();
        assert!(frame.contains("mode: build"), "approval must refresh the retained modeline: {frame:?}");
        assert!(!frame.contains("mode: plan"), "fresh approval frame must not retain the previous mode: {frame:?}");
        assert!(frame.contains("provider/model"));
        rpc.complete_turn(&initial.id, "next-plain-prompt").await;
        terminal.input.type_text("/workspace-sessions\r");
        terminal.output_contains("Workspace sessions").await;
        terminal.output_contains(&initial.id).await;
        let dismissed = terminal.output.text().len();
        terminal.input.type_text("\x1b");
        wait_until(|| terminal.output.text()[dismissed..].contains("\x1b[2K❯ ")).await;
        {
            let state = rpc.state.lock().unwrap();
            assert_eq!(state.sent.len(), 2, "no-dialog terminal plan or navigation must not send an extra prompt");
            assert_eq!(state.updated.len(), 1);
            assert_eq!(state.listens.len(), 1);
            assert!(state.created.is_empty());
            assert!(state.interrupted.is_empty());
            assert_eq!(state.sessions[&initial.id].working_directory, rpc.root.display().to_string());
        }
        terminal.input.end();
    }).await;
    assert_eq!(result.unwrap(), 0);
    assert!(terminal.restored.load(Ordering::Acquire));
    assert!(
        !stop.is_cancelled(),
        "normal EOF must not require the watchdog"
    );
    assert!(terminal.error.text().is_empty());
    rpc.close().await;
}

#[tokio::test]
async fn plan_mode_only_choice_refreshes_retained_modeline_without_send() {
    let mut rpc = Fixture::start().await;
    let initial = wire::UserSession {
        id: "supplied-mode-only-owner".into(),
        model: "provider/model".into(),
        mode: "plan".into(),
        working_directory: rpc.root.display().to_string(),
        ..Default::default()
    };
    rpc.state
        .lock()
        .unwrap()
        .sessions
        .insert(initial.id.clone(), initial.clone());
    let mut terminal = TerminalFixture::new();
    let cli = rpc.cli(terminal.take(), "", Some(initial.clone()));
    let stop = rpc.stopping.clone();
    let result = fixture::run_flow(cli, stop.clone(), terminal.end_input(), async {
        terminal.output_contains("mode: plan").await;
        terminal.output_contains("❯ ").await;
        rpc.wait(|state| state.listens.len() == 1).await;
        for published in [
            event(
                "mode-only-started",
                wire::event::Payload::TurnStarted(wire::TurnStarted::new()),
            ),
            event(
                "mode-only-plan",
                wire::event::Payload::PlanCompleted(wire::PlanCompleted {
                    markdown: "# Mode-only plan".into(),
                    dialog: Some(wire::TurnCompleteDialog {
                        prompt: "Choose build without a prompt:".into(),
                        choices: vec![wire::DialogChoice {
                            value: "build".into(),
                            description: "Change mode only".into(),
                            action: Some(wire::ChoiceAction {
                                mode: "build".into(),
                                prompt: String::new(),
                                ..Default::default()
                            })
                            .into(),
                            ..Default::default()
                        }],
                        ..Default::default()
                    })
                    .into(),
                    ..Default::default()
                }),
            ),
            event(
                "mode-only-completed",
                wire::event::Payload::ModeTurnCompleted(wire::ModeTurnCompleted::new()),
            ),
        ] {
            assert_eq!(published.agent_session_id, "root");
            rpc.publish(&initial.id, published).await;
        }
        terminal
            .output_contains("Choose build without a prompt:")
            .await;
        let output = terminal.output.text();
        assert!(
            output.find("Mode-only plan").unwrap()
                < output.find("Choose build without a prompt:").unwrap()
        );
        assert!(rpc.state.lock().unwrap().sent.is_empty());
        let chosen = terminal.output.text().len();
        terminal.input.type_text("build\r");
        rpc.wait(|state| state.updated.len() == 1).await;
        wait_until(|| terminal.output.text()[chosen..].contains("\x1b[2K❯ ")).await;
        {
            let state = rpc.state.lock().unwrap();
            assert_eq!(state.updated[0].user_session_id, initial.id);
            assert_eq!(state.updated[0].mode, "build");
            assert_eq!(state.sessions[&initial.id].mode, "build");
            assert!(
                state.sent.is_empty(),
                "empty action prompt changes mode without sending"
            );
        }
        let fresh = terminal.output.text().len();
        terminal.columns.store(81, Ordering::Release);
        wait_until(|| {
            let output = terminal.output.text();
            output[fresh..].contains("mode: ") && output[fresh..].contains("\x1b[2K❯ ")
        })
        .await;
        let frame = terminal.output.text()[fresh..].to_owned();
        assert!(
            frame.contains("mode: build"),
            "mode-only approval must refresh the retained modeline without a key: {frame:?}"
        );
        assert!(
            !frame.contains("mode: plan"),
            "fresh mode-only frame must not retain the previous mode: {frame:?}"
        );
        assert!(frame.contains("provider/model"));
        terminal.input.type_text("editable after mode-only");
        terminal.output_contains("❯ editable after mode-only").await;
        let cleared = terminal.output.text().len();
        terminal.input.type_text("\x01\x0b");
        wait_until(|| terminal.output.text()[cleared..].contains("\x1b[2K❯ ")).await;
        {
            let state = rpc.state.lock().unwrap();
            assert!(state.sent.is_empty());
            assert!(state.created.is_empty());
            assert!(state.interrupted.is_empty());
            assert_eq!(state.updated.len(), 1);
            assert_eq!(state.listens.len(), 1);
            assert_eq!(state.listens[0].user_session_id, initial.id);
            assert!(!state.listens[0].replay);
            assert_eq!(state.streams[&initial.id].len(), 1);
            assert!(!state.streams[&initial.id][0].is_closed());
            assert_eq!(
                state.sessions[&initial.id].working_directory,
                rpc.root.display().to_string()
            );
        }
        terminal.input.end();
    })
    .await;
    assert_eq!(result.unwrap(), 0);
    assert!(terminal.restored.load(Ordering::Acquire));
    assert!(
        !stop.is_cancelled(),
        "normal EOF must not require the watchdog"
    );
    assert!(terminal.error.text().is_empty());
    assert!(rpc.state.lock().unwrap().sent.is_empty());
    rpc.close().await;
}

#[tokio::test]
async fn initial_prompt_waits_for_mode_terminal_and_reports_failed_or_invalid_create() {
    for failed in [false, true] {
        let mut rpc = Fixture::start().await;
        let mut terminal = TerminalFixture::new();
        let cli = rpc.cli(terminal.take(), "initial", None);
        let stop = rpc.stopping.clone();
        let result = fixture::run_flow(cli, stop.clone(), terminal.end_input(), async {
            rpc.wait(|state| state.sent.len() == 1).await;
            assert!(!rpc.state.lock().unwrap().created[0].interactive_permissions);
            rpc.publish(
                "session-1",
                event(
                    "started",
                    wire::event::Payload::TurnStarted(wire::TurnStarted {
                        model: "model".into(),
                        ..Default::default()
                    }),
                ),
            )
            .await;
            rpc.publish(
                "session-1",
                event(
                    "turn-ended",
                    wire::event::Payload::TurnEnded(wire::TurnEnded {
                        finish_reason: "stop".into(),
                        ..Default::default()
                    }),
                ),
            )
            .await;
            tokio::time::sleep(Duration::from_millis(50)).await;
            assert!(
                !terminal.restored.load(Ordering::Acquire),
                "ordinary TurnEnded must not terminate mode turn"
            );
            rpc.publish(
                "session-1",
                if failed {
                    event(
                        "failed",
                        wire::event::Payload::TurnFailed(wire::TurnFailed {
                            message: "controlled failure".into(),
                            ..Default::default()
                        }),
                    )
                } else {
                    event(
                        "complete",
                        wire::event::Payload::ModeTurnCompleted(wire::ModeTurnCompleted::new()),
                    )
                },
            )
            .await;
        })
        .await;
        assert_eq!(result.unwrap(), i32::from(failed));
        assert!(terminal.restored.load(Ordering::Acquire));
        rpc.close().await;
    }
    let mut rpc = Fixture::start().await;
    rpc.state.lock().unwrap().create_failure = Some(Status::invalid_argument("bad source model"));
    let mut terminal = TerminalFixture::new();
    let code = rpc
        .cli(terminal.take(), "initial", None)
        .run(rpc.stopping.as_ref())
        .await
        .unwrap();
    assert_eq!(code, 1);
    assert_eq!(terminal.error.text(), "parrot: bad source model\n");
    assert!(rpc.state.lock().unwrap().sent.is_empty());
    rpc.close().await;
}

#[tokio::test]
async fn alias_warnings_sort_and_loaded_banner_only_for_created_loaded_session() {
    for supplied in [false, true] {
        let mut rpc = Fixture::start().await;
        {
            let mut state = rpc.state.lock().unwrap();
            state.loaded_on_create = true;
            state.aliases = ["zeta", "Alpha"]
                .into_iter()
                .map(|name| wire::ModelAlias {
                    name: name.into(),
                    ..Default::default()
                })
                .collect();
        }
        let initial = supplied.then(|| wire::UserSession {
            id: "session-1".into(),
            model: "provider/model".into(),
            mode: "build".into(),
            loaded: true,
            working_directory: rpc.root.to_string_lossy().into_owned(),
            ..Default::default()
        });
        if let Some(session) = &initial {
            rpc.state
                .lock()
                .unwrap()
                .sessions
                .insert(session.id.clone(), session.clone());
        }
        let mut terminal = TerminalFixture::new();
        let cli = rpc.cli(terminal.take(), "", initial);
        let stop = rpc.stopping.clone();
        let result = fixture::run_flow(cli, stop.clone(), terminal.end_input(), async {
            terminal
                .output_contains("warning: model alias \"zeta\" is not configured.")
                .await;
            let output = terminal.output.text();
            assert!(
                output.find("model alias \"Alpha\"").unwrap()
                    < output.find("model alias \"zeta\"").unwrap()
            );
            assert_eq!(output.contains("Loaded session session-1"), !supplied);
            terminal.input.end();
        })
        .await;
        assert_eq!(result.unwrap(), 0);
        rpc.close().await;
    }
}

#[tokio::test]
async fn unavailable_attachment_preparation_never_submits_raw_reference() {
    let mut rpc = Fixture::start().await;
    let mut terminal = TerminalFixture::new();
    let cli = rpc.cli(terminal.take(), "", None);
    let stop = rpc.stopping.clone();
    let result = fixture::run_flow(cli, stop.clone(), terminal.end_input(), async {
        terminal.output_contains("❯ ").await;
        terminal.input.type_text("look @not-authorized.png\r");
        wait_until(|| {
            terminal
                .error
                .text()
                .contains("attachments unavailable without authorized read service")
        })
        .await;
        assert!(rpc.state.lock().unwrap().sent.is_empty());
        terminal.input.end();
    })
    .await;
    assert_eq!(result.unwrap(), 0);
    rpc.close().await;
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
#[test]
fn real_pty_requires_supported_native_terminal_platform() {
    panic!(
        "real PTY terminal acceptance requires native Linux or macOS; target {} is unsupported",
        std::env::consts::OS
    );
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
#[tokio::test]
async fn real_pty_raw_input_output_resize_interrupt_and_stream_failure_restore_termios() {
    for ending in ["eof", "stream-failure", "cooperative-cancellation"] {
        let mut rpc = Fixture::start().await;
        let mut pty = fixture::PtyFixture::new();
        let terminal = pty.take();
        let cli = rpc.cli(terminal, "", None);
        let stop = rpc.stopping.clone();
        let result = fixture::run_flow(cli, stop.clone(), pty.end_input(), async {
            pty.output_contains("❯ ").await;
            pty.type_text("first\r");
            rpc.wait(|state| state.sent.len() == 1).await;
            pty.type_text("draft native");
            pty.output_contains("draft native").await;
            pty.type_text("\x03");
            rpc.wait(|state| state.interrupted.len() == 1).await;
            pty.columns.store(32, Ordering::Release);
            pty.type_text("\r");
            rpc.wait(|state| state.sent.len() == 2).await;
            assert_eq!(rpc.state.lock().unwrap().sent[1].text, "draft native");
            match ending {
                "stream-failure" => {
                    rpc.fail_stream("session-1", Status::unavailable("real PTY stream failure"))
                        .await
                }
                "cooperative-cancellation" => stop.cancel(),
                "eof" => pty.type_text("\x04"),
                _ => unreachable!(),
            }
        })
        .await;
        match ending {
            "stream-failure" => assert!(
                matches!(result, Err(parrot_cli::terminal_dialogs::EnhancedFailure::Rpc(status)) if status.code() == Code::Unavailable && status.message() == "real PTY stream failure")
            ),
            "cooperative-cancellation" => assert!(
                matches!(result, Err(parrot_cli::terminal_dialogs::EnhancedFailure::Rpc(status)) if status.code() == Code::Cancelled)
            ),
            "eof" => assert_eq!(result.unwrap(), 0),
            _ => unreachable!(),
        }
        pty.assert_restored_and_join();
        assert!(pty.output.text().contains("\x1b[?2004h"));
        assert!(pty.output.text().contains("\x1b[?2004l"));
        rpc.close().await;
    }
}
