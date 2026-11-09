use parrot_core::{
    BoxFuture, CancellationToken, Content, Message, ProviderRequest, Role,
    compaction::{
        CompactionEffects, CompactionFailure, CompactionGroup, CompactionPlan, CompactionProgress,
        CompactionResult, SummaryProvider, SummaryResponse, compact,
    },
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

#[derive(Debug, Eq, PartialEq)]
enum NativeError {
    Compaction(CompactionFailure),
    Effect(&'static str),
}

impl From<CompactionFailure> for NativeError {
    fn from(error: CompactionFailure) -> Self {
        Self::Compaction(error)
    }
}

struct RecordedEffects {
    trace: Arc<Mutex<Vec<&'static str>>>,
    fail_at: Option<&'static str>,
}

impl CompactionEffects for RecordedEffects {
    type Error = NativeError;

    fn estimate_tokens(&self, messages: &[Message]) -> Result<i64, NativeError> {
        Ok(messages.len() as i64)
    }

    fn estimate_summary_tokens(
        &self,
        _: &str,
        chunk_tokens: i64,
        next_group: &[Message],
    ) -> Result<i64, NativeError> {
        Ok(chunk_tokens + next_group.len() as i64)
    }

    fn estimate_context_tokens(
        &self,
        _: &str,
        _: &[parrot_core::ToolDefinition],
        _: &[Message],
    ) -> Result<i64, NativeError> {
        self.trace.lock().unwrap().push("fit");
        if self.fail_at == Some("fit") {
            Err(NativeError::Effect("fit"))
        } else {
            Ok(1)
        }
    }

    fn spill_group<'a, 'c>(
        &'a mut self,
        _: &'a [Message],
        _: CancellationToken<'c>,
    ) -> BoxFuture<'a, Result<Vec<Message>, NativeError>>
    where
        'c: 'a,
    {
        panic!("these complete user groups must never spill")
    }

    fn report_progress<'a, 'c>(
        &'a self,
        progress: CompactionProgress,
        _: CancellationToken<'c>,
    ) -> BoxFuture<'a, Result<(), NativeError>>
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
                Err(NativeError::Effect(step))
            } else {
                Ok(())
            }
        })
    }
}

struct ScriptedSummaryProvider {
    trace: Arc<Mutex<Vec<&'static str>>>,
    replies: Mutex<VecDeque<Result<SummaryResponse, NativeError>>>,
    requests: Mutex<Vec<ProviderRequest>>,
    cancelled: Arc<AtomicBool>,
    cancel_before_cleanup: bool,
}

impl SummaryProvider for ScriptedSummaryProvider {
    type Error = NativeError;

    fn summarize<'a, 'c>(
        &'a self,
        request: ProviderRequest,
        _: CancellationToken<'c>,
    ) -> BoxFuture<'a, Result<SummaryResponse, NativeError>>
    where
        'c: 'a,
    {
        Box::pin(async move {
            self.trace.lock().unwrap().push("provider-start");
            self.requests.lock().unwrap().push(request);
            let result = self.replies.lock().unwrap().pop_front().unwrap();
            if self.cancel_before_cleanup {
                self.cancelled.store(true, Ordering::SeqCst);
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
    provider: ScriptedSummaryProvider,
    plan: CompactionPlan,
}

impl Fixture {
    fn new(replies: impl IntoIterator<Item = Result<SummaryResponse, NativeError>>) -> Self {
        let trace = Arc::new(Mutex::new(Vec::new()));
        Self {
            effects: RecordedEffects {
                trace: trace.clone(),
                fail_at: None,
            },
            provider: ScriptedSummaryProvider {
                trace,
                replies: Mutex::new(replies.into_iter().collect()),
                requests: Mutex::new(Vec::new()),
                cancelled: Arc::new(AtomicBool::new(false)),
                cancel_before_cleanup: false,
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
                instructions: String::new(),
                tools: Vec::new(),
                fixed_message: Message::default(),
                summary_instructions: "summarize".into(),
                summary_prefix: "summary: ".into(),
                model: "model".into(),
                reasoning: None,
            },
        }
    }

    fn run(&mut self) -> Result<Option<CompactionResult>, NativeError> {
        let is_cancelled = || self.provider.cancelled.load(Ordering::SeqCst);
        let cancelled = || -> BoxFuture<'_, ()> {
            Box::pin(poll_fn(|_| {
                if is_cancelled() {
                    Poll::Ready(())
                } else {
                    Poll::Pending
                }
            }))
        };
        finish(compact(
            Some(self.plan.clone()),
            &mut self.effects,
            &self.provider,
            CancellationToken::new(&is_cancelled, &cancelled),
        ))
    }
}

#[test]
fn native_effect_failures_keep_identity_at_each_compaction_boundary() {
    for step in ["progress-start", "progress-completed", "fit"] {
        let mut fixture = Fixture::new([Ok(SummaryResponse::Summary("summary".into()))]);
        fixture.effects.fail_at = Some(step);

        assert_eq!(fixture.run(), Err(NativeError::Effect(step)));
        assert_eq!(fixture.effects.trace.lock().unwrap().last(), Some(&step));
        assert_eq!(
            fixture.provider.requests.lock().unwrap().len(),
            usize::from(step != "progress-start")
        );
    }
}

#[test]
fn native_provider_failure_stops_before_completed_progress_or_fit() {
    let mut fixture = Fixture::new([Err(NativeError::Effect("provider"))]);

    assert_eq!(fixture.run(), Err(NativeError::Effect("provider")));
    assert_eq!(
        *fixture.effects.trace.lock().unwrap(),
        ["progress-start", "provider-start", "provider-closed"]
    );
}

#[test]
fn cancellation_during_a_summary_joins_provider_cleanup_before_returning() {
    let mut fixture = Fixture::new([Ok(SummaryResponse::Summary("summary".into()))]);
    fixture.provider.cancel_before_cleanup = true;

    assert_eq!(
        fixture.run(),
        Err(NativeError::Compaction(CompactionFailure::Cancelled))
    );
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
        Err(NativeError::Compaction(CompactionFailure::NoSummary(
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
