use super::*;

#[test]
fn context_reminder_is_selected_from_the_last_stored_reminder() {
    for (last_percentage, estimated_tokens, expected) in [
        (None, 490, None),
        (None, 550, Some(50)),
        (Some(50), 590, None),
        (Some(50), 610, Some(60)),
        (Some(60), 550, None),
    ] {
        let mut state = MemoryState::new([Reply::final_response("answer")]);
        state.prime_context(estimated_tokens);
        state.prompts().checkpoint = last_percentage.map(|percentage| ContextReminderCheckpoint {
            canonical_model: "memory/model".into(),
            context_limit: 1000,
            percentage,
        });

        assert_eq!(state.run(), Ok(Outcome::Completed("answer".into())));
        assert_eq!(
            state
                .reminder_appends
                .first()
                .map(|(checkpoint, _, _)| checkpoint.percentage),
            expected,
            "last reminder {last_percentage:?}, estimated tokens {estimated_tokens}",
        );
    }
}

#[test]
fn stored_context_reminders_are_not_repeated_on_tool_followups_or_later_turns() {
    let mut state = MemoryState::new([
        Reply::tools("call"),
        Reply::final_response("first answer"),
        Reply::final_response("second answer"),
    ]);
    state.prime_context(550);

    assert_eq!(state.run(), Ok(Outcome::Completed("first answer".into())));
    state.enqueue_message(Delivery::Queue, "another question");
    assert_eq!(state.run(), Ok(Outcome::Completed("second answer".into())));

    assert_eq!(state.reminder_appends.len(), 1);
    assert_eq!(state.reminder_publications, 1);
    let requests = state.provider.requests.lock().unwrap();
    assert_eq!(requests.len(), 3);
    for request in requests.iter() {
        assert_eq!(
            request
                .messages
                .iter()
                .filter(|message| {
                    message.role == Role::System
                        && message.contents == [Content::Text("configured reminder".into())]
                })
                .count(),
            1,
        );
    }
}

#[test]
fn core_records_native_pending_prompts_once() {
    let mut state = MemoryState::new([Reply::tools("call"), Reply::final_response("answer")]);
    let prompt = Message {
        role: Role::System,
        contents: vec![Content::Text("caller-owned prompt".into())],
        ..Message::default()
    };

    state.prompts().pending_prompts.push_back(prompt.clone());
    let tools = state.tools.clone();
    let provider = state.provider.clone();

    state.budget = TurnBudget::new(state.unlimited);

    let result = finish(run_turn(&mut state, &tools, &provider));

    assert_eq!(result, Ok(Outcome::Completed("answer".into())));
    assert_eq!(state.recorded_prompts, [prompt.clone()]);
    let requests = provider.requests.lock().unwrap();
    assert_eq!(requests.len(), 2);
    for request in requests.iter() {
        assert_eq!(
            request
                .messages
                .iter()
                .filter(|message| *message == &prompt)
                .count(),
            1
        );
    }
    assert!(state.prompts().pending_prompts.is_empty());
    assert_eq!(state.prompts().acknowledged_prompts.len(), 1);
}

#[test]
fn pending_steer_and_status_prompts_are_fitted_without_a_context_reminder() {
    for role in [Role::User, Role::System] {
        let mut state = MemoryState::new([Reply::final_response("answer")]);
        state.prime_context(100);
        let prompt = Message {
            role,
            contents: vec![Content::Text("pending prompt".into())],
            provider_content: "{\"native\":true}".into(),
            ..Message::default()
        };
        state.prompts().pending_prompts.push_back(prompt.clone());
        state.request().estimate_script = [100, 150, 160, 160].into();

        assert_eq!(state.run(), Ok(Outcome::Completed("answer".into())));
        assert!(state.reminder_appends.is_empty());
        assert_eq!(state.recorded_prompts, [prompt.clone()]);
        assert_eq!(
            state.prompts().acknowledged_prompts[0].estimated_tokens,
            160
        );
        let request = state.request();
        let estimates = &request.estimates;
        assert_eq!(estimates[1].messages.last(), Some(&prompt));
        assert_eq!(estimates[1].tokens, 150);
        let requests = state.provider.requests.lock().unwrap();
        assert_eq!(requests[0].messages.last(), Some(&prompt));
        assert_eq!(requests[0].messages, estimates.last().unwrap().messages);
        let trace = state.trace.values();
        let fitted = trace
            .iter()
            .position(|step| step == "record_prompt")
            .unwrap();
        let acknowledged = trace
            .iter()
            .position(|step| step == "acknowledge_prompt")
            .unwrap();
        let published = trace
            .iter()
            .position(|step| step == "publish_prompt")
            .unwrap();
        let invoked = trace.iter().position(|step| step == "invoke").unwrap();
        assert!(fitted < acknowledged && acknowledged < published && published < invoked);
    }
}

