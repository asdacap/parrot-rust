use super::{
    fitting::fit_compaction,
    model::{CompactionConfiguration, CompactionInput, CompactionResult, FittedCompaction},
    test_support::*,
    *,
};
use parrot_core::{
    CompactionFailure, Content, ContextConfiguration, Error, Message, NeverCancelled, Role,
};
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};

fn configuration(limit: i32) -> CompactionConfiguration {
    CompactionConfiguration {
        context: ContextConfiguration {
            canonical_model: "test/model".into(),
            context_window: limit,
            maximum_input_tokens: limit,
            maximum_output_tokens: 1,
            trigger_percent: 90,
            context_limit: None,
        },
        target: None,
        target_percent: 50,
        maximum_input_tokens: 100,
        summary_output_tokens: 10,
        profile_maximum_input_tokens: None,
        profile_summary_output_tokens: None,
        model: "model".into(),
        reasoning: None,
        summary_instructions: "instructions".into(),
        summary_prefix: "prefix:".into(),
    }
}

fn input() -> CompactionInput {
    CompactionInput {
        groups: Vec::new(),
        base_watermark: 0,
        instructions: "sampled instructions".into(),
        messages: vec![message("original history")],
        fixed_message: Message {
            role: Role::System,
            ..message("old status")
        },
    }
}

fn result() -> CompactionResult {
    let summary = Message {
        role: Role::System,
        ..message("summary")
    };
    CompactionResult {
        history: vec![summary.clone(), input().fixed_message, message("retained")],
        summary,
        watermark: 42,
    }
}

fn fit(
    input: &CompactionInput,
    configuration: &CompactionConfiguration,
    result: Option<CompactionResult>,
    effects: &Effects,
    estimates: &dyn parrot_core::Provider,
) -> Result<Option<FittedCompaction>, Error> {
    let script = Arc::new(Script::default());
    let selected = script.selection();
    let sessions = crate::sessions::ProviderSessions::new();
    let templates = templates();
    finish(fit_compaction(
        input,
        &[],
        configuration,
        result,
        &Execution {
            estimates,
            sessions: &sessions,
            selected: &selected.canonical_model,
            effects,
            templates: &templates,
            cancellation: &NeverCancelled,
        },
    ))
}

#[test]
fn fits_actual_status_and_validates_complete_replacement() {
    let estimates = Estimates::default();
    let effects = Effects {
        status: "actual status".into(),
        ..Default::default()
    };
    let fitted = fit(
        &input(),
        &configuration(100),
        Some(result()),
        &effects,
        &estimates,
    )
    .unwrap()
    .unwrap();
    assert_eq!(fitted.status, "actual status");
    assert_eq!(fitted.result.watermark, 42);
    {
        let requests = estimates.requests.lock().unwrap();
        let last = requests.last().unwrap();
        assert_eq!(last.instructions, "sampled instructions");
        assert_eq!(
            last.messages[1].contents,
            [Content::Text("actual status".into())]
        );
        assert_eq!(last.messages[2], message("retained"));
    }
    let effects = Effects {
        status: "x".repeat(200),
        ..Default::default()
    };
    assert_eq!(
        fit(
            &input(),
            &configuration(100),
            Some(result()),
            &effects,
            &estimates
        ),
        Err(CompactionFailure::FittedInputLimitExceeded.into())
    );
}

#[test]
fn no_progress_validates_current_messages_without_rendering_status() {
    let estimates = Estimates::default();
    let effects = Effects {
        status: "must not render".into(),
        ..Default::default()
    };
    let mut input = input();
    input.base_watermark = 42;
    assert_eq!(
        fit(
            &input,
            &configuration(100),
            Some(result()),
            &effects,
            &estimates
        ),
        Ok(None)
    );
    assert_eq!(
        estimates.requests.lock().unwrap().last().unwrap().messages,
        input.messages
    );
    assert!(effects.contexts.lock().unwrap().is_empty());
    for limit in [0, -1] {
        assert_eq!(
            fit(&input, &configuration(limit), None, &effects, &estimates),
            Err(CompactionFailure::NoSafeBoundary.into())
        );
    }
}

#[test]
fn fitting_propagates_estimate_failure_before_status_effects() {
    let estimates = Estimates {
        fail_at: Some(1),
        ..Default::default()
    };
    let effects = Effects::default();
    assert_eq!(
        fit(
            &input(),
            &configuration(100),
            Some(result()),
            &effects,
            &estimates
        ),
        Err(failure("estimate"))
    );
    assert!(effects.contexts.lock().unwrap().is_empty());
}

struct IncreasingEstimates(AtomicUsize);
impl parrot_core::Provider for IncreasingEstimates {
    fn estimate_input_tokens(
        &self,
        _: &str,
        _: &[parrot_core::ToolDefinition],
        _: &[Message],
    ) -> Result<i64, Error> {
        Ok(self.0.fetch_add(1, Ordering::SeqCst) as i64 + 1)
    }
}

#[test]
fn dynamic_compaction_status_caps_rerenders() {
    let effects = Effects {
        usage_status: true,
        ..Default::default()
    };
    let fitted = fit(
        &input(),
        &configuration(100),
        Some(result()),
        &effects,
        &IncreasingEstimates(AtomicUsize::new(0)),
    )
    .unwrap()
    .unwrap();
    assert_eq!(fitted.status, "9");
    assert_eq!(effects.contexts.lock().unwrap().len(), 9);
}
