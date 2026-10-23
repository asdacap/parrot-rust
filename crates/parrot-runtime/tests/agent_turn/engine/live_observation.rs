use super::*;
use parrot_runtime::agent_turn::lifecycle::DrainBackend;
use std::sync::Weak;

fn estimate(
    fixture: &Fixture,
    selection: &TurnSelection<Security>,
    request: &Request,
) -> ContextSnapshot {
    fixture
        .controller()
        .compactor()
        .planner()
        .estimate_selected_context(
            &selection.model,
            &request.instructions,
            &request.tools,
            &request.messages,
        )
        .unwrap()
}

#[tokio::test]
async fn live_context_observes_current_selection_and_retained_catalog_during_provider_stream() {
    let fixture = Fixture::new();
    let gate = Gate::new();
    let mut step = Step::complete("answer");
    step.gate = Some(gate.clone());
    let provider = Scripted::new(vec![step]);
    let tools = Arc::new(Tools {
        definitions: vec![sessions::Tool {
            name: "read".into(),
            description: "real retained definition".into(),
            parameters_json: "{\"type\":\"object\"}".into(),
        }],
        ..Tools::default()
    });
    let runtime = fixture
        .runtime(
            provider.clone(),
            tools.clone(),
            vec![],
            Arc::new(Signal::new()),
            0,
        )
        .await;
    let sent = runtime
        .send("question", protocol::Delivery::DELIVERY_STEER)
        .await;
    gate.entered().await;
    let backend = runtime.session.backend();
    let first = bounded(backend.observe_context(&NeverCancelled))
        .await
        .unwrap();
    let mut request = provider.requests.lock().unwrap()[0].clone();
    assert_eq!(
        first.context.estimated_tokens,
        estimate(&fixture, &first.selection, &request).estimated_tokens
    );
    assert_eq!(
        backend
            .advertised_tool_definitions(&first.selection)
            .unwrap(),
        tools.definitions
    );
    let mut changed = backend.current_selection().unwrap();
    changed.profile.id = "changed while streaming".into();
    changed.profile.disabled_tools.push("read".into());
    backend.update_selection(changed).unwrap();
    let second = bounded(backend.observe_context(&NeverCancelled))
        .await
        .unwrap();
    assert_eq!(second.selection.profile.id, "changed while streaming");
    assert!(
        backend
            .advertised_tool_definitions(&second.selection)
            .unwrap()
            .is_empty()
    );
    request.tools.clear();
    assert_eq!(
        second.context.estimated_tokens,
        estimate(&fixture, &second.selection, &request).estimated_tokens
    );
    assert_eq!(tools.materializations.load(Ordering::SeqCst), 1);
    assert!(!sent.execution.is_finished());
    gate.open();
    bounded(sent.execution.join()).await.unwrap();
    runtime.close().await;
}

#[tokio::test]
async fn live_context_does_not_wait_for_held_tool_reconciliation() {
    let fixture = Fixture::new();
    let gate = Gate::new();
    let tools = Arc::new(Tools {
        reconcile_gate: Mutex::new(Some(gate.clone())),
        ..Tools::default()
    });
    let runtime = fixture
        .runtime(
            Scripted::new(vec![Step::complete("done")]),
            tools,
            vec![],
            Arc::new(Signal::new()),
            0,
        )
        .await;
    let sent = runtime
        .send("question", protocol::Delivery::DELIVERY_STEER)
        .await;
    gate.entered().await;
    let observed = bounded(runtime.session.backend().observe_context(&NeverCancelled))
        .await
        .unwrap();
    assert!(observed.context.estimated_tokens > 0);
    assert!(!sent.execution.is_finished());
    gate.open();
    bounded(sent.execution.join()).await.unwrap();
    runtime.close().await;
}

struct RetryOnce(AtomicBool);
impl TurnCompletion<Security> for RetryOnce {
    fn complete<'a>(
        &'a self,
        _: &'a CompletionCandidate<Security>,
        _: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<CompletionDecision, TurnError>> {
        Box::pin(async move {
            Ok(if self.0.swap(false, Ordering::SeqCst) {
                CompletionDecision::Retry {
                    system_message: "in-memory-only retry instructions".into(),
                    retain_candidate_assistant: true,
                    completion_retry_pending: Some(true),
                }
            } else {
                CompletionDecision::Complete
            })
        })
    }
}
#[tokio::test]
async fn live_context_includes_retry_messages_not_yet_in_sqlite() {
    let fixture = Fixture::new();
    let gate = Gate::new();
    let mut second = Step::complete("final");
    second.gate = Some(gate.clone());
    let provider = Scripted::new(vec![Step::complete("uncommitted candidate"), second]);
    let runtime = fixture
        .runtime(
            provider.clone(),
            Arc::new(Tools::default()),
            vec![Arc::new(RetryOnce(AtomicBool::new(true)))],
            Arc::new(Signal::new()),
            0,
        )
        .await;
    let sent = runtime
        .send("question", protocol::Delivery::DELIVERY_STEER)
        .await;
    gate.entered().await;
    let observed = bounded(runtime.session.backend().observe_context(&NeverCancelled))
        .await
        .unwrap();
    let request = provider.requests.lock().unwrap()[1].clone();
    assert!(
        request
            .messages
            .iter()
            .any(|m| m.content() == "uncommitted candidate")
    );
    assert!(
        request
            .messages
            .iter()
            .any(|m| m.content() == "in-memory-only retry instructions")
    );
    let durable = fixture.conversation().await.messages();
    assert!(
        !durable
            .iter()
            .any(|m| m.content() == "uncommitted candidate"
                || m.content() == "in-memory-only retry instructions")
    );
    assert_eq!(
        observed.context.estimated_tokens,
        estimate(&fixture, &observed.selection, &request).estimated_tokens
    );
    let mut stale = request.clone();
    stale.messages = durable.iter().map(provider_message).collect();
    assert_ne!(
        observed.context.estimated_tokens,
        estimate(&fixture, &observed.selection, &stale).estimated_tokens
    );
    gate.open();
    bounded(sent.execution.join()).await.unwrap();
    runtime.close().await;
}

