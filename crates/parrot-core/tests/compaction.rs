use parrot_core::{
    CancellationToken, CompactionFailure, CompactionProgress, Content, Error, ProviderRequest,
    Role, SummaryResponse, ToolCall, compaction::compact,
};
use std::{
    future::Future,
    sync::Arc,
    task::{Context, Poll, Wake, Waker},
    thread,
};

#[path = "compaction/progress_publisher.rs"]
mod progress_publisher;
#[path = "compaction/support.rs"]
mod support;
use support::{Effects, Provider, message, plan};

#[path = "compaction/fitting.rs"]
mod fitting;

#[path = "compaction/preparation.rs"]
mod preparation;

struct ThreadWake(thread::Thread);
impl Wake for ThreadWake {
    fn wake(self: Arc<Self>) {
        self.0.unpark();
    }
}
fn finish<F: Future>(future: F) -> F::Output {
    let waker = Waker::from(Arc::new(ThreadWake(thread::current())));
    let mut context = Context::from_waker(&waker);
    let mut future = std::pin::pin!(future);
    loop {
        match future.as_mut().poll(&mut context) {
            Poll::Ready(output) => return output,
            Poll::Pending => thread::park(),
        }
    }
}

fn run(
    plan: Option<parrot_core::CompactionPlan>,
    effects: &mut Effects,
    provider: &Provider,
) -> Result<Option<parrot_core::CompactionResult>, Error> {
    let provider = provider.with_estimates(effects);
    let publisher = progress_publisher::ProgressPublisher {
        progress: effects.progress.clone(),
    };
    finish(compact(
        plan,
        effects,
        &provider,
        &publisher,
        parrot_core::CancellationToken::uncancelled(),
    ))
}

fn texts(request: &ProviderRequest) -> Vec<&str> {
    request
        .messages
        .iter()
        .flat_map(|message| &message.contents)
        .filter_map(|content| match content {
            Content::Text(text) => Some(text.as_str()),
            Content::Image { .. } | Content::Artifact(_) => None,
        })
        .collect()
}

#[test]
fn compaction_failures_preserve_cancellation_and_native_diagnostic_categories() {
    assert_eq!(
        Error::from(CompactionFailure::TokenOverflow).code,
        parrot_core::ErrorCode::Unexpected
    );
    assert_eq!(
        Error::from(CompactionFailure::GroupExceedsBudget).code,
        parrot_core::ErrorCode::InvalidOperation
    );
    assert_eq!(
        Error::from(CompactionFailure::TokenOverflow).detail,
        Some(parrot_core::ErrorDetail::Compaction(
            CompactionFailure::TokenOverflow
        ))
    );
    assert_eq!(
        Error::from(CompactionFailure::GroupExceedsBudget).detail,
        Some(parrot_core::ErrorDetail::Compaction(
            CompactionFailure::GroupExceedsBudget
        ))
    );
    assert_eq!(
        Error::from(CompactionFailure::Cancelled),
        Error::cancelled()
    );
}

#[test]
fn no_plan_invokes_neither_provider_nor_progress() {
    let mut effects = Effects::default();
    let provider = Provider::default();
    assert_eq!(run(None, &mut effects, &provider), Ok(None));
    assert!(effects.progress.lock().unwrap().is_empty());
    assert!(effects.scratch_writes.lock().unwrap().is_empty());
    assert!(provider.requests.lock().unwrap().is_empty());
}

#[test]
fn core_builds_private_summary_requests_and_preserves_the_retained_suffix() {
    let mut plan = plan(&["first", "second", "tail"]);
    plan.groups[2].messages[0].provider_id = "opaque-provider".into();
    plan.groups[2].messages[0].provider_content = " { \"future\" : 1.00 } ".into();
    let retained = plan.groups[2].messages.clone();
    let mut effects = Effects::default();
    let provider = Provider::new([SummaryResponse::Summary("result".into())]);
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
                parrot_core::Message {
                    role: Role::System,
                    contents: vec![Content::Text(plan.summary_instructions)],
                    ..parrot_core::Message::default()
                },
                message("first"),
                message("second")
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
    assert_eq!((result.watermark, result.retained_messages), (42, 1));
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
    let provider = Provider::new([
        SummaryResponse::Summary("x".into()),
        SummaryResponse::Summary("y".into()),
    ]);
    let result = run(Some(plan), &mut effects, &provider).unwrap().unwrap();
    let requests = provider.requests.lock().unwrap();
    assert_eq!(texts(&requests[0]), ["instructions", "first-14-bytes!"]);
    assert_eq!(
        texts(&requests[1]),
        ["instructions", "prefix:x", "other-14-bytes!"]
    );
    assert_eq!(result.summary.contents, [Content::Text("prefix:y".into())]);
}

