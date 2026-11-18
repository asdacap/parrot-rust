use super::{
    engine::compact,
    model::{CompactionGroup, CompactionPlan, CompactionResult, SummaryResponse},
    test_support::*,
    *,
};
use crate::sessions::Request;
use parrot_core::{CompactionFailure, Content, Error, NeverCancelled, Reasoning, Role, ToolCall};
use std::sync::{Arc, atomic::AtomicBool};

fn plan(groups: &[&str]) -> CompactionPlan {
    CompactionPlan {
        groups: groups
            .iter()
            .map(|text| CompactionGroup {
                messages: vec![message(text)],
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

fn texts(request: &Request) -> Vec<&str> {
    request
        .messages
        .iter()
        .flat_map(|message| message.contents())
        .filter_map(|content| match content {
            crate::sessions::Content::Text(text) => Some(text.as_str()),
            crate::sessions::Content::Image { .. } => None,
        })
        .collect()
}

#[test]
fn no_plan_invokes_neither_provider_nor_progress() {
    let mut effects = Effects::default();
    let provider = Arc::new(Script::default());
    assert_eq!(run(None, &mut effects, &provider), Ok(None));
    assert!(effects.progress.lock().unwrap().is_empty());
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
        Request {
            fast_mode: false,
            model: plan.model,
            instructions: String::new(),
            messages: vec![
                message("first").into(),
                message("second").into(),
                parrot_core::Message {
                    role: Role::System,
                    contents: vec![Content::Text(plan.summary_instructions)],
                    ..parrot_core::Message::default()
                }
                .into(),
            ],
            tools: Vec::new(),
            max_output_tokens: plan.summary_output_tokens,
            reasoning: plan.reasoning,
            provider_preferences: String::new(),
            include_router_metadata: false,
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
fn oversized_user_text_is_truncated_to_the_actual_budget_without_a_file() {
    let original = "界🦜".repeat(500);
    let mut plan = plan(&[&original, "tail"]);
    plan.input_budget = 48;
    let effects = Effects::default();
    let provider = Script::summaries([SummaryResponse::Summary("done".into())]);
    let estimates = Estimates {
        input_overhead: 7,
        ..Default::default()
    };
    run_with(
        Some(plan.clone()),
        &effects,
        &provider,
        &estimates,
        &NeverCancelled,
    )
    .unwrap()
    .unwrap();
    let requests = provider.requests.lock().unwrap();
    assert_eq!(requests.len(), 1);
    let crate::sessions::Content::Text(clipped) = &requests[0].messages[0].contents()[0] else {
        panic!("expected retained text prefix")
    };
    assert!(clipped.ends_with("[...]"));
    assert!(original.starts_with(clipped.strip_suffix("[...]").unwrap()));
    assert!(clipped.len() < original.len());
    let core_messages: Vec<parrot_core::Message> = requests[0]
        .messages
        .clone()
        .into_iter()
        .map(Into::into)
        .collect();
    assert!(
        parrot_core::Provider::estimate_input_tokens(&estimates, "", &[], &core_messages).unwrap()
            <= plan.input_budget
    );
    assert_eq!(
        plan.groups[0].messages[0].contents,
        [Content::Text(original)]
    );
}

#[test]
fn oversized_tool_and_user_groups_are_truncated_without_changing_source_history() {
    let mut plan = plan(&[
        &"tool-output".repeat(100),
        &"user-input".repeat(100),
        "tail",
    ]);
    plan.input_budget = 40;
    plan.groups[0].messages[0].role = Role::Assistant;
    plan.groups[0].messages[0].tool_calls.push(ToolCall {
        id: "call".into(),
        name: "tool".into(),
        arguments_json: " { \"x\" : 1.00 } ".into(),
    });
    let original = plan.clone();
    let mut effects = Effects::default();
    let provider = Script::summaries([
        SummaryResponse::Summary("first".into()),
        SummaryResponse::Summary("done".into()),
    ]);
    run(Some(plan.clone()), &mut effects, &provider)
        .unwrap()
        .unwrap();
    let requests = provider.requests.lock().unwrap();
    assert!(!requests.is_empty());
    assert_eq!(
        requests[0].messages[0].tool_calls,
        original.groups[0].messages[0].tool_calls
    );
    assert_eq!(requests[0].messages[0].role, Role::Assistant);
    assert!(
        requests
            .iter()
            .flat_map(texts)
            .any(|text| text.ends_with("[...]"))
    );
    assert_eq!(plan, original);
    assert!(requests.iter().all(|request| {
        texts(request)
            .iter()
            .map(|text| text.len() as i64)
            .sum::<i64>()
            <= 40
    }));
}

#[test]
fn a_previous_summary_forces_late_truncation_of_the_atomic_group() {
    let mut plan = plan(&["first-14-bytes!", "other-14-bytes!", "tail"]);
    plan.input_budget = 37;
    plan.groups[1].messages[0].role = Role::Assistant;
    plan.groups[1].messages[0].tool_calls.push(ToolCall {
        id: "call".into(),
        name: "tool".into(),
        arguments_json: " { \"x\" : 1.00 } ".into(),
    });
    let calls = plan.groups[1].messages[0].tool_calls.clone();
    let mut effects = Effects::default();
    let provider = Script::summaries([
        SummaryResponse::Summary("123456789012".into()),
        SummaryResponse::Summary("done".into()),
    ]);
    run(Some(plan), &mut effects, &provider).unwrap().unwrap();
    let requests = provider.requests.lock().unwrap();
    assert_eq!(requests.len(), 2);
    assert_eq!(texts(&requests[1])[0], "prefix:123456789012");
    let clipped = texts(&requests[1])[1];
    assert!(clipped.ends_with("[...]"));
    assert!("other-14-bytes!".starts_with(clipped.strip_suffix("[...]").unwrap()));
    assert!(
        texts(&requests[1])
            .iter()
            .map(|text| text.len())
            .sum::<usize>()
            <= 37
    );
    assert_eq!(texts(&requests[1])[2], "instructions");
    assert_eq!(requests[1].messages[1].tool_calls, calls);
}

#[test]
fn truncation_omits_images_but_keeps_atomic_tool_calls_and_result_identities() {
    let mut plan = plan(&[&"oversized tool call".repeat(20), "tail"]);
    plan.input_budget = 30;
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
    let source = plan.groups[0].messages.clone();
    let mut effects = Effects::default();
    let provider = Script::summaries([SummaryResponse::Summary("done".into())]);
    run(Some(plan.clone()), &mut effects, &provider)
        .unwrap()
        .unwrap();
    let requests = provider.requests.lock().unwrap();
    let messages = &requests[0].messages;
    assert_eq!(messages[0].role, Role::Assistant);
    assert_eq!(messages[0].tool_calls, source[0].tool_calls);
    assert_eq!(messages[1].role, Role::Tool);
    assert_eq!(messages[1].tool_call_id, "call");
    assert_eq!(
        messages[1].contents(),
        [crate::sessions::Content::Text("[...]".into())]
    );
    assert!(
        texts(&requests[0])
            .iter()
            .map(|text| text.len() as i64)
            .sum::<i64>()
            <= 30
    );
    assert_eq!(plan.groups[0].messages, source);
}

#[test]
fn irreducible_request_overhead_returns_an_error_without_a_summary_request() {
    let mut plan = plan(&["too large", "tail"]);
    plan.input_budget = 10;
    let script = Arc::new(Script::default());
    let effects = Effects::default();
    let estimates = Estimates {
        input_overhead: 20,
        ..Default::default()
    };
    assert_eq!(
        run_with(Some(plan), &effects, &script, &estimates, &NeverCancelled),
        Err(CompactionFailure::GroupExceedsBudget.into())
    );
    assert!(script.requests.lock().unwrap().is_empty());
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
        let core_messages: Vec<parrot_core::Message> = request
            .messages
            .clone()
            .into_iter()
            .map(Into::into)
            .collect();
        assert!(
            estimates
                .requests
                .lock()
                .unwrap()
                .iter()
                .any(|estimate| estimate.instructions == request.instructions
                    && estimate.tools == request.tools
                    && estimate.messages == core_messages)
        );
    }
    assert_eq!(result.summary.contents, [Content::Text("prefix:y".into())]);
}

#[test]
fn cancelled_truncation_does_not_invoke_provider() {
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
    assert!(script.requests.lock().unwrap().is_empty());
}
