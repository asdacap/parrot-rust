mod cadence;
mod context;
mod execution;

use parrot_domain::{
    contracts::async_ops::{BoxFuture, Cancellation},
    values::config::{ModelProfileConfig, ModelProfiles},
};
use parrot_provider::{
    catalogs::Model,
    routing::{
        aliases::{ModelAliasCatalog, ModelAliasDefinition, ModelRouter, ModelRouting},
        registry::{ProviderModel, ProviderRegistry},
    },
    sessions::{
        Completion, Content, Error, EventStream, Message, Provider, Request, Session, Tool,
        ToolCall,
    },
};
use parrot_runtime::compaction::*;
use std::{
    collections::BTreeMap,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
};

struct ImageProvider {
    tokens: i64,
    calculations: AtomicUsize,
}
impl Provider for ImageProvider {
    fn id(&self) -> &str {
        "provider"
    }
    fn call(&self, _: Request) -> Result<Box<dyn EventStream>, Error> {
        panic!("planning must not call a provider")
    }
    fn has_credential<'a>(&'a self, _: &'a dyn Cancellation) -> BoxFuture<'a, Result<bool, Error>> {
        panic!("no credentials")
    }
    fn seed_models(&self) -> Vec<Model> {
        Vec::new()
    }
    fn list_models<'a>(
        &'a self,
        _: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<Vec<Model>, Error>> {
        panic!("no catalogue call")
    }
    fn calculate_image_tokens(&self, _: &Model, content: &Content) -> i64 {
        assert!(matches!(content, Content::Image { .. }));
        self.calculations.fetch_add(1, Ordering::Relaxed);
        self.tokens
    }
    fn open_session(&self) -> Result<Arc<dyn Session>, Error> {
        panic!("no session")
    }
}
fn model(context: i32) -> ProviderModel {
    let mut value = Model::new("model", "provider");
    value.context_window = context;
    value.max_output_tokens = 1;
    ProviderModel {
        provider: Arc::new(ImageProvider {
            tokens: 4096,
            calculations: AtomicUsize::new(0),
        }),
        model: value,
        variant: None,
    }
}
fn planner(trigger: i32, target: i32, input: i32) -> CompactionPlanner {
    CompactionPlanner::new(
        CompactionSettings {
            trigger_percent: trigger,
            target_percent: target,
            maximum_input_tokens: input,
            summary_output_tokens: 100,
            summary_instructions: "Summarize.".into(),
            summary_prefix: "Summary: ".into(),
        },
        ModelProfiles::new(&BTreeMap::new()),
    )
}
fn call(id: &str) -> ToolCall {
    ToolCall {
        id: id.into(),
        name: "read".into(),
        arguments_json: "{}".into(),
    }
}
fn completion(input_tokens: i32) -> Completion {
    Completion {
        input_tokens,
        output_tokens: 200,
        ..Completion::default()
    }
}

