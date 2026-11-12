use parrot_core::{
    CompactionConfiguration, CompactionGroup, CompactionInput, CompactionPlan, Content,
    ContextConfiguration, ContextLimit, ContextModel, Error, ErrorCode, ErrorDetail, ErrorKind,
    Message, PlanningError, Role, ToolCall,
    compaction::{CompactionPolicy, group_messages, is_complete},
};

fn message(tokens: i64) -> Message {
    Message {
        role: Role::User,
        contents: vec![Content::Text(tokens.to_string())],
        ..Message::default()
    }
}

fn input(costs: &[i64]) -> CompactionInput {
    let messages: Vec<_> = costs.iter().copied().map(message).collect();
    CompactionInput {
        groups: group_messages(&messages),
        base_watermark: 0,
        instructions: "sampled instructions".into(),
        messages,
        fixed_message: message(0),
    }
}

fn configuration() -> CompactionConfiguration {
    CompactionConfiguration {
        context: ContextConfiguration {
            model: ContextModel {
                canonical_model: "provider/model".into(),
                context_window: 1000,
                maximum_input_tokens: 1000,
                maximum_output_tokens: 1,
            },
            trigger_percent: 90,
            context_limit: None,
        },
        target: None,
        target_percent: 30,
        maximum_input_tokens: 600,
        summary_output_tokens: 20,
        profile_maximum_input_tokens: None,
        profile_summary_output_tokens: None,
        model: "model".into(),
        reasoning: None,
        summary_instructions: "summarize".into(),
        summary_prefix: "summary: ".into(),
    }
}

fn planning_error(failure: PlanningError) -> Error {
    Error {
        kind: ErrorKind::Failed,
        code: if failure == PlanningError::TokenOverflow {
            ErrorCode::Unexpected
        } else {
            ErrorCode::InvalidOperation
        },
        message: failure.to_string(),
        response_body: String::new(),
        detail: Some(ErrorDetail::Planning(failure)),
    }
}

fn select(input: &CompactionInput, target: Option<i64>) -> Result<Option<CompactionPlan>, Error> {
    CompactionPlan::select_for_target(input, &[], &configuration(), target)
}

#[test]
fn raw_targets_preserve_override_unknown_window_and_output_reservation_rules() {
    for (limit, target, window, capacity, reserved, expected_trigger, expected_target) in [
        (None, None, 1000, 800, None, Some(720), Some(240)),
        (
            Some(ContextLimit::Percentage(20)),
            None,
            1000,
            800,
            None,
            Some(200),
            Some(60),
        ),
        (
            Some(ContextLimit::Tokens(100_000)),
            None,
            1000,
            800,
            None,
            Some(800),
            Some(240),
        ),
        (
            Some(ContextLimit::Tokens(100)),
            Some(ContextLimit::Percentage(20)),
            1000,
            800,
            None,
            Some(100),
            Some(200),
        ),
        (
            Some(ContextLimit::Percentage(20)),
            None,
            0,
            800,
            None,
            None,
            None,
        ),
        (
            None,
            Some(ContextLimit::Percentage(20)),
            0,
            800,
            None,
            Some(720),
            None,
        ),
        (
            Some(ContextLimit::Tokens(800)),
            None,
            1000,
            800,
            Some(500),
            Some(500),
            Some(240),
        ),
        (
            Some(ContextLimit::Tokens(800)),
            Some(ContextLimit::Tokens(700)),
            1000,
            800,
            Some(500),
            Some(500),
            Some(700),
        ),
        (
            Some(ContextLimit::Tokens(i64::MAX)),
            None,
            0,
            0,
            None,
            Some(i64::MAX),
            Some(2_767_011_611_056_432_742),
        ),
    ] {
        let policy = CompactionPolicy::resolve(
            limit.as_ref(),
            target.as_ref(),
            window,
            capacity,
            90,
            30,
            reserved,
        );
        assert_eq!(policy.trigger_tokens, expected_trigger);
        assert_eq!(policy.target_tokens, expected_target);
    }
}

