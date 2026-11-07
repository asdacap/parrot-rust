use super::*;

#[test]
fn state_restores_results_and_closes_incomplete_calls_without_execution_before_readiness_or_cancellation()
 {
    for cancelled in [false, true] {
        let mut state = MemoryState::new([]);
        state.work = false;
        state.turns = 0;
        state.cancelled = cancelled;
        state.restored_results.push(ToolResult {
            call_id: "persisted".into(),
            tool_name: "read".into(),
            text: "persisted result".into(),
            ..ToolResult::default()
        });
        state.historical_calls.push(ToolCall {
            id: "unresolved".into(),
            name: "read".into(),
            arguments_json: "{}".into(),
        });
        let expected = if cancelled {
            Err(Error::cancelled())
        } else {
            Ok(Outcome::Completed(String::new()))
        };
        assert_eq!(state.run(), expected);
        assert_eq!(
            state
                .applied_results
                .iter()
                .map(|result| result.call_id.as_str())
                .collect::<Vec<_>>(),
            ["persisted", "unresolved"]
        );
        assert_eq!(state.applied_results[0].text, "persisted result");
        assert_eq!(state.applied_results[0].status, ToolStatus::Finished);
        assert_eq!(state.applied_results[1].status, ToolStatus::Error);
        assert_eq!(
            state.applied_results[1].text,
            "The process crashed before the tool completed."
        );
        assert_eq!(
            state.applied_results[1].error_message,
            "The process crashed before the tool completed."
        );
        assert!(state.tools.requests.lock().unwrap().is_empty());
        assert_eq!(state.trace.values(), ["reconcile"]);
        assert!(state.provider.requests.lock().unwrap().is_empty());
        assert_eq!(state.run(), expected);
        assert_eq!(state.applied_results.len(), 2);
        assert!(state.tools.requests.lock().unwrap().is_empty());
    }
}

#[test]
fn core_executes_the_complete_provider_batch_once_and_records_all_results_once() {
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
        ["record_tools", "execute", "record_results"]
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

struct EchoTools;

impl Tools for EchoTools {
    fn capture_catalog(&self) -> BoxFuture<'_, Result<ToolCatalog, Error>> {
        Box::pin(async { Ok(ToolCatalog::default()) })
    }

    fn execute(&self, calls: Vec<ToolCall>) -> BoxFuture<'_, Result<Vec<ToolResult>, Error>> {
        Box::pin(async move {
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
    state.work = true;
    state.turns = 1;
    let provider = state.provider.clone();
    assert_eq!(
        finish(run(&mut state, &EchoTools, &provider)),
        Ok(Outcome::Completed("second answer".into()))
    );
    assert_eq!(state.recorded_tools.len(), 2);
    assert_eq!(state.applied_results.len(), 2);
    assert_eq!(state.applied_results[0].text, "result for same-call");
    assert_eq!(state.applied_results[1].text, " { \"same\" : true } ");
}