#[test]
fn original_calibration_rows_and_saturation() {
    for (previous, actual, current, expected) in [
        (1000, 1500, 1000, 1500),
        (1000, 1500, 1200, 1700),
        (1000, 1500, 200, 700),
        (1000, 500, 1200, 1200),
        (1000, 1500, i64::MAX, i64::MAX),
    ] {
        let mut budget = ProviderTokenBudget::default();
        budget.observe_usage("provider/model", previous, &completion(actual));
        assert_eq!(
            budget.estimate_input_tokens("provider/model", current),
            expected
        );
    }
}
#[test]
fn original_missing_usage_switch_and_reset() {
    for missing in [0, -1] {
        let mut budget = ProviderTokenBudget::default();
        budget.observe_usage("provider/model", 1000, &completion(missing));
        assert_eq!(budget.estimate_input_tokens("provider/model", 1200), 1200);
        budget.observe_usage("provider/model", 1000, &completion(1500));
        budget.observe_usage("provider/model", 1200, &completion(missing));
        assert_eq!(budget.estimate_input_tokens("provider/model", 1400), 1900);
    }
    let mut budget = ProviderTokenBudget::default();
    assert_eq!(budget.estimate_input_tokens("provider/model", -1), 0);
    budget.observe_usage("provider/model", 1000, &completion(1500));
    assert_eq!(budget.estimate_input_tokens("other/model", 1200), 1200);
    budget.observe_usage("other/model", 1200, &completion(2000));
    assert_eq!(budget.estimate_input_tokens("provider/model", 1400), 1400);
    assert_eq!(budget.estimate_input_tokens("other/model", 1400), 2200);
    budget.reset();
    assert_eq!(budget.estimate_input_tokens("other/model", 200), 200);
}
#[test]
fn original_output_budget_rows() {
    for (context, output, input, estimate, expected) in [
        (0, 0, 0, 1000, 32768),
        (0, 8192, 0, 1000, 8192),
        (0, 65536, 0, 1000, 32768),
        (200000, 0, 0, 1000, 32768),
        (2000, 0, 0, 1001, 948),
        (2000, 100, 0, 1001, 100),
        (2000, 0, 1001, 1001, 948),
    ] {
        let mut selected = model(context);
        selected.model.max_output_tokens = output;
        selected.model.max_input_tokens = input;
        assert_eq!(
            ProviderTokenBudget::default().calculate_maximum_output_tokens(
                "provider/model",
                estimate,
                &selected.model
            ),
            Ok(expected)
        );
    }
}
#[test]
fn original_actual_usage_five_percent_rows() {
    let mut budget = ProviderTokenBudget::default();
    budget.observe_usage("provider/model", 120000, &completion(147776));
    let mut selected = model(180000);
    selected.model.max_output_tokens = 0;
    for (estimate, expected_input, expected_output) in
        [(120000, 147776, 24835), (121000, 148776, 23785)]
    {
        assert_eq!(
            budget.estimate_input_tokens("provider/model", estimate),
            expected_input
        );
        let output = budget
            .calculate_maximum_output_tokens("provider/model", estimate, &selected.model)
            .unwrap();
        assert_eq!(output, expected_output);
        assert!(expected_input + i64::from(output) + (expected_input + 19) / 20 <= 180000);
    }
}
#[test]
fn compaction_boundaries_preserve_full_requested_output_and_request_headroom() {
    for (context, output) in [
        (180_000, 0),
        (180_000, 8_192),
        (180_000, 65_536),
        (1_000, 100),
        (22, 2),
        (23, 3),
        (i32::MAX, 0),
        (1_000, 1),
    ] {
        let mut selected = model(context);
        selected.model.max_output_tokens = output;
        let snapshot = planner(100, 30, 60000)
            .estimate_context(&selected, "", &[], &[])
            .unwrap();
        let trigger = snapshot.trigger_tokens.unwrap();
        let budget = ProviderTokenBudget::default();
        let requested = selected.model.requested_output_tokens();
        assert_eq!(
            budget.calculate_maximum_output_tokens("provider/model", trigger, &selected.model),
            Ok(requested)
        );
        let above =
            budget.calculate_maximum_output_tokens("provider/model", trigger + 1, &selected.model);
        if output == 1 {
            assert_eq!(above, Err(PlanningError::NoOutputCapacity));
        } else {
            assert!(above.unwrap() < requested);
        }
        assert!(!snapshot.exceeds_compaction_trigger(trigger));
        assert!(snapshot.exceeds_compaction_trigger(trigger + 1));
    }
}

