mod context;
mod execution;

use parrot_domain::{
    contracts::async_ops::{BoxFuture, Cancellation},
    values::config::ModelProfiles,
};
use parrot_provider::{
    catalogs::Model,
    routing::{
        aliases::{
            ModelAliasCatalog, ModelAliasDefinition, ModelRouter, ModelRouting,
            ResolvedModelSelection,
        },
        registry::{ProviderModel, ProviderRegistry},
    },
    sessions::{Completion, Content, Error, Message, Provider, Session, Tool, ToolCall},
};
use parrot_runtime::compaction::*;
use std::{
    collections::BTreeMap,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
};

pub(crate) fn group_messages(messages: &[Message]) -> Vec<CompactionGroup<'_>> {
    let converted: Vec<_> = messages.iter().cloned().map(Into::into).collect();
    let mut offset = 0;
    parrot_core::compaction::group_messages(&converted)
        .into_iter()
        .map(|group| {
            let start = offset;
            offset += group.messages.len();
            CompactionGroup {
                messages: &messages[start..offset],
                end_watermark: group.end_watermark,
                is_complete: group.is_complete,
            }
        })
        .collect()
}

struct ImageProvider {
    tokens: i64,
    calculations: AtomicUsize,
}
impl Provider for ImageProvider {
    fn id(&self) -> &str {
        "provider"
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
fn select(selected: &ProviderModel) -> ResolvedModelSelection {
    let registry = Arc::new(
        ProviderRegistry::new(
            vec![selected.provider.clone()],
            BTreeMap::from([("provider".into(), vec![selected.model.clone()])]),
        )
        .unwrap(),
    );
    let catalog = Arc::new(ModelAliasCatalog::new(registry.clone(), []).unwrap());
    let routing = Arc::new(ModelRouting::new(catalog, "provider/model"));
    ModelRouter::new(registry, routing)
        .resolve("provider/model")
        .unwrap()
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
        (0, 65536, 0, 1000, 65536),
        (1000000, 131072, 0, 1000, 131072),
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
            .estimate_selected_context(&select(&selected), "", &[], &[])
            .unwrap();
        let trigger = snapshot.trigger_tokens.unwrap();
        let budget = ProviderTokenBudget::default();
        let requested = if output > 0 { output } else { 32 * 1024 };
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
            .estimate_selected_context(&select(&model(limit)), instructions, &[], &[])
            .unwrap();
        assert_eq!(snapshot.estimated_tokens, estimate);
        assert_eq!(snapshot.context_limit, limit);
        assert_eq!(snapshot.usage_percent, usage);
        assert_eq!(snapshot.trigger_percent, 50);
        assert_eq!(snapshot.exceeds_trigger(), trigger);
    }
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
    let snapshot = planner(90, 30, 60000)
        .estimate_selected_context(&selected, "", &[], &[])
        .unwrap();
    assert_eq!(snapshot.context_limit, 1000);
    assert_eq!(snapshot.input_limit, 1000);
    assert_eq!(snapshot.trigger_percent, 40);
    assert!(snapshot.has_context_limit_override);
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
    assert_eq!(groups[0].messages[1].tool_call_id, "b");
    assert_eq!(groups[2].end_watermark, 3);
    let incomplete = group_messages(&history[..2]);
    assert!(!incomplete[0].is_complete);
}
#[test]
fn retained_budget_preserves_reservation_lifecycle() {
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
fn original_retained_budget_exact_once_pending_commit_rollback_release() {
    let budget = RetainedAgentBudget::new(2);
    let pending = budget.reserve().unwrap();
    let retained = budget.reserve().unwrap();
    retained.commit();
    retained.commit();
    assert!(budget.reserve().is_err());
    pending.rollback();
    pending.rollback();
    let replacement = budget.reserve().unwrap();
    replacement.commit();
    replacement.rollback();
    assert!(budget.reserve().is_err());
    retained.release();
    retained.release();
    let final_reservation = budget.reserve().unwrap();
    final_reservation.rollback();
    replacement.release();
}

#[test]
fn o200k_base_text_totals_preserve_image_and_request_overhead() {
    let provider = Arc::new(ImageProvider {
        tokens: 123,
        calculations: AtomicUsize::new(0),
    });
    let mut selected = model(384);
    selected.provider = provider.clone();
    let history = [
        Message::new(
            parrot_provider::sessions::Role::User,
            vec![
                Content::Text("日本語のテキスト".into()),
                Content::image_file("does-not-exist", "image/png", 200, 100),
            ],
        ),
        Message::assistant("Hello, world!", vec![call("call")]),
        Message::tool_result("call", "🙂🙂a"),
    ];
    let tools = [Tool {
        name: "tool".into(),
        description: "日本語のテキスト".into(),
        parameters_json: "{}".into(),
    }];
    assert_eq!(
        CompactionPlanner::estimate_tokens(&selected, &history).unwrap(),
        (6 + 123 + 8) + (4 + 1 + 1 + 1 + 8) + (3 + 1 + 8)
    );
    assert_eq!(provider.calculations.load(Ordering::Relaxed), 1);
    assert_eq!(
        CompactionPlanner::estimate_input_tokens(&selected, "Hello, world!", &tools, &history)
            .unwrap(),
        4 + (1 + 6 + 1 + 12) + 164 + 4
    );
    let selection = select(&selected);
    let planner = planner(50, 30, 600);
    let boundary = planner
        .estimate_selected_context(&selection, "Hello, world!", &tools, &history)
        .unwrap();
    assert_eq!(boundary.estimated_tokens, 192);
    assert_eq!(boundary.trigger_tokens, Some(192));
    assert!(!boundary.exceeds_trigger());
    let above = planner
        .estimate_selected_context(&selection, "Hello, world! Hello", &tools, &history)
        .unwrap();
    assert_eq!(above.estimated_tokens, 193);
    assert!(above.exceeds_trigger());
}
