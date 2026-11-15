use super::*;

#[test]
fn compact_tools_trigger_one_compaction_after_the_entire_batch_is_recorded() {
    let calls: Vec<_> = ["compact-first", "ordinary", "compact-last"]
        .into_iter()
        .map(|id| ToolCall {
            id: id.into(),
            name: if id == "ordinary" {
                "read"
            } else {
                "compact_context"
            }
            .into(),
            arguments_json: "{}".into(),
        })
        .collect();
    let mut state = MemoryState::new([
        Reply::with_tool_calls(calls),
        Reply::final_response("answer"),
    ]);
    state.compact_with("tool-triggered summary");

    assert_eq!(state.run(), Ok(Outcome::Completed("answer".into())));
    assert_eq!(state.applied_results.len(), 3);
    assert_eq!(state.compaction.lock().unwrap().results.len(), 1);
    let trace = state.trace.values();
    assert!(
        trace
            .iter()
            .rposition(|step| step == "record_results")
            .unwrap()
            < trace.iter().position(|step| step == "compact").unwrap()
    );
}

#[test]
fn core_reads_compact_tool_targets_from_the_provider_call() {
    for (arguments, target) in [
        (
            r#"{"target_context_size":"1"}"#,
            parrot_core::ContextLimit::Tokens(1),
        ),
        (
            r#"{"target_context_size":"1%"}"#,
            parrot_core::ContextLimit::Percentage(1),
        ),
        (
            r#"{"target_context_size":"10k"}"#,
            parrot_core::ContextLimit::Tokens(10000),
        ),
    ] {
        let mut state = MemoryState::new([
            Reply::with_tool_calls(vec![ToolCall {
                id: "compact".into(),
                name: "compact_context".into(),
                arguments_json: arguments.into(),
            }]),
            Reply::final_response("answer"),
        ]);
        state.compact_with("targeted summary");
        assert_eq!(state.run(), Ok(Outcome::Completed("answer".into())));
        let compaction = state.compaction.lock().unwrap();
        assert_eq!(compaction.requests.len(), 1);
        assert_eq!(compaction.requests[0].target, Some(target));
    }
}

#[test]
fn unsuccessful_compact_tools_do_not_trigger_compaction() {
    for status in [
        ToolStatus::Cancelled,
        ToolStatus::Error,
        ToolStatus::ImageBudgetExceeded,
    ] {
        let mut state = MemoryState::new([
            Reply::with_tool_calls(vec![ToolCall {
                id: "call-1".into(),
                name: "compact_context".into(),
                arguments_json: "{}".into(),
            }]),
            Reply::final_response("answer"),
        ]);
        state
            .tools
            .replies
            .lock()
            .unwrap()
            .push_back(vec![ToolResult {
                call_id: "call-1".into(),
                tool_name: "compact_context".into(),
                status,
                ..ToolResult::default()
            }]);

        assert_eq!(state.run(), Ok(Outcome::Completed("answer".into())));
        assert!(state.compaction.lock().unwrap().requests.is_empty());
    }
}

#[test]
fn invalid_compact_arguments_do_not_trigger_compaction() {
    for arguments in [
        r#"{"target_context_size":"invalid"}"#,
        r#"{"target_context_size":12}"#,
        r#"{"target_context_size":""}"#,
        r#"{"unknown":"value"}"#,
        "null",
    ] {
        let mut state = MemoryState::new([
            Reply::with_tool_calls(vec![ToolCall {
                id: "compact".into(),
                name: "compact_context".into(),
                arguments_json: arguments.into(),
            }]),
            Reply::final_response("answer"),
        ]);

        assert_eq!(state.run(), Ok(Outcome::Completed("answer".into())));
        assert!(state.compaction.lock().unwrap().requests.is_empty());
    }
}

#[test]
fn ordinary_tool_results_do_not_trigger_compaction() {
    let mut state = MemoryState::new([
        Reply::with_tool_calls(vec![ToolCall {
            id: "compact_context".into(),
            name: "read".into(),
            arguments_json: "{}".into(),
        }]),
        Reply::final_response("answer"),
    ]);

    assert_eq!(state.run(), Ok(Outcome::Completed("answer".into())));
    assert!(state.compaction.lock().unwrap().requests.is_empty());
}