#[test]
fn compaction_uses_one_estimator_for_complete_summary_requests() {
    struct InputOnlyProvider {
        provider: Provider,
        estimates: std::sync::Mutex<Vec<ProviderRequest>>,
    }

    impl parrot_core::Provider for InputOnlyProvider {
        fn estimate_input_tokens(
            &self,
            instructions: &str,
            definitions: &[parrot_core::ToolDefinition],
            messages: &[parrot_core::Message],
        ) -> Result<i64, Error> {
            self.estimates.lock().unwrap().push(ProviderRequest {
                instructions: instructions.into(),
                tools: definitions.to_vec(),
                messages: messages.to_vec(),
                ..ProviderRequest::default()
            });
            Ok(4 + messages
                .iter()
                .flat_map(|message| &message.contents)
                .map(|content| match content {
                    Content::Text(text) => text.len() as i64,
                    _ => panic!("these summary requests contain only text"),
                })
                .sum::<i64>())
        }

        fn summarize<'a, 'c>(
            &'a self,
            request: ProviderRequest,
            cancellation: CancellationToken<'c>,
        ) -> parrot_core::BoxFuture<'a, Result<SummaryResponse, Error>>
        where
            'c: 'a,
        {
            parrot_core::Provider::summarize(&self.provider, request, cancellation)
        }
    }

    let mut plan = plan(&["first-14-bytes!", "other-14-bytes!", "tail"]);
    plan.input_budget = 40;
    let mut effects = Effects::default();
    let provider = InputOnlyProvider {
        provider: Provider::new([
            SummaryResponse::Summary("x".into()),
            SummaryResponse::Summary("y".into()),
        ]),
        estimates: std::sync::Mutex::new(Vec::new()),
    };
    let publisher = progress_publisher::ProgressPublisher {
        progress: effects.progress.clone(),
    };
    let result = finish(compact(
        Some(plan),
        &mut effects,
        &provider,
        &publisher,
        CancellationToken::uncancelled(),
    ))
    .unwrap()
    .unwrap();
    let requests = provider.provider.requests.lock().unwrap();
    assert_eq!(requests.len(), 2);
    assert_eq!(texts(&requests[0]), ["instructions", "first-14-bytes!"]);
    assert_eq!(
        texts(&requests[1]),
        ["instructions", "prefix:x", "other-14-bytes!"]
    );
    let estimates = provider.estimates.lock().unwrap();
    for request in requests.iter() {
        assert!(estimates.iter().any(|estimate| {
            estimate.instructions == request.instructions
                && estimate.tools == request.tools
                && estimate.messages == request.messages
        }));
    }
    assert_eq!(result.summary.contents, [Content::Text("prefix:y".into())]);
}

#[test]
fn empty_summary_fallback_halves_odd_atomic_groups_and_carries_each_actual_summary() {
    let plan = plan(&["a", "b", "c", "d", "e", "tail"]);
    let mut effects = Effects::default();
    let provider = Provider::new([
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
            vec!["instructions", "a", "b", "c", "d", "e"],
            vec!["instructions", "a", "b", "c"],
            vec!["instructions", "a", "b"],
            vec!["instructions", "prefix:A", "c"],
            vec!["instructions", "prefix:B", "d", "e"],
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
    let provider = Provider::default();
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
    let provider = Provider::new([
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
        ["instructions", "prefix:123456789012", "blob"]
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
    let provider = Provider::new([SummaryResponse::Summary("done".into())]);
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
    assert_eq!(texts(&requests[0]), ["instructions", "blob"]);
    assert_eq!(requests[0].messages[1].role, Role::System);
}

#[test]
fn cancelled_group_serialization_does_not_write_scratch_or_invoke_the_provider() {
    struct Cancelled;

    impl parrot_core::Cancellation for Cancelled {
        fn is_cancelled(&self) -> bool {
            true
        }

        fn cancelled(&self) -> parrot_core::BoxFuture<'_, ()> {
            Box::pin(async {})
        }
    }

    let mut plan = plan(&["oversized tool group", "tail"]);
    plan.input_budget = 12 + 5;
    plan.groups[0].messages[0].role = Role::Assistant;
    plan.groups[0].messages[0].tool_calls.push(ToolCall {
        id: "call".into(),
        name: "tool".into(),
        arguments_json: "{}".into(),
    });
    let mut effects = Effects::default();
    let provider = Provider::default().with_estimates(&effects);
    let publisher = progress_publisher::ProgressPublisher {
        progress: effects.progress.clone(),
    };
    let result = finish(compact(
        Some(plan),
        &mut effects,
        &provider,
        &publisher,
        CancellationToken::new(&Cancelled),
    ));
    assert_eq!(result, Err(Error::cancelled()));
    assert!(effects.scratch_writes.lock().unwrap().is_empty());
    assert!(provider.requests.lock().unwrap().is_empty());
}

#[test]
fn a_replacement_exceeding_the_final_input_limit_is_rejected_after_the_summary() {
    let mut plan = plan(&["first", "tail"]);
    plan.input_token_limit = 1;
    let mut effects = Effects::default();
    let provider = Provider::new([SummaryResponse::Summary("summary".into())]);
    assert_eq!(
        run(Some(plan), &mut effects, &provider),
        Err(CompactionFailure::InputLimitExceeded.into())
    );
    assert_eq!(provider.requests.lock().unwrap().len(), 1);
}
