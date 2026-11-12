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
fn pending_input_readiness_does_not_consume_input_after_a_completed_turn() {
    let mut state = MemoryState::new([Reply::final_response("new answer")]);
    state.append_response(&ProviderResponse {
        assistant_text: "old answer".into(),
        ..ProviderResponse::default()
    });
    assert!(finish(state.has_pending_input()).unwrap());
    assert!(finish(state.has_pending_input()).unwrap());
    assert_eq!(state.pending_messages.len(), 1);
    assert!(state.trace.values().is_empty());
    assert_eq!(state.run(), Ok(Outcome::Completed("new answer".into())));
    assert_eq!(state.admissions, [InputAdmission::SteersOrNextQueued]);
    assert!(!finish(state.has_pending_input()).unwrap());
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

    state.budget = AgentState::new(false);

    assert_eq!(
        finish(run_turn(
            &mut state,
            &tools,
            &provider,
            CancellationToken::new(provider.cancellation.as_ref())
        )),
        Ok(Outcome::Completed("first answer".into()))
    );
    assert_eq!(state.provider.requests.lock().unwrap().len(), 1);
    assert_eq!(state.pending_messages.len(), 1);
    assert_eq!(state.final_requests, [1]);

    assert_eq!(
        finish(run_turn(
            &mut state,
            &tools,
            &provider,
            CancellationToken::new(provider.cancellation.as_ref())
        )),
        Ok(Outcome::Completed("second answer".into()))
    );
    assert_eq!(state.provider.requests.lock().unwrap().len(), 2);
    assert!(state.pending_messages.is_empty());
    assert_eq!(state.capture_catalogs()[1], ["read"]);
    assert_eq!(state.final_requests, [1, 2]);

    assert_eq!(
        finish(run(
            &mut state,
            &tools,
            &provider,
            CancellationToken::new(provider.cancellation.as_ref())
        )),
        Ok(Outcome::Completed(String::new()))
    );
    assert_eq!(state.provider.requests.lock().unwrap().len(), 2);
}

#[test]
fn run_turn_opens_the_turn_and_presents_the_selected_model() {
    let mut state = MemoryState::new([Reply::final_response("answer")]);
    state.usage_totals = parrot_core::UsageTotals {
        input_tokens: 321,
        output_tokens: 45,
    };
    let tools = state.tools.clone();
    let provider = state.provider.clone();
    assert_eq!(
        finish(run_turn(
            &mut state,
            &tools,
            &provider,
            CancellationToken::new(provider.cancellation.as_ref())
        )),
        Ok(Outcome::Completed("answer".into()))
    );
    assert_eq!(
        *state.started_turns.lock().unwrap(),
        [TurnModel {
            selector: "memory/model".into(),
            alias_icon: None,
        }]
    );
    assert_eq!(state.turn_ids, ["turn-start-0"]);
    assert_eq!(
        state.closed_turns,
        [ConversationAddition {
            messages: vec![Message::text(Role::Assistant, "answer")],
            persistence: ConversationPersistence::CloseTurn {
                start_event_id: "turn-start-0".into(),
                finish_reason: FinishReason::Stop,
                usage: parrot_core::UsageTotals {
                    input_tokens: 321,
                    output_tokens: 45,
                },
            },
        }]
    );
}

