use super::*;

#[test]
fn ordered_blocker_messages_are_recorded_together_and_retain_the_candidate_once() {
    let mut state = MemoryState::new([
        Reply::final_response("candidate"),
        Reply::Failed("stop after feedback"),
    ]);
    let messages = vec!["zeta".into(), "alpha".into(), "zeta".into()];
    state.prompts().exit_blockers.push_back(messages.clone());
    assert_eq!(state.run(), Err(Error::failed("stop after feedback")));
    assert_eq!(state.provider.requests.lock().unwrap().len(), 2);
    assert_eq!(state.completion_blocker_feedback.len(), 1);
    assert_eq!(state.completion_blocker_feedback[0].messages, messages);
    assert_eq!(
        state.completion_blocker_feedback[0]
            .candidate
            .assistant_text,
        "candidate"
    );
    let turn = state.last_turn.as_ref().unwrap();
    assert_eq!(turn.messages.len(), 5);
    assert_eq!(turn.messages[0].role, Role::User);
    assert_eq!(turn.messages[1].role, Role::Assistant);
    assert_eq!(
        turn.messages[2..]
            .iter()
            .map(|message| (message.role, message.contents.clone()))
            .collect::<Vec<_>>(),
        messages
            .into_iter()
            .map(|message| (Role::System, vec![Content::Text(message)]))
            .collect::<Vec<_>>()
    );
    assert_eq!(
        state
            .trace
            .values()
            .iter()
            .filter(|entry| *entry == "exit_blockers")
            .count(),
        1
    );
    assert!(!state.trace.values().iter().any(|entry| entry == "complete"));
    assert_eq!(
        state
            .trace
            .values()
            .iter()
            .filter(|step| *step == "catalog")
            .count(),
        2
    );
}

#[test]
fn an_empty_blocker_list_closes_the_candidate_without_reading_another_snapshot() {
    let mut state = MemoryState::new([Reply::final_response("accepted answer")]);
    state.prompts().exit_blockers.push_back(Vec::new());
    state
        .prompts()
        .exit_blockers
        .push_back(vec!["not observed".into()]);
    assert_eq!(
        state.run(),
        Ok(Outcome::Completed("accepted answer".into()))
    );
    assert_eq!(state.request().rendered_instructions.len(), 1);
    assert!(state.completion_blocker_feedback.is_empty());
    assert_eq!(state.prompts().exit_blockers.len(), 1);
    assert_eq!(
        state
            .trace
            .values()
            .iter()
            .filter(|entry| *entry == "exit_blockers")
            .count(),
        1
    );
    assert_eq!(state.trace.values().last().unwrap(), "complete");
    assert_eq!(
        state
            .trace
            .values()
            .iter()
            .filter(|step| *step == "complete")
            .count(),
        1
    );
}

#[test]
fn nonempty_lists_with_empty_messages_still_require_a_retry() {
    for messages in [
        vec![String::new()],
        vec![String::new(), String::new()],
        vec![String::new(), "unfinished".into(), String::new()],
    ] {
        let mut state = MemoryState::new([
            Reply::final_response("candidate"),
            Reply::final_response("answer"),
        ]);
        state.prompts().exit_blockers.push_back(messages.clone());
        assert_eq!(state.run(), Ok(Outcome::Completed("answer".into())));
        assert_eq!(state.request().rendered_instructions.len(), 2);
        assert_eq!(state.completion_blocker_feedback.len(), 1);
        assert_eq!(state.completion_blocker_feedback[0].messages, messages);
        assert_eq!(
            state.completion_blocker_feedback[0]
                .candidate
                .assistant_text,
            "candidate"
        );
    }
}

#[test]
fn every_blocker_list_retains_the_exact_candidate_and_forces_continuation() {
    for messages in [
        vec!["unfinished".into()],
        vec!["first".into(), "second".into()],
    ] {
        let candidate = ProviderResponse {
            assistant_text: "retained candidate".into(),
            finish_reason: FinishReason::Stop,
            provider_id: "raw-provider".into(),
            provider_content: " { \"raw\" : [1.2300, true] } ".into(),
            ..ProviderResponse::default()
        };
        let mut state = MemoryState::new([
            Reply::Response(candidate.clone()),
            Reply::Failed("forced request"),
        ]);
        state.prompts().exit_blockers.push_back(messages.clone());
        assert_eq!(state.run(), Err(Error::failed("forced request")));
        assert_eq!(state.request().rendered_instructions.len(), 2);
        assert!(state.pending_messages.is_empty());
        assert_eq!(state.completion_blocker_feedback.len(), 1);
        assert_eq!(state.completion_blocker_feedback[0].messages, messages);
        assert_eq!(state.completion_blocker_feedback[0].candidate, candidate);
        let turn = state.last_turn.as_ref().unwrap();
        assert_eq!(
            turn.messages
                .iter()
                .find(|message| message.role == Role::Assistant)
                .unwrap()
                .provider_content,
            candidate.provider_content
        );
        assert_eq!(turn.messages.last().unwrap().role, Role::System);
        assert_eq!(
            state
                .trace
                .values()
                .iter()
                .filter(|step| *step == "catalog")
                .count(),
            2
        );
        assert!(!state.trace.values().iter().any(|entry| entry == "complete"));
    }
}

