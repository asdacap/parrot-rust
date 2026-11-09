use super::*;

#[test]
fn no_turn_and_completed_turns_without_pending_input_do_not_invoke_the_provider() {
    let mut empty = MemoryState::new([]);
    empty.pending_messages.clear();
    assert_eq!(empty.run(), Ok(Outcome::Completed(String::new())));
    assert!(empty.trace.values().is_empty());

    let mut completed = MemoryState::new([]);
    completed.pending_messages.clear();
    let response = ProviderResponse {
        assistant_text: "previous answer".into(),
        ..ProviderResponse::default()
    };
    completed.append_response(&response);
    completed.append_turn_message(Message {
        role: Role::System,
        contents: vec![Content::Text("trailing system context".into())],
        ..Message::default()
    });
    let before = finish(completed.get_last_turn()).unwrap().unwrap();
    assert_eq!(before.result.as_ref().unwrap().role, Role::Assistant);
    assert_eq!(completed.run(), Ok(Outcome::Completed(String::new())));
    assert_eq!(finish(completed.get_last_turn()).unwrap(), Some(before));
    assert!(completed.trace.values().is_empty());
    assert!(completed.provider.requests.lock().unwrap().is_empty());
}

#[test]
fn a_missing_result_without_a_user_or_tool_tail_does_not_start_execution() {
    for message in [
        Message {
            role: Role::System,
            contents: vec![Content::Text("context only".into())],
            ..Message::default()
        },
        Message {
            role: Role::Assistant,
            tool_calls: vec![ToolCall {
                id: "historical-call".into(),
                name: "read".into(),
                arguments_json: "{}".into(),
            }],
            ..Message::default()
        },
    ] {
        let mut state = MemoryState::new([]);
        state.pending_messages.clear();
        state.append_turn_message(message);
        assert!(state.last_turn.as_ref().unwrap().result.is_none());
        assert_eq!(state.run(), Ok(Outcome::Completed(String::new())));
        assert!(state.trace.values().is_empty());
        assert!(state.provider.requests.lock().unwrap().is_empty());
        assert!(state.tools.requests.lock().unwrap().is_empty());
    }
}

#[test]
fn visible_user_and_tool_results_require_a_response_without_pending_input() {
    for role in [Role::User, Role::Tool] {
        let mut state = MemoryState::new([Reply::final_response("answer")]);
        state.pending_messages.clear();
        state.append_turn_message(Message {
            role,
            contents: vec![Content::Text("response needed".into())],
            tool_call_id: if role == Role::Tool {
                "existing-call".into()
            } else {
                String::new()
            },
            ..Message::default()
        });
        state.append_turn_message(Message {
            role: Role::System,
            contents: vec![Content::Text("context only".into())],
            ..Message::default()
        });
        state.request().messages = state.last_turn.as_ref().unwrap().messages.clone();
        assert_eq!(state.run(), Ok(Outcome::Completed("answer".into())));
        assert_eq!(state.admissions, [InputAdmission::SteersOnly]);
        assert_eq!(state.provider.requests.lock().unwrap().len(), 1);
        assert!(state.tools.requests.lock().unwrap().is_empty());
    }
}

#[test]
fn pending_message_is_a_stable_peek_and_admits_input_after_a_completed_turn() {
    let mut state = MemoryState::new([Reply::final_response("new answer")]);
    state.append_response(&ProviderResponse {
        assistant_text: "old answer".into(),
        ..ProviderResponse::default()
    });
    let pending = finish(state.get_pending_message()).unwrap().unwrap();
    assert_eq!(pending.delivery, Delivery::Queue);
    assert!(!pending.input_id.is_empty());
    assert!(!pending.message_id.is_empty());
    assert_eq!(finish(state.get_pending_message()).unwrap(), Some(pending));
    assert_eq!(state.pending_messages.len(), 1);
    assert!(state.trace.values().is_empty());
    assert_eq!(state.run(), Ok(Outcome::Completed("new answer".into())));
    assert_eq!(state.admissions, [InputAdmission::SteersOrNextQueued]);
    assert!(state.pending_messages.is_empty());
}

