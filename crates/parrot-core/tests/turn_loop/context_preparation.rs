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

    assert_eq!(
        state.status_steered,
        [(
            "fitted status".into(),
            status_standing(&[("runtime:context-reminder", "band-50")])
        )]
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
    assert_eq!(state.final_requests, [1, 2]);
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
fn raw_and_calibrated_triggers_compact_and_use_returned_epoch() {
    for calibrated in [false, true] {
        let mut state = MemoryState::new([Reply::final_response("answer")]);
        state.request().context.configuration.model.context_window = 1000;
        state
            .request()
            .context
            .configuration
            .model
            .maximum_output_tokens = 100;
        state.request().context.configuration.trigger_percent = 80;
        state.request().estimated_input_tokens = 100;
        if calibrated {
            state
                .request()
                .context
                .calibration
                .observe_usage("memory/model", 0, 950);
        }
        state.request().estimate_script.push_back(0);
        state
            .request()
            .estimate_script
            .push_back(if calibrated { 10 } else { 801 });
        let messages = vec![Message {
            role: Role::System,
            contents: vec![Content::Text("compacted history".into())],
            ..Message::default()
        }];
        let mut compacted_state = state.request().context.clone();
        compacted_state.calibration.reset();
        state.compaction_plan = Some(compaction_support::plan(&["original", "retained"]));
        state.summary_provider =
            compaction_support::Provider::new([parrot_core::compaction::SummaryResponse::Summary(
                "automatic summary".into(),
            )]);
        state.compactions.push_back(CompactedContext {
            instructions: "sampled compacted instructions".into(),
            messages: messages.clone(),
            state: compacted_state,
            reduced: true,
        });
        assert_eq!(state.run(), Ok(Outcome::Completed("answer".into())));
        assert_eq!(state.request().rendered_instructions, ["core request"]);
        assert_eq!(state.summary_provider.requests.lock().unwrap().len(), 1);
        let compacted = state.recorded_compactions[0]
            .as_ref()
            .unwrap()
            .as_ref()
            .unwrap();
        assert_eq!(
            compacted.summary.contents,
            [Content::Text("prefix:automatic summary".into())]
        );
        let requests = state.provider.requests.lock().unwrap();
        assert_eq!(requests[0].instructions, "sampled compacted instructions");
        assert_eq!(requests[0].messages, messages);
        assert_eq!(requests[0].max_output_tokens, 100);
        assert_eq!(state.compacted_definitions, [requests[0].tools.clone()]);
        let estimates = state.capture_request_estimates();
        assert_eq!(estimates[0].tokens, 100);
        assert_eq!(estimates[0].instructions, requests[0].instructions);
        assert_eq!(estimates[0].messages, requests[0].messages);
        let trace = state.trace.values();
        assert!(trace.iter().any(|step| step == "compact"));
    }
}

#[test]
fn separate_context_reads_use_compaction_configuration_and_calibration() {
    let mut state = MemoryState::new([Reply::final_response("answer")]);
    state.request().context.configuration.model.context_window = 1000;
    state
        .request()
        .context
        .configuration
        .model
        .maximum_output_tokens = 100;
    state.request().estimated_input_tokens = 100;
    state
        .request()
        .context
        .calibration
        .observe_usage("memory/model", 0, 950);
    let mut compacted = state.request().context.clone();
    compacted.configuration.model.context_window = 2000;
    compacted.configuration.model.maximum_output_tokens = 200;
    compacted.calibration.reset();
    state.compactions.push_back(CompactedContext {
        instructions: "compacted instructions".into(),
        messages: vec![Message {
            role: Role::System,
            contents: vec![Content::Text("compacted history".into())],
            ..Message::default()
        }],
        state: compacted,
        reduced: true,
    });

    assert_eq!(state.run(), Ok(Outcome::Completed("answer".into())));
    assert_eq!(state.compacted_definitions.len(), 1);
    assert_eq!(
        state.provider.requests.lock().unwrap()[0].max_output_tokens,
        200
    );
}

#[test]
fn automatic_summary_failure_is_recorded_before_context_preparation_stops() {
    let mut state = MemoryState::new([Reply::final_response("not invoked")]);
    state.prime_context(490);
    state.request().estimated_input_tokens = 901;
    state.compaction_plan = Some(compaction_support::plan(&["original", "retained"]));
    let error = Error::failed("automatic summary failed");
    state
        .summary_provider
        .replies
        .lock()
        .unwrap()
        .push_back(Err(error.clone()));

    assert_eq!(state.run(), Err(error.clone()));
    assert_eq!(state.summary_provider.requests.lock().unwrap().len(), 1);
    assert_eq!(state.recorded_compactions, [Err(error)]);
    assert!(state.provider.requests.lock().unwrap().is_empty());
    assert_eq!(state.trace.values().last().unwrap(), "compact");
}

#[test]
fn equality_at_the_raw_trigger_does_not_compact_or_discard_requested_output() {
    let mut state = MemoryState::new([Reply::final_response("answer")]);
    state.request().context.configuration.model.context_window = 1000;
    state
        .request()
        .context
        .configuration
        .model
        .maximum_output_tokens = 100;
    state.request().context.configuration.trigger_percent = 80;
    state.request().estimated_input_tokens = 800;
    assert_eq!(state.run(), Ok(Outcome::Completed("answer".into())));
    assert!(state.compacted_definitions.is_empty());
    assert_eq!(
        state.provider.requests.lock().unwrap()[0].max_output_tokens,
        100
    );
}

#[test]
fn post_compaction_raw_input_failure_precedes_further_provider_effects() {
    let mut state = MemoryState::new([Reply::final_response("not invoked")]);
    state.request().context.configuration.model.context_window = 1000;
    state
        .request()
        .context
        .configuration
        .model
        .maximum_input_tokens = 700;
    state
        .request()
        .context
        .configuration
        .model
        .maximum_output_tokens = 100;
    state.request().estimated_input_tokens = 701;
    let context = state.request().context.clone();
    state.compactions.push_back(CompactedContext {
        instructions: "compacted but too large".into(),
        messages: vec![],
        state: context,
        reduced: false,
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
    assert!(state.compacted_definitions.is_empty());
    assert_eq!(
        state.provider.requests.lock().unwrap()[0].max_output_tokens,
        32768
    );
}

#[test]
fn context_primitive_failures_stop_at_the_effect_without_invoking_provider_or_tools() {
    for stage in [
        "context_configuration",
        "token_calibration",
        "estimate",
        "final_request",
    ] {
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
