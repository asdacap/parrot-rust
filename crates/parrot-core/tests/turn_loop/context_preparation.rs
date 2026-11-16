use super::*;

#[test]
fn reminder_band_steering_does_not_repeat_on_tool_followups_or_later_turns() {
    let mut state = MemoryState::new([
        Reply::tools("call"),
        Reply::final_response("first answer"),
        Reply::final_response("second answer"),
    ]);
    state.status_offering = status_offer(&[("runtime:context-reminder", true, "band-50")]);

    assert_eq!(state.run(), Ok(Outcome::Completed("first answer".into())));
    state.enqueue_message(Delivery::Queue, "another question");
    assert_eq!(state.run(), Ok(Outcome::Completed("second answer".into())));

    assert_eq!(state.status_steered, ["runtime:context-reminder"]);
    assert_eq!(
        state.budget.standing_statuses,
        status_standing(&[("runtime:context-reminder", "band-50")])
    );
    let steered = state
        .trace
        .values()
        .iter()
        .filter(|step| *step == "steer_status")
        .count();
    assert_eq!(steered, 1);
}

#[test]
fn final_phase_sends_the_configured_steer() {
    let mut state = MemoryState::new([Reply::final_response("answer")]);
    state.maximum = 1;
    state.skill_templates.lock().unwrap().insert(
        ConfigKey::FinalRequestTemplate,
        "Finish using the available information.".into(),
    );

    assert_eq!(state.run(), Ok(Outcome::Completed("answer".into())));
    let requests = state.provider.requests.lock().unwrap();
    assert!(requests[0].messages.iter().any(|message| {
        message.role == Role::System
            && message.contents
                == [Content::Text(
                    "Finish using the available information.".into(),
                )]
    }));
}

#[test]
fn core_records_each_final_request_before_rendering_and_retains_final_catalogs() {
    let mut state = MemoryState::new([Reply::tools("call"), Reply::final_response("answer")]);
    state.maximum = 1;
    assert_eq!(state.run(), Ok(Outcome::Completed("answer".into())));
    assert_eq!(state.final_requests, [0]);
    assert_eq!(state.capture_catalogs(), [vec!["read"], vec!["read"]]);
    assert!(state.tools.flags.lock().unwrap()[0].final_phase);
    let trace = state.trace.values();
    assert!(
        trace
            .iter()
            .position(|step| step == "final_request")
            .unwrap()
            < trace.iter().rposition(|step| step == "render").unwrap()
    );
}

#[test]
fn raw_and_calibrated_triggers_compact_and_keep_sampled_instructions() {
    for calibrated in [false, true] {
        let mut state = MemoryState::new([Reply::final_response("answer")]);
        state.request().model.configuration.context_window = 1000;
        state.request().model.configuration.maximum_output_tokens = 100;
        state.request().model.configuration.trigger_percent = 80;
        state.request().estimated_input_tokens = 100;
        if calibrated {
            state
                .budget
                .last_request_statistic
                .observe_usage("memory/model", 0, 950);
        }
        state
            .request()
            .estimate_script
            .push_back(if calibrated { 10 } else { 801 });
        let messages = vec![Message {
            role: Role::System,
            contents: vec![Content::Text("compacted history".into())],
            ..Message::default()
        }];
        let compacted_state = state.request().model.clone();
        state.compact_with("automatic summary");
        state.compactions.push_back(CompactedContext {
            messages: messages.clone(),
            state: compacted_state,
        });
        assert_eq!(state.run(), Ok(Outcome::Completed("answer".into())));
        assert_eq!(state.request().rendered_instructions, ["core request"]);
        assert_eq!(state.compaction.lock().unwrap().requests.len(), 1);
        assert_eq!(
            state.compaction.lock().unwrap().requests[0].trigger,
            parrot_core::CompactionTrigger::Automatic
        );
        let requests = state.provider.requests.lock().unwrap();
        assert_eq!(requests[0].instructions, "core request");
        assert_eq!(requests[0].messages, messages);
        assert_eq!(requests[0].max_output_tokens, 100);
        let estimates = state.capture_request_estimates();
        assert_eq!(estimates[0].tokens, 100);
        assert_eq!(estimates[0].instructions, requests[0].instructions);
        assert_eq!(estimates[0].messages, requests[0].messages);
        let trace = state.trace.values();
        assert!(trace.iter().any(|step| step == "compact"));
    }
}

