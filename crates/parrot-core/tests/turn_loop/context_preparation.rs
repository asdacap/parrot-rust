use super::*;

#[test]
fn core_records_final_request_once_before_rendering_and_retains_final_catalogs() {
    let mut state = MemoryState::new([Reply::tools("call"), Reply::final_response("answer")]);
    state.maximum = 1;
    assert_eq!(state.run(), Ok(Outcome::Completed("answer".into())));
    assert_eq!(state.final_requests, [0]);
    assert_eq!(state.capture_catalogs(), [vec!["read"], vec!["read"]]);
    assert!(state.recorded_phases[0].enters_final_phase);
    let trace = state.trace.values();
    assert!(
        trace
            .iter()
            .position(|step| step == "final_request")
            .unwrap()
            < trace.iter().position(|step| step == "render").unwrap()
    );
    assert_eq!(trace.iter().filter(|step| *step == "checkpoint").count(), 1);
}

#[test]
fn raw_and_calibrated_triggers_compact_before_checkpoint_restore_and_use_returned_epoch() {
    for calibrated in [false, true] {
        let mut state = MemoryState::new([Reply::final_response("answer")]);
        state.context.configuration.model.context_window = 1000;
        state.context.configuration.model.maximum_output_tokens = 100;
        state.context.configuration.trigger_percent = 80;
        state.estimated_input_tokens = 100;
        if calibrated {
            state
                .context
                .calibration
                .observe_usage("memory/model", 0, 950);
        }
        state
            .estimate_script
            .lock()
            .unwrap()
            .push_back(if calibrated { 10 } else { 801 });
        let messages = vec![Message {
            role: Role::System,
            contents: vec![Content::Text("compacted history".into())],
            ..Message::default()
        }];
        let mut compacted_state = state.context.clone();
        compacted_state.calibration.reset();
        compacted_state
            .cadence
            .acknowledge(
                &ContextSnapshot::estimate(100, &compacted_state.configuration),
                "memory/model",
                messages.len() as i64,
            )
            .unwrap();
        state.compactions.push_back(CompactedContext {
            instructions: "sampled compacted instructions".into(),
            messages: messages.clone(),
            state: compacted_state,
            reduced: true,
        });
        assert_eq!(state.run(), Ok(Outcome::Completed("answer".into())));
        assert_eq!(state.rendered_instructions, ["core request"]);
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
        assert!(
            trace.iter().position(|step| step == "compact").unwrap()
                < trace.iter().position(|step| step == "checkpoint").unwrap()
        );
    }
}

#[test]
fn equality_at_the_raw_trigger_does_not_compact_or_discard_requested_output() {
    let mut state = MemoryState::new([Reply::final_response("answer")]);
    state.context.configuration.model.context_window = 1000;
    state.context.configuration.model.maximum_output_tokens = 100;
    state.context.configuration.trigger_percent = 80;
    state.estimated_input_tokens = 800;
    assert_eq!(state.run(), Ok(Outcome::Completed("answer".into())));
    assert!(state.compacted_definitions.is_empty());
    assert_eq!(
        state.provider.requests.lock().unwrap()[0].max_output_tokens,
        100
    );
}

#[test]
fn post_compaction_raw_input_failure_precedes_checkpoint_or_provider_effects() {
    let mut state = MemoryState::new([Reply::final_response("not invoked")]);
    state.context.configuration.model.context_window = 1000;
    state.context.configuration.model.maximum_input_tokens = 700;
    state.context.configuration.model.maximum_output_tokens = 100;
    state.estimated_input_tokens = 701;
    state.compactions.push_back(CompactedContext {
        instructions: "compacted but too large".into(),
        messages: vec![],
        state: state.context.clone(),
        reduced: false,
    });
    let error = state.run().unwrap_err();
    assert_eq!(error.code, ErrorCode::InvalidOperation);
    assert_eq!(
        error.message,
        "The compacted conversation exceeds the selected model input limit."
    );
    assert!(state.provider.requests.lock().unwrap().is_empty());
    assert!(state.cadence_writes.is_empty());
    assert!(!state.trace.values().iter().any(|step| step == "checkpoint"));
}