fn sampled_instructions(epoch: usize) -> String {
    format!("sampled live instructions {}", "epoch ".repeat(epoch * 8))
}

struct GatedPrompt {
    gates: VecDeque<Option<Arc<Gate>>>,
    sampled: Arc<AtomicUsize>,
}
impl SystemPrompt for GatedPrompt {
    fn renew_epoch(&mut self) {}
    fn sample_epoch<'a>(
        &'a mut self,
        cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, PromptResult<()>> {
        Box::pin(async move {
            if let Some(Some(gate)) = self.gates.pop_front() {
                gate.wait(cancellation)
                    .await
                    .map_err(|_| parrot_runtime::prompt_context::PromptError::Cancelled)?;
            }
            cancellation
                .check()
                .map_err(|_| parrot_runtime::prompt_context::PromptError::Cancelled)?;
            self.sampled.fetch_add(1, Ordering::SeqCst);
            Ok(())
        })
    }
    fn build(
        &mut self,
        _: &parrot_provider::routing::aliases::ResolvedModelSelection,
    ) -> PromptResult<String> {
        assert!(self.sampled.load(Ordering::SeqCst) > 0);
        Ok(sampled_instructions(self.sampled.load(Ordering::SeqCst)))
    }
}

#[tokio::test]
async fn live_context_waits_only_for_initial_sampling_and_waiter_cancel_does_not_cancel_sampler() {
    let fixture = Fixture::new();
    let gate = Gate::new();
    let sampled = Arc::new(AtomicUsize::new(0));
    let runtime = fixture
        .runtime_with_overrides(
            Scripted::new(vec![]),
            Arc::new(Tools::default()),
            vec![],
            Arc::new(Signal::new()),
            0,
            RuntimeOverrides {
                prompt: Some(Box::new(GatedPrompt {
                    gates: VecDeque::from([Some(gate.clone())]),
                    sampled: sampled.clone(),
                })),
                status: None,
                ..RuntimeOverrides::default()
            },
        )
        .await;
    let backend = runtime.session.backend().clone();
    let first = tokio::spawn(async move { backend.observe_context(&NeverCancelled).await });
    gate.entered().await;
    let cancellation = Arc::new(Signal::new());
    let backend = runtime.session.backend().clone();
    let cancel = cancellation.clone();
    let waiter = tokio::spawn(async move { backend.observe_context(cancel.as_ref()).await });
    tokio::task::yield_now().await;
    assert!(!first.is_finished());
    assert!(!waiter.is_finished());
    cancellation.cancel();
    assert!(bounded(waiter).await.unwrap().err().unwrap().is_cancelled());
    assert!(!first.is_finished());
    assert_eq!(sampled.load(Ordering::SeqCst), 0);
    gate.open();
    bounded(first).await.unwrap().unwrap();
    bounded(runtime.session.backend().observe_context(&NeverCancelled))
        .await
        .unwrap();
    assert_eq!(sampled.load(Ordering::SeqCst), 1);
    runtime.close().await;
}

#[tokio::test]
async fn live_context_cancelled_initial_sampler_does_not_mark_epoch_sampled() {
    let fixture = Fixture::new();
    let gate = Gate::new();
    let sampled = Arc::new(AtomicUsize::new(0));
    let runtime = fixture
        .runtime_with_overrides(
            Scripted::new(vec![]),
            Arc::new(Tools::default()),
            vec![],
            Arc::new(Signal::new()),
            0,
            RuntimeOverrides {
                prompt: Some(Box::new(GatedPrompt {
                    gates: VecDeque::from([Some(gate.clone())]),
                    sampled: sampled.clone(),
                })),
                status: None,
                ..RuntimeOverrides::default()
            },
        )
        .await;
    let cancellation = Arc::new(Signal::new());
    let backend = runtime.session.backend().clone();
    let cancel = cancellation.clone();
    let first = tokio::spawn(async move { backend.observe_context(cancel.as_ref()).await });
    gate.entered().await;
    cancellation.cancel();
    assert!(bounded(first).await.unwrap().err().unwrap().is_cancelled());
    assert_eq!(sampled.load(Ordering::SeqCst), 0);
    bounded(runtime.session.backend().observe_context(&NeverCancelled))
        .await
        .unwrap();
    assert_eq!(sampled.load(Ordering::SeqCst), 1);
    runtime.close().await;
}

