use super::*;

#[test]
fn pending_asks_precede_work_and_exits_without_touching_lower_priority_errors() {
    for stage in ["active_work", "exit_reminders"] {
        let mut state = MemoryState::new([
            Reply::final_response("candidate"),
            Reply::Failed("stop after feedback"),
        ]);
        state
            .pending_ask_reminders
            .push_back(Some("pending ask".into()));
        state
            .active_work_reminders
            .push_back(Some("active work".into()));
        state.exit_reminders.push_back(Some("exit reminder".into()));
        state.fail_at = Some(stage);
        assert_eq!(state.run(), Err(Error::failed("stop after feedback")));
        assert_eq!(state.provider.requests.lock().unwrap().len(), 2);
        assert_eq!(state.completion_feedback.len(), 1);
        assert_eq!(
            state.completion_feedback[0].kind,
            CompletionFeedbackKind::PendingAsks
        );
        assert_eq!(state.completion_feedback[0].message, "pending ask");
        assert!(state.completion_feedback[0].candidate.is_some());
        assert_eq!(state.active_work_reminders.len(), 1);
        assert_eq!(state.exit_reminders.len(), 1);
        let trace = state.trace.values();
        assert!(!trace.iter().any(|entry| entry == "active_work"
            || entry == "exit_reminders"
            || entry == "complete"));
        assert!(state.open);
    }
}

#[test]
fn active_work_precedes_exit_reminders_without_touching_their_error() {
    let mut state = MemoryState::new([
        Reply::final_response("discarded candidate"),
        Reply::Failed("stop after feedback"),
    ]);
    state
        .active_work_reminders
        .push_back(Some("active work".into()));
    state.exit_reminders.push_back(Some("exit reminder".into()));
    state.fail_at = Some("exit_reminders");
    assert_eq!(state.run(), Err(Error::failed("stop after feedback")));
    assert_eq!(state.completion_feedback.len(), 1);
    assert_eq!(
        state.completion_feedback[0].kind,
        CompletionFeedbackKind::ActiveWork
    );
    assert!(state.completion_feedback[0].candidate.is_none());
    assert_eq!(state.exit_reminders.len(), 1);
    assert!(
        !state
            .trace
            .values()
            .iter()
            .any(|entry| entry == "exit_reminders" || entry == "complete")
    );
    let turn = state.last_turn.as_ref().unwrap();
    assert!(turn.result.is_none());
    assert_eq!(turn.messages[0].role, Role::User);
    assert!(
        turn.messages
            .iter()
            .all(|message| message.role != Role::Assistant)
    );
}

#[test]
fn empty_context_in_each_category_still_requires_a_retry() {
    for kind in [
        CompletionFeedbackKind::PendingAsks,
        CompletionFeedbackKind::ActiveWork,
        CompletionFeedbackKind::ExitReminders,
    ] {
        let mut state = MemoryState::new([
            Reply::final_response("candidate"),
            Reply::final_response("answer"),
        ]);
        match kind {
            CompletionFeedbackKind::PendingAsks => {
                state.pending_ask_reminders.push_back(Some(String::new()))
            }
            CompletionFeedbackKind::ActiveWork => {
                state.active_work_reminders.push_back(Some(String::new()))
            }
            CompletionFeedbackKind::ExitReminders => {
                state.exit_reminders.push_back(Some(String::new()))
            }
        }
        assert_eq!(state.run(), Ok(Outcome::Completed("answer".into())));
        assert_eq!(state.requests.len(), 2);
        assert_eq!(state.completion_feedback.len(), 1);
        assert_eq!(state.completion_feedback[0].kind, kind);
        assert_eq!(state.completion_feedback[0].message, "");
        assert_eq!(
            state.completion_feedback[0].candidate.is_some(),
            kind != CompletionFeedbackKind::ActiveWork
        );
    }
}

