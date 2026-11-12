use parrot_core::{
    BoxFuture, CancellationToken, CompactionFailure, CompactionGroup, CompactionPlan,
    CompactionProgress, CompactionResult, ConfigKey, ConfigValue, Content, Error, ErrorCode,
    ErrorDetail, ErrorKind, EventPublisher, Message, PlanningError, Provider, ProviderRequest,
    Role, States, SummaryResponse, compaction::compact,
};
use std::{
    collections::VecDeque,
    future::{Future, poll_fn},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    task::{Context, Poll, Wake, Waker},
    thread,
};

struct ThreadWake(thread::Thread);

impl Wake for ThreadWake {
    fn wake(self: Arc<Self>) {
        self.0.unpark();
    }
}

fn finish<F: Future>(future: F) -> F::Output {
    let waker = Waker::from(Arc::new(ThreadWake(thread::current())));
    let mut context = Context::from_waker(&waker);
    let mut future = std::pin::pin!(future);
    loop {
        match future.as_mut().poll(&mut context) {
            Poll::Ready(result) => return result,
            Poll::Pending => thread::park(),
        }
    }
}

fn native_error(step: &str) -> Error {
    Error {
        code: ErrorCode::Http,
        response_body: format!("native response for {step}"),
        detail: Some(ErrorDetail::Planning(PlanningError::NoCompactionCapacity)),
        ..Error::failed(format!("native {step} failure"))
    }
}

fn compaction_error(failure: CompactionFailure) -> Error {
    Error {
        kind: ErrorKind::Failed,
        code: ErrorCode::InvalidOperation,
        message: failure.to_string(),
        response_body: String::new(),
        detail: Some(ErrorDetail::Compaction(failure)),
    }
}

struct RecordedEffects {
    trace: Arc<Mutex<Vec<&'static str>>>,
    fail_at: Option<&'static str>,
}

impl States for RecordedEffects {
    fn get_config(&self, _: ConfigKey) -> BoxFuture<'_, Result<ConfigValue, Error>> {
        Box::pin(async { Err(Error::unsupported("test state configuration")) })
    }

    fn write_to_scratch<'a, 'c>(
        &'a self,
        _: &'a [u8],
        _: CancellationToken<'c>,
    ) -> BoxFuture<'a, Result<String, Error>>
    where
        'c: 'a,
    {
        panic!("these complete user groups must never spill")
    }
}

struct RecordedPublisher {
    trace: Arc<Mutex<Vec<&'static str>>>,
    fail_at: Option<&'static str>,
}

impl EventPublisher for RecordedPublisher {
    fn on_epoch_started(&self) -> BoxFuture<'_, Result<(), Error>> {
        Box::pin(async { Err(Error::unsupported("RecordedPublisher.on_epoch_started")) })
    }
    fn report_compaction_started(&self) -> BoxFuture<'_, Result<(), Error>> {
        Box::pin(async {
            Err(Error::unsupported(
                "RecordedPublisher.report_compaction_started",
            ))
        })
    }
    fn report_compaction_failed(&self, _: String) -> BoxFuture<'_, Result<(), Error>> {
        Box::pin(async {
            Err(Error::unsupported(
                "RecordedPublisher.report_compaction_failed",
            ))
        })
    }
    fn report_compaction_finished(&self) -> BoxFuture<'_, Result<(), Error>> {
        Box::pin(async {
            Err(Error::unsupported(
                "RecordedPublisher.report_compaction_finished",
            ))
        })
    }
    fn report_compaction_progress<'a, 'c>(
        &'a self,
        progress: CompactionProgress,
        _: CancellationToken<'c>,
    ) -> BoxFuture<'a, Result<(), Error>>
    where
        'c: 'a,
    {
        Box::pin(async move {
            let step = if progress.completed_groups == 0 {
                "progress-start"
            } else {
                "progress-completed"
            };
            self.trace.lock().unwrap().push(step);
            if self.fail_at == Some(step) {
                Err(native_error(step))
            } else {
                Ok(())
            }
        })
    }
}

struct ScriptedSummaryProvider {
    trace: Arc<Mutex<Vec<&'static str>>>,
    replies: Mutex<VecDeque<Result<SummaryResponse, Error>>>,
    requests: Mutex<Vec<ProviderRequest>>,
    cancelled: Arc<AtomicBool>,
    cancel_before_cleanup: bool,
    fail_at: Option<&'static str>,
}

struct SummaryCancellation<'a>(&'a AtomicBool);

impl parrot_core::Cancellation for SummaryCancellation<'_> {
    fn is_cancelled(&self) -> bool {
        self.0.load(Ordering::SeqCst)
    }

    fn cancelled(&self) -> BoxFuture<'_, ()> {
        Box::pin(poll_fn(|_| {
            if self.0.load(Ordering::SeqCst) {
                Poll::Ready(())
            } else {
                Poll::Pending
            }
        }))
    }
}

impl Provider for ScriptedSummaryProvider {
    fn estimate_input_tokens(
        &self,
        instructions: &str,
        _: &[parrot_core::ToolDefinition],
        messages: &[Message],
    ) -> Result<i64, Error> {
        if instructions.is_empty() {
            return Ok(messages.len() as i64);
        }
        self.trace.lock().unwrap().push("fit");
        if self.fail_at == Some("fit") {
            Err(native_error("fit"))
        } else {
            Ok(1)
        }
    }

