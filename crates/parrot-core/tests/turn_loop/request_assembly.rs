use super::*;

#[test]
fn state_supplies_fresh_raw_instructions_on_each_request() {
    let mut state = MemoryState::new([
        Reply::final_response("candidate"),
        Reply::final_response("answer"),
    ]);
    state
        .prompts()
        .exit_blockers
        .push_back(vec!["retry".into()]);
    state.request().renderings = ["first state instructions", "updated state instructions"]
        .into_iter()
        .map(|instructions| RenderedContext {
            instructions: instructions.into(),
            maximum_output_tokens: 100,
            messages: Vec::new(),
        })
        .collect();

    let tools = state.tools.clone();
    let provider = state.provider.clone();

    state.budget = TurnBudget::new(state.unlimited);

    assert_eq!(
        finish(run(
            &mut state,
            &tools,
            &provider,
            CancellationToken::new(provider.cancellation.as_ref())
        )),
        Ok(Outcome::Completed("answer".into()))
    );
    let requests = provider.requests.lock().unwrap();
    assert_eq!(requests.len(), 2);
    assert_eq!(requests[0].instructions, "first state instructions");
    assert_eq!(requests[1].instructions, "updated state instructions");
}

#[test]
fn core_assembles_every_request_field_from_explicit_state_inputs_after_preparation() {
    let mut state = MemoryState::new([Reply::final_response("answer")]);
    state.model = "provider/model-with-options".into();
    state.reasoning = Some(Reasoning {
        effort: "custom effort".into(),
        summary: "raw summary mode".into(),
    });
    state.provider_preferences = " { \"future\" : [1.2300, true] } \n".into();
    state.include_router_metadata = true;
    state.request().messages = vec![Message {
        contents: vec![Content::Text("stale before preparation".into())],
        ..Message::default()
    }];
    let messages = vec![
        Message {
            role: Role::Assistant,
            contents: vec![Content::Text("raw assistant \n\ttext".into())],
            tool_calls: vec![ToolCall {
                id: "historical-call".into(),
                name: "read alias".into(),
                arguments_json: " { \"n\" : 1.2300 } ".into(),
            }],
            tool_call_id: String::new(),
            provider_id: "future-provider".into(),
            provider_content: " { \"opaque\" : 1.2300 } \n".into(),
        },
        Message {
            role: Role::User,
            contents: vec![
                Content::Text("prefix".into()),
                Content::Image {
                    path: "relative/raw image.webp".into(),
                    url: "https://example.test/image?raw=1".into(),
                    media_type: "image/webp".into(),
                    width: 641,
                    height: 479,
                    bytes: Some(vec![0, 1, 127, 255]),
                },
                Content::Text("suffix".into()),
            ],
            ..Message::default()
        },
    ];
    let instructions = "prepared instructions \n\tunchanged".to_owned();
    state.request().renderings.push_back(RenderedContext {
        instructions: instructions.clone(),
        maximum_output_tokens: 1379,
        messages: messages.clone(),
    });
    assert_eq!(state.run(), Ok(Outcome::Completed("answer".into())));
    let requests = state.provider.requests.lock().unwrap();
    assert_eq!(requests.len(), 1);
    assert_eq!(
        requests[0],
        ProviderRequest {
            model: state.model.clone(),
            instructions,
            messages,
            tools: state.capture_request_estimates()[0].definitions.clone(),
            max_output_tokens: 1379,
            reasoning: state.reasoning.clone(),
            provider_preferences: state.provider_preferences.clone(),
            include_router_metadata: true,
        }
    );
    assert_eq!(
        state
            .trace
            .values()
            .into_iter()
            .filter(|step| matches!(step.as_str(), "render" | "messages" | "invoke"))
            .collect::<Vec<_>>(),
        ["render", "messages", "messages", "invoke"]
    );
}