#[test]
fn original_exhausted_capacity_rows_exact_messages() {
    for (context, input, actual, expected) in [
        (2000, 1400, 1500, PlanningError::InputLimitExceeded),
        (0, 1400, 1500, PlanningError::InputLimitExceeded),
        (1500, 0, 1500, PlanningError::NoOutputCapacity),
        (1575, 0, 1500, PlanningError::NoOutputCapacity),
        (1000, 0, 1500, PlanningError::NoOutputCapacity),
    ] {
        let mut selected = model(context);
        selected.model.max_input_tokens = input;
        let mut budget = ProviderTokenBudget::default();
        budget.observe_usage("provider/model", 1000, &completion(actual));
        assert_eq!(
            budget.calculate_maximum_output_tokens("provider/model", 1000, &selected.model),
            Err(expected)
        );
        let message = if expected == PlanningError::InputLimitExceeded {
            "The conversation exceeds the selected model input limit."
        } else {
            "The conversation leaves no output capacity in the selected model context window."
        };
        assert_eq!(expected.to_string(), message);
    }
}
#[test]
fn original_context_usage_rows_strict_boundary() {
    for (limit, instructions, estimate, usage, trigger) in [
        (100, "", 4, Some(4), false),
        (8, "", 4, Some(50), false),
        (8, "x", 5, Some(62), true),
        (3, "", 4, Some(100), true),
        (0, "", 4, None, false),
        (-10, "", 4, None, false),
    ] {
        let snapshot = planner(50, 30, 60000)
            .estimate_context(&model(limit), instructions, &[], &[])
            .unwrap();
        assert_eq!(snapshot.estimated_tokens, estimate);
        assert_eq!(snapshot.context_limit, limit);
        assert_eq!(snapshot.usage_percent, usage);
        assert_eq!(snapshot.trigger_percent, 50);
        assert_eq!(snapshot.exceeds_trigger(), trigger);
    }
}
#[test]
fn actual_messages_tools_utf16_and_input_limits() {
    let selected = model(1000);
    let history = [Message::user("x".repeat(2000))];
    let tools = [Tool {
        name: "tool".into(),
        description: "description".into(),
        parameters_json: "s".repeat(2000),
    }];
    let planner = planner(90, 30, 60000);
    assert!(
        !planner
            .estimate_context(&selected, "", &[], &history)
            .unwrap()
            .exceeds_trigger()
    );
    assert!(
        planner
            .estimate_context(&model(500), "", &[], &history)
            .unwrap()
            .exceeds_trigger()
    );
    assert!(
        planner
            .estimate_context(&selected, &"i".repeat(2000), &[], &history)
            .unwrap()
            .exceeds_trigger()
    );
    assert!(
        planner
            .estimate_context(&selected, "", &tools, &history)
            .unwrap()
            .exceeds_trigger()
    );
    assert_eq!(
        CompactionPlanner::estimate_input_tokens(&selected, "😀😀😀", &[], &[]).unwrap(),
        6
    );
    let mut narrow = selected;
    narrow.model.max_input_tokens = 100;
    let snapshot = planner
        .estimate_context(&narrow, "", &[], &history)
        .unwrap();
    assert_eq!(snapshot.context_limit, 1000);
    assert_eq!(snapshot.input_limit, 100);
    assert_eq!(snapshot.trigger_tokens, Some(90));
    assert_eq!(snapshot.usage_percent, Some(51));
}
#[test]
fn image_estimation_calls_real_provider_api_without_loading_missing_file() {
    let provider = Arc::new(ImageProvider {
        tokens: 123,
        calculations: AtomicUsize::new(0),
    });
    let selected = ProviderModel {
        provider: provider.clone(),
        model: Model::new("model", "provider"),
        variant: None,
    };
    let message = Message::new(
        parrot_provider::sessions::Role::User,
        vec![
            Content::Text("12345".into()),
            Content::image_file("does-not-exist", "image/png", 200, 100),
        ],
    );
    assert_eq!(
        CompactionPlanner::estimate_tokens(&selected, &[message]).unwrap(),
        2 + 123 + 8
    );
    assert_eq!(provider.calculations.load(Ordering::Relaxed), 1);
}
#[test]
fn actual_selected_policy_and_override_preserve_total_window() {
    let canonical = model(1000);
    let registry = Arc::new(
        ProviderRegistry::new(
            vec![canonical.provider.clone()],
            BTreeMap::from([("provider".into(), vec![canonical.model.clone()])]),
        )
        .unwrap(),
    );
    let mut alias = ModelAliasDefinition::new("small", "provider/model", "test alias");
    alias.context_limit = Some(ContextSize::parse("40%").unwrap());
    let catalog = Arc::new(ModelAliasCatalog::new(registry.clone(), [alias]).unwrap());
    let routing = Arc::new(ModelRouting::new(catalog, "provider/model"));
    let selected = ModelRouter::new(registry, routing)
        .resolve("small")
        .unwrap();
    let planner = planner(90, 30, 60000);
    let policy = planner.resolve_policy(&selected, None);
    assert_eq!(policy.trigger_tokens, Some(400));
    assert_eq!(policy.target_tokens, Some(120));
    assert_eq!(
        planner
            .resolve_policy(&selected, Some(&ContextSize::parse("10%").unwrap()))
            .target_tokens,
        Some(100)
    );
    let snapshot = planner
        .estimate_selected_context(&selected, "", &[], &[])
        .unwrap();
    assert_eq!(snapshot.context_limit, 1000);
    assert_eq!(snapshot.input_limit, 1000);
    assert_eq!(snapshot.trigger_percent, 40);
    assert!(snapshot.has_context_limit_override);
    let history = [Message::user("first"), Message::user("retained")];
    let groups = group_messages(&history);
    let fixed = Message::user("fixed");
    let input = PlanningInput {
        instructions: "",
        tools: &[],
        groups: &groups,
        base_watermark: 0,
        fixed_message: &fixed,
    };
    let plan = planner
        .plan_selected(&selected, None, &input)
        .unwrap()
        .unwrap();
    assert_eq!(plan.keep_group_from, 1);
    assert_eq!(plan.input_budget, 1000);
}
#[test]
fn selected_and_raw_planning_reserve_output_without_recomputing_targets() {
    for (limit, window, input, output, trigger, expected) in [
        (None, 180_000, 0, 0, 95, 140_220),
        (None, 180_000, 0, 65_536, 95, 140_220),
        (None, 180_000, 0, 8_192, 95, 163_626),
        (None, 180_000, 0, 0, 50, 90_000),
        (Some("100k"), 180_000, 0, 0, 95, 100_000),
        (Some("95%"), 180_000, 0, 0, 95, 140_220),
        (Some("180k"), 180_000, 0, 0, 95, 140_220),
        (None, 180_000, 100_000, 0, 95, 95_000),
        (Some("180k"), 180_000, 100_000, 0, 95, 100_000),
        (None, 0, 100_000, 0, 95, 95_000),
        (Some("100k"), 0, 100_000, 0, 95, 100_000),
        (None, 1_000, 0, 0, 95, 0),
        (None, i32::MAX, 0, 0, 100, 2_045_191_313),
    ] {
        let mut canonical = model(window);
        canonical.model.max_input_tokens = input;
        canonical.model.max_output_tokens = output;
        let registry = Arc::new(
            ProviderRegistry::new(
                vec![canonical.provider.clone()],
                BTreeMap::from([("provider".into(), vec![canonical.model.clone()])]),
            )
            .unwrap(),
        );
        let mut alias = ModelAliasDefinition::new("selected", "provider/model", "test alias");
        alias.context_limit = limit.map(|text| ContextSize::parse(text).unwrap());
        let catalog = Arc::new(ModelAliasCatalog::new(registry.clone(), [alias]).unwrap());
        let selected = ModelRouter::new(
            registry,
            Arc::new(ModelRouting::new(catalog, "provider/model")),
        )
        .resolve("selected")
        .unwrap();
        let planner = planner(trigger, 50, 60000);
        let snapshot = planner
            .estimate_selected_context(&selected, "", &[], &[])
            .unwrap();
        assert_eq!(snapshot.trigger_tokens, Some(expected));
        if limit.is_none() {
            let raw = planner.estimate_context(&canonical, "", &[], &[]).unwrap();
            assert_eq!(raw.trigger_tokens, Some(expected));
        }
        if window == 180_000 && input == 0 && output == 0 && trigger == 95 {
            let expected_target = match limit {
                Some("100k") => 50_000,
                Some("95%") => 85_500,
                _ => 90_000,
            };
            assert_eq!(
                planner.resolve_policy(&selected, None).target_tokens,
                Some(expected_target)
            );
            assert_eq!(
                planner
                    .resolve_policy(&selected, Some(&ContextSize::parse("160k").unwrap()))
                    .target_tokens,
                Some(160_000)
            );
            if limit.is_none() {
                let snapshot = planner
                    .estimate_context(&canonical, &"x".repeat((160_000 - 4) * 4), &[], &[])
                    .unwrap();
                assert_eq!(snapshot.estimated_tokens, 160_000);
                assert!(snapshot.exceeds_trigger());
            }
        }
    }
    let unknown = planner(95, 50, 60000)
        .estimate_context(&model(0), "", &[], &[])
        .unwrap();
    assert_eq!(unknown.trigger_tokens, None);
}