    fn summarize<'a, 'c>(
        &'a self,
        request: ProviderRequest,
        cancellation: CancellationToken<'c>,
    ) -> BoxFuture<'a, Result<SummaryResponse, Error>>
    where
        'c: 'a,
    {
        Box::pin(async move {
            self.trace.lock().unwrap().push("provider-start");
            self.requests.lock().unwrap().push(request);
            let result = self.replies.lock().unwrap().pop_front().unwrap();
            if self.cancel_before_cleanup {
                self.cancelled.store(true, Ordering::SeqCst);
                assert!(cancellation.is_cancelled());
                cancellation.cancelled().await;
                let mut yielded = false;
                poll_fn(|context| {
                    if yielded {
                        Poll::Ready(())
                    } else {
                        yielded = true;
                        context.waker().wake_by_ref();
                        Poll::Pending
                    }
                })
                .await;
            }
            self.trace.lock().unwrap().push("provider-closed");
            result
        })
    }
}

struct Fixture {
    effects: RecordedEffects,
    publisher: RecordedPublisher,
    provider: ScriptedSummaryProvider,
    plan: CompactionPlan,
}

impl Fixture {
    fn new(replies: impl IntoIterator<Item = Result<SummaryResponse, Error>>) -> Self {
        let trace = Arc::new(Mutex::new(Vec::new()));
        Self {
            effects: RecordedEffects {
                trace: trace.clone(),
                fail_at: None,
            },
            publisher: RecordedPublisher {
                trace: trace.clone(),
                fail_at: None,
            },
            provider: ScriptedSummaryProvider {
                trace,
                replies: Mutex::new(replies.into_iter().collect()),
                requests: Mutex::new(Vec::new()),
                cancelled: Arc::new(AtomicBool::new(false)),
                cancel_before_cleanup: false,
                fail_at: None,
            },
            plan: CompactionPlan {
                groups: ["first", "second", "retained"]
                    .into_iter()
                    .map(|text| CompactionGroup {
                        messages: vec![Message {
                            role: Role::User,
                            contents: vec![Content::Text(text.into())],
                            ..Message::default()
                        }],
                        is_complete: true,
                    })
                    .collect(),
                keep_group_from: 2,
                watermark: 42,
                input_budget: 100,
                summary_output_tokens: 10,
                input_token_limit: 100,
                instructions: "conversation instructions".into(),
                tools: Vec::new(),
                fixed_message: Message::default(),
                summary_instructions: "summarize".into(),
                summary_prefix: "summary: ".into(),
                model: "model".into(),
                reasoning: None,
            },
        }
    }

    fn run(&mut self) -> Result<Option<CompactionResult>, Error> {
        self.provider.fail_at = self.effects.fail_at;
        self.publisher.fail_at = self.effects.fail_at;
        let source = SummaryCancellation(&self.provider.cancelled);
        finish(compact(
            Some(self.plan.clone()),
            &mut self.effects,
            &self.provider,
            &self.publisher,
            CancellationToken::new(&source),
        ))
    }
}

#[test]
fn native_effect_failures_keep_identity_at_each_compaction_boundary() {
    for step in ["progress-start", "progress-completed", "fit"] {
        let mut fixture = Fixture::new([Ok(SummaryResponse::Summary("summary".into()))]);
        fixture.effects.fail_at = Some(step);

        assert_eq!(fixture.run(), Err(native_error(step)));
        assert_eq!(fixture.effects.trace.lock().unwrap().last(), Some(&step));
        assert_eq!(
            fixture.provider.requests.lock().unwrap().len(),
            usize::from(step != "progress-start")
        );
    }
}

#[test]
fn native_provider_failure_stops_before_completed_progress_or_fit() {
    let mut fixture = Fixture::new([Err(native_error("provider"))]);

    assert_eq!(fixture.run(), Err(native_error("provider")));
    assert_eq!(
        *fixture.effects.trace.lock().unwrap(),
        ["progress-start", "provider-start", "provider-closed"]
    );
}

#[test]
fn cancellation_during_a_summary_joins_provider_cleanup_before_returning() {
    let mut fixture = Fixture::new([Ok(SummaryResponse::Summary("summary".into()))]);
    fixture.provider.cancel_before_cleanup = true;

    assert_eq!(fixture.run(), Err(Error::cancelled()));
    assert_eq!(
        *fixture.effects.trace.lock().unwrap(),
        ["progress-start", "provider-start", "provider-closed"]
    );
}

#[test]
fn fallback_reports_the_terminal_diagnostic_without_retrying_a_failed_single_group() {
    let mut fixture = Fixture::new([
        Ok(SummaryResponse::NoSummary("batch diagnostic".into())),
        Ok(SummaryResponse::NoSummary("single-group diagnostic".into())),
        Ok(SummaryResponse::Summary("must not be requested".into())),
    ]);

    assert_eq!(
        fixture.run(),
        Err(compaction_error(CompactionFailure::NoSummary(
            "single-group diagnostic".into()
        )))
    );
    let requests = fixture.provider.requests.lock().unwrap();
    assert_eq!(requests.len(), 2);
    assert_eq!(requests[0].messages.len(), 3);
    assert_eq!(requests[1].messages.len(), 2);
    assert_eq!(requests[1].messages[1], fixture.plan.groups[0].messages[0]);
    assert_eq!(fixture.provider.replies.lock().unwrap().len(), 1);
}
