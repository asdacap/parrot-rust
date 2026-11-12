use super::*;

#[test]
fn initialized_results_remain_untouched_when_core_has_no_work_or_is_cancelled() {
    for cancelled in [false, true] {
        let mut state = MemoryState::new([]);
        state.pending_messages.clear();
        if cancelled {
            state.cancellation.cancel();
        }
        let initialized = vec![ToolResult {
            call_id: "existing-result".into(),
            tool_name: "read".into(),
            text: "already initialized result".into(),
            ..ToolResult::default()
        }];
        state.applied_results = initialized.clone();
        let expected = if cancelled {
            Err(Error::cancelled())
        } else {
            Ok(Outcome::Completed(String::new()))
        };
        assert_eq!(state.run(), expected);
        assert_eq!(state.applied_results, initialized);
        assert!(state.tools.requests.lock().unwrap().is_empty());
        assert!(state.provider.requests.lock().unwrap().is_empty());
        assert!(state.recorded_tools.is_empty());
        assert!(state.trace.values().is_empty());
    }
}

#[test]
fn core_executes_the_provider_batch_once_and_records_each_result_once() {
    let calls: Vec<_> = ["first", "second", "third"]
        .into_iter()
        .map(|id| ToolCall {
            id: id.into(),
            name: "read".into(),
            arguments_json: format!(" {{ \"call\" : \"{id}\" }} "),
        })
        .collect();
    let results: Vec<_> = calls
        .iter()
        .zip([
            ToolStatus::Finished,
            ToolStatus::Cancelled,
            ToolStatus::Error,
        ])
        .map(|(call, status)| ToolResult {
            call_id: call.id.clone(),
            tool_name: call.name.clone(),
            status,
            text: format!("result for {}", call.id),
            ..ToolResult::default()
        })
        .collect();
    let mut state = MemoryState::new([
        Reply::with_tool_calls(calls.clone()),
        Reply::final_response("answer"),
    ]);
    state
        .tools
        .replies
        .lock()
        .unwrap()
        .push_back(results.clone());
    assert_eq!(state.run(), Ok(Outcome::Completed("answer".into())));
    assert_eq!(
        state
            .trace
            .values()
            .into_iter()
            .filter(|step| matches!(step.as_str(), "record_tools" | "execute" | "record_results"))
            .collect::<Vec<_>>(),
        [
            "record_tools",
            "execute",
            "record_results",
            "record_results",
            "record_results"
        ]
    );
    assert_eq!(*state.tools.requests.lock().unwrap(), [calls]);
    assert_eq!(state.recorded_tools.len(), 1);
    assert_eq!(state.applied_results, results);
    assert_eq!(
        state
            .applied_results
            .iter()
            .map(|result| result.call_id.as_str())
            .collect::<Vec<_>>(),
        ["first", "second", "third"]
    );
}

#[test]
fn failed_result_recording_stops_without_reexecuting_the_batch_or_requesting_more_work() {
    let calls: Vec<_> = ["first", "second"]
        .into_iter()
        .map(|id| ToolCall {
            id: id.into(),
            name: "read".into(),
            arguments_json: "{}".into(),
        })
        .collect();
    let mut state = MemoryState::new([Reply::with_tool_calls(calls.clone())]);
    state.fail_at = Some("record_results");
    assert_eq!(state.run(), Err(Error::failed("record_results")));
    assert_eq!(*state.tools.requests.lock().unwrap(), [calls]);
    assert_eq!(state.provider.requests.lock().unwrap().len(), 1);
    assert_eq!(state.recorded_tools.len(), 1);
    assert!(state.applied_results.is_empty());
    assert_eq!(state.trace.values().last().unwrap(), "record_results");
}

#[test]
fn empty_execution_output_errors_without_recording_or_repeating_work() {
    let mut state = MemoryState::new([Reply::tools("call")]);
    state.tools.replies.lock().unwrap().push_back(Vec::new());
    assert_eq!(
        state.run(),
        Err(Error::failed("tool execution returned no results"))
    );
    assert_eq!(state.tools.requests.lock().unwrap().len(), 1);
    let trace = state.trace.values();
    assert_eq!(trace.last().unwrap(), "execute");
    assert!(trace.iter().any(|step| step == "record_tools"));
    assert!(!trace.iter().any(|step| step == "record_results"));
    assert!(state.applied_results.is_empty());
}

