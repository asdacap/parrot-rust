use super::{
    engine::compact,
    model::{CompactionGroup, CompactionPlan, CompactionResult, SummaryResponse},
    test_support::*,
    *,
};
use parrot_core::{
    CompactionFailure, Content, Error, NeverCancelled, ProviderRequest, Reasoning, Role, ToolCall,
};
use std::sync::{Arc, atomic::AtomicBool};

fn plan(groups: &[&str]) -> CompactionPlan {
    CompactionPlan {
        groups: groups
            .iter()
            .map(|text| CompactionGroup {
                messages: vec![message(text)],
                is_complete: true,
            })
            .collect(),
        keep_group_from: groups.len() - 1,
        watermark: 42,
        input_budget: 1000,
        summary_output_tokens: 19,
        input_token_limit: 10000,
        instructions: String::new(),
        tools: Vec::new(),
        fixed_message: message("status"),
        summary_instructions: "instructions".into(),
        summary_prefix: "prefix:".into(),
        model: "summary-model".into(),
        reasoning: Some(Reasoning {
            effort: "low".into(),
            summary: "auto".into(),
        }),
    }
}

fn run(
    plan: Option<CompactionPlan>,
    effects: &mut Effects,
    script: &Arc<Script>,
) -> Result<Option<CompactionResult>, Error> {
    run_with(
        plan,
        effects,
        script,
        &Estimates::default(),
        &NeverCancelled,
    )
}

fn run_with(
    plan: Option<CompactionPlan>,
    effects: &Effects,
    script: &Arc<Script>,
    estimates: &Estimates,
    cancellation: &dyn parrot_core::Cancellation,
) -> Result<Option<CompactionResult>, Error> {
    let sessions = crate::sessions::ProviderSessions::new();
    let selection = script.selection();
    let templates = templates();
    finish(compact(
        plan,
        &Execution {
            estimates,
            sessions: &sessions,
            selected: &selection.canonical_model,
            effects,
            templates: &templates,
            cancellation,
        },
    ))
}

fn texts(request: &ProviderRequest) -> Vec<&str> {
    request
        .messages
        .iter()
        .flat_map(|message| &message.contents)
        .filter_map(|content| match content {
            Content::Text(text) => Some(text.as_str()),
            Content::Image { .. } => None,
        })
        .collect()
}

#[test]
fn no_plan_invokes_neither_provider_nor_progress() {
    let mut effects = Effects::default();
    let provider = Arc::new(Script::default());
    assert_eq!(run(None, &mut effects, &provider), Ok(None));
    assert!(effects.progress.lock().unwrap().is_empty());
    assert!(effects.scratch_writes.lock().unwrap().is_empty());
    assert!(provider.requests.lock().unwrap().is_empty());
}

#[test]
fn builds_private_summary_requests_and_preserves_the_retained_suffix() {
    let mut plan = plan(&["first", "second", "tail"]);
    plan.groups[2].messages[0].provider_id = "opaque-provider".into();
    plan.groups[2].messages[0].provider_content = " { \"future\" : 1.00 } ".into();
    let retained = plan.groups[2].messages.clone();
    let mut effects = Effects::default();
    let provider = Script::summaries([SummaryResponse::Summary("result".into())]);
    let result = run(Some(plan.clone()), &mut effects, &provider)
        .unwrap()
        .unwrap();
    let requests = provider.requests.lock().unwrap();
    assert_eq!(requests.len(), 1);
    assert_eq!(
        requests[0],
        ProviderRequest {
            model: plan.model,
            messages: vec![
                message("first"),
                message("second"),
                parrot_core::Message {
                    role: Role::System,
                    contents: vec![Content::Text(plan.summary_instructions)],
                    ..parrot_core::Message::default()
                },
            ],
            max_output_tokens: plan.summary_output_tokens,
            reasoning: plan.reasoning,
            ..ProviderRequest::default()
        }
    );
    assert_eq!(result.history[0], result.summary);
    assert_eq!(result.history[1], plan.fixed_message);
    assert_eq!(result.history[2..], retained);
    assert_eq!(
        result.summary.contents,
        [Content::Text("prefix:result".into())]
    );
    assert_eq!((result.watermark, result.history.len() - 2), (42, 1));
    assert_eq!(
        *effects.progress.lock().unwrap(),
        [
            CompactionProgress {
                completed_groups: 0,
                total_groups: 1
            },
            CompactionProgress {
                completed_groups: 1,
                total_groups: 1
            }
        ]
    );
}