#[test]
fn factual_getter_failures_stop_before_turn_or_provider_effects() {
    for stage in ["last_turn", "pending_message"] {
        let mut state = MemoryState::new([Reply::final_response("not invoked")]);
        state.fail_at = Some(stage);
        assert_eq!(state.run(), Err(Error::failed(stage)));
        assert_eq!(state.trace.values(), [stage]);
        assert_eq!(state.pending_messages.len(), 1);
        assert!(state.provider.requests.lock().unwrap().is_empty());
        assert!(state.tools.requests.lock().unwrap().is_empty());
    }
}

#[test]
fn run_turn_completes_one_turn_at_a_time_and_reuses_the_existing_budget() {
    let mut state = MemoryState::new([
        Reply::final_response("first answer"),
        Reply::final_response("second answer"),
    ]);
    state.maximum = 1;
    state.enqueue_message(Delivery::Queue, "second question");
    let tools = state.tools.clone();
    let provider = state.provider.clone();
    let source = state.cancellation.clone();
    let is_cancelled = || source.is_cancelled();
    let cancelled = || source.cancelled();
    let token = CancellationToken::new(&is_cancelled, &cancelled);
    let options = state.capture_options();
    let mut budget = TurnBudget::new(false);
    let mut preparation = state.preparation();
    assert_eq!(
        finish(run_turn(
            &mut state,
            &mut preparation,
            &tools,
            &provider,
            &options,
            &mut budget,
            token
        )),
        Ok(Outcome::Completed("first answer".into()))
    );
    assert_eq!(state.provider.requests.lock().unwrap().len(), 1);
    assert_eq!(state.pending_messages.len(), 1);
    assert_eq!(state.final_requests, [0]);
    let mut preparation = state.preparation();
    assert_eq!(
        finish(run_turn(
            &mut state,
            &mut preparation,
            &tools,
            &provider,
            &options,
            &mut budget,
            token
        )),
        Ok(Outcome::Completed("second answer".into()))
    );
    assert_eq!(state.provider.requests.lock().unwrap().len(), 2);
    assert!(state.pending_messages.is_empty());
    assert_eq!(state.capture_catalogs()[1], ["read"]);
    assert_eq!(state.final_requests, [0]);
    let mut preparation = state.preparation();
    assert_eq!(
        finish(run(
            &mut state,
            &mut preparation,
            &tools,
            &provider,
            &options,
            &mut budget,
            token
        )),
        Ok(Outcome::Completed(String::new()))
    );
    assert_eq!(state.provider.requests.lock().unwrap().len(), 2);
}

#[test]
fn run_convenience_drains_real_pending_messages_and_returns_the_latest_answer() {
    let mut state = MemoryState::new([
        Reply::final_response("first answer"),
        Reply::final_response("second answer"),
    ]);
    state.enqueue_message(Delivery::Queue, "second question");
    assert_eq!(state.run(), Ok(Outcome::Completed("second answer".into())));
    assert!(state.pending_messages.is_empty());
    assert_eq!(state.provider.requests.lock().unwrap().len(), 2);
    assert_eq!(
        state
            .trace
            .values()
            .iter()
            .filter(|step| *step == "catalog")
            .count(),
        2
    );
    assert_eq!(
        state
            .trace
            .values()
            .iter()
            .filter(|step| *step == "complete")
            .count(),
        2
    );
}

#[test]
fn admission_uses_actual_turn_facts_after_native_preparation() {
    let mut state = MemoryState::new([Reply::final_response("answer")]);
    state.append_response(&ProviderResponse {
        assistant_text: "old answer".into(),
        ..ProviderResponse::default()
    });
    state.append_turn_message(Message {
        role: Role::User,
        contents: vec![Content::Text("status added a new input".into())],
        ..Message::default()
    });
    let pending = finish(state.get_pending_message()).unwrap();
    let tools = state.tools.clone();
    let provider = state.provider.clone();
    let source = state.cancellation.clone();
    let is_cancelled = || source.is_cancelled();
    let cancelled = || source.cancelled();
    let options = state.capture_options();
    let mut budget = TurnBudget::new(false);
    let mut preparation = state.preparation();
    assert_eq!(
        finish(run_turn(
            &mut state,
            &mut preparation,
            &tools,
            &provider,
            &options,
            &mut budget,
            CancellationToken::new(&is_cancelled, &cancelled),
        )),
        Ok(Outcome::Completed("answer".into()))
    );
    assert_eq!(state.admissions, [InputAdmission::SteersOnly]);
    assert_eq!(finish(state.get_pending_message()).unwrap(), pending);
    assert_eq!(state.pending_messages.len(), 1);
}

