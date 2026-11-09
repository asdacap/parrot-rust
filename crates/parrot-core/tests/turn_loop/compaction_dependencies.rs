use super::*;
use parrot_core::compaction::{
    CompactionConfiguration, CompactionDependencies, CompactionPlan, StatusRenderer,
};

#[test]
fn compaction_reads_return_owned_conversation_data_without_invoking_a_summary() {
    fn retain_owned<T: Send + Sync + 'static>(value: T) -> T {
        value
    }

    let mut state = MemoryState::new([]);
    state.instructions = "sampled epoch".into();
    state.messages = vec![compaction_support::message("saved conversation")];
    let provider = state.summary_provider.clone();
    let input = retain_owned(finish(state.read_compaction(&[])).unwrap());
    drop(state);

    assert_eq!(input.instructions, "sampled epoch");
    assert_eq!(
        input.messages,
        [compaction_support::message("saved conversation")]
    );
    assert_eq!(input.groups[0].messages, input.messages);
    assert_eq!(input.groups[0].end_watermark, 1);
    assert_eq!(input.base_watermark, 0);
    assert!(provider.requests.lock().unwrap().is_empty());
}

struct MemoryStatus {
    contexts: Mutex<Vec<ContextSnapshot>>,
    failure: Option<Error>,
}

impl StatusRenderer for MemoryStatus {
    type Error = Error;

    fn render<'a, 'c>(
        &'a self,
        context: ContextSnapshot,
        _: CancellationToken<'c>,
    ) -> BoxFuture<'a, Result<String, Error>>
    where
        'c: 'a,
    {
        Box::pin(async move {
            self.contexts.lock().unwrap().push(context);
            if let Some(error) = &self.failure {
                return Err(error.clone());
            }
            Ok("fitted status".into())
        })
    }
}

pub(super) struct MemoryCompaction {
    configuration: CompactionConfiguration,
    effects: compaction_support::Effects,
    provider: compaction_support::Provider,
    status: MemoryStatus,
}

impl Default for MemoryCompaction {
    fn default() -> Self {
        Self::new(None, compaction_support::Provider::default())
    }
}

impl MemoryCompaction {
    pub(super) fn new(
        plan: Option<CompactionPlan>,
        provider: compaction_support::Provider,
    ) -> Self {
        let input_limit = plan.as_ref().map_or(10000, |plan| plan.input_token_limit);
        let configuration = CompactionConfiguration {
            context: ContextConfiguration {
                model: ContextModel {
                    canonical_model: "summary-model".into(),
                    context_window: if plan.is_some() { input_limit } else { 0 },
                    maximum_input_tokens: input_limit,
                    maximum_output_tokens: 1,
                },
                trigger_percent: 90,
                context_limit: None,
            },
            target: plan
                .is_none()
                .then_some(parrot_core::ContextLimit::Percentage(50)),
            target_percent: 50,
            maximum_input_tokens: plan.as_ref().map_or(1000, |plan| plan.input_budget as i32),
            summary_output_tokens: plan.as_ref().map_or(19, |plan| plan.summary_output_tokens),
            profile_maximum_input_tokens: None,
            profile_summary_output_tokens: None,
            model: plan
                .as_ref()
                .map_or_else(|| "summary-model".into(), |plan| plan.model.clone()),
            reasoning: plan.as_ref().and_then(|plan| plan.reasoning.clone()),
            summary_instructions: plan.as_ref().map_or_else(
                || "instructions".into(),
                |plan| plan.summary_instructions.clone(),
            ),
            summary_prefix: plan
                .as_ref()
                .map_or_else(|| "prefix:".into(), |plan| plan.summary_prefix.clone()),
        };
        Self {
            configuration,
            effects: compaction_support::Effects::default(),
            provider,
            status: MemoryStatus {
                contexts: Mutex::default(),
                failure: None,
            },
        }
    }

    pub(super) fn dependencies(&mut self) -> CompactionDependencies<'_> {
        CompactionDependencies {
            configuration: &self.configuration,
            effects: &mut self.effects,
            provider: &self.provider,
            status: &self.status,
        }
    }
}

#[test]
fn core_plans_and_fits_using_only_owned_data_and_injected_native_effects() {
    let mut state = MemoryState::new([
        Reply::TooLarge("oversized"),
        Reply::final_response("answer"),
    ]);
    state.compaction_plan = Some(compaction_support::plan(&["saved group", "retained tail"]));
    let provider =
        compaction_support::Provider::new([parrot_core::compaction::SummaryResponse::Summary(
            "injected summary".into(),
        )]);
    let mut compaction = MemoryCompaction::new(state.compaction_plan.clone(), provider.clone());
    assert_eq!(
        state.run_with_compaction(&mut compaction.dependencies()),
        Ok(Outcome::Completed("answer".into()))
    );

    let summaries = provider.requests.lock().unwrap();
    assert_eq!(summaries.len(), 1);
    assert_eq!(
        summaries[0].messages[1],
        compaction_support::message("saved group")
    );
    assert!(summaries[0].tools.is_empty());
    assert!(state.summary_provider.requests.lock().unwrap().is_empty());
    assert_eq!(state.fitted_statuses, ["fitted status"]);
    let result = state.recorded_compactions[0]
        .as_ref()
        .unwrap()
        .as_ref()
        .unwrap();
    assert_eq!(
        result.summary.contents,
        [Content::Text("prefix:injected summary".into())]
    );
    assert_eq!(result.watermark, 42);
    assert_eq!(state.provider.requests.lock().unwrap().len(), 2);
    let estimates = compaction.effects.contexts.lock().unwrap();
    let fitted = estimates.last().unwrap();
    assert_eq!(fitted.instructions, "core request");
    assert_eq!(
        fitted
            .tools
            .iter()
            .map(|tool| tool.name.as_str())
            .collect::<Vec<_>>(),
        ["read", "write"]
    );
    assert!(
        fitted
            .messages
            .iter()
            .any(|message| message.contents == [Content::Text("fitted status".into())])
    );
    assert!(!compaction.status.contexts.lock().unwrap().is_empty());
    assert_eq!(compaction.effects.progress.lock().unwrap().len(), 2);
}

#[test]
fn planning_and_fitting_failures_are_recorded_before_propagation() {
    for (step, message) in [("plan", "planning failed"), ("fit", "fitting failed")] {
        let mut state = MemoryState::new([Reply::TooLarge("oversized")]);
        state.compaction_plan = Some(compaction_support::plan(&["saved group", "tail"]));
        let provider =
            compaction_support::Provider::new([parrot_core::compaction::SummaryResponse::Summary(
                "not committed".into(),
            )]);
        let mut compaction = MemoryCompaction::new(state.compaction_plan.clone(), provider.clone());
        let error = Error::failed(message);
        if step == "plan" {
            compaction.effects.context_failure = Some((1, error.clone()));
        } else {
            compaction.status.failure = Some(error.clone());
        }
        assert_eq!(
            state.run_with_compaction(&mut compaction.dependencies()),
            Err(error.clone())
        );
        assert_eq!(state.recorded_compactions, [Err(error)]);
        assert!(state.fitted_statuses.is_empty());
        assert_eq!(state.provider.requests.lock().unwrap().len(), 1);
        assert_eq!(
            provider.requests.lock().unwrap().len(),
            usize::from(step == "fit")
        );
    }
}