#[test]
fn separate_context_reads_use_updated_configuration_and_calibration() {
    let mut state = MemoryState::new([Reply::final_response("answer")]);
    state.request().model.configuration.context_window = 1000;
    state.request().model.configuration.maximum_output_tokens = 100;
    state.request().estimated_input_tokens = 100;
    state
        .budget
        .last_request_statistic
        .observe_usage("memory/model", 0, 950);
    let mut compacted = state.request().model.clone();
    compacted.configuration.context_window = 2000;
    compacted.configuration.maximum_output_tokens = 200;
    state.compact_with("summary");
    state.compactions.push_back(CompactedContext {
        messages: vec![Message {
            role: Role::System,
            contents: vec![Content::Text("compacted history".into())],
            ..Message::default()
        }],
        state: compacted,
    });

    assert_eq!(state.run(), Ok(Outcome::Completed("answer".into())));
    assert_eq!(state.compaction.lock().unwrap().results.len(), 1);
    assert_eq!(
        state.provider.requests.lock().unwrap()[0].max_output_tokens,
        200
    );
}

#[test]
fn automatic_compaction_failure_stops_before_provider_invocation() {
    let mut state = MemoryState::new([Reply::final_response("not invoked")]);
    state.prime_context(490);
    state.request().estimated_input_tokens = 901;
    let error = Error::failed("automatic compaction failed");
    state
        .compaction
        .lock()
        .unwrap()
        .replies
        .push_back(Err(error.clone()));
    assert_eq!(state.run(), Err(error.clone()));
    assert_eq!(state.compaction.lock().unwrap().requests.len(), 1);
    assert_eq!(state.compaction.lock().unwrap().results, [Err(error)]);
    assert!(state.provider.requests.lock().unwrap().is_empty());
    assert_eq!(state.trace.values().last().unwrap(), "compact");
}

#[test]
fn equality_at_the_raw_trigger_does_not_compact_or_discard_requested_output() {
    let mut state = MemoryState::new([Reply::final_response("answer")]);
    state.request().model.configuration.context_window = 1000;
    state.request().model.configuration.maximum_output_tokens = 100;
    state.request().model.configuration.trigger_percent = 80;
    state.request().estimated_input_tokens = 800;
    assert_eq!(state.run(), Ok(Outcome::Completed("answer".into())));
    assert!(state.compaction.lock().unwrap().requests.is_empty());
    assert_eq!(
        state.provider.requests.lock().unwrap()[0].max_output_tokens,
        100
    );
}

#[test]
fn post_compaction_raw_input_failure_precedes_further_provider_effects() {
    let mut state = MemoryState::new([Reply::final_response("not invoked")]);
    state.request().model.configuration.context_window = 1000;
    state.request().model.configuration.maximum_input_tokens = 700;
    state.request().model.configuration.maximum_output_tokens = 100;
    state.request().estimated_input_tokens = 701;
    let model = state.request().model.clone();
    state.compactions.push_back(CompactedContext {
        messages: vec![],
        state: model,
    });
    let error = state.run().unwrap_err();
    assert_eq!(error.code, ErrorCode::InvalidOperation);
    assert_eq!(
        error.message,
        "The compacted conversation exceeds the selected model input limit."
    );
    assert!(state.provider.requests.lock().unwrap().is_empty());
    assert!(
        !state
            .trace
            .values()
            .iter()
            .any(|step| step == "steer_status")
    );
}

#[test]
fn unknown_context_uses_output_configuration_without_inventing_reminder_or_compaction() {
    let mut state = MemoryState::new([Reply::final_response("answer")]);
    state.request().estimated_input_tokens = i64::MAX;
    assert_eq!(state.run(), Ok(Outcome::Completed("answer".into())));
    assert!(state.compaction.lock().unwrap().requests.is_empty());
    assert_eq!(
        state.provider.requests.lock().unwrap()[0].max_output_tokens,
        32768
    );
}

#[test]
fn model_output_limit_above_fallback_reaches_provider_request() {
    let mut state = MemoryState::new([Reply::final_response("answer")]);
    state.request().model.configuration.context_window = 1000000;
    state.request().model.configuration.maximum_output_tokens = 131072;
    state.request().estimated_input_tokens = 1000;

    assert_eq!(state.run(), Ok(Outcome::Completed("answer".into())));
    assert_eq!(
        state.provider.requests.lock().unwrap()[0].max_output_tokens,
        131072
    );
}