#[test]
fn interruption_supplies_the_recovered_start_event_and_terminal_content_without_activation() {
    let mut state = MemoryState::new([]);
    state.usage_totals = parrot_core::UsageTotals {
        input_tokens: 100,
        output_tokens: 20,
    };
    state.skill_templates.lock().unwrap().insert(
        ConfigKey::InterruptedTemplate,
        "interrupted response".into(),
    );

    assert_eq!(
        finish(parrot_core::interrupt_turn(
            &mut state,
            "recovered-start-event"
        )),
        Ok(Some("interrupted response".into()))
    );
    assert!(state.turn_ids.is_empty());
    assert_eq!(
        state.closed_turns,
        [ConversationAddition {
            messages: vec![Message::text(Role::Assistant, "interrupted response")],
            persistence: ConversationPersistence::CloseTurn {
                start_event_id: "recovered-start-event".into(),
                finish_reason: FinishReason::Interrupted,
                usage: parrot_core::UsageTotals {
                    input_tokens: 100,
                    output_tokens: 20,
                },
            },
        }]
    );
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
            .closed_turns
            .iter()
            .map(|addition| match &addition.persistence {
                ConversationPersistence::CloseTurn { start_event_id, .. } =>
                    start_event_id.as_str(),
                _ => panic!("expected conversation-turn closure"),
            })
            .collect::<Vec<_>>(),
        ["turn-start-0", "turn-start-1"]
    );
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
    let pending = finish(state.has_pending_input()).unwrap();
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
        Ok(Outcome::Completed("answer".into()))
    );
    assert_eq!(state.admissions, [InputAdmission::SteersOnly]);
    assert_eq!(finish(state.has_pending_input()).unwrap(), pending);
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
    assert!(finish(state.has_pending_input()).unwrap());
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
        Ok(Outcome::Completed("answer".into()))
    );
    assert_eq!(state.admissions, [InputAdmission::SteersOnly]);
    assert_eq!(state.pending_messages.len(), 1);
    assert_eq!(state.pending_messages[0].0, Delivery::Queue);
    let turn = state.last_turn.as_ref().unwrap();
    assert_eq!(
        turn.messages[0].contents,
        [Content::Text("second steer".into())]
    );
    assert_eq!(turn.result.as_ref().unwrap().role, Role::Assistant);
}

#[test]
fn promotion_returns_exact_multimodal_input_without_request_preparation() {
    let mut state = MemoryState::new([]);
    state.pending_messages.clear();
    state.fail_at = Some("render");
    let original_turn = state.last_turn.clone();
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
    state
        .pending_messages
        .push_back((Delivery::Steer, message.clone()));

    assert_eq!(
        finish(state.promote_pending_inputs(InputAdmission::SteersOnly)),
        Ok(vec![PromotedInput {
            message: message.clone(),
            publication: Default::default(),
        }])
    );
    assert!(state.pending_messages.is_empty());
    assert_eq!(state.last_turn, original_turn);
    assert!(state.skill_lookups.lock().unwrap().is_empty());
    assert!(state.skill_reads.lock().unwrap().is_empty());
    assert!(state.request().rendered_instructions.is_empty());
    assert!(state.recorded_input_contexts.is_empty());
    assert!(state.provider.requests.lock().unwrap().is_empty());
}

