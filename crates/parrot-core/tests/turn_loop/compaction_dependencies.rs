use super::*;
use parrot_core::compaction::{
    CompactionContext, CompactionDependencies, CompactionInput, CompactionPlan, CompactionResult,
    FittedCompaction,
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

#[derive(Default)]
struct PlannedContext {
    plan: Option<CompactionPlan>,
}

impl CompactionContext for PlannedContext {
    fn plan(
        &self,
        input: &CompactionInput,
        definitions: &[ToolDefinition],
    ) -> Result<Option<CompactionPlan>, Error> {
        Ok(self.plan.clone().map(|mut plan| {
            plan.instructions = input.instructions.clone();
            plan.tools = definitions.to_vec();
            plan
        }))
    }

    fn fit<'a, 'c>(
        &'a self,
        _: &'a CompactionInput,
        _: &'a [ToolDefinition],
        result: Option<CompactionResult>,
        _: CancellationToken<'c>,
    ) -> BoxFuture<'a, Result<Option<FittedCompaction>, Error>>
    where
        'c: 'a,
    {
        Box::pin(async move {
            Ok(result.map(|result| FittedCompaction {
                result,
                status: "fitted status".into(),
            }))
        })
    }
}

#[derive(Default)]
pub(super) struct MemoryCompaction {
    context: PlannedContext,
    effects: compaction_support::Effects,
    provider: compaction_support::Provider,
}

impl MemoryCompaction {
    pub(super) fn new(
        plan: Option<CompactionPlan>,
        provider: compaction_support::Provider,
    ) -> Self {
        Self {
            context: PlannedContext { plan },
            provider,
            ..Self::default()
        }
    }

    pub(super) fn dependencies(&mut self) -> CompactionDependencies<'_> {
        CompactionDependencies {
            context: &self.context,
            effects: &mut self.effects,
            provider: &self.provider,
        }
    }
}

struct RecordedContext {
    inputs: Mutex<Vec<(CompactionInput, Vec<ToolDefinition>)>>,
    fail_at: Option<&'static str>,
}

impl CompactionContext for RecordedContext {
    fn plan(
        &self,
        input: &CompactionInput,
        definitions: &[ToolDefinition],
    ) -> Result<Option<CompactionPlan>, Error> {
        self.inputs
            .lock()
            .unwrap()
            .push((input.clone(), definitions.to_vec()));
        if self.fail_at == Some("plan") {
            return Err(Error::failed("planning failed"));
        }
        let mut plan = compaction_support::plan(&["saved group", "retained tail"]);
        plan.instructions = input.instructions.clone();
        plan.tools = definitions.to_vec();
        Ok(Some(plan))
    }

    fn fit<'a, 'c>(
        &'a self,
        input: &'a CompactionInput,
        definitions: &'a [ToolDefinition],
        result: Option<CompactionResult>,
        _: CancellationToken<'c>,
    ) -> BoxFuture<'a, Result<Option<FittedCompaction>, Error>>
    where
        'c: 'a,
    {
        Box::pin(async move {
            let inputs = self.inputs.lock().unwrap();
            assert_eq!(inputs[0], (input.clone(), definitions.to_vec()));
            if self.fail_at == Some("fit") {
                return Err(Error::failed("fitting failed"));
            }
            Ok(result.map(|result| FittedCompaction {
                result,
                status: "fitted status".into(),
            }))
        })
    }
}

#[test]
fn caller_supplied_compaction_services_plan_summarize_and_fit_before_retry() {
    let mut state = MemoryState::new([
        Reply::TooLarge("oversized"),
        Reply::final_response("answer"),
    ]);
    let context = RecordedContext {
        inputs: Mutex::default(),
        fail_at: None,
    };
    let provider =
        compaction_support::Provider::new([parrot_core::compaction::SummaryResponse::Summary(
            "injected summary".into(),
        )]);
    let mut effects = compaction_support::Effects::default();
    let mut compaction = CompactionDependencies {
        context: &context,
        effects: &mut effects,
        provider: &provider,
    };
    assert_eq!(
        state.run_with_compaction(&mut compaction),
        Ok(Outcome::Completed("answer".into()))
    );

    let inputs = context.inputs.lock().unwrap();
    assert_eq!(inputs.len(), 1);
    assert_eq!(inputs[0].0.instructions, "core request");
    assert_eq!(
        inputs[0]
            .1
            .iter()
            .map(|tool| tool.name.as_str())
            .collect::<Vec<_>>(),
        ["read", "write"]
    );
    assert_eq!(provider.requests.lock().unwrap().len(), 1);
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
    assert_eq!(state.provider.requests.lock().unwrap().len(), 2);
    let estimates = effects.contexts.lock().unwrap();
    assert_eq!(estimates.len(), 1);
    assert_eq!(estimates[0].instructions, "core request");
    assert_eq!(estimates[0].tools, inputs[0].1);
    assert_eq!(estimates[0].messages, result.history);
    assert_eq!(effects.progress.lock().unwrap().len(), 2);
}

#[test]
fn planning_and_fitting_failures_are_recorded_before_propagation() {
    for (step, message) in [("plan", "planning failed"), ("fit", "fitting failed")] {
        let mut state = MemoryState::new([Reply::TooLarge("oversized")]);
        let context = RecordedContext {
            inputs: Mutex::default(),
            fail_at: Some(step),
        };
        let provider =
            compaction_support::Provider::new([parrot_core::compaction::SummaryResponse::Summary(
                "not committed".into(),
            )]);
        let mut effects = compaction_support::Effects::default();
        let mut compaction = CompactionDependencies {
            context: &context,
            effects: &mut effects,
            provider: &provider,
        };
        let error = Error::failed(message);
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