#[test]
fn pending_prompt_is_recorded_when_context_capacity_is_unknown() {
    let mut state = MemoryState::new([Reply::final_response("answer")]);
    let prompt = Message {
        role: Role::System,
        contents: vec![Content::Text("pending status".into())],
        ..Message::default()
    };
    state.prompts().pending_prompts.push_back(prompt.clone());

    assert_eq!(state.run(), Ok(Outcome::Completed("answer".into())));
    assert_eq!(state.recorded_prompts, [prompt.clone()]);
    assert!(!state.prompts().acknowledged_prompts[0].is_available());
    assert_eq!(
        state.provider.requests.lock().unwrap()[0].messages.last(),
        Some(&prompt)
    );
    assert!(state.compacted_definitions.is_empty());
}

#[test]
fn oversized_pending_status_prompt_compacts_without_recording_or_acknowledging_it() {
    let mut state = MemoryState::new([Reply::final_response("answer")]);
    state.prime_context(100);
    state.prompts().pending_prompts.push_back(Message {
        role: Role::System,
        contents: vec![Content::Text("oversized status".into())],
        ..Message::default()
    });
    state.request().estimate_script = [100, 910, 100, 100].into();
    let messages = vec![Message {
        role: Role::System,
        contents: vec![Content::Text("compacted status".into())],
        ..Message::default()
    }];
    let context = state.request().context.clone();
    state.compactions.push_back(CompactedContext {
        instructions: "compacted instructions".into(),
        messages: messages.clone(),
        state: context,
        reduced: true,
    });

    assert_eq!(state.run(), Ok(Outcome::Completed("answer".into())));
    assert_eq!(state.compacted_definitions.len(), 1);
    assert!(state.recorded_prompts.is_empty());
    assert!(state.prompts().acknowledged_prompts.is_empty());
    assert_eq!(
        state.provider.requests.lock().unwrap()[0].messages,
        messages
    );
}

#[test]
fn pending_prompt_failures_stop_before_later_effects() {
    for stage in [
        "pending_prompt",
        "record_prompt",
        "acknowledge_prompt",
        "publish_prompt",
    ] {
        let mut state = MemoryState::new([Reply::final_response("not invoked")]);
        state.prompts().pending_prompts.push_back(Message {
            role: Role::System,
            contents: vec![Content::Text("pending status".into())],
            ..Message::default()
        });
        state.fail_at = Some(stage);

        assert_eq!(state.run(), Err(Error::failed(stage)));
        assert_eq!(state.trace.values().last().unwrap(), stage);
        assert!(state.provider.requests.lock().unwrap().is_empty());
    }
}

