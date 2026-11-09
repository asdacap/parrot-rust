use super::*;
use parrot_core::compaction::{
    CompactionConfiguration, CompactionInput, CompactionResult, StatusRenderer, fit_compaction,
    fit_status,
};
use parrot_core::{ContextConfiguration, ContextModel, ContextSnapshot, Message};
use std::sync::Mutex;

fn configuration(input_limit: i32) -> CompactionConfiguration {
    CompactionConfiguration {
        context: ContextConfiguration {
            model: ContextModel {
                canonical_model: "provider/model".into(),
                context_window: input_limit,
                maximum_input_tokens: input_limit,
                maximum_output_tokens: 1,
            },
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
        summary_instructions: "summarize".into(),
        summary_prefix: "summary: ".into(),
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
        retained_messages: 1,
        watermark: 42,
    }
}

struct Status {
    text: String,
    contexts: Mutex<Vec<ContextSnapshot>>,
}

impl StatusRenderer for Status {
    type Error = Error;

    fn render<'a, 'c>(
        &'a self,
        context: ContextSnapshot,
        _: CancellationToken<'c>,
    ) -> parrot_core::BoxFuture<'a, Result<String, Error>>
    where
        'c: 'a,
    {
        Box::pin(async move {
            self.contexts.lock().unwrap().push(context);
            Ok(self.text.clone())
        })
    }
}

#[test]
fn core_status_fitting_compares_the_complete_snapshot_and_caps_rerenders() {
    let config = configuration(100);
    let mut estimates = 0;
    let renders = Mutex::new(Vec::new());
    let fitted = finish(fit_status(
        |_| {
            estimates += 1;
            let mut snapshot = ContextSnapshot::estimate(10, &config.context);
            if estimates > 1 {
                snapshot.input_limit = 50;
            }
            Ok::<_, Error>(snapshot)
        },
        |snapshot| {
            renders.lock().unwrap().push(snapshot);
            std::future::ready(Ok("actual status".to_owned()))
        },
    ))
    .unwrap();
    assert_eq!(fitted, "actual status");
    assert_eq!(estimates, 3);
    assert_eq!(renders.lock().unwrap().len(), 2);

    let mut estimates = 0;
    let renders = Mutex::new(Vec::new());
    let fitted = finish(fit_status(
        |_| {
            estimates += 1;
            Ok::<_, Error>(ContextSnapshot::estimate(estimates, &config.context))
        },
        |snapshot| {
            let text = snapshot.estimated_tokens.to_string();
            renders.lock().unwrap().push(snapshot);
            std::future::ready(Ok(text))
        },
    ))
    .unwrap();
    assert_eq!(estimates, 9);
    assert_eq!(renders.lock().unwrap().len(), 9);
    assert_eq!(fitted, "9");
}

#[test]
fn core_status_fitting_propagates_estimate_failure_before_rendering() {
    let renders = Mutex::new(0);
    let error = Error::failed("native estimate failed");
    let outcome = finish(fit_status(
        |_| Err::<ContextSnapshot, _>(error.clone()),
        |_| {
            *renders.lock().unwrap() += 1;
            std::future::ready(Ok("must not render".to_owned()))
        },
    ));
    assert_eq!(outcome, Err(error));
    assert_eq!(*renders.lock().unwrap(), 0);
}

#[test]
fn core_fits_actual_status_and_validates_the_final_replacement() {
    let effects = Effects::default();
    let status = Status {
        text: "actual status".into(),
        contexts: Mutex::default(),
    };
    let is_cancelled = || false;
    let cancelled = || Box::pin(std::future::pending()) as parrot_core::BoxFuture<'_, ()>;
    let fitted = finish(fit_compaction(
        &input(),
        &[],
        &configuration(100),
        Some(result()),
        &effects,
        &status,
        CancellationToken::new(&is_cancelled, &cancelled),
    ))
    .unwrap()
    .unwrap();
    assert_eq!(fitted.status, "actual status");
    assert_eq!(fitted.result.watermark, 42);
    {
        let contexts = effects.contexts.lock().unwrap();
        assert_eq!(
            contexts.last().unwrap().instructions,
            "sampled instructions"
        );
        assert_eq!(
            contexts.last().unwrap().messages[1].contents,
            [Content::Text("actual status".into())]
        );
        assert_eq!(contexts.last().unwrap().messages[2], message("retained"));
    }

    let status = Status {
        text: "x".repeat(200),
        contexts: Mutex::default(),
    };
    assert_eq!(
        finish(fit_compaction(
            &input(),
            &[],
            &configuration(100),
            Some(result()),
            &effects,
            &status,
            CancellationToken::new(&is_cancelled, &cancelled),
        )),
        Err(CompactionFailure::FittedInputLimitExceeded.into())
    );
}

#[test]
fn core_no_progress_checks_current_messages_without_rendering_status() {
    let effects = Effects::default();
    let status = Status {
        text: "must not render".into(),
        contexts: Mutex::default(),
    };
    let is_cancelled = || false;
    let cancelled = || Box::pin(std::future::pending()) as parrot_core::BoxFuture<'_, ()>;
    let mut input = input();
    input.base_watermark = 42;
    assert_eq!(
        finish(fit_compaction(
            &input,
            &[],
            &configuration(100),
            Some(result()),
            &effects,
            &status,
            CancellationToken::new(&is_cancelled, &cancelled),
        )),
        Ok(None)
    );
    assert_eq!(
        effects.contexts.lock().unwrap().last().unwrap().messages,
        input.messages
    );
    assert!(status.contexts.lock().unwrap().is_empty());
    for limit in [0, -1] {
        assert_eq!(
            finish(fit_compaction(
                &input,
                &[],
                &configuration(limit),
                None,
                &effects,
                &status,
                CancellationToken::new(&is_cancelled, &cancelled),
            )),
            Err(CompactionFailure::NoSafeBoundary.into())
        );
    }
}