#[derive(Default)]
struct ReentrantStatus {
    backend: Mutex<Option<Weak<Backend>>>,
    calls: AtomicUsize,
}
impl ReentrantStatus {
    async fn capture(&self, cancellation: &dyn Cancellation) -> ContextResult<()> {
        let backend = self
            .backend
            .lock()
            .unwrap()
            .as_ref()
            .unwrap()
            .upgrade()
            .unwrap();
        backend
            .observe_context(cancellation)
            .await
            .map_err(|error| ContextError::Operation(error.to_string()))?;
        self.calls.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }
}
impl ContextStatus for ReentrantStatus {
    fn observe<'a>(
        &'a self,
        _: &'a ContextSnapshot,
        cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, ContextResult<String>> {
        Box::pin(async move {
            self.capture(cancellation).await?;
            Ok("actual reentrant status".into())
        })
    }
    fn observe_fitted<'a>(
        &'a self,
        estimate: &'a (dyn Fn(&str) -> ContextResult<ContextSnapshot> + Send + Sync),
        cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, ContextResult<String>> {
        Box::pin(async move {
            self.capture(cancellation).await?;
            estimate("actual reentrant status")?;
            Ok("actual reentrant status".into())
        })
    }
}
struct ReentrantBinding(Arc<ReentrantStatus>);
impl TurnStatus<Security> for ReentrantBinding {
    fn bind(&self, _: &TurnSelection<Security>) -> Arc<dyn ContextStatus> {
        self.0.clone()
    }
}

#[tokio::test]
async fn live_context_resample_wait_then_summary_and_reentrant_status_are_unlocked_until_atomic_reload()
 {
    let fixture = Fixture::new();
    fixture.seed().await;
    let sample_gate = Gate::new();
    let summary_gate = Gate::new();
    let sampled = Arc::new(AtomicUsize::new(0));
    let status = Arc::new(ReentrantStatus::default());
    let mut step = Step::complete("summary after live sampling");
    step.gate = Some(summary_gate.clone());
    let runtime = fixture
        .runtime_with_overrides(
            Scripted::new(vec![step]),
            Arc::new(Tools::default()),
            vec![],
            Arc::new(Signal::new()),
            0,
            RuntimeOverrides {
                prompt: Some(Box::new(GatedPrompt {
                    gates: VecDeque::from([None, Some(sample_gate.clone())]),
                    sampled: sampled.clone(),
                })),
                status: Some(Arc::new(ReentrantBinding(status.clone()))),
                ..RuntimeOverrides::default()
            },
        )
        .await;
    *status.backend.lock().unwrap() = Some(Arc::downgrade(runtime.session.backend()));
    let before = bounded(runtime.session.backend().observe_context(&NeverCancelled))
        .await
        .unwrap();
    let session = runtime.session.clone();
    let compact = tokio::spawn(async move {
        session
            .compact_context(None, Arc::new(NeverCancelled))
            .await
    });
    sample_gate.entered().await;
    let caller = Arc::new(Signal::new());
    let backend = runtime.session.backend().clone();
    let cancel = caller.clone();
    let waiter = tokio::spawn(async move { backend.observe_context(cancel.as_ref()).await });
    tokio::task::yield_now().await;
    assert!(!waiter.is_finished());
    caller.cancel();
    assert!(bounded(waiter).await.unwrap().err().unwrap().is_cancelled());
    sample_gate.open();
    summary_gate.entered().await;
    assert_eq!(sampled.load(Ordering::SeqCst), 2);
    assert!(status.calls.load(Ordering::SeqCst) > 0);
    let during = bounded(runtime.session.backend().observe_context(&NeverCancelled))
        .await
        .unwrap();
    assert!(during.context.estimated_tokens > before.context.estimated_tokens);
    let old_messages_new_epoch = Request {
        instructions: sampled_instructions(2),
        messages: fixture
            .conversation()
            .await
            .messages()
            .iter()
            .map(provider_message)
            .collect(),
        ..Request::default()
    };
    assert_eq!(
        during.context.estimated_tokens,
        estimate(&fixture, &during.selection, &old_messages_new_epoch).estimated_tokens
    );
    assert!(!compact.is_finished());
    summary_gate.open();
    assert!(bounded(compact).await.unwrap().unwrap().reduced);
    let after = bounded(runtime.session.backend().observe_context(&NeverCancelled))
        .await
        .unwrap();
    assert!(after.context.estimated_tokens < before.context.estimated_tokens);
    let request = Request {
        instructions: sampled_instructions(2),
        messages: fixture
            .conversation()
            .await
            .messages()
            .iter()
            .map(provider_message)
            .collect(),
        ..Request::default()
    };
    assert_eq!(
        after.context.estimated_tokens,
        estimate(&fixture, &after.selection, &request).estimated_tokens
    );
    assert!(status.calls.load(Ordering::SeqCst) >= 2);
    runtime.close().await;
}
