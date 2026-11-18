use super::*;

fn waiting_reply(name: &str) -> Reply {
    Reply::with_tool_calls(vec![ToolCall {
        id: "waiting-call".into(),
        name: name.into(),
        arguments_json: "{}".into(),
    }])
}

fn system_messages(request: &ProviderRequest) -> Vec<&str> {
    request
        .messages
        .iter()
        .filter(|message| message.role == Role::System)
        .filter_map(|message| match message.contents.as_slice() {
            [Content::Text(text)] => Some(text.as_str()),
            _ => None,
        })
        .collect()
}

#[test]
fn three_identical_steps_steer_a_stall_notice_before_the_next_request() {
    let mut state = MemoryState::new([
        Reply::tools("call"),
        Reply::tools("call"),
        Reply::tools("call"),
        Reply::tools("call"),
        Reply::final_response("answer"),
    ]);

    assert_eq!(state.run(), Ok(Outcome::Completed("answer".into())));
    assert_eq!(state.stall_steered, ["this is stall 1"]);

    let requests = state.provider.requests.lock().unwrap();
    for request in &requests[0..4] {
        assert!(system_messages(request).is_empty());
    }
    assert_eq!(
        system_messages(&requests[4]),
        ["this is stall 1"],
        "the stall notice steers the request after the third identical step"
    );
}

#[test]
fn a_further_identical_step_reports_the_next_stall_index() {
    let mut state = MemoryState::new([
        Reply::tools("call"),
        Reply::tools("call"),
        Reply::tools("call"),
        Reply::tools("call"),
        Reply::tools("call"),
        Reply::final_response("answer"),
    ]);

    assert_eq!(state.run(), Ok(Outcome::Completed("answer".into())));
    assert_eq!(state.stall_steered, ["this is stall 1", "this is stall 2"]);
}

#[test]
fn different_step_content_does_not_stall() {
    let mut state = MemoryState::new([
        Reply::tools("call"),
        Reply::tools("other"),
        Reply::tools("call"),
        Reply::final_response("answer"),
    ]);

    assert_eq!(state.run(), Ok(Outcome::Completed("answer".into())));
    assert!(state.stall_steered.is_empty());
}

#[test]
fn a_steered_step_resets_the_stall_run() {
    let mut state = MemoryState::new([
        Reply::tools("call"),
        Reply::tools("call"),
        Reply::tools("call"),
        Reply::tools("call"),
        Reply::final_response("answer"),
    ]);
    state.steers_after_results.push_back("steer".into());

    assert_eq!(state.run(), Ok(Outcome::Completed("answer".into())));
    assert!(state.stall_steered.is_empty());
}

#[test]
fn wait_and_monitor_calls_reset_the_stall_run() {
    for name in ["wait", "monitor"] {
        let mut state = MemoryState::new([
            Reply::tools("call"),
            Reply::tools("call"),
            waiting_reply(name),
            Reply::final_response("answer"),
        ]);

        assert_eq!(state.run(), Ok(Outcome::Completed("answer".into())));
        assert!(state.stall_steered.is_empty(), "{name} resets the run");
    }
}

#[test]
fn the_configured_threshold_controls_when_a_stall_fires() {
    let mut state = MemoryState::new([
        Reply::tools("call"),
        Reply::tools("call"),
        Reply::tools("call"),
        Reply::final_response("answer"),
    ]);
    state.stall_steps = 1;

    assert_eq!(state.run(), Ok(Outcome::Completed("answer".into())));
    assert_eq!(state.stall_steered, ["this is stall 1"]);
}

#[test]
fn the_stall_run_survives_a_turn_boundary() {
    let mut state = MemoryState::new([
        Reply::tools("call"),
        Reply::tools("call"),
        Reply::tools("call"),
        Reply::tools("call"),
        Reply::tools("call"),
    ]);
    state.maximum = 1;

    assert_eq!(
        state.run(),
        Ok(Outcome::BudgetExhausted(RUNAWAY_NOTICE.into()))
    );
    assert_eq!(state.stall_steered, ["this is stall 1"]);

    state
        .provider
        .replies
        .lock()
        .unwrap()
        .extend([Reply::tools("call"), Reply::final_response("answer")]);

    assert_eq!(state.run(), Ok(Outcome::Completed("answer".into())));
    assert_eq!(
        state.stall_steered,
        ["this is stall 1", "this is stall 2", "this is stall 3"],
        "the persisted run steers the first request of the next turn"
    );
}
