use super::*;

struct WaitingProvider {
    estimates: MemoryProvider,
    entered: AtomicBool,
    cleaned: AtomicBool,
}

impl Provider for WaitingProvider {
    fn estimate_input_tokens(
        &self,
        instructions: &str,
        definitions: &[ToolDefinition],
        messages: &[Message],
    ) -> Result<i64, Error> {
        self.estimates
            .estimate_input_tokens(instructions, definitions, messages)
    }

    fn invoke<'a, 'c>(
        &'a self,
        _request: ProviderRequest,
        cancellation: CancellationToken<'c>,
    ) -> BoxFuture<'a, Result<ProviderResponse, parrot_core::ProviderInvokeError>>
    where
        'c: 'a,
    {
        Box::pin(async move {
            self.entered.store(true, Ordering::SeqCst);
            cancellation.cancelled().await;
            let mut yielded = false;
            std::future::poll_fn(|context| {
                if yielded {
                    Poll::Ready(())
                } else {
                    yielded = true;
                    context.waker().wake_by_ref();
                    Poll::Pending
                }
            })
            .await;
            self.cleaned.store(true, Ordering::SeqCst);
            Err(Error::cancelled().into())
        })
    }
}

#[test]
fn entry_cancellation_reaches_inflight_provider_and_awaits_cleanup() {
    let mut state = MemoryState::new([]);
    let source = CancellationSource::default();
    let tools = state.tools.clone();
    let provider = WaitingProvider {
        estimates: state.provider.clone(),
        entered: AtomicBool::new(false),
        cleaned: AtomicBool::new(false),
    };
    let waker = Waker::from(Arc::new(ThreadWake(thread::current())));
    let mut context = Context::from_waker(&waker);
    state.budget = AgentState::default();
    let mut running = std::pin::pin!(run(
        &mut state,
        &tools,
        &provider,
        CancellationToken::new(&source),
    ));
    assert!(running.as_mut().poll(&mut context).is_pending());
    assert!(provider.entered.load(Ordering::SeqCst));
    source.cancel();
    assert!(running.as_mut().poll(&mut context).is_pending());
    assert!(!provider.cleaned.load(Ordering::SeqCst));
    assert_eq!(
        running.as_mut().poll(&mut context),
        Poll::Ready(Err(Error::cancelled()))
    );
    assert!(provider.cleaned.load(Ordering::SeqCst));
    assert!(!provider.estimates.cancellation.is_cancelled());
}

#[test]
fn precancelled_entry_does_not_start_provider_work() {
    let mut state = MemoryState::new([Reply::final_response("unreachable")]);
    let source = CancellationSource::default();
    source.cancel();
    let tools = state.tools.clone();
    let provider = state.provider.clone();
    assert_eq!(
        finish(run(
            &mut state,
            &tools,
            &provider,
            CancellationToken::new(&source),
        )),
        Err(Error::cancelled())
    );
    assert!(provider.requests.lock().unwrap().is_empty());
    assert!(state.trace.values().is_empty());
}