#[test]
fn steer_admission_is_batched_without_consuming_queued_input_mid_turn() {
    let mut state = MemoryState::new([Reply::final_response("answer")]);
    state.append_turn_message(Message {
        role: Role::Tool,
        contents: vec![Content::Text("existing tool result".into())],
        tool_call_id: "existing-call".into(),
        ..Message::default()
    });
    state.enqueue_message(Delivery::Steer, "first steer");
    state.enqueue_message(Delivery::Steer, "second steer");
    let steer = finish(state.get_pending_message()).unwrap().unwrap();
    assert_eq!(steer.delivery, Delivery::Steer);
    let tools = state.tools.clone();
    let provider = state.provider.clone();
    let source = state.cancellation.clone();
    let is_cancelled = || source.is_cancelled();
    let cancelled = || source.cancelled();
    let options = state.capture_options();
    let mut budget = TurnBudget::new(false);
    let mut preparation = state.preparation();
    assert_eq!(
        finish(run_turn(
            &mut state,
            &mut preparation,
            &tools,
            &provider,
            &options,
            &mut budget,
            CancellationToken::new(&is_cancelled, &cancelled),
        )),
        Ok(Outcome::Completed("answer".into()))
    );
    assert_eq!(state.admissions, [InputAdmission::SteersOnly]);
    assert_eq!(state.pending_messages.len(), 1);
    assert_eq!(state.pending_messages[0].0.delivery, Delivery::Queue);
    let turn = state.last_turn.as_ref().unwrap();
    assert_eq!(
        turn.messages[0].contents,
        [Content::Text("second steer".into())]
    );
    assert_eq!(turn.result.as_ref().unwrap().role, Role::Assistant);
}

#[test]
fn exit_blockers_retain_the_candidate_and_continue_without_pending_input() {
    let mut state = MemoryState::new([
        Reply::final_response("candidate"),
        Reply::final_response("answer"),
    ]);
    state.exit_blockers.push_back(vec!["work remains".into()]);
    assert_eq!(state.run(), Ok(Outcome::Completed("answer".into())));
    assert_eq!(state.provider.requests.lock().unwrap().len(), 2);
    assert_eq!(state.completion_feedback.len(), 1);
    assert_eq!(
        state.completion_feedback[0].candidate.assistant_text,
        "candidate"
    );
    assert!(state.pending_messages.is_empty());
    assert_eq!(
        state
            .trace
            .values()
            .iter()
            .filter(|step| *step == "catalog")
            .count(),
        1
    );
}

#[test]
fn promotion_returns_exact_multimodal_input_without_request_preparation() {
    let mut state = MemoryState::new([]);
    state.pending_messages.clear();
    state.fail_at = Some("render");
    let message = Message {
        role: Role::User,
        contents: vec![
            Content::Text("use $selected before the image".into()),
            Content::Image {
                path: "images/input.webp".into(),
                url: "https://example.test/input.webp".into(),
                media_type: "image/webp".into(),
                width: 17,
                height: 29,
                bytes: Some(vec![0, 127, 255]),
            },
            Content::Text("after the image".into()),
        ],
        provider_id: "native-provider".into(),
        provider_content: " { \"opaque\" : 1.2300 } ".into(),
        ..Message::default()
    };
    state.pending_messages.push_back((
        PendingMessage {
            input_id: "promoted-input".into(),
            message_id: "original-message".into(),
            delivery: Delivery::Steer,
        },
        message.clone(),
    ));

    assert_eq!(
        finish(state.promote_input(InputAdmission::SteersOnly)),
        Ok(vec![PromotedInput {
            input_id: "promoted-input".into(),
            message_id: "original-message".into(),
            message: message.clone(),
        }])
    );
    assert!(state.pending_messages.is_empty());
    assert_eq!(state.last_turn.as_ref().unwrap().messages, [message]);
    assert!(state.prepared_inputs.lock().unwrap().is_empty());
    assert!(state.request().rendered_instructions.is_empty());
    assert!(state.recorded_input_contexts.is_empty());
    assert!(state.provider.requests.lock().unwrap().is_empty());
}

