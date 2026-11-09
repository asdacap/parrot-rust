use parrot_core::{
    Content, ContextConfiguration, ContextLimit, ContextModel, Message, Role, ToolCall,
    compaction::{
        CompactionConfiguration, CompactionGroup, CompactionInput, CompactionPlan,
        CompactionPolicy, PlanningError, group_messages, is_complete,
    },
};
use std::cell::RefCell;

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

fn estimate(messages: &[Message]) -> Result<i64, PlanningError> {
    messages
        .iter()
        .flat_map(|message| &message.contents)
        .try_fold(0_i64, |total, content| {
            let Content::Text(text) = content else {
                panic!("these planning inputs contain only text")
            };
            total
                .checked_add(text.parse::<i64>().unwrap())
                .ok_or(PlanningError::TokenOverflow)
        })
}

fn select(
    input: &CompactionInput,
    target: Option<i64>,
) -> Result<Option<CompactionPlan>, PlanningError> {
    CompactionPlan::select_for_target(
        input,
        &[],
        &configuration(),
        target,
        estimate,
        |_, _, messages| estimate(messages),
    )
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
    let mut input = input(&[10, 10, 50, 50]);
    input.groups[1].has_checkpoint_before = true;
    let mut configuration = configuration();
    configuration.target = Some(ContextLimit::Tokens(100));
    let plan = CompactionPlan::select(&input, &[], &configuration, estimate, |_, _, messages| {
        estimate(messages)
    })
    .unwrap()
    .unwrap();
    assert_eq!(plan.keep_group_from, 3);
    configuration.target = Some(ContextLimit::Percentage(12));
    let plan = CompactionPlan::select(&input, &[], &configuration, estimate, |_, _, messages| {
        estimate(messages)
    })
    .unwrap()
    .unwrap();
    assert_eq!(plan.keep_group_from, 1);
    assert_eq!(plan.watermark, 1);
}

#[test]
fn checkpoint_fallback_only_uses_input_capacity_when_natural_suffix_exceeds_target() {
    let mut input = input(&[10, 10, 50, 50]);
    input.groups[1].has_checkpoint_before = true;
    for (target, expected_keep) in [(40, 1), (100, 3), (111, 1)] {
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
fn unavailable_targets_and_single_groups_do_not_estimate_or_validate_summary_capacity() {
    let mut configuration = configuration();
    configuration.maximum_input_tokens = 0;
    for (input, target) in [(input(&[1, 1]), None), (input(&[1]), Some(100))] {
        let result = CompactionPlan::select_for_target::<PlanningError>(
            &input,
            &[],
            &configuration,
            target,
            |_| panic!("an unavailable cut must not estimate groups"),
            |_, _, _| panic!("an unavailable cut must not estimate context"),
        );
        assert!(result.unwrap().is_none());
    }
    let mut input = input(&[1, 1]);
    input.base_watermark = 1;
    assert!(
        CompactionPlan::select_for_target(
            &input,
            &[],
            &configuration,
            Some(100),
            estimate,
            |_, _, messages| estimate(messages)
        )
        .unwrap()
        .is_none()
    );
    input.base_watermark = 0;
    assert_eq!(
        CompactionPlan::select_for_target(
            &input,
            &[],
            &configuration,
            Some(100),
            estimate,
            |_, _, messages| estimate(messages)
        ),
        Err(PlanningError::NoCompactionCapacity)
    );
}

#[test]
fn selection_preserves_native_estimate_order_and_checked_suffix_overflow() {
    let conversation = input(&[1, 2, 3]);
    let calls = RefCell::new(Vec::new());
    CompactionPlan::select_for_target(
        &conversation,
        &[],
        &configuration(),
        Some(100),
        |messages| {
            calls.borrow_mut().push(estimate(messages)?);
            estimate(messages)
        },
        |_, _, messages| {
            calls.borrow_mut().push(0);
            estimate(messages)
        },
    )
    .unwrap();
    assert_eq!(*calls.borrow(), [0, 3, 2, 1]);
    assert_eq!(
        select(&input(&[1, i64::MAX]), Some(100)),
        Err(PlanningError::TokenOverflow)
    );
}

#[test]
fn pure_spill_selection_uses_profile_capacity_and_requires_complete_tool_groups() {
    let call = ToolCall {
        id: "call".into(),
        name: "tool".into(),
        arguments_json: "{}".into(),
    };
    let mut assistant = message(0);
    assistant.role = Role::Assistant;
    assistant.tool_calls = vec![call];
    let mut tool = message(30);
    tool.role = Role::Tool;
    tool.tool_call_id = "call".into();
    let messages = vec![assistant, tool, message(1)];
    let input = CompactionInput {
        groups: group_messages(&messages),
        messages,
        ..input(&[])
    };
    let mut configuration = configuration();
    configuration.profile_maximum_input_tokens = Some(10);
    let mut plan = CompactionPlan::select_for_target(
        &input,
        &[],
        &configuration,
        Some(100),
        estimate,
        |_, _, messages| estimate(messages),
    )
    .unwrap()
    .unwrap();
    assert_eq!(
        plan.select_spill_groups(|_, _, messages| estimate(messages))
            .unwrap(),
        [0]
    );
    assert_eq!(plan.groups[0].messages, input.groups[0].messages);
    plan.groups[0].is_complete = false;
    assert_eq!(
        plan.select_spill_groups(|_, _, messages| estimate(messages)),
        Err(PlanningError::GroupExceedsBudget)
    );
    plan.groups[0].is_complete = true;
    plan.groups[0].messages[0].role = Role::User;
    assert_eq!(
        plan.select_spill_groups(|_, _, messages| estimate(messages)),
        Err(PlanningError::GroupExceedsBudget)
    );
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

#[derive(Debug, Eq, PartialEq)]
enum EstimateFailure {
    Planning(PlanningError),
    Native(&'static str),
}

impl From<PlanningError> for EstimateFailure {
    fn from(error: PlanningError) -> Self {
        Self::Planning(error)
    }
}

#[test]
fn native_estimation_failure_is_not_replaced_by_later_capacity_validation() {
    let mut configuration = configuration();
    configuration.maximum_input_tokens = 0;
    let error = CompactionPlan::select_for_target(
        &input(&[1, 1]),
        &[],
        &configuration,
        Some(100),
        |_| Err(EstimateFailure::Native("exact native failure")),
        |_, _, _| Ok(0),
    )
    .unwrap_err();
    assert_eq!(error, EstimateFailure::Native("exact native failure"));
}
