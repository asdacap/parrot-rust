use super::*;

#[test]
fn state_recovers_and_records_tools_before_readiness_or_cancellation() {
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
        state.pending_tools.push_back(ToolExecutionRequest {
            assistant_sequence: 9,
            calls: vec![ToolCall {
                id: "unresolved".into(),
                name: "read".into(),
                arguments_json: "{}".into(),
            }],
            ..ToolExecutionRequest::default()
        });
        assert_eq!(
            state.run(),
            if cancelled {
                Err(Error::cancelled())
            } else {
                Ok(Outcome::Completed(String::new()))
            }
        );
        assert_eq!(
            state
                .applied_results
                .iter()
                .map(|result| result.call_id.as_str())
                .collect::<Vec<_>>(),
            ["persisted", "unresolved"]
        );
        assert_eq!(*state.tools.phases.lock().unwrap(), [ToolPhase::Recovery]);
        assert_eq!(
            state.trace.values(),
            ["reconcile", "execute", "record_results", "reconcile"]
        );
        assert!(state.provider.requests.lock().unwrap().is_empty());
    }
}

#[test]
fn state_records_each_execution_prefix_before_preparing_the_next_chunk() {
    let mut state = MemoryState::new([]);
    state.work = false;
    state.turns = 0;
    state.tools.output_count = 1;
    state.pending_tools.push_back(ToolExecutionRequest {
        assistant_sequence: 37,
        starts_batch: true,
        calls: ["first", "second", "third"]
            .into_iter()
            .map(|id| ToolCall {
                id: id.into(),
                name: "read".into(),
                arguments_json: format!("{{\"call\":\"{id}\"}}"),
            })
            .collect(),
        prior_image_bytes: vec![8, 13],
        image_budget_exceeded: true,
        cancel_remaining: true,
    });
    assert_eq!(state.run(), Ok(Outcome::Completed(String::new())));
    assert_eq!(
        state.trace.values(),
        [
            "reconcile",
            "execute",
            "record_results",
            "reconcile",
            "execute",
            "record_results",
            "reconcile",
            "execute",
            "record_results",
            "reconcile",
        ]
    );
    let requests = state.tools.requests.lock().unwrap();
    assert_eq!(
        requests
            .iter()
            .map(|request| request.calls.len())
            .collect::<Vec<_>>(),
        [3, 2, 1]
    );
    assert_eq!(
        requests
            .iter()
            .map(|request| request.starts_batch)
            .collect::<Vec<_>>(),
        [true, false, false]
    );
    assert!(requests.iter().all(|request| {
        request.assistant_sequence == 37
            && request.prior_image_bytes == [8, 13]
            && request.image_budget_exceeded
            && request.cancel_remaining
    }));
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
fn failed_result_recording_stops_before_another_execution_chunk() {
    let mut state = MemoryState::new([]);
    state.work = false;
    state.turns = 0;
    state.tools.output_count = 1;
    state.fail_at = Some("record_results");
    state.pending_tools.push_back(ToolExecutionRequest {
        calls: ["first", "second"]
            .into_iter()
            .map(|id| ToolCall {
                id: id.into(),
                name: "read".into(),
                arguments_json: "{}".into(),
            })
            .collect(),
        ..ToolExecutionRequest::default()
    });
    assert_eq!(state.run(), Err(Error::failed("record_results")));
    assert_eq!(state.tools.requests.lock().unwrap().len(), 1);
    assert_eq!(state.pending_tools.front().unwrap().calls.len(), 2);
    assert!(state.applied_results.is_empty());
    assert_eq!(state.trace.values().last().unwrap(), "record_results");
}

#[test]
fn empty_execution_output_errors_without_recording_or_repeating_work() {
    let mut state = MemoryState::new([]);
    state.work = false;
    state.turns = 0;
    state.tools.output_count = 0;
    state.pending_tools.push_back(ToolExecutionRequest {
        calls: vec![ToolCall {
            id: "pending".into(),
            name: "read".into(),
            arguments_json: "{}".into(),
        }],
        ..ToolExecutionRequest::default()
    });
    assert_eq!(
        state.run(),
        Err(Error::failed("tool execution returned no results"))
    );
    assert_eq!(state.tools.requests.lock().unwrap().len(), 1);
    assert_eq!(state.trace.values(), ["reconcile", "execute"]);
    assert!(state.applied_results.is_empty());
}

#[test]
fn complete_tool_results_reach_state_without_rewriting_metadata() {
    let mut state = MemoryState::new([]);
    state.work = false;
    state.turns = 0;
    state.pending_tools.push_back(ToolExecutionRequest {
        calls: vec![ToolCall {
            id: "image-process".into(),
            name: "read".into(),
            arguments_json: "{}".into(),
        }],
        ..ToolExecutionRequest::default()
    });
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
        yielded_process: Some(ToolProcess {
            process_id: "process".into(),
            name: "name".into(),
            inventory_instance_id: "inventory".into(),
            visible_revision: u64::MAX,
            stdout_path: Some("stdout".into()),
            stderr_path: None,
            activity_kind: -37,
        }),
    };
    state
        .tools
        .replies
        .lock()
        .unwrap()
        .push_back(vec![result.clone()]);
    assert_eq!(state.run(), Ok(Outcome::Completed(String::new())));
    assert_eq!(state.applied_results, [result]);
}

struct EchoTools;

impl Tools for EchoTools {
    fn capture_catalog(&self) -> BoxFuture<'_, Result<ToolCatalog, Error>> {
        Box::pin(async { Ok(ToolCatalog::default()) })
    }

    fn execute(
        &self,
        request: ToolExecutionRequest,
        context: ToolContext,
    ) -> BoxFuture<'_, Result<Vec<ToolResult>, Error>> {
        Box::pin(async move {
            assert_eq!(context.phase, ToolPhase::Recovery);
            Ok(request
                .calls
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
fn independent_tool_implementations_share_the_same_state_and_result_protocol() {
    let mut state = MemoryState::new([]);
    state.work = false;
    state.turns = 0;
    let request = ToolExecutionRequest {
        calls: vec![ToolCall {
            id: "same-call".into(),
            name: "read".into(),
            arguments_json: " { \"same\" : true } ".into(),
        }],
        ..ToolExecutionRequest::default()
    };
    state.pending_tools.push_back(request.clone());
    assert_eq!(state.run(), Ok(Outcome::Completed(String::new())));
    state.pending_tools.push_back(request);
    let provider = state.provider.clone();
    assert_eq!(
        finish(run(&mut state, &EchoTools, &provider)),
        Ok(Outcome::Completed(String::new()))
    );
    assert_eq!(state.applied_results.len(), 2);
    assert_eq!(state.applied_results[0].text, "result for same-call");
    assert_eq!(state.applied_results[1].text, " { \"same\" : true } ");
}
