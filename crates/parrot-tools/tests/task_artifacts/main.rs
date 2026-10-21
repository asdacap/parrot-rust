mod loader;

use parrot_tools::task_artifacts::{
    AcceptanceVerdict, ExecutionStatus, MAX_DESCRIPTION_LENGTH, TaskPayload, parse_artifact,
    parse_leaf_response, parse_task_set, validate_merged_graph,
};

fn task(name: &str, dependencies: &[&str]) -> String {
    let deps = if dependencies.is_empty() {
        String::new()
    } else {
        format!(
            ",\"dependencies\":[{}]",
            dependencies
                .iter()
                .map(|value| format!("\"{value}\""))
                .collect::<Vec<_>>()
                .join(",")
        )
    };
    format!(
        "{{\"name\":\"{name}\",\"description\":\"d\",\"payload\":\"p\",\"acceptance_criteria\":\"a\"{deps}}}"
    )
}

#[test]
fn original_nested_and_top_level_sibling_artifact_preserves_order_and_defaults() {
    let artifact = parse_artifact(r#"{"schema_version":1,"tasks":[
      {"name":"first","description":"first description","payload":"do it","acceptance_criteria":"prove it"},
      {"name":"parent","dependencies":["first"],"description":"parent description","payload":[{"name":"child","description":"child description","payload":"child work","acceptance_criteria":"child proof"}],"acceptance_criteria":"parent proof","model":"high_llm"}
    ]}"#).unwrap();
    assert_eq!(artifact.schema_version, 1);
    assert_eq!(
        artifact
            .tasks
            .iter()
            .map(|task| task.name.as_str())
            .collect::<Vec<_>>(),
        ["first", "parent"]
    );
    assert_eq!(artifact.tasks[0].state, ExecutionStatus::Pending);
    assert!(!artifact.tasks[0].hidden);
    assert_eq!(artifact.tasks[1].dependencies, ["first"]);
    assert_eq!(artifact.tasks[1].model.as_deref(), Some("high_llm"));
    assert_eq!(artifact.display_name(), "2 top-level tasks");
    assert!(
        matches!(&artifact.tasks[1].payload, TaskPayload::Tasks(children) if children[0].name == "child")
    );
}

#[test]
fn original_artifact_defaults_optional_state_and_parses_all_states() {
    let values = ["pending", "running", "succeeded", "failed", "canceled"];
    let expected = [
        ExecutionStatus::Pending,
        ExecutionStatus::Running,
        ExecutionStatus::Succeeded,
        ExecutionStatus::Failed,
        ExecutionStatus::Canceled,
    ];
    for (state, expected) in values.into_iter().zip(expected) {
        let json = format!(
            r#"{{"schema_version":1,"tasks":[{{"name":"x","description":"d","payload":"p","acceptance_criteria":"a","state":"{state}","hidden":true,"result":"ok","failure":"reason"}}]}}"#
        );
        let parsed = parse_artifact(&json).unwrap();
        let task = &parsed.tasks[0];
        assert_eq!(task.state, expected);
        assert!(task.hidden);
        assert_eq!(task.result.as_deref(), Some("ok"));
        assert_eq!(task.failure.as_deref(), Some("reason"));
    }
}

