use super::*;
use parrot_core::compaction::{
    CompactionConfiguration, CompactionInput, CompactionResult, fit_compaction, fit_status,
};
use parrot_core::{
    ConfigKey, ConfigValue, ContextConfiguration, ContextModel, ContextSnapshot, ErrorCode,
    Message, States,
};
use std::sync::{
    Mutex,
    atomic::{AtomicUsize, Ordering},
};

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
    text: Option<String>,
    contexts: Mutex<Vec<ContextSnapshot>>,
}

impl Status {
    fn new(text: &str) -> Self {
        Self {
            text: Some(text.into()),
            contexts: Mutex::default(),
        }
    }
}

impl States for Status {
    fn get_config(&self, _: ConfigKey) -> parrot_core::BoxFuture<'_, Result<ConfigValue, Error>> {
        Box::pin(async { Err(Error::failed("status configuration is unavailable")) })
    }

    fn render_status<'a, 'c>(
        &'a self,
        context: ContextSnapshot,
        _: CancellationToken<'c>,
    ) -> parrot_core::BoxFuture<'a, Result<String, Error>>
    where
        'c: 'a,
    {
        Box::pin(async move {
            let content = self
                .text
                .clone()
                .unwrap_or_else(|| context.estimated_tokens.to_string());
            self.contexts.lock().unwrap().push(context);
            Ok(content)
        })
    }
}

enum StatusEstimate {
    Content,
    Increase,
    Failure(Error),
}

struct Estimates {
    effects: Effects,
    calls: AtomicUsize,
    status: StatusEstimate,
}

impl Estimates {
    fn new(status: StatusEstimate) -> Self {
        Self {
            effects: Effects::default(),
            calls: AtomicUsize::new(0),
            status,
        }
    }
}

impl parrot_core::Provider for Estimates {
    fn estimate_input_tokens(
        &self,
        instructions: &str,
        definitions: &[parrot_core::ToolDefinition],
        messages: &[Message],
    ) -> Result<i64, Error> {
        let call = self.calls.fetch_add(1, Ordering::SeqCst) + 1;
        match &self.status {
            StatusEstimate::Content => {
                self.effects
                    .estimate_context_tokens(instructions, definitions, messages)
            }
            StatusEstimate::Increase => Ok(call as i64),
            StatusEstimate::Failure(error) => Err(error.clone()),
        }
    }
}

#[test]
fn core_status_fitting_estimates_the_full_request_without_mutating_it() {
    let provider = Estimates::new(StatusEstimate::Content);
    let status = Status {
        text: None,
        contexts: Mutex::default(),
    };
    let request = ProviderRequest {
        instructions: "sampled instructions".into(),
        messages: vec![message("history"), message("pending input")],
        tools: vec![parrot_core::ToolDefinition {
            name: "test tool".into(),
            description: "test definition".into(),
            parameters_json: "{}".into(),
        }],
        ..ProviderRequest::default()
    };
    let original = request.clone();
    let configuration = configuration(100).context;
    let fitted = finish(fit_status(
        &request,
        &configuration,
        &status,
        &provider,
        parrot_core::CancellationToken::uncancelled(),
    ))
    .unwrap();
    assert_eq!(fitted, "22");
    assert_eq!(request, original);
    let contexts = provider.effects.contexts.lock().unwrap();
    assert!(contexts.len() > 1);
    for estimate in contexts.iter() {
        assert_eq!(estimate.instructions, request.instructions);
        assert_eq!(estimate.tools, request.tools);
        assert_eq!(&estimate.messages[..2], request.messages);
        assert_eq!(estimate.messages.len(), 3);
        assert_eq!(estimate.messages[2].role, Role::System);
    }
    assert_eq!(
        contexts.last().unwrap().messages[2].contents,
        [Content::Text(fitted)]
    );
    assert_eq!(
        status.contexts.lock().unwrap().last(),
        Some(&ContextSnapshot::estimate(22, &configuration))
    );
}

#[test]
fn core_status_fitting_caps_rerenders() {
    let provider = Estimates::new(StatusEstimate::Increase);
    let status = Status {
        text: None,
        contexts: Mutex::default(),
    };
    let fitted = finish(fit_status(
        &ProviderRequest::default(),
        &configuration(100).context,
        &status,
        &provider,
        parrot_core::CancellationToken::uncancelled(),
    ))
    .unwrap();
    assert_eq!(provider.calls.load(Ordering::SeqCst), 9);
    assert_eq!(status.contexts.lock().unwrap().len(), 9);
    assert_eq!(fitted, "9");
}

#[test]
fn core_status_fitting_propagates_estimate_failure_before_rendering() {
    let error = Error {
        code: ErrorCode::Timeout,
        response_body: "native estimate diagnostic".into(),
        detail: Some(parrot_core::ErrorDetail::Compaction(
            CompactionFailure::NoSummary("native detail".into()),
        )),
        ..Error::failed("native estimate failed")
    };
    let provider = Estimates::new(StatusEstimate::Failure(error.clone()));
    let status = Status::new("must not render");
    let outcome = finish(fit_status(
        &ProviderRequest::default(),
        &configuration(100).context,
        &status,
        &provider,
        parrot_core::CancellationToken::uncancelled(),
    ));
    assert_eq!(outcome, Err(error));
    assert!(status.contexts.lock().unwrap().is_empty());
}

#[test]
fn core_fits_actual_status_and_validates_the_final_replacement() {
    let provider = Estimates::new(StatusEstimate::Content);
    let status = Status::new("actual status");
    let fitted = finish(fit_compaction(
        &input(),
        &[],
        &configuration(100),
        Some(result()),
        &status,
        &provider,
        parrot_core::CancellationToken::uncancelled(),
    ))
    .unwrap()
    .unwrap();
    assert_eq!(fitted.status, "actual status");
    assert_eq!(fitted.result.watermark, 42);
    {
        let contexts = provider.effects.contexts.lock().unwrap();
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

    let status = Status::new(&"x".repeat(200));
    assert_eq!(
        finish(fit_compaction(
            &input(),
            &[],
            &configuration(100),
            Some(result()),
            &status,
            &provider,
            parrot_core::CancellationToken::uncancelled()
        )),
        Err(CompactionFailure::FittedInputLimitExceeded.into())
    );
}

#[test]
fn core_no_progress_checks_current_messages_without_rendering_status() {
    let provider = Estimates::new(StatusEstimate::Content);
    let status = Status::new("must not render");
    let mut input = input();
    input.base_watermark = 42;
    assert_eq!(
        finish(fit_compaction(
            &input,
            &[],
            &configuration(100),
            Some(result()),
            &status,
            &provider,
            parrot_core::CancellationToken::uncancelled()
        )),
        Ok(None)
    );
    assert_eq!(
        provider
            .effects
            .contexts
            .lock()
            .unwrap()
            .last()
            .unwrap()
            .messages,
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
                &status,
                &provider,
                parrot_core::CancellationToken::uncancelled()
            )),
            Err(CompactionFailure::NoSafeBoundary.into())
        );
    }
}