#[test]
fn asks_and_exit_reminders_retain_the_exact_candidate_and_force_continuation() {
    for kind in [
        CompletionFeedbackKind::PendingAsks,
        CompletionFeedbackKind::ExitReminders,
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
        match kind {
            CompletionFeedbackKind::PendingAsks => state
                .pending_ask_reminders
                .push_back(Some("pending ask".into())),
            CompletionFeedbackKind::ExitReminders => {
                state.exit_reminders.push_back(Some("exit reminder".into()))
            }
            CompletionFeedbackKind::ActiveWork => unreachable!(),
        }
        assert_eq!(state.run(), Err(Error::failed("forced request")));
        assert_eq!(state.requests.len(), 2);
        assert!(state.pending_messages.is_empty());
        assert_eq!(state.completion_feedback.len(), 1);
        assert_eq!(
            state.completion_feedback[0].candidate,
            Some(candidate.clone())
        );
        let turn = state.last_turn.as_ref().unwrap();
        assert_eq!(
            turn.result.as_ref().unwrap().provider_content,
            candidate.provider_content
        );
        assert_eq!(turn.messages.last().unwrap().role, Role::System);
        assert!(state.open);
        assert!(!state.trace.values().iter().any(|entry| entry == "complete"));
    }
}

#[test]
fn fixed_priority_restarts_budget_and_closes_only_after_all_categories_clear() {
    let mut state = MemoryState::new([
        Reply::final_response("ask candidate"),
        Reply::final_response("work candidate"),
        Reply::final_response("exit candidate"),
        Reply::final_response("accepted answer"),
    ]);
    state.maximum = 1;
    state
        .pending_ask_reminders
        .push_back(Some("pending ask".into()));
    state
        .active_work_reminders
        .push_back(Some("active work".into()));
    state.exit_reminders.push_back(Some("exit reminder".into()));
    assert_eq!(
        state.run(),
        Ok(Outcome::Completed("accepted answer".into()))
    );
    assert_eq!(state.requests.len(), 4);
    assert!(
        state
            .requests
            .iter()
            .all(|(phase, _)| phase.final_phase && phase.enters_final_phase)
    );
    assert_eq!(
        state
            .completion_feedback
            .iter()
            .map(|feedback| feedback.kind)
            .collect::<Vec<_>>(),
        [
            CompletionFeedbackKind::PendingAsks,
            CompletionFeedbackKind::ActiveWork,
            CompletionFeedbackKind::ExitReminders
        ]
    );
    assert!(state.completion_feedback[0].candidate.is_some());
    assert!(state.completion_feedback[1].candidate.is_none());
    assert!(state.completion_feedback[2].candidate.is_some());
    let trace = state.trace.values();
    assert_eq!(trace.iter().filter(|entry| *entry == "begin").count(), 1);
    assert_eq!(trace.iter().filter(|entry| *entry == "complete").count(), 1);
    assert_eq!(trace.last().unwrap(), "complete");
    let turn = state.last_turn.as_ref().unwrap();
    let assistant_texts = turn
        .messages
        .iter()
        .filter(|message| message.role == Role::Assistant)
        .map(|message| message.contents.clone())
        .collect::<Vec<_>>();
    assert_eq!(
        assistant_texts,
        [
            vec![Content::Text("ask candidate".into())],
            vec![Content::Text("exit candidate".into())],
            vec![Content::Text("accepted answer".into())]
        ]
    );
    assert!(!state.open);
}

#[test]
fn feedback_recording_errors_stop_before_applying_context_or_restarting_budget() {
    let mut state = MemoryState::new([Reply::final_response("candidate")]);
    state
        .pending_ask_reminders
        .push_back(Some("pending ask".into()));
    state.maximum = 1;
    state.fail_at = Some("completion_feedback");
    let tools = state.tools.clone();
    let provider = state.provider.clone();
    let source = state.cancellation.clone();
    let is_cancelled = || source.is_cancelled();
    let cancelled = || source.cancelled();
    let mut budget = TurnBudget::new(false);
    assert_eq!(
        finish(run_turn(
            &mut state,
            &tools,
            &provider,
            &mut budget,
            CancellationToken::new(&is_cancelled, &cancelled)
        )),
        Err(Error::failed("completion_feedback"))
    );
    assert_eq!(state.requests.len(), 1);
    assert!(state.completion_feedback.is_empty());
    let turn = state.last_turn.as_ref().unwrap();
    assert_eq!(turn.messages.len(), 1);
    assert_eq!(turn.messages[0].role, Role::User);
    assert!(turn.result.is_none());
    assert!(state.open);
    assert_eq!(state.trace.values().last().unwrap(), "completion_feedback");
    assert!(budget.admit(1, false).is_none());
}