#[test]
fn groups_only_capture_contiguous_tool_results_and_keep_order() {
    let history = [
        Message::assistant("", vec![call("a"), call("b")]),
        Message::tool_result("b", "second"),
        Message::tool_result("a", "first"),
        Message::user("next"),
        Message::tool_result("orphan", "later"),
    ];
    let groups = group_messages(&history);
    assert_eq!(
        groups.iter().map(|g| g.messages.len()).collect::<Vec<_>>(),
        vec![3, 1, 1]
    );
    assert!(groups[0].is_complete);
    assert!(is_eligible_for_spill(&groups[0]));
    assert_eq!(groups[0].messages[1].tool_call_id, "b");
    assert_eq!(groups[2].end_watermark, 3);
    let incomplete = group_messages(&history[..2]);
    assert!(!incomplete[0].is_complete);
    assert!(!is_eligible_for_spill(&incomplete[0]));
}
#[test]
fn checkpoint_cut_and_incomplete_group_never_split_causal_history() {
    let selected = model(1000);
    let history = [
        Message::user("a"),
        Message::user("b"),
        Message::user("c"),
        Message::user("d"),
    ];
    let mut groups = group_messages(&history);
    groups[1].has_checkpoint_before = true;
    let fixed = Message::user("fixed");
    let planner = planner(90, 30, 600);
    let input = PlanningInput {
        instructions: "",
        tools: &[],
        groups: &groups,
        base_watermark: 0,
        fixed_message: &fixed,
    };
    assert_eq!(
        planner
            .plan(&selected, Some(100), &input)
            .unwrap()
            .unwrap()
            .keep_group_from,
        1
    );
    let mut incomplete = group_messages(&history);
    incomplete[1].is_complete = false;
    let mut input = PlanningInput {
        groups: &incomplete,
        ..input
    };
    let plan = planner.plan(&selected, Some(100), &input).unwrap().unwrap();
    assert_eq!(plan.keep_group_from, 1);
    assert_eq!(plan.watermark, 1);
    input.base_watermark = 1;
    assert!(
        planner
            .plan(&selected, Some(100), &input)
            .unwrap()
            .is_none()
    );
    let mut earliest_incomplete = group_messages(&history);
    earliest_incomplete[0].is_complete = false;
    let input = PlanningInput {
        groups: &earliest_incomplete,
        base_watermark: 0,
        ..input
    };
    assert!(
        planner
            .plan(&selected, Some(100), &input)
            .unwrap()
            .is_none()
    );
}
#[test]
fn checkpoint_fallback_only_uses_input_limit_when_required_suffix_exceeds_target() {
    let selected = model(1000);
    let history = [
        Message::user("a"),
        Message::user("b"),
        Message::user("x".repeat(200)),
        Message::user("y".repeat(200)),
    ];
    let mut groups = group_messages(&history);
    groups[1].has_checkpoint_before = true;
    let fixed = Message::user("fixed");
    let input = PlanningInput {
        instructions: "",
        tools: &[],
        groups: &groups,
        base_watermark: 0,
        fixed_message: &fixed,
    };
    assert_eq!(
        planner(90, 30, 600)
            .plan(&selected, Some(10), &input)
            .unwrap()
            .unwrap()
            .keep_group_from,
        1
    );
    // Required suffix fits 100, but checkpoint suffix costs 140: no fallback.
    assert_eq!(
        planner(90, 30, 600)
            .plan(&selected, Some(100), &input)
            .unwrap()
            .unwrap()
            .keep_group_from,
        3
    );
    assert_eq!(
        planner(90, 30, 600)
            .plan(&selected, Some(140), &input)
            .unwrap()
            .unwrap()
            .keep_group_from,
        1
    );
}
#[test]
fn spill_plans_only_complete_assistant_tool_groups_and_preserves_watermark() {
    let selected = model(1000);
    let history = [
        Message::assistant("", vec![call("a")]),
        Message::tool_result("a", "x".repeat(1000)),
        Message::user("tail"),
    ];
    let groups = group_messages(&history);
    let fixed = Message::user("fixed");
    let input = PlanningInput {
        instructions: "",
        tools: &[],
        groups: &groups,
        base_watermark: 0,
        fixed_message: &fixed,
    };
    let plan = planner(90, 30, 100)
        .plan(&selected, Some(300), &input)
        .unwrap()
        .unwrap();
    assert_eq!(plan.spill_groups, vec![0]);
    assert_eq!(plan.watermark, 1);
    let non_tool = [Message::user("x".repeat(1000)), Message::user("tail")];
    let non_tool_groups = group_messages(&non_tool);
    let input = PlanningInput {
        groups: &non_tool_groups,
        ..input
    };
    assert_eq!(
        planner(90, 30, 100).plan(&selected, Some(300), &input),
        Err(PlanningError::GroupExceedsBudget)
    );
}
#[test]
fn actual_profiles_control_input_and_summary_output_without_provider_calls() {
    let profiles = ModelProfiles::new(&BTreeMap::from([(
        "provider".into(),
        ModelProfileConfig {
            maximum_input_tokens: Some(50),
            summary_output_tokens: Some(77),
            ..Default::default()
        },
    )]));
    let planner = CompactionPlanner::new(
        CompactionSettings {
            trigger_percent: 90,
            target_percent: 30,
            maximum_input_tokens: 600,
            summary_output_tokens: 100,
            summary_instructions: "".into(),
            summary_prefix: "prefix".into(),
        },
        profiles,
    );
    let history = [Message::user("first"), Message::user("last")];
    let groups = group_messages(&history);
    let fixed = Message::user("fixed");
    let input = PlanningInput {
        instructions: "",
        tools: &[],
        groups: &groups,
        base_watermark: 0,
        fixed_message: &fixed,
    };
    let plan = planner
        .plan(&model(1000), Some(300), &input)
        .unwrap()
        .unwrap();
    assert_eq!((plan.input_budget, plan.summary_output_tokens), (50, 77));
    assert_eq!(
        planner
            .estimate_summary_request_tokens(&model(1000), "", 0, &history[..1])
            .unwrap(),
        18
    );
    assert_eq!(
        planner
            .estimate_summary_request_tokens(&model(1000), "summary", 5, &history[..1])
            .unwrap(),
        35
    );
    assert_eq!(
        planner.plan(&model(0), Some(300), &input),
        Err(PlanningError::NoCompactionCapacity)
    );
    assert!(planner.plan(&model(1000), None, &input).unwrap().is_none());
}
#[test]
fn cadence_and_retained_budget_are_the_existing_domain_values() {
    let mut cadence = ContextCadence::new();
    assert_eq!(
        cadence
            .observe(
                &ContextSnapshot::new(40, 100, Some(40), 90),
                "provider/model",
                1
            )
            .unwrap(),
        None
    );
    assert_eq!(
        cadence
            .observe(
                &ContextSnapshot::new(51, 100, Some(51), 90),
                "provider/model",
                2
            )
            .unwrap(),
        Some(50)
    );
    assert_eq!(
        cadence
            .observe(
                &ContextSnapshot::new(59, 100, Some(59), 90),
                "provider/model",
                3
            )
            .unwrap(),
        None
    );
    let budget = RetainedAgentBudget::new(1);
    let reservation = budget.reserve().unwrap();
    assert!(budget.reserve().is_err());
    reservation.commit();
    reservation.rollback();
    assert!(budget.reserve().is_err());
    reservation.release();
    assert!(budget.reserve().is_ok());
}