#[test]
fn context_primitive_failures_stop_at_the_effect_without_invoking_provider_or_tools() {
    for stage in ["context_configuration", "estimate", "final_request"] {
        let mut state = MemoryState::new([Reply::final_response("not invoked")]);
        if stage == "final_request" {
            state.maximum = 1;
        }
        state.fail_at = Some(stage);
        assert_eq!(state.run(), Err(Error::failed(stage)));
        assert_eq!(state.trace.values().last().unwrap(), stage);
        assert!(state.provider.requests.lock().unwrap().is_empty());
        assert!(state.tools.requests.lock().unwrap().is_empty());
    }
}

#[test]
fn retry_messages_use_configured_template_overrides() {
    for (reply, key, content) in [
        (
            Reply::truncated("cut"),
            ConfigKey::TruncatedToolCallTemplate,
            "Retry the incomplete tool call.",
        ),
        (
            Reply::invalid("bad"),
            ConfigKey::InvalidToolCallTemplate,
            "Retry the invalid tool call.",
        ),
    ] {
        let mut state = MemoryState::new([reply, Reply::final_response("answer")]);
        state
            .skill_templates
            .lock()
            .unwrap()
            .insert(key, content.into());

        assert_eq!(state.run(), Ok(Outcome::Completed("answer".into())));
        let requests = state.provider.requests.lock().unwrap();
        assert_eq!(requests.len(), 2);
        assert!(requests[1].messages.iter().any(|message| {
            message.role == Role::System && message.contents == [Content::Text(content.into())]
        }));
    }
}

#[test]
fn completed_usage_calibrates_later_turns_and_completion_blocker_restarts() {
    let mut response = match Reply::final_response("candidate") {
        Reply::Response(response) => response,
        _ => unreachable!(),
    };
    response.input_tokens = 350;
    let mut state = MemoryState::new([
        Reply::Response(response),
        Reply::final_response("first answer"),
        Reply::final_response("second answer"),
    ]);
    state.request().estimated_input_tokens = 100;
    state
        .prompts()
        .exit_blockers
        .push_back(vec!["finish the task".into()]);

    assert_eq!(state.run(), Ok(Outcome::Completed("first answer".into())));
    assert_eq!(
        state
            .budget
            .last_request_statistic
            .estimate_input_tokens("memory/model", 200),
        450
    );
    state.enqueue_message(Delivery::Queue, "another question");
    assert_eq!(state.run(), Ok(Outcome::Completed("second answer".into())));
    assert_eq!(
        state
            .budget
            .last_request_statistic
            .estimate_input_tokens("memory/model", 200),
        450
    );
    assert_eq!(
        state
            .budget
            .last_request_statistic
            .estimate_input_tokens("other/model", 200),
        200
    );
}

#[test]
fn discarded_tool_responses_still_record_provider_usage() {
    for reply in [Reply::truncated("cut"), Reply::invalid("invalid")] {
        let Reply::Response(mut response) = reply else {
            unreachable!()
        };
        response.input_tokens = 350;
        let mut state =
            MemoryState::new([Reply::Response(response), Reply::final_response("answer")]);
        state.request().estimated_input_tokens = 100;

        assert_eq!(state.run(), Ok(Outcome::Completed("answer".into())));
        assert_eq!(
            state
                .budget
                .last_request_statistic
                .estimate_input_tokens("memory/model", 200),
            450
        );
    }
}

#[test]
fn failed_invocations_calibrate_later_turns_from_returned_usage() {
    for error in [
        Error::failed("failure after completion"),
        Error::cancelled(),
    ] {
        let mut state = MemoryState::new([
            Reply::UsageThenError {
                error: error.clone(),
                input_tokens: 350,
            },
            Reply::final_response("next answer"),
        ]);
        state.request().estimated_input_tokens = 100;

        assert_eq!(state.run(), Err(error));
        assert_eq!(
            state
                .budget
                .last_request_statistic
                .estimate_input_tokens("memory/model", 200),
            450
        );
        assert_eq!(state.run(), Ok(Outcome::Completed("next answer".into())));
        assert_eq!(
            state
                .budget
                .last_request_statistic
                .estimate_input_tokens("memory/model", 200),
            450
        );
    }
}