#[test]
fn chunking_uses_the_actual_previous_summary_without_replaying_consumed_groups() {
    let mut plan = plan(&["first-14-bytes!", "other-14-bytes!", "tail"]);
    plan.input_budget = 12 + 25;
    let mut effects = Effects::default();
    let provider = Script::summaries([
        SummaryResponse::Summary("x".into()),
        SummaryResponse::Summary("y".into()),
    ]);
    let result = run(Some(plan), &mut effects, &provider).unwrap().unwrap();
    let requests = provider.requests.lock().unwrap();
    assert_eq!(texts(&requests[0]), ["first-14-bytes!", "instructions"]);
    assert_eq!(
        texts(&requests[1]),
        ["prefix:x", "other-14-bytes!", "instructions"]
    );
    assert_eq!(result.summary.contents, [Content::Text("prefix:y".into())]);
}

#[test]
fn empty_summary_fallback_halves_odd_atomic_groups_and_carries_each_actual_summary() {
    let plan = plan(&["a", "b", "c", "d", "e", "tail"]);
    let mut effects = Effects::default();
    let provider = Script::summaries([
        SummaryResponse::NoSummary("root empty".into()),
        SummaryResponse::NoSummary("first half empty".into()),
        SummaryResponse::Summary("A".into()),
        SummaryResponse::Summary("B".into()),
        SummaryResponse::Summary("C".into()),
    ]);
    let result = run(Some(plan), &mut effects, &provider).unwrap().unwrap();
    let requests = provider.requests.lock().unwrap();
    assert_eq!(
        requests.iter().map(texts).collect::<Vec<_>>(),
        [
            vec!["a", "b", "c", "d", "e", "instructions"],
            vec!["a", "b", "c", "instructions"],
            vec!["a", "b", "instructions"],
            vec!["prefix:A", "c", "instructions"],
            vec!["prefix:B", "d", "e", "instructions"],
        ]
    );
    assert_eq!(result.summary.contents, [Content::Text("prefix:C".into())]);
    assert_eq!(
        effects.progress.lock().unwrap().last(),
        Some(&CompactionProgress {
            completed_groups: 5,
            total_groups: 5
        })
    );
}

#[test]
fn initial_spills_commit_in_source_order_before_a_later_ineligible_group_fails() {
    let mut plan = plan(&[
        "too large first tool group",
        "too large plain user group",
        "tail",
    ]);
    plan.input_budget = 12 + 5;
    plan.groups[0].messages[0].role = Role::Assistant;
    plan.groups[0].messages[0].tool_calls.push(ToolCall {
        id: "call".into(),
        name: "tool".into(),
        arguments_json: "{}".into(),
    });

    let mut effects = Effects::default();
    let provider = Arc::new(Script::default());
    assert_eq!(
        run(Some(plan), &mut effects, &provider),
        Err(CompactionFailure::GroupExceedsBudget.into())
    );
    let writes = effects.scratch_writes.lock().unwrap();
    assert_eq!(writes.len(), 1);
    let artifact: serde_json::Value = serde_json::from_slice(&writes[0]).unwrap();
    assert_eq!(
        artifact,
        serde_json::json!({ "messages": [{
            "role": "Assistant",
            "contents": [{ "kind": "Text", "text": "too large first tool group", "path": "", "media_type": "" }],
            "tool_calls": [{ "id": "call", "name": "tool", "arguments_json": "{}" }],
            "tool_call_id": ""
        }] })
    );
    assert!(provider.requests.lock().unwrap().is_empty());
}

#[test]
fn a_previous_summary_can_force_one_late_spill_of_the_original_atomic_group() {
    let mut plan = plan(&["first-14-bytes!", "other-14-bytes!", "tail"]);
    plan.input_budget = 12 + 25;
    plan.groups[1].messages[0].role = Role::Assistant;
    plan.groups[1].messages[0].tool_calls.push(ToolCall {
        id: "call".into(),
        name: "tool".into(),
        arguments_json: " { \"x\" : 1.00 } ".into(),
    });

    let mut effects = Effects::default();
    let provider = Script::summaries([
        SummaryResponse::Summary("123456789012".into()),
        SummaryResponse::Summary("done".into()),
    ]);
    run(Some(plan), &mut effects, &provider).unwrap().unwrap();
    let writes = effects.scratch_writes.lock().unwrap();
    assert_eq!(writes.len(), 1);
    let artifact: serde_json::Value = serde_json::from_slice(&writes[0]).unwrap();
    assert_eq!(
        artifact,
        serde_json::json!({ "messages": [{
            "role": "Assistant",
            "contents": [{ "kind": "Text", "text": "other-14-bytes!", "path": "", "media_type": "" }],
            "tool_calls": [{ "id": "call", "name": "tool", "arguments_json": " { \"x\" : 1.00 } " }],
            "tool_call_id": ""
        }] })
    );
    let requests = provider.requests.lock().unwrap();
    assert_eq!(
        texts(&requests[1]),
        ["prefix:123456789012", "blob", "instructions"]
    );
}