#[test]
fn long_history_retains_every_message_after_first_incomplete_group() {
    let history: Vec<_> = (0..200)
        .map(|index| Message::user(format!("message {index}")))
        .collect();
    let mut groups = group_messages(&history);
    groups[150].is_complete = false;
    let fixed = Message::user("fixed");
    let input = PlanningInput {
        instructions: "",
        tools: &[],
        groups: &groups,
        base_watermark: 0,
        fixed_message: &fixed,
    };
    let plan = planner(90, 30, 60_000)
        .plan(&model(100_000), Some(1000), &input)
        .unwrap()
        .unwrap();
    assert_eq!(plan.keep_group_from, 150);
    assert_eq!(plan.watermark, 150);
    let retained: Vec<_> = groups[plan.keep_group_from..]
        .iter()
        .flat_map(|group| group.messages)
        .collect();
    assert_eq!(retained.len(), 50);
    assert!(
        retained
            .iter()
            .zip(&history[150..])
            .all(|(actual, expected)| std::ptr::eq(*actual, expected))
    );
}

#[test]
fn completeness_uses_source_ordinal_ids_not_result_count_or_assistant_text() {
    let duplicate_calls = [
        Message::assistant("", vec![call("A"), call("A")]),
        Message::tool_result("A", "result"),
    ];
    assert!(is_complete(&duplicate_calls));
    let wrong_case = [
        Message::assistant("", vec![call("A")]),
        Message::tool_result("a", "result"),
    ];
    assert!(!is_complete(&wrong_case));
    assert!(is_complete(&[Message::tool_result("orphan", "result")]));
}