#[test]
fn complete_fresh_tool_results_reach_state_without_rewriting_metadata() {
    let mut state = MemoryState::new([
        Reply::with_tool_calls(vec![ToolCall {
            id: "image-process".into(),
            name: "read".into(),
            arguments_json: "{}".into(),
        }]),
        Reply::final_response("answer"),
    ]);
    let result = ToolResult {
        call_id: "image-process".into(),
        tool_name: "read".into(),
        status: ToolStatus::Finished,
        text: "raw result \n\ttext".into(),
        error_message: String::new(),
        images: vec![ToolImage {
            artifact_id: "artifact".into(),
            sha256: "digest".into(),
            media_type: "image/webp".into(),
            byte_length: 99,
            width: 640,
            height: 480,
            frame_count: 2,
            aggregate_pixels: 614_400,
            display_name: "display".into(),
            origin: "origin".into(),
        }],
        metadata: Some(vec![0, 255, 128, 1]),
    };
    state
        .tools
        .replies
        .lock()
        .unwrap()
        .push_back(vec![result.clone()]);
    assert_eq!(state.run(), Ok(Outcome::Completed("answer".into())));
    assert_eq!(state.applied_results, [result]);
    assert_eq!(state.recorded_tools.len(), 1);
}

#[test]
fn core_passes_each_recording_receipt_and_turn_flags_into_execution() {
    let mut state = MemoryState::new([
        Reply::tools("first"),
        Reply::tools("second"),
        Reply::final_response("answer"),
    ]);
    state.maximum = 2;

    assert_eq!(state.run(), Ok(Outcome::Completed("answer".into())));
    assert_eq!(
        *state.tools.receipts.lock().unwrap(),
        [
            ToolCallReceipt {
                assistant_sequence: 1,
            },
            ToolCallReceipt {
                assistant_sequence: 2,
            },
        ]
    );
    let flags = state.tools.flags.lock().unwrap();
    assert_eq!(flags.len(), 2);
    assert!(!flags[0].final_phase);
    assert!(flags[1].final_phase);
}

struct EchoTools;

impl Tools<Vec<u8>> for EchoTools {
    fn capture_catalog(&self) -> BoxFuture<'_, Result<ToolCatalog, Error>> {
        Box::pin(async { Ok(ToolCatalog::default()) })
    }

    fn execute<'a>(
        &'a self,
        request: ToolExecutionRequest,
        _cancellation: CancellationToken<'a>,
    ) -> BoxFuture<'a, Result<Vec<ToolResult<Vec<u8>>>, Error>> {
        Box::pin(async move {
            let ToolExecutionRequest {
                calls,
                receipt: _receipt,
                flags: _flags,
                ..
            } = request;
            Ok(calls
                .into_iter()
                .map(|call| ToolResult {
                    call_id: call.id,
                    tool_name: call.name,
                    text: call.arguments_json,
                    ..ToolResult::default()
                })
                .collect())
        })
    }
}

#[test]
fn independent_tool_implementations_share_the_same_state_and_fresh_result_protocol() {
    let response = Reply::with_tool_calls(vec![ToolCall {
        id: "same-call".into(),
        name: "read".into(),
        arguments_json: " { \"same\" : true } ".into(),
    }]);
    let mut state = MemoryState::new([
        response.clone(),
        Reply::final_response("first answer"),
        response,
        Reply::final_response("second answer"),
    ]);
    assert_eq!(state.run(), Ok(Outcome::Completed("first answer".into())));
    state.enqueue_message(Delivery::Queue, "next question");
    let provider = state.provider.clone();
    assert_eq!(
        state.run_with(&EchoTools, &provider),
        Ok(Outcome::Completed("second answer".into()))
    );
    assert_eq!(state.recorded_tools.len(), 2);
    assert_eq!(state.applied_results.len(), 2);
    assert_eq!(state.applied_results[0].text, "result for same-call");
    assert_eq!(state.applied_results[1].text, " { \"same\" : true } ");
}