#[test]
fn promoted_inputs_are_supplied_to_preparation_and_derived_context_reaches_the_provider() {
    let mut state = MemoryState::new([Reply::final_response("answer")]);
    state.pending_messages.clear();
    state.enqueue_message(Delivery::Queue, "queued for another turn");
    state.enqueue_message(Delivery::Steer, "use $first");
    state.enqueue_message(Delivery::Steer, "then $second");
    let context = DerivedContext {
        content: "FIRST BODY\nSECOND BODY".into(),
        loaded_paths: vec![
            "skills/first/SKILL.md".into(),
            "skills/second/SKILL.md".into(),
        ],
    };
    state
        .derived_contexts
        .lock()
        .unwrap()
        .push_back(context.clone());
    let tools = state.tools.clone();
    let provider = state.provider.clone();
    let options = state.capture_options();
    let mut budget = TurnBudget::new(false);
    let source = state.cancellation.clone();
    let is_cancelled = || source.is_cancelled();
    let cancelled = || source.cancelled();
    let mut preparation = state.preparation();

    assert_eq!(
        finish(run_turn(
            &mut state,
            &mut preparation,
            &tools,
            &provider,
            &options,
            &mut budget,
            CancellationToken::new(&is_cancelled, &cancelled),
        )),
        Ok(Outcome::Completed("answer".into()))
    );
    assert_eq!(
        *state.prepared_inputs.lock().unwrap(),
        [vec![
            PromotedInput {
                input_id: "input-2".into(),
                message_id: "message-2".into(),
                message: Message {
                    role: Role::User,
                    contents: vec![Content::Text("use $first".into())],
                    ..Message::default()
                },
            },
            PromotedInput {
                input_id: "input-3".into(),
                message_id: "message-3".into(),
                message: Message {
                    role: Role::User,
                    contents: vec![Content::Text("then $second".into())],
                    ..Message::default()
                },
            },
        ]]
    );
    assert_eq!(state.recorded_input_contexts, [context]);
    assert_eq!(state.pending_messages.len(), 1);
    assert_eq!(state.pending_messages[0].0.delivery, Delivery::Queue);
    assert_eq!(
        provider.requests.lock().unwrap()[0].messages,
        [Message {
            role: Role::User,
            contents: vec![Content::Text("FIRST BODY\nSECOND BODY".into())],
            ..Message::default()
        }]
    );
}

#[test]
fn input_preparation_and_recording_failures_preserve_promoted_inputs_without_invoking_provider() {
    for stage in ["prepare_inputs", "record_input_context"] {
        let mut state = MemoryState::new([Reply::final_response("not invoked")]);
        state.fail_at = Some(stage);
        state
            .derived_contexts
            .lock()
            .unwrap()
            .push_back(DerivedContext {
                content: "prepared skill body".into(),
                loaded_paths: vec!["skills/selected/SKILL.md".into()],
            });

        assert_eq!(state.run(), Err(Error::failed(stage)));
        assert!(state.pending_messages.is_empty());
        assert_eq!(
            state.last_turn.as_ref().unwrap().messages[0].contents,
            [Content::Text("question".into())]
        );
        assert_eq!(
            state.prepared_inputs.lock().unwrap().len(),
            usize::from(stage == "record_input_context")
        );
        assert!(state.recorded_input_contexts.is_empty());
        assert!(state.request().rendered_instructions.is_empty());
        assert!(state.request().messages.is_empty());
        assert!(state.provider.requests.lock().unwrap().is_empty());
        assert!(state.tools.requests.lock().unwrap().is_empty());
        assert!(!state.trace.values().iter().any(|step| step == "complete"));
        assert_eq!(state.trace.values().last().unwrap(), stage);
    }
}

#[test]
fn empty_derived_context_records_no_message_or_loaded_paths() {
    let mut state = MemoryState::new([]);
    let original = state.last_turn.clone();
    assert_eq!(
        finish(state.record_input_context(&DerivedContext {
            content: String::new(),
            loaded_paths: vec!["unused/SKILL.md".into()],
        })),
        Ok(())
    );
    assert_eq!(state.last_turn, original);
    assert!(state.recorded_input_contexts.is_empty());
    assert!(state.request().messages.is_empty());
    assert!(state.trace.values().is_empty());
}
