use std::sync::Arc;

use parrot_cli::presenters::{AgentSessionState, ToolTerminalStatus};
use parrot_domain::protocol::messages::parrot as wire;

#[test]
fn response_is_sanitized_bounded_to_ten_source_lines_and_terminal_once() {
    let mut state = AgentSessionState::new("worker");
    assert!(state.start_turn(None));
    assert!(!state.start_turn(None));
    state.collect_response(&format!(
        "\x1b[2J{}",
        (1..=12)
            .map(|line| format!("line-{line}"))
            .collect::<Vec<_>>()
            .join("\n")
    ));
    let response = state.finish_child_turn().unwrap();
    assert_eq!(response.lines().count(), 10);
    assert!(response.starts_with("[2Jline-1\n"));
    assert!(response.ends_with("line-10"));
    assert!(!response.contains('\x1b'));
    state.collect_response("late text");
    assert!(state.drain_response().is_none());
    assert!(state.finish_child_turn().is_none());
}

#[test]
fn tool_chunks_preserve_latest_name_and_stop_after_terminal() {
    let mut state = AgentSessionState::new("worker");
    state.collect_tool_call(&wire::ToolCallChunk {
        tool_call_id: "call".into(),
        tool_name: "read".into(),
        arguments_fragment: "{\"path\":".into(),
        ..Default::default()
    });
    state.collect_tool_call(&wire::ToolCallChunk {
        tool_call_id: "call".into(),
        arguments_fragment: "\"file\"}".into(),
        ..Default::default()
    });
    assert!(state.start_tool(
        &wire::ToolStarted {
            tool_call_id: "call".into(),
            tool_name: "read".into(),
            ..Default::default()
        },
        false
    ));
    assert!(!state.start_tool(
        &wire::ToolStarted {
            tool_call_id: "call".into(),
            ..Default::default()
        },
        false
    ));
    let event = wire::Event {
        agent_session_id: "worker".into(),
        payload: Some(wire::event::Payload::ToolFinished(wire::ToolFinished {
            tool_call_id: "call".into(),
            tool_name: "read".into(),
            result: Some("".into()),
            ..Default::default()
        })),
        ..Default::default()
    };
    let (call, terminal) = state
        .finish_tool(&event, true, Arc::new(str::to_owned))
        .unwrap();
    assert_eq!(call.arguments_json, "{\"path\":\"file\"}");
    assert!(terminal.result_present);
    assert!(terminal.result.is_empty());
    assert!(terminal.start_omitted);
    assert_eq!(terminal.status, ToolTerminalStatus::Succeeded);
    assert!(!state.is_tool_active("call"));
    state.collect_tool_call(&wire::ToolCallChunk {
        tool_call_id: "call".into(),
        arguments_fragment: "must not reappear".into(),
        ..Default::default()
    });
    assert!(!state.start_tool(
        &wire::ToolStarted {
            tool_call_id: "call".into(),
            ..Default::default()
        },
        false
    ));
    assert!(
        state
            .tool_call("call", Arc::new(str::to_owned))
            .arguments_json
            .is_empty()
    );
}

#[test]
fn folded_tools_follow_latest_start_and_remove_at_terminal() {
    let mut state = AgentSessionState::new("worker");
    state.start_turn(None);
    for (id, name) in [("a", "status"), ("b", "question")] {
        state.start_tool(
            &wire::ToolStarted {
                tool_call_id: id.into(),
                tool_name: name.into(),
                ..Default::default()
            },
            true,
        );
    }
    assert_eq!(state.agent_label(), "Working: question");
    assert!(state.is_folded_activity("tool:b"));
    let event = wire::Event {
        payload: Some(wire::event::Payload::ToolCancelled(wire::ToolCancelled {
            tool_call_id: "b".into(),
            tool_name: "question".into(),
            ..Default::default()
        })),
        ..Default::default()
    };
    let (_, terminal) = state
        .finish_tool(&event, false, Arc::new(str::to_owned))
        .unwrap();
    assert_eq!(terminal.status, ToolTerminalStatus::Cancelled);
    assert_eq!(state.agent_label(), "Working: status");
}

#[test]
fn request_phase_cannot_resurrect_idle_activity() {
    let mut state = AgentSessionState::new("worker");
    let phase = wire::ProviderRequestPhaseChangedEvent {
        phase: wire::ProviderRequestPhase::PROVIDER_REQUEST_PHASE_REQUESTING.into(),
        attempt: 0,
        ..Default::default()
    };
    state.observe_request_phase(&phase);
    assert_eq!(state.agent_spinner_text(), "");
    state.start_turn(None);
    state.observe_request_phase(&phase);
    assert_eq!(state.agent_spinner_text(), "Requesting…");
    state.observe_request_phase(&wire::ProviderRequestPhaseChangedEvent {
        attempt: 3,
        ..phase
    });
    assert_eq!(state.agent_spinner_text(), "Requesting (attempt 3)…");
    state.observe_request_phase(&wire::ProviderRequestPhaseChangedEvent {
        phase: wire::ProviderRequestPhase::PROVIDER_REQUEST_PHASE_HEADERS_RECEIVED.into(),
        ..Default::default()
    });
    assert_eq!(state.agent_spinner_text(), "Waiting for first token…");
    state.finish_child_turn();
    state.observe_request_phase(&wire::ProviderRequestPhaseChangedEvent {
        phase: wire::ProviderRequestPhase::PROVIDER_REQUEST_PHASE_HEADERS_RECEIVED.into(),
        ..Default::default()
    });
    assert_eq!(state.agent_spinner_text(), "");
}

#[test]
fn progress_revision_and_terminal_retirement_preserve_failed_or_empty_graphs() {
    let mut state = AgentSessionState::new("worker");
    let make = |revision, status| wire::AgentTaskProgressSnapshot {
        revision,
        root_nodes: vec![wire::AgentTaskProgressNode {
            name: "task".into(),
            status,
            ..Default::default()
        }],
        ..Default::default()
    };
    assert!(state.offer_agent_task_progress(&make(
        2,
        wire::AgentTaskProgressStatus::AGENT_TASK_PROGRESS_STATUS_FAILED.into()
    )));
    assert!(!state.offer_agent_task_progress(&make(
        2,
        wire::AgentTaskProgressStatus::AGENT_TASK_PROGRESS_STATUS_SUCCEEDED.into()
    )));
    assert!(!state.offer_agent_task_progress(&make(
        1,
        wire::AgentTaskProgressStatus::AGENT_TASK_PROGRESS_STATUS_SUCCEEDED.into()
    )));
    assert!(!state.retire_agent_task_progress(2));
    assert!(state.offer_agent_task_progress(&make(
        3,
        wire::AgentTaskProgressStatus::AGENT_TASK_PROGRESS_STATUS_SUCCEEDED.into()
    )));
    assert!(!state.retire_agent_task_progress(2));
    assert!(state.retire_agent_task_progress(3));
    assert!(!state.retire_agent_task_progress(3));
    assert!(!state.offer_agent_task_progress(&make(
        3,
        wire::AgentTaskProgressStatus::AGENT_TASK_PROGRESS_STATUS_RUNNING.into()
    )));
    assert!(
        state.offer_agent_task_progress(&wire::AgentTaskProgressSnapshot {
            revision: 4,
            ..Default::default()
        })
    );
    assert!(!state.retire_agent_task_progress(4));
}