#[test]
fn final_phase_sends_the_configured_steer() {
    let mut state = MemoryState::new([Reply::final_response("answer")]);
    state.maximum = 1;
    state.skill_templates.lock().unwrap().insert(
        "agent-session.final-request".into(),
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
    assert_eq!(state.final_requests, [0, 1]);
    assert_eq!(state.capture_catalogs(), [vec!["read"], vec!["read"]]);
    assert!(state.tools.flags.lock().unwrap()[0].final_phase);
    let trace = state.trace.values();
    assert!(
        trace
            .iter()
            .position(|step| step == "final_request")
            .unwrap()
            < trace.iter().position(|step| step == "render").unwrap()
    );
    assert_eq!(trace.iter().filter(|step| *step == "checkpoint").count(), 2);
}

#[test]
fn raw_and_calibrated_triggers_compact_before_reminder_lookup_and_use_returned_epoch() {
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
        assert!(
            trace.iter().position(|step| step == "compact").unwrap()
                < trace.iter().position(|step| step == "checkpoint").unwrap()
        );
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
    assert!(state.trace.values().iter().any(|step| step == "checkpoint"));
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
fn post_compaction_raw_input_failure_precedes_checkpoint_or_provider_effects() {
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
    assert!(!state.trace.values().iter().any(|step| step == "checkpoint"));
}

#[test]
fn reminder_append_receives_raw_usage_not_the_checkpoint_band_or_candidate_usage() {
    let mut state = MemoryState::new([Reply::final_response("answer")]);
    state.prime_context(490);
    state.request().estimate_script = [550, 630, 630, 630].into();
    assert_eq!(state.run(), Ok(Outcome::Completed("answer".into())));
    assert_eq!(state.reminder_appends.len(), 1);
    assert_eq!(state.reminder_appends[0].0.percentage, 50);
    assert_eq!(state.reminder_appends[0].2, 55);
    let request = state.request();
    let estimates = &request.estimates;
    assert_eq!(estimates[1].tokens, 630);
    assert_eq!(estimates[1].messages.len(), 2);
    assert_eq!(state.reminder_publications, 1);
    let requests = state.provider.requests.lock().unwrap();
    assert_eq!(requests[0].messages, estimates.last().unwrap().messages);
    assert_eq!(
        requests[0].messages.last().unwrap().contents,
        [Content::Text("configured reminder".into())]
    );
}

#[test]
fn duplicate_reminder_does_not_change_history_or_publish() {
    let mut state = MemoryState::new([Reply::final_response("answer")]);
    state.prime_context(490);
    state.append_reminder = false;
    state.request().estimate_script = [550, 630, 550].into();
    assert_eq!(state.run(), Ok(Outcome::Completed("answer".into())));
    assert_eq!(state.reminder_appends.len(), 1);
    assert_eq!(state.request().messages.len(), 1);
    assert_eq!(state.reminder_publications, 0);
    assert_eq!(
        state.provider.requests.lock().unwrap()[0].messages,
        state.request().messages
    );
}

#[test]
fn stored_reminder_prevents_a_duplicate_after_publication_failure() {
    let mut state = MemoryState::new([Reply::final_response("answer")]);
    state.prime_context(550);
    state.fail_at = Some("publish_reminder");
    assert_eq!(state.run(), Err(Error::failed("publish_reminder")));
    assert!(state.provider.requests.lock().unwrap().is_empty());
    assert_eq!(state.prompts().checkpoint.as_ref().unwrap().percentage, 50);

    state.fail_at = None;
    assert_eq!(state.run(), Ok(Outcome::Completed("answer".into())));
    assert_eq!(state.reminder_appends.len(), 1);
    assert_eq!(state.reminder_publications, 1);
}

#[test]
fn reminder_rendering_failure_does_not_suppress_a_retry() {
    let mut state = MemoryState::new([Reply::final_response("answer")]);
    state.prime_context(550);
    state.fail_at = Some("reminder");
    assert_eq!(state.run(), Err(Error::failed("reminder")));
    assert!(state.provider.requests.lock().unwrap().is_empty());
    assert!(state.prompts().checkpoint.is_none());

    state.fail_at = None;
    assert_eq!(state.run(), Ok(Outcome::Completed("answer".into())));
    assert_eq!(state.reminder_appends.len(), 1);
    assert_eq!(state.reminder_publications, 1);
}

#[test]
fn crossing_reminder_candidate_compacts_without_appending_the_reminder() {
    let mut state = MemoryState::new([Reply::final_response("answer")]);
    state.prime_context(490);
    state.request().estimate_script = [550, 910, 100, 100].into();
    let messages = vec![Message {
        role: Role::System,
        contents: vec![Content::Text("compacted".into())],
        ..Message::default()
    }];
    let compacted_state = state.request().context.clone();
    state.compaction_plan = Some(compaction_support::plan(&["original", "retained"]));
    state.summary_provider =
        compaction_support::Provider::new([parrot_core::compaction::SummaryResponse::Summary(
            "reminder summary".into(),
        )]);
    state.compactions.push_back(CompactedContext {
        instructions: "compacted reminder candidate".into(),
        messages: messages.clone(),
        state: compacted_state,
        reduced: true,
    });
    assert_eq!(state.run(), Ok(Outcome::Completed("answer".into())));
    assert!(state.reminder_appends.is_empty());
    assert_eq!(state.reminder_publications, 0);
    assert_eq!(state.request().rendered_instructions.len(), 1);
    assert_eq!(state.summary_provider.requests.lock().unwrap().len(), 1);
    let compacted = state.recorded_compactions[0]
        .as_ref()
        .unwrap()
        .as_ref()
        .unwrap();
    assert_eq!(
        compacted.summary.contents,
        [Content::Text("prefix:reminder summary".into())]
    );
    let requests = state.provider.requests.lock().unwrap();
    assert_eq!(requests[0].instructions, "compacted reminder candidate");
    assert_eq!(requests[0].messages, messages);
}

#[test]
fn unknown_context_uses_output_configuration_without_inventing_reminder_or_compaction() {
    let mut state = MemoryState::new([Reply::final_response("answer")]);
    state.request().estimated_input_tokens = i64::MAX;
    assert_eq!(state.run(), Ok(Outcome::Completed("answer".into())));
    assert!(state.compacted_definitions.is_empty());
    assert!(state.reminder_appends.is_empty());
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
        "checkpoint",
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
fn reminder_append_failure_does_not_suppress_a_retry() {
    let mut state = MemoryState::new([Reply::final_response("answer")]);
    state.prime_context(550);
    state.fail_at = Some("append_reminder");
    assert_eq!(state.run(), Err(Error::failed("append_reminder")));
    assert!(state.provider.requests.lock().unwrap().is_empty());
    assert!(state.prompts().checkpoint.is_none());
    assert_eq!(state.reminder_publications, 0);

    state.fail_at = None;
    assert_eq!(state.run(), Ok(Outcome::Completed("answer".into())));
    assert_eq!(state.reminder_appends.len(), 1);
    assert_eq!(state.reminder_publications, 1);
}

#[test]
fn pending_context_reminders_use_configured_template_overrides() {
    for override_limit in [false, true] {
        let mut state = MemoryState::new([Reply::final_response("answer")]);
        state.prime_context(490);
        state.request().estimated_input_tokens = 550;
        if override_limit {
            state.request().context.configuration.context_limit =
                Some(parrot_core::ContextLimit::Tokens(800));
        }
        let key = if override_limit {
            "agent-session.context-limit-reminder"
        } else {
            "agent-session.context-reminder"
        };
        state.skill_templates.lock().unwrap().insert(
            key.into(),
            "Prepared reminder at {percentage}% with {estimated_tokens} tokens.".into(),
        );

        assert_eq!(state.run(), Ok(Outcome::Completed("answer".into())));
        let requests = state.provider.requests.lock().unwrap();
        assert!(requests[0].messages.iter().any(|message| {
            message.role == Role::System
                && message.contents
                    == [Content::Text(
                        "Prepared reminder at 50% with 550 tokens.".into(),
                    )]
        }));
    }
}

#[test]
fn retry_messages_use_configured_template_overrides() {
    for (reply, key, content) in [
        (
            Reply::truncated("cut"),
            "agent-session.truncated-tool-call",
            "Retry the incomplete tool call.",
        ),
        (
            Reply::invalid("bad"),
            "agent-session.invalid-tool-call",
            "Retry the invalid tool call.",
        ),
    ] {
        let mut state = MemoryState::new([reply, Reply::final_response("answer")]);
        state
            .skill_templates
            .lock()
            .unwrap()
            .insert(key.into(), content.into());

        assert_eq!(state.run(), Ok(Outcome::Completed("answer".into())));
        let requests = state.provider.requests.lock().unwrap();
        assert_eq!(requests.len(), 2);
        assert!(requests[1].messages.iter().any(|message| {
            message.role == Role::System && message.contents == [Content::Text(content.into())]
        }));
    }
}