#[test]
fn oversized_groups_write_json_with_image_path_before_rendering_the_notice() {
    let mut plan = plan(&["oversized tool call", "tail"]);
    plan.input_budget = 12 + 5;
    plan.groups[0].messages[0].role = Role::Assistant;
    plan.groups[0].messages[0].tool_calls.push(ToolCall {
        id: "call".into(),
        name: "tool".into(),
        arguments_json: " { \"x\" : 1.00 } ".into(),
    });
    plan.groups[0].messages.push(parrot_core::Message {
        role: Role::Tool,
        tool_call_id: "call".into(),
        contents: vec![Content::Image {
            path: "image.bin".into(),
            url: String::new(),
            media_type: "image/test".into(),
            width: 1,
            height: 1,
            bytes: None,
        }],
        ..parrot_core::Message::default()
    });
    let mut effects = Effects::default();
    let provider = Script::summaries([SummaryResponse::Summary("done".into())]);
    run(Some(plan), &mut effects, &provider).unwrap().unwrap();
    let writes = effects.scratch_writes.lock().unwrap();
    assert_eq!(writes.len(), 1);
    let artifact: serde_json::Value = serde_json::from_slice(&writes[0]).unwrap();
    assert_eq!(
        artifact["messages"][0]["tool_calls"][0]["arguments_json"],
        " { \"x\" : 1.00 } "
    );
    assert_eq!(artifact["messages"][1]["role"], "Tool");
    assert_eq!(artifact["messages"][1]["tool_call_id"], "call");
    assert_eq!(artifact["messages"][1]["contents"][0]["path"], "image.bin");
    assert_eq!(
        artifact["messages"][1]["contents"][0]["media_type"],
        "image/test"
    );
    let requests = provider.requests.lock().unwrap();
    assert_eq!(texts(&requests[0]), ["blob", "instructions"]);
    assert_eq!(requests[0].messages[0].role, Role::System);
}

#[test]
fn a_replacement_exceeding_the_final_input_limit_is_rejected_after_the_summary() {
    let mut plan = plan(&["first", "tail"]);
    plan.input_token_limit = 1;
    let mut effects = Effects::default();
    let provider = Script::summaries([SummaryResponse::Summary("summary".into())]);
    assert_eq!(
        run(Some(plan), &mut effects, &provider),
        Err(CompactionFailure::InputLimitExceeded.into())
    );
    assert_eq!(provider.requests.lock().unwrap().len(), 1);
}

#[test]
fn complete_summary_requests_include_estimator_overhead() {
    let mut plan = plan(&["first-14-bytes!", "other-14-bytes!", "tail"]);
    plan.input_budget = 40;
    let effects = Effects::default();
    let script = Script::summaries([
        SummaryResponse::Summary("x".into()),
        SummaryResponse::Summary("y".into()),
    ]);
    let estimates = Estimates {
        input_overhead: 4,
        ..Default::default()
    };
    let result = run_with(Some(plan), &effects, &script, &estimates, &NeverCancelled)
        .unwrap()
        .unwrap();
    let requests = script.requests.lock().unwrap();
    assert_eq!(requests.len(), 2);
    assert_eq!(texts(&requests[0]), ["first-14-bytes!", "instructions"]);
    assert_eq!(
        texts(&requests[1]),
        ["prefix:x", "other-14-bytes!", "instructions"]
    );
    for request in requests.iter() {
        assert!(
            estimates
                .requests
                .lock()
                .unwrap()
                .iter()
                .any(|estimate| estimate.instructions == request.instructions
                    && estimate.tools == request.tools
                    && estimate.messages == request.messages)
        );
    }
    assert_eq!(result.summary.contents, [Content::Text("prefix:y".into())]);
}

#[test]
fn cancelled_serialization_neither_writes_scratch_nor_invokes_provider() {
    let mut plan = plan(&["oversized tool group", "tail"]);
    plan.input_budget = 17;
    plan.groups[0].messages[0].role = Role::Assistant;
    plan.groups[0].messages[0].tool_calls.push(ToolCall {
        id: "call".into(),
        name: "tool".into(),
        arguments_json: "{}".into(),
    });
    let effects = Effects::default();
    let script = Arc::new(Script::default());
    let cancellation = Flag(AtomicBool::new(true));
    assert_eq!(
        run_with(
            Some(plan),
            &effects,
            &script,
            &Estimates::default(),
            &cancellation
        ),
        Err(Error::cancelled())
    );
    assert!(effects.scratch_writes.lock().unwrap().is_empty());
    assert!(script.requests.lock().unwrap().is_empty());
}