#[test]
fn reminder_append_receives_raw_usage_not_the_checkpoint_band_or_candidate_usage() {
    let mut state = MemoryState::new([Reply::final_response("answer")]);
    state.prime_context(490);
    *state.estimate_script.lock().unwrap() = [550, 630, 630, 630].into();
    assert_eq!(state.run(), Ok(Outcome::Completed("answer".into())));
    assert_eq!(state.reminder_appends.len(), 1);
    assert_eq!(state.reminder_appends[0].0.percentage, 50);
    assert_eq!(state.reminder_appends[0].2, 55);
    let rendered = state.rendered_reminders.lock().unwrap();
    assert_eq!(rendered[0].0.usage_percent, Some(55));
    assert_eq!(rendered[0].1, 50);
    let estimates = state.estimates.lock().unwrap();
    assert_eq!(estimates[1].tokens, 630);
    assert_eq!(estimates[1].messages.len(), 2);
    assert_eq!(state.reminder_publications, 1);
    let requests = state.provider.requests.lock().unwrap();
    assert_eq!(requests[0].messages, estimates.last().unwrap().messages);
    assert_eq!(
        requests[0].messages.last().unwrap().contents,
        [Content::Text(state.reminder.clone())]
    );
    let trace = state.trace.values();
    assert!(
        trace
            .iter()
            .position(|step| step == "append_reminder")
            .unwrap()
            < trace.iter().rposition(|step| step == "cadence").unwrap()
    );
    assert!(
        trace.iter().rposition(|step| step == "cadence").unwrap()
            < trace
                .iter()
                .position(|step| step == "publish_reminder")
                .unwrap()
    );
}

#[test]
fn duplicate_reminder_acknowledges_candidate_tokens_with_unchanged_history_without_publication() {
    let mut state = MemoryState::new([Reply::final_response("answer")]);
    state.prime_context(490);
    state.append_reminder = false;
    *state.estimate_script.lock().unwrap() = [550, 630, 550].into();
    assert_eq!(state.run(), Ok(Outcome::Completed("answer".into())));
    assert_eq!(state.reminder_appends.len(), 1);
    assert_eq!(state.messages.len(), 1);
    assert_eq!(state.reminder_publications, 0);
    let mut cadence = state.context.cadence.clone();
    assert_eq!(
        cadence.observe(
            &ContextSnapshot::estimate(690, &state.context.configuration),
            "memory/model",
            1
        ),
        Ok(None)
    );
    assert_eq!(
        cadence.observe(
            &ContextSnapshot::estimate(700, &state.context.configuration),
            "memory/model",
            2
        ),
        Ok(Some(70))
    );
    assert_eq!(
        state.provider.requests.lock().unwrap()[0].messages,
        state.messages
    );
}

#[test]
fn cadence_is_acknowledged_before_failed_reminder_publication_and_provider_is_not_entered() {
    let mut state = MemoryState::new([Reply::final_response("not invoked")]);
    state.prime_context(490);
    state.fail_at = Some("publish_reminder");
    *state.estimate_script.lock().unwrap() = [550, 630, 630].into();
    assert_eq!(state.run(), Err(Error::failed("publish_reminder")));
    assert!(state.provider.requests.lock().unwrap().is_empty());
    assert_eq!(state.reminder_publications, 1);
    assert_eq!(state.messages.len(), 2);
    assert_eq!(state.checkpoint.as_ref().unwrap().percentage, 50);
    let mut cadence = state.context.cadence.clone();
    assert_eq!(
        cadence.observe(
            &ContextSnapshot::estimate(690, &state.context.configuration),
            "memory/model",
            2
        ),
        Ok(None)
    );
    assert_eq!(
        cadence.observe(
            &ContextSnapshot::estimate(700, &state.context.configuration),
            "memory/model",
            3
        ),
        Ok(Some(70))
    );
    let trace = state.trace.values();
    assert_eq!(trace.last().unwrap(), "publish_reminder");
    assert!(trace.iter().rposition(|step| step == "cadence").unwrap() < trace.len() - 1);
}