#[test]
fn oversized_compaction_retries_prepare_and_read_fresh_context_and_messages() {
    let mut state = MemoryState::new([
        Reply::TooLarge("oversized"),
        Reply::final_response("answer"),
    ]);
    let before = vec![Message {
        contents: vec![Content::Text("uncompacted messages".into())],
        ..Message::default()
    }];
    let after = vec![Message {
        role: Role::System,
        contents: vec![Content::Text("compacted summary".into())],
        ..Message::default()
    }];
    state.request().renderings = [
        RenderedContext {
            instructions: "before compaction".into(),
            maximum_output_tokens: 91,
            messages: before.clone(),
        },
        RenderedContext {
            instructions: "after compaction".into(),
            maximum_output_tokens: 4093,
            messages: after.clone(),
        },
    ]
    .into();
    assert_eq!(state.run(), Ok(Outcome::Completed("answer".into())));
    let requests = state.provider.requests.lock().unwrap();
    assert_eq!(requests.len(), 2);
    assert_eq!(requests[0].instructions, "before compaction");
    assert_eq!(requests[0].max_output_tokens, 91);
    assert_eq!(requests[0].messages, before);
    assert_eq!(requests[1].instructions, "after compaction");
    assert_eq!(requests[1].max_output_tokens, 4093);
    assert_eq!(requests[1].messages, after);
    for (request, estimate) in requests.iter().zip(state.capture_request_estimates()) {
        assert_eq!(request.tools, estimate.definitions);
        assert_eq!(request.messages, estimate.messages);
        assert_eq!(request.instructions, estimate.instructions);
    }
    assert_eq!(
        state
            .trace
            .values()
            .into_iter()
            .filter(|step| matches!(step.as_str(), "render" | "messages" | "invoke" | "compact"))
            .collect::<Vec<_>>(),
        [
            "render", "messages", "messages", "invoke", "compact", "render", "messages",
            "messages", "invoke"
        ]
    );
}

#[test]
fn tool_followup_reads_messages_and_context_again_after_results_are_recorded() {
    let mut state = MemoryState::new([Reply::tools("call"), Reply::final_response("answer")]);
    let messages = vec![Message {
        role: Role::Tool,
        contents: vec![Content::Text("state-projected tool result".into())],
        tool_call_id: "call-1".into(),
        ..Message::default()
    }];
    state.request().renderings = [
        RenderedContext {
            instructions: "initial".into(),
            maximum_output_tokens: 83,
            messages: vec![],
        },
        RenderedContext {
            instructions: "followup".into(),
            maximum_output_tokens: 41,
            messages: messages.clone(),
        },
    ]
    .into();
    assert_eq!(state.run(), Ok(Outcome::Completed("answer".into())));
    let requests = state.provider.requests.lock().unwrap();
    assert_eq!(requests.len(), 2);
    assert!(requests[0].messages.is_empty());
    assert_eq!(requests[1].messages, messages);
    assert_eq!(requests[1].instructions, "followup");
    assert_eq!(requests[1].max_output_tokens, 41);
    assert_eq!(state.applied_results.len(), 1);
    assert_eq!(
        state
            .trace
            .values()
            .into_iter()
            .filter(|step| matches!(
                step.as_str(),
                "record_results" | "render" | "messages" | "invoke"
            ))
            .collect::<Vec<_>>(),
        [
            "render",
            "messages",
            "messages",
            "invoke",
            "record_results",
            "render",
            "messages",
            "messages",
            "invoke"
        ]
    );
}

#[test]
fn context_preparation_and_message_read_errors_stop_before_provider_invocation() {
    for stage in ["instruction_state", "render", "messages"] {
        let mut state = MemoryState::new([Reply::final_response("not invoked")]);
        state.fail_at = Some(stage);
        assert_eq!(state.run(), Err(Error::failed(stage)));
        assert!(state.provider.requests.lock().unwrap().is_empty());
        assert!(state.recorded_tools.is_empty());
        assert!(state.tools.requests.lock().unwrap().is_empty());
        assert_eq!(state.trace.values().last().unwrap(), stage);
        assert_eq!(
            state.trace.values().iter().any(|step| step == "messages"),
            stage == "messages"
        );
    }
}

#[test]
fn three_inputs_render_raw_system_state_with_configured_scalar_and_minijinja_templates() {
    let mut state = MemoryState::new([Reply::final_response("answer")]);
    state.request().instructions = "raw system instructions".into();
    state.prompt_templates = vec![
        ("memory:00-list".into(), "configured.list".into()),
        ("memory:10-scalar".into(), "configured.scalar".into()),
    ];
    state.template_arguments.insert(
        "memory:00-list".into(),
        vec![
            ("title".into(), serde_json::json!("literal {{ untouched }}")),
            ("items".into(), serde_json::json!(["alpha", "beta"])),
        ],
    );
    state.template_arguments.insert(
        "memory:10-scalar".into(),
        vec![("content".into(), serde_json::json!("opaque {title}"))],
    );
    state
        .template_engines
        .insert("configured.list".into(), "minijinja".into());
    state.skill_templates.lock().unwrap().extend([
        (
            "configured.list".into(),
            "{{ title }}{% for item in items %}[{{ item }}]{% endfor %}".into(),
        ),
        ("configured.scalar".into(), "configured {content}".into()),
    ]);
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

    let requests = provider.requests.lock().unwrap();
    assert_eq!(requests.len(), 1);
    assert_eq!(
        requests[0].instructions,
        "literal {{ untouched }}[alpha][beta]\n\nconfigured opaque {title}\n\nraw system instructions"
    );
    assert_eq!(
        state.capture_request_estimates()[0].instructions,
        requests[0].instructions
    );
}