#[test]
fn task_set_allows_dependencies_for_later_merged_graph_validation() {
    let incoming = parse_task_set(r#"[{"name":"child","dependencies":["later"],"description":"d","payload":"p","acceptance_criteria":"a"}]"#).unwrap();
    assert!(
        validate_merged_graph(&incoming, "tasks")
            .unwrap_err()
            .to_string()
            .contains("missing sibling dependency 'later'")
    );
    let mut merged = incoming;
    merged.extend(parse_task_set(&format!("[{}]", task("later", &[]))).unwrap());
    validate_merged_graph(&merged, "tasks").unwrap();
}

#[test]
fn dependency_validation_matches_sibling_duplicates_missing_self_and_cycles() {
    for (names, deps) in [
        (vec!["a", "a"], vec![]),
        (vec!["a"], vec!["missing"]),
        (vec!["a"], vec!["a"]),
        (vec!["a", "b"], vec!["b"]),
    ] {
        let siblings = names
            .iter()
            .enumerate()
            .map(|(index, name)| task(name, if index + 1 == names.len() { &deps } else { &[] }))
            .collect::<Vec<_>>()
            .join(",");
        let json = format!(
            r#"{{"schema_version":1,"tasks":[{{"name":"root","description":"d","payload":[{siblings}],"acceptance_criteria":"a"}}]}}"#
        );
        assert!(parse_artifact(&json).is_err(), "{json}");
    }
    let cycle = format!(
        r#"{{"schema_version":1,"tasks":[{},{}]}}"#,
        task("a", &["b"]),
        task("b", &["a"])
    );
    assert!(
        parse_artifact(&cycle)
            .unwrap_err()
            .to_string()
            .contains("dependency cycle")
    );
}

#[test]
fn precise_field_errors_and_source_utf16_description_limit() {
    let prefix = r#"{"schema_version":1,"tasks":[{"name":"x","description":"d","payload":[{"name":"y","description":"d","payload":"p","acceptance_criteria":"a","hidden":1}],"acceptance_criteria":"a"}]}"#;
    assert_eq!(
        parse_artifact(prefix).unwrap_err().to_string(),
        "tasks[0] payload[0] hidden must be a boolean."
    );
    let at_limit = "d".repeat(MAX_DESCRIPTION_LENGTH);
    let acceptable = format!(
        r#"{{"schema_version":1,"tasks":[{{"name":"x","description":"{at_limit}","payload":"p","acceptance_criteria":"a"}}]}}"#
    );
    assert_eq!(
        parse_artifact(&acceptable).unwrap().tasks[0]
            .description
            .encode_utf16()
            .count(),
        MAX_DESCRIPTION_LENGTH
    );
    let astral = "😀".repeat(MAX_DESCRIPTION_LENGTH / 2 + 1);
    let too_long = format!(
        r#"{{"schema_version":1,"tasks":[{{"name":"x","description":"{astral}","payload":"p","acceptance_criteria":"a"}}]}}"#
    );
    assert!(
        parse_artifact(&too_long)
            .unwrap_err()
            .to_string()
            .contains("description must be at most 300 characters")
    );
}

#[test]
fn source_envelope_schema_union_unknown_and_empty_rules_are_enforced() {
    for input in [
        r#"{"schema_version":2,"tasks":[]}"#,
        r#"{"schema_version":1,"tasks":null}"#,
        r#"{"schema_version":1,"tasks":[],"unknown":true}"#,
        r#"{"schema_version":1,"tasks":[{"name":"x","description":"d","payload":null,"acceptance_criteria":"a"}]}"#,
        r#"{"schema_version":1,"tasks":[]}"#,
        r#"{"schema_version":1,"tasks":[{"name":"x","description":"d","payload":"p","acceptance_criteria":"a","unexpected":0}]}"#,
    ] {
        assert!(parse_artifact(input).is_err(), "{input}");
    }
    assert_eq!(parse_artifact(r#"{"schema_version":1,"tasks":[{"name":"x","description":"d","payload":"p","acceptance_criteria":"a"}]}"#).unwrap().display_name(), "x");
}

#[test]
fn dependency_and_instruction_errors_keep_source_field_context() {
    let invalid_dependencies = r#"{"schema_version":1,"tasks":[{"name":"x","dependencies":null,"description":"d","payload":"p","acceptance_criteria":"a"}]}"#;
    assert_eq!(
        parse_artifact(invalid_dependencies)
            .unwrap_err()
            .to_string(),
        "tasks[0] dependencies must be an array."
    );
    let blank_dependency = r#"{"schema_version":1,"tasks":[{"name":"x","dependencies":[" "],"description":"d","payload":"p","acceptance_criteria":"a"}]}"#;
    assert_eq!(
        parse_artifact(blank_dependency).unwrap_err().to_string(),
        "tasks[0] dependency must be nonblank."
    );
    let blank_instruction = r#"{"schema_version":1,"tasks":[{"name":"x","description":"d","payload":"  ","acceptance_criteria":"a"}]}"#;
    assert_eq!(
        parse_artifact(blank_instruction).unwrap_err().to_string(),
        "tasks[0] payload must be nonblank."
    );
}

#[test]
fn duplicate_field_behavior_is_observed_at_objects_rejected_by_source_unknown_field_validator() {
    for json in [
        r#"{"schema_version":1,"schema_version":1,"tasks":[{"name":"x","description":"d","payload":"p","acceptance_criteria":"a"}]}"#,
        r#"{"schema_version":1,"tasks":[{"name":"x","name":"x","description":"d","payload":"p","acceptance_criteria":"a"}]}"#,
    ] {
        let error = parse_artifact(json).unwrap_err().to_string();
        assert!(error.contains("duplicate field"), "{error}");
    }
    // Duplicate dependency values are a separate source rejection.
    let json = r#"{"schema_version":1,"tasks":[{"name":"x","dependencies":["a","a"],"description":"d","payload":"p","acceptance_criteria":"a"},{"name":"a","description":"d","payload":"p","acceptance_criteria":"a"}]}"#;
    assert!(
        parse_artifact(json)
            .unwrap_err()
            .to_string()
            .contains("dependencies must be distinct")
    );
}

#[test]
fn source_leaf_response_verdict_shapes_and_unknowns() {
    assert_eq!(
        parse_leaf_response(r#"{"result":"work result","verdict":"accept"}"#)
            .unwrap()
            .verdict,
        AcceptanceVerdict::Accept
    );
    assert_eq!(
        parse_leaf_response(
            r#"{"result":"work result","verdict":"reject_and_halt","feedback":"no"}"#
        )
        .unwrap()
        .verdict,
        AcceptanceVerdict::RejectAndHalt {
            feedback: "no".into()
        }
    );
    assert_eq!(parse_leaf_response(r#"{"result":"work result","verdict":"reject_and_retry","feedback":"fix","payload":"again","replacement_result":"new context"}"#).unwrap().verdict, AcceptanceVerdict::RejectAndRetry { feedback: "fix".into(), payload: Some("again".into()), replacement_result: Some("new context".into()) });
    assert!(parse_leaf_response(r#"{"result":"x","verdict":"accept","unknown":true}"#).is_err());
}

#[test]
fn latest_syntactically_valid_fence_is_selected_before_typed_validation() {
    let input = "```json\n{\"result\":\"earlier\",\"verdict\":\"accept\"}\n```\n```json\n{\"result\":\"latest\",\"verdict\":\"reject_and_retry\",\"feedback\":1e400}\n```";
    let error = parse_leaf_response(input).unwrap_err().to_string();
    assert_eq!(error, "leaf response feedback must be a string.");
}

#[test]
fn overflowing_numeric_token_does_not_make_latest_json_fence_invalid() {
    let input = "```json\n{\"result\":\"earlier\",\"verdict\":\"accept\"}\n```\n```json\n{\"result\":\"latest\",\"verdict\":\"reject_and_retry\",\"feedback\":1e400}\n```";
    assert_eq!(
        parse_leaf_response(input).unwrap_err().to_string(),
        "leaf response feedback must be a string."
    );
}

#[test]
fn merged_graph_rejects_duplicate_names_across_incoming_sets() {
    let first = parse_task_set(
        r#"[{"name":"same","description":"first","payload":"p","acceptance_criteria":"a"}]"#,
    )
    .unwrap();
    let second = parse_task_set(r#"[{"name":"same","dependencies":["other"],"description":"second","payload":"p","acceptance_criteria":"a"},{"name":"other","description":"other","payload":"p","acceptance_criteria":"a"}]"#).unwrap();
    let mut merged = first;
    merged.extend(second);
    assert_eq!(
        validate_merged_graph(&merged, "tasks")
            .unwrap_err()
            .to_string(),
        "tasks contains duplicate task name 'same'."
    );
}

fn deeply_nested_array(count: usize) -> String {
    format!("{}0{}", "[".repeat(count), "]".repeat(count))
}

#[test]
fn source_json_depth_boundary_is_exactly_64_for_artifacts_and_task_sets() {
    let artifact = |nesting| {
        format!(
            "{{\"schema_version\":1,\"tasks\":[{{\"name\":\"x\",\"description\":\"d\",\"payload\":\"p\",\"acceptance_criteria\":\"a\",\"unknown\":{}}}]}}",
            deeply_nested_array(nesting)
        )
    };
    let error64 = parse_artifact(&artifact(61)).unwrap_err().to_string();
    assert!(error64.contains("unknown field 'unknown'"), "{error64}");
    let error65 = parse_artifact(&artifact(62)).unwrap_err().to_string();
    assert_eq!(
        error65,
        "The response does not end with a JSON object or array envelope."
    );

    let task_set = |nesting| {
        format!(
            "[{{\"name\":\"x\",\"description\":\"d\",\"payload\":\"p\",\"acceptance_criteria\":\"a\",\"unknown\":{}}}]",
            deeply_nested_array(nesting)
        )
    };
    let error64 = parse_task_set(&task_set(62)).unwrap_err().to_string();
    assert!(error64.contains("unknown field 'unknown'"), "{error64}");
    let error65 = parse_task_set(&task_set(63)).unwrap_err().to_string();
    assert!(error65.contains("Invalid JSON"), "{error65}");
}

#[test]
fn source_envelope_selection_uses_last_json_fence_before_trailing_json() {
    let source_case = "Sure.\n```json\n{\"schema_version\":1,\"tasks\":[{\"name\":\"single\",\"description\":\"d\",\"payload\":\"p\",\"acceptance_criteria\":\"a\"}]}\n```\nDone.";
    assert_eq!(
        parse_artifact(source_case).unwrap().display_name(),
        "single"
    );
    assert!(parse_leaf_response("no json here at all").is_err());
    assert!(
        parse_leaf_response("{\"result\":\"x\",\"verdict\":\"accept\"}\ntrailing prose").is_err()
    );
}