#[test]
fn skill_read_and_recording_failures_preserve_promoted_inputs_without_invoking_provider() {
    for stage in [
        "resolve_skill_references",
        "read_skill_content",
        "render_template",
        "record_input_context",
    ] {
        let mut state = MemoryState::new([Reply::final_response("not invoked")]);
        state.pending_messages.clear();
        state.enqueue_message(Delivery::Steer, "use $selected");
        state.fail_at = Some(stage);
        state.skill_rows.push((
            Skill {
                id: "selected-row".into(),
                name: "selected".into(),
                path: "skills/selected/SKILL.md".into(),
            },
            SkillContent::Text("prepared skill body".into()),
        ));

        assert_eq!(state.run(), Err(Error::failed(stage)));
        assert!(state.pending_messages.is_empty());
        assert_eq!(
            state.last_turn.as_ref().unwrap().messages[0].contents,
            [Content::Text("use $selected".into())]
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
fn input_without_skill_mentions_adds_no_derived_message_or_loaded_event() {
    let mut state = MemoryState::new([Reply::final_response("answer")]);
    state.pending_messages.clear();
    state.enqueue_message(Delivery::Steer, "ordinary input");
    assert_eq!(state.run(), Ok(Outcome::Completed("answer".into())));
    assert!(state.recorded_input_contexts.is_empty());
    assert!(state.loaded_skill_paths().is_empty());
}

#[test]
fn core_loads_skill_rows_without_native_input_preparation() {
    let mut state = MemoryState::new([Reply::final_response("answer")]);
    state.pending_messages.clear();
    state.enqueue_message(Delivery::Steer, "use $selected");
    state.skill_rows.push((
        Skill {
            id: "row-1".into(),
            name: "selected".into(),
            path: "/skills/selected/SKILL.md".into(),
        },
        SkillContent::Text("instructions from the skill row".into()),
    ));

    assert_eq!(state.run(), Ok(Outcome::Completed("answer".into())));
    assert_eq!(state.recorded_input_contexts.len(), 1);
    assert_eq!(
        state.recorded_input_contexts[0].content,
        "<skill>\n<name>selected</name>\n<path>/skills/selected/SKILL.md</path>\ninstructions from the skill row\n</skill>"
    );
    assert_eq!(state.loaded_skill_paths(), ["/skills/selected/SKILL.md"]);
}

#[test]
fn sequential_run_turn_calls_retain_the_total_settlement_extension_limit() {
    let mut state = MemoryState::new([
        Reply::tools("first tool"),
        Reply::final_response("first answer"),
        Reply::tools("second tool"),
        Reply::tools("third tool"),
        Reply::tools("fourth tool"),
        Reply::tools("fifth tool"),
    ]);
    state.maximum = 1;
    state.enqueue_message(Delivery::Queue, "second question");
    let tools = state.tools.clone();
    let provider = state.provider.clone();

    assert_eq!(
        finish(run_turn(
            &mut state,
            &tools,
            &provider,
            CancellationToken::new(provider.cancellation.as_ref())
        )),
        Ok(Outcome::Completed("first answer".into()))
    );
    assert_eq!(state.provider.requests.lock().unwrap().len(), 2);
    assert_eq!(state.applied_results.len(), 1);
    assert_eq!(
        finish(run_turn(
            &mut state,
            &tools,
            &provider,
            CancellationToken::new(provider.cancellation.as_ref())
        )),
        Ok(Outcome::BudgetExhausted(RUNAWAY_NOTICE.into()))
    );
    assert_eq!(state.provider.requests.lock().unwrap().len(), 6);
    assert_eq!(state.applied_results.len(), 5);
    assert!(state.provider.replies.lock().unwrap().is_empty());
    assert_eq!(
        state
            .last_turn
            .as_ref()
            .unwrap()
            .messages
            .last()
            .unwrap()
            .role,
        Role::Tool
    );
}

#[test]
fn provider_errors_write_the_consumed_request_budget_back_to_states() {
    let mut state = MemoryState::new([Reply::Failed("provider failure")]);
    state.maximum = 1;
    let tools = state.tools.clone();
    let provider = state.provider.clone();

    assert_eq!(
        finish(run_turn(
            &mut state,
            &tools,
            &provider,
            CancellationToken::new(provider.cancellation.as_ref())
        )),
        Err(Error::failed("provider failure"))
    );
    assert_eq!(state.provider.requests.lock().unwrap().len(), 1);
    assert!(state.budget.admit(1).is_none());
    assert!(state.recorded_tools.is_empty());
    assert!(state.last_turn.as_ref().unwrap().result.is_none());
}

#[test]
fn budget_read_failures_stop_before_turn_effects_and_do_not_attempt_writeback() {
    let mut state = MemoryState::new([Reply::final_response("not invoked")]);
    let error = Error::failed("budget read failed");
    state.budget_read_error = Some(error.clone());
    let tools = state.tools.clone();
    let provider = state.provider.clone();
    assert_eq!(
        finish(run_turn(
            &mut state,
            &tools,
            &provider,
            CancellationToken::new(provider.cancellation.as_ref())
        )),
        Err(error)
    );
    assert_eq!(state.budget_writes, 0);
    assert!(state.provider.requests.lock().unwrap().is_empty());
    assert!(state.trace.values().is_empty());
    assert!(state.admissions.is_empty());
}

#[test]
fn state_write_failures_stop_before_provider_invocation_and_take_precedence() {
    for reply in [
        Reply::final_response("answer"),
        Reply::Failed("provider failed"),
    ] {
        let mut state = MemoryState::new([reply]);
        let error = Error {
            code: ErrorCode::Io,
            response_body: "native write diagnostic".into(),
            ..Error::failed("budget write failed")
        };
        state.budget_write_error = Some(error.clone());
        let tools = state.tools.clone();
        let provider = state.provider.clone();
        assert_eq!(
            finish(run_turn(
                &mut state,
                &tools,
                &provider,
                CancellationToken::new(provider.cancellation.as_ref())
            )),
            Err(error)
        );
        assert!(state.provider.requests.lock().unwrap().is_empty());
    }
}