#[test]
fn blocker_snapshots_restart_budget_and_close_only_after_the_list_clears() {
    let mut state = MemoryState::new([
        Reply::final_response("first candidate"),
        Reply::final_response("second candidate"),
        Reply::final_response("third candidate"),
        Reply::final_response("accepted answer"),
    ]);
    state.maximum = 1;
    let snapshots = vec![
        vec!["first blocker".into(), "second blocker".into()],
        vec![String::new()],
        vec!["last blocker".into()],
    ];
    state.prompts().exit_blockers.extend(snapshots.clone());
    assert_eq!(
        state.run(),
        Ok(Outcome::Completed("accepted answer".into()))
    );
    assert_eq!(state.request().rendered_instructions.len(), 4);
    assert_eq!(state.final_requests, [1, 2, 3, 4]);
    assert!(
        state
            .capture_catalogs()
            .iter()
            .all(|tools| tools == &["read"])
    );
    assert_eq!(
        state
            .completion_blocker_feedback
            .iter()
            .map(|feedback| feedback.messages.clone())
            .collect::<Vec<_>>(),
        snapshots
    );
    assert_eq!(
        state
            .completion_blocker_feedback
            .iter()
            .map(|feedback| feedback.candidate.assistant_text.as_str())
            .collect::<Vec<_>>(),
        ["first candidate", "second candidate", "third candidate"]
    );
    let trace = state.trace.values();
    assert_eq!(trace.iter().filter(|entry| *entry == "catalog").count(), 4);
    assert_eq!(trace.iter().filter(|entry| *entry == "complete").count(), 1);
    assert_eq!(
        trace
            .iter()
            .filter(|entry| *entry == "exit_blockers")
            .count(),
        4
    );
    assert_eq!(trace.last().unwrap(), "complete");
    let turn = state.last_turn.as_ref().unwrap();
    assert_eq!(
        turn.messages
            .iter()
            .filter(|message| message.role == Role::Assistant)
            .map(|message| message.contents.clone())
            .collect::<Vec<_>>(),
        [
            vec![Content::Text("first candidate".into())],
            vec![Content::Text("second candidate".into())],
            vec![Content::Text("third candidate".into())],
            vec![Content::Text("accepted answer".into())]
        ]
    );
    assert_eq!(
        turn.messages.last().unwrap().contents,
        [Content::Text("accepted answer".into())]
    );
}

#[test]
fn feedback_recording_errors_stop_before_applying_context_or_restarting_budget() {
    let mut state = MemoryState::new([Reply::final_response("candidate")]);
    state
        .prompts()
        .exit_blockers
        .push_back(vec!["unfinished".into()]);
    state.maximum = 1;
    state.fail_at = Some("completion_blocker_feedback");
    let tools = state.tools.clone();
    let provider = state.provider.clone();

    state.budget = AgentState::new(false);

    assert_eq!(
        finish(run_turn(
            &mut state,
            &tools,
            &provider,
            CancellationToken::new(provider.cancellation.as_ref())
        )),
        Err(Error::failed("completion_blocker_feedback"))
    );
    assert_eq!(state.request().rendered_instructions.len(), 1);
    assert!(state.completion_blocker_feedback.is_empty());
    let turn = state.last_turn.as_ref().unwrap();
    assert_eq!(turn.messages.len(), 1);
    assert_eq!(turn.messages[0].role, Role::User);
    assert!(turn.needs_response());
    assert_eq!(
        state
            .trace
            .values()
            .iter()
            .filter(|step| *step == "catalog")
            .count(),
        1
    );
    assert!(!state.trace.values().iter().any(|step| step == "complete"));
    assert_eq!(
        state.trace.values().last().unwrap(),
        "completion_blocker_feedback"
    );
    assert!(state.budget.admit(1).is_none());
}

#[test]
fn blocker_read_errors_stop_before_recording_closure_or_budget_reset() {
    let mut state = MemoryState::new([Reply::final_response("candidate")]);
    state
        .prompts()
        .exit_blockers
        .push_back(vec!["not read".into()]);
    state.maximum = 1;
    state.fail_at = Some("exit_blockers");
    let tools = state.tools.clone();
    let provider = state.provider.clone();

    state.budget = AgentState::new(false);

    assert_eq!(
        finish(run_turn(
            &mut state,
            &tools,
            &provider,
            CancellationToken::new(provider.cancellation.as_ref())
        )),
        Err(Error::failed("exit_blockers"))
    );
    assert_eq!(state.request().rendered_instructions.len(), 1);
    assert!(state.completion_blocker_feedback.is_empty());
    assert_eq!(state.prompts().exit_blockers.len(), 1);
    assert_eq!(state.last_turn.as_ref().unwrap().messages.len(), 1);
    assert_eq!(
        state
            .trace
            .values()
            .iter()
            .filter(|step| *step == "catalog")
            .count(),
        1
    );
    assert!(!state.trace.values().iter().any(|step| step == "complete"));
    assert_eq!(state.trace.values().last().unwrap(), "exit_blockers");
    assert!(state.budget.admit(1).is_none());
}