#[test]
fn selection_uses_core_configuration_without_a_native_planning_hook() {
    let input = input(&[10, 10, 50, 50]);
    let mut configuration = configuration();
    configuration.target = Some(ContextLimit::Tokens(100));
    let plan = CompactionPlan::select(&input, &[], &configuration)
        .unwrap()
        .unwrap();
    assert_eq!(plan.keep_group_from, 3);
    configuration.target = Some(ContextLimit::Percentage(12));
    let plan = CompactionPlan::select(&input, &[], &configuration)
        .unwrap()
        .unwrap();
    assert_eq!(plan.keep_group_from, 3);
    assert_eq!(plan.watermark, 3);
}

#[test]
fn selection_retains_the_normal_suffix_for_each_target() {
    let input = input(&[10, 10, 50, 50]);
    for (target, expected_keep) in [(40, 3), (100, 3), (111, 3)] {
        assert_eq!(
            select(&input, Some(target))
                .unwrap()
                .unwrap()
                .keep_group_from,
            expected_keep
        );
    }
}

#[test]
fn incomplete_prefix_and_base_watermark_prevent_unsafe_or_nonadvancing_plans() {
    let mut input = input(&[1, 1, 1, 1]);
    input.groups[1].is_complete = false;
    let plan = select(&input, Some(100)).unwrap().unwrap();
    assert_eq!(plan.keep_group_from, 1);
    assert_eq!(plan.watermark, 1);
    input.base_watermark = 1;
    assert!(select(&input, Some(100)).unwrap().is_none());
    input.base_watermark = 0;
    input.groups[0].is_complete = false;
    assert!(select(&input, Some(100)).unwrap().is_none());
}

#[test]
fn unavailable_targets_and_single_groups_do_not_validate_summary_capacity() {
    let mut configuration = configuration();
    configuration.maximum_input_tokens = 0;
    for (input, target) in [(input(&[1, 1]), None), (input(&[1]), Some(100))] {
        let result = CompactionPlan::select_for_target(&input, &[], &configuration, target);
        assert!(result.unwrap().is_none());
    }
    let mut input = input(&[1, 1]);
    input.base_watermark = 1;
    assert!(
        CompactionPlan::select_for_target(&input, &[], &configuration, Some(100))
            .unwrap()
            .is_none()
    );
    input.base_watermark = 0;
    assert_eq!(
        CompactionPlan::select_for_target(&input, &[], &configuration, Some(100)),
        Err(planning_error(PlanningError::NoCompactionCapacity))
    );
}

#[test]
fn selection_preserves_plan_boundaries() {
    let plan = select(&input(&[1, 2, 3]), Some(100)).unwrap().unwrap();
    assert_eq!(plan.keep_group_from, 2);
    assert_eq!(plan.watermark, 2);
}

#[test]
fn selection_uses_profile_capacity() {
    let mut configuration = configuration();
    configuration.profile_maximum_input_tokens = Some(10);
    let plan = CompactionPlan::select_for_target(&input(&[1, 2]), &[], &configuration, Some(100))
        .unwrap()
        .unwrap();
    assert_eq!(plan.input_budget, 10);
}

#[test]
fn grouping_preserves_contiguous_tool_results_and_exact_call_identity() {
    let mut assistant = message(0);
    assistant.role = Role::Assistant;
    assistant.tool_calls = ["a", "b"]
        .into_iter()
        .map(|id| ToolCall {
            id: id.into(),
            name: "tool".into(),
            arguments_json: "{}".into(),
        })
        .collect();
    let mut second = message(2);
    second.role = Role::Tool;
    second.tool_call_id = "b".into();
    let mut first = message(1);
    first.role = Role::Tool;
    first.tool_call_id = "a".into();
    let messages = [assistant, second, first, message(3)];
    let groups = group_messages(&messages);
    assert_eq!(
        groups
            .iter()
            .map(|group| group.messages.len())
            .collect::<Vec<_>>(),
        [3, 1]
    );
    assert_eq!(
        groups
            .iter()
            .map(|group| group.end_watermark)
            .collect::<Vec<_>>(),
        [1, 2]
    );
    assert!(is_complete(&groups[0].messages));
    assert!(!is_complete(&messages[..2]));
    assert!(
        CompactionGroup {
            messages: groups[0].messages.clone(),
            is_complete: groups[0].is_complete
        }
        .can_spill()
    );
}
