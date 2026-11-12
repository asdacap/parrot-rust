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
    state.compaction_plan = Some(compaction_support::plan(&["original", "retained"]));
    state.summary_provider =
        compaction_support::Provider::new([parrot_core::SummaryResponse::Summary(
            "tool-triggered summary".into(),
        )]);

    assert_eq!(state.run(), Ok(Outcome::Completed("answer".into())));
    assert_eq!(state.applied_results.len(), 3);
    assert_eq!(state.summary_provider.requests.lock().unwrap().len(), 1);
    assert_eq!(state.recorded_compactions.len(), 1);
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
    for (arguments, reduced) in [
        (r#"{"target_context_size":"1"}"#, true),
        (r#"{"target_context_size":"1%"}"#, false),
        (r#"{"target_context_size":"10k"}"#, true),
    ] {
        let mut state = MemoryState::new([
            Reply::with_tool_calls(vec![ToolCall {
                id: "compact".into(),
                name: "compact_context".into(),
                arguments_json: arguments.into(),
            }]),
            Reply::final_response("answer"),
        ]);
        state.compaction_plan = Some(compaction_support::plan(&["original", "retained"]));
        state.summary_provider =
            compaction_support::Provider::new([parrot_core::SummaryResponse::Summary(
                "targeted summary".into(),
            )]);
        let mut compaction = MemoryCompaction::new(
            state.compaction_plan.clone(),
            state.summary_provider.clone(),
        );
        compaction.configuration.context.model.context_window = 0;

        assert_eq!(
            state.run_with_compaction(&mut compaction),
            Ok(Outcome::Completed("answer".into()))
        );
        assert_eq!(
            state.summary_provider.requests.lock().unwrap().len(),
            usize::from(reduced)
        );
        assert_eq!(state.recorded_compactions.len(), 1);
        assert_eq!(
            state.recorded_compactions[0].as_ref().unwrap().is_some(),
            reduced
        );
        assert!(compaction.configuration.target.is_none());
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
        assert!(state.recorded_compactions.is_empty());
        assert!(state.summary_provider.requests.lock().unwrap().is_empty());
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
        assert!(state.recorded_compactions.is_empty());
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
    assert!(state.recorded_compactions.is_empty());
}
