use super::*;
use parrot_core::{CompactionConfiguration, CompactionPlan};

#[test]
fn compaction_reads_return_owned_conversation_data_without_invoking_a_summary() {
    fn retain_owned<T: Send + Sync + 'static>(value: T) -> T {
        value
    }

    let state = MemoryState::new([]);
    state.request().messages = vec![compaction_support::message("saved conversation")];
    let provider = state.summary_provider.clone();
    let input = retain_owned(finish(state.read_conversation_state()).unwrap());
    drop(state);

    assert_eq!(
        input.messages,
        [compaction_support::message("saved conversation")]
    );
    assert_eq!(input.history[0].message, input.messages[0]);
    assert_eq!(input.history[0].sequence, 1);
    assert_eq!(input.history[0].tool_owner_sequence, None);
    assert_eq!(input.summary, None);
    assert!(provider.requests.lock().unwrap().is_empty());
}

pub(super) struct MemoryStatus {
    pub(super) contexts: Mutex<Vec<ContextSnapshot>>,
    pub(super) failure: Option<(usize, Error)>,
}

impl MemoryStatus {
    pub(super) fn offering(
        &self,
        context: ContextSnapshot,
    ) -> BoxFuture<'_, Result<StatusOffering, Error>> {
        let text = {
            let mut contexts = self.contexts.lock().unwrap();
            contexts.push(context);
            if let Some((call, error)) = &self.failure
                && *call == contexts.len()
            {
                return Box::pin(async { Err(error.clone()) });
            }
            "fitted status".to_owned()
        };
        Box::pin(async move {
            let mut offering = StatusOffering::default();
            offering.insert(
                StatusKey::new("status"),
                StatusOffer {
                    hash: String::new(),
                    text,
                    is_auto_update: false,
                },
            );
            Ok(offering)
        })
    }
}

pub(super) struct MemoryCompaction {
    pub(super) configuration: CompactionConfiguration,
    pub(super) effects: compaction_support::Effects,
    pub(super) provider: compaction_support::Provider,
    pub(super) status: MemoryStatus,
    pub(super) saving_count: usize,
    pub(super) saving_error: Option<Error>,
    pub(super) failures: Vec<String>,
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
        let effects = compaction_support::Effects::default();
        let provider = provider.with_estimates(&effects);
        Self {
            configuration,
            effects,
            provider,
            status: MemoryStatus {
                contexts: Mutex::default(),
                failure: None,
            },
            saving_count: 0,
            saving_error: None,
            failures: Vec::new(),
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
    let provider = compaction_support::Provider::new([parrot_core::SummaryResponse::Summary(
        "injected summary".into(),
    )]);
    let mut compaction = MemoryCompaction::new(state.compaction_plan.clone(), provider.clone());
    assert_eq!(
        state.run_with_compaction(&mut compaction),
        Ok(Outcome::Completed("answer".into()))
    );

    let summaries = provider.requests.lock().unwrap();
    assert_eq!(summaries.len(), 1);
    assert_eq!(
        summaries[0].messages[0],
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
    assert_eq!(compaction.saving_count, 1);
    assert!(compaction.saving_error.is_none());
}

#[test]
fn saving_publication_failure_prevents_persistence_and_reports_failure() {
    let mut state = MemoryState::new([Reply::TooLarge("oversized")]);
    state.compaction_plan = Some(compaction_support::plan(&["saved group", "retained tail"]));
    let provider = compaction_support::Provider::new([parrot_core::SummaryResponse::Summary(
        "not committed".into(),
    )]);
    let mut compaction = MemoryCompaction::new(state.compaction_plan.clone(), provider);
    let error = Error::failed("saving publication failed");
    compaction.saving_error = Some(error.clone());

    assert_eq!(
        state.run_with_compaction(&mut compaction),
        Err(error.clone())
    );
    assert_eq!(compaction.saving_count, 1);
    assert_eq!(compaction.failures, [error.to_string()]);
    assert_eq!(state.recorded_compactions, [Err(error)]);
    assert!(state.fitted_statuses.is_empty());
    assert!(!state.compacting.load(Ordering::SeqCst));
    assert_eq!(state.provider.requests.lock().unwrap().len(), 1);
}

#[test]
fn preparation_failures_stop_without_recording_or_invoking_a_summary() {
    for stage in ["conversation", "estimate", "status"] {
        let mut state = MemoryState::new([Reply::TooLarge("oversized")]);
        state.compaction_plan = Some(compaction_support::plan(&["saved group", "tail"]));
        let provider = compaction_support::Provider::default();
        let mut compaction = MemoryCompaction::new(state.compaction_plan.clone(), provider.clone());
        let error = Error::failed(stage);
        match stage {
            "conversation" => {
                state.fail_at = Some(stage);
            }
            "estimate" => compaction.effects.context_failure = Some((1, error.clone())),
            "status" => compaction.status.failure = Some((1, error.clone())),
            _ => unreachable!(),
        }
        assert_eq!(state.run_with_compaction(&mut compaction), Err(error));
        assert!(state.recorded_compactions.is_empty());
        assert!(provider.requests.lock().unwrap().is_empty());
        assert_eq!(
            state.provider.requests.lock().unwrap().len(),
            usize::from(stage != "conversation")
        );
    }
}

#[test]
fn planning_and_fitting_failures_are_recorded_before_propagation() {
    for (step, message) in [("plan", "planning failed"), ("fit", "fitting failed")] {
        let mut state = MemoryState::new([Reply::TooLarge("oversized")]);
        state.compaction_plan = Some(compaction_support::plan(&["saved group", "tail"]));
        let provider = compaction_support::Provider::new([parrot_core::SummaryResponse::Summary(
            "not committed".into(),
        )]);
        let mut compaction = MemoryCompaction::new(state.compaction_plan.clone(), provider.clone());
        let error = Error::failed(message);
        if step == "plan" {
            compaction.effects.context_failure = Some((2, error.clone()));
        } else {
            compaction.status.failure = Some((2, error.clone()));
        }
        assert_eq!(
            state.run_with_compaction(&mut compaction),
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