#[test]
fn cadence_observation_is_saved_before_reminder_rendering_can_fail() {
    let mut state = MemoryState::new([Reply::final_response("not invoked")]);
    state.prime_context(490);
    state.estimated_input_tokens = 550;
    state.fail_at = Some("reminder");
    assert_eq!(state.run(), Err(Error::failed("reminder")));
    assert!(state.provider.requests.lock().unwrap().is_empty());
    assert!(state.reminder_appends.is_empty());
    let mut cadence = state.context.cadence.clone();
    assert_eq!(
        cadence.observe(
            &ContextSnapshot::estimate(590, &state.context.configuration),
            "memory/model",
            1
        ),
        Ok(None)
    );
    assert_eq!(
        cadence.observe(
            &ContextSnapshot::estimate(600, &state.context.configuration),
            "memory/model",
            2
        ),
        Ok(Some(60))
    );
}

#[test]
fn crossing_reminder_candidate_compacts_without_append_or_a_second_cadence_observation() {
    let mut state = MemoryState::new([Reply::final_response("answer")]);
    state.prime_context(490);
    *state.estimate_script.lock().unwrap() = [550, 910, 100, 100].into();
    let messages = vec![Message {
        role: Role::System,
        contents: vec![Content::Text("compacted".into())],
        ..Message::default()
    }];
    let mut compacted_state = state.context.clone();
    compacted_state
        .cadence
        .acknowledge(
            &ContextSnapshot::estimate(100, &compacted_state.configuration),
            "memory/model",
            1,
        )
        .unwrap();
    state.compactions.push_back(CompactedContext {
        instructions: "compacted reminder candidate".into(),
        messages: messages.clone(),
        state: compacted_state,
        reduced: true,
    });
    assert_eq!(state.run(), Ok(Outcome::Completed("answer".into())));
    assert!(state.reminder_appends.is_empty());
    assert_eq!(state.reminder_publications, 0);
    assert_eq!(state.cadence_writes.len(), 1);
    assert_eq!(state.rendered_instructions.len(), 1);
    let requests = state.provider.requests.lock().unwrap();
    assert_eq!(requests[0].instructions, "compacted reminder candidate");
    assert_eq!(requests[0].messages, messages);
}

#[test]
fn unknown_context_uses_output_configuration_without_inventing_reminder_or_compaction() {
    let mut state = MemoryState::new([Reply::final_response("answer")]);
    state.estimated_input_tokens = i64::MAX;
    assert_eq!(state.run(), Ok(Outcome::Completed("answer".into())));
    assert!(state.compacted_definitions.is_empty());
    assert!(state.rendered_reminders.lock().unwrap().is_empty());
    assert!(state.reminder_appends.is_empty());
    assert_eq!(
        state.provider.requests.lock().unwrap()[0].max_output_tokens,
        32768
    );
}

#[test]
fn context_primitive_failures_stop_at_the_effect_without_invoking_provider_or_tools() {
    for stage in [
        "context",
        "estimate",
        "checkpoint",
        "cadence",
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
fn reminder_append_failure_keeps_observed_band_without_acknowledging_or_publishing_candidate() {
    let mut state = MemoryState::new([Reply::final_response("not invoked")]);
    state.prime_context(490);
    state.fail_at = Some("append_reminder");
    *state.estimate_script.lock().unwrap() = [550, 630].into();
    assert_eq!(state.run(), Err(Error::failed("append_reminder")));
    assert!(state.provider.requests.lock().unwrap().is_empty());
    assert!(state.reminder_appends.is_empty());
    assert!(state.checkpoint.is_none());
    assert_eq!(state.messages.len(), 1);
    assert_eq!(state.reminder_publications, 0);
    let mut cadence = state.context.cadence.clone();
    assert_eq!(
        cadence.observe(
            &ContextSnapshot::estimate(590, &state.context.configuration),
            "memory/model",
            1
        ),
        Ok(None)
    );
    assert_eq!(
        cadence.observe(
            &ContextSnapshot::estimate(600, &state.context.configuration),
            "memory/model",
            2
        ),
        Ok(Some(60))
    );
}

#[test]
fn restored_cadence_is_saved_even_when_the_observation_is_invalid() {
    let mut state = MemoryState::new([Reply::final_response("not invoked")]);
    state.context.configuration.model.canonical_model = " \t".into();
    let error = state.run().unwrap_err();
    assert_eq!(error.code, ErrorCode::Unexpected);
    assert_eq!(
        error.message,
        "canonical model must not be empty or whitespace"
    );
    assert!(state.context.cadence.is_restored());
    assert_eq!(state.cadence_writes.len(), 1);
    assert!(state.provider.requests.lock().unwrap().is_empty());
}
