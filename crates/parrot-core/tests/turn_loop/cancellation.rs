use super::*;
use std::sync::{atomic::AtomicUsize, mpsc};

#[derive(Default)]
pub(super) struct CancellationSource {
    cancelled: AtomicBool,
    waker: Mutex<Option<Waker>>,
}

impl CancellationSource {
    pub(super) fn cancel(&self) {
        self.cancelled.store(true, Ordering::SeqCst);
        let waker = self.waker.lock().unwrap().take();
        if let Some(waker) = waker {
            waker.wake();
        }
    }

    pub(super) fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::SeqCst)
    }

    pub(super) fn cancelled(&self) -> BoxFuture<'_, ()> {
        Box::pin(std::future::poll_fn(|context| {
            let mut waker = self.waker.lock().unwrap();
            if self.is_cancelled() {
                Poll::Ready(())
            } else {
                *waker = Some(context.waker().clone());
                Poll::Pending
            }
        }))
    }
}

#[derive(Default)]
struct WakeCounter(AtomicUsize);

impl Wake for WakeCounter {
    fn wake(self: Arc<Self>) {
        self.0.fetch_add(1, Ordering::SeqCst);
    }
}

#[test]
fn copied_tokens_observe_live_and_prior_cancellation() {
    let source = CancellationSource::default();
    let is_cancelled = || source.is_cancelled();
    let cancelled = || source.cancelled();
    let token = CancellationToken::new(&is_cancelled, &cancelled);
    let copied = token;
    assert!(!token.is_cancelled());
    assert!(!copied.is_cancelled());
    source.cancel();
    assert!(token.is_cancelled());
    assert!(copied.is_cancelled());
    finish(token.cancelled());
    finish(copied.cancelled());
}

#[test]
fn token_notification_wakes_pending_work() {
    let source = CancellationSource::default();
    let is_cancelled = || source.is_cancelled();
    let cancelled = || source.cancelled();
    let token = CancellationToken::new(&is_cancelled, &cancelled);
    let wakes = Arc::new(WakeCounter::default());
    let waker = Waker::from(wakes.clone());
    let mut context = Context::from_waker(&waker);
    let mut waiting = token.cancelled();
    assert!(waiting.as_mut().poll(&mut context).is_pending());
    assert_eq!(wakes.0.load(Ordering::SeqCst), 0);
    source.cancel();
    assert_eq!(wakes.0.load(Ordering::SeqCst), 1);
    assert!(waiting.as_mut().poll(&mut context).is_ready());
}

struct CancellingCatalog {
    trace: Trace,
    cancellation: Arc<CancellationSource>,
}

impl Tools for CancellingCatalog {
    fn capture_catalog(&self) -> BoxFuture<'_, Result<ToolCatalog, Error>> {
        Box::pin(async move {
            self.trace.step("catalog", None)?;
            self.cancellation.cancel();
            Ok(ToolCatalog::default())
        })
    }

    fn execute<'a>(
        &'a self,
        _calls: Vec<ToolCall>,
        _receipt: ToolCallReceipt,
        _flags: TurnFlags,
        _cancellation: CancellationToken<'a>,
    ) -> BoxFuture<'a, Result<Vec<ToolResult>, Error>> {
        Box::pin(async { panic!("cancellation after initialization must prevent tool execution") })
    }
}

#[test]
fn cancellation_during_catalog_capture_stops_before_the_first_request_iteration() {
    let mut state = MemoryState::new([]);
    let provider = state.provider.clone();
    let tools = CancellingCatalog {
        trace: state.trace.clone(),
        cancellation: state.cancellation.clone(),
    };
    assert_eq!(state.run_with(&tools, &provider), Err(Error::cancelled()));
    assert_eq!(state.trace.values(), ["catalog"]);
    assert_eq!(state.turn_reads.load(Ordering::SeqCst), 1);
    assert_eq!(state.pending_messages.len(), 1);
    assert!(state.admissions.is_empty());
    assert!(provider.requests.lock().unwrap().is_empty());
    assert!(state.recorded_tools.is_empty());
    assert!(state.completion_feedback.is_empty());
}

struct JoiningTools {
    admitted: AtomicBool,
    joined: Arc<AtomicBool>,
}

impl Tools for JoiningTools {
    fn capture_catalog(&self) -> BoxFuture<'_, Result<ToolCatalog, Error>> {
        Box::pin(async { Ok(ToolCatalog::default()) })
    }

    fn execute<'a>(
        &'a self,
        calls: Vec<ToolCall>,
        _receipt: ToolCallReceipt,
        _flags: TurnFlags,
        cancellation: CancellationToken<'a>,
    ) -> BoxFuture<'a, Result<Vec<ToolResult>, Error>> {
        Box::pin(async move {
            assert_eq!(calls.len(), 1);
            assert!(!cancellation.is_cancelled());
            let (release, work) = mpsc::sync_channel(1);
            let joined = self.joined.clone();
            let worker = thread::spawn(move || {
                work.recv().unwrap();
                joined.store(true, Ordering::SeqCst);
            });
            self.admitted.store(true, Ordering::SeqCst);
            cancellation.cancelled().await;
            assert!(cancellation.is_cancelled());
            release.send(()).unwrap();
            worker.join().unwrap();
            Err(Error::cancelled())
        })
    }
}

#[test]
fn core_passes_live_cancellation_to_tools_and_awaits_their_owned_work() {
    let mut state = MemoryState::new([Reply::tools("call")]);
    let provider = state.provider.clone();
    let source = state.cancellation.clone();
    let is_cancelled = || source.is_cancelled();
    let cancelled = || source.cancelled();
    let token = CancellationToken::new(&is_cancelled, &cancelled);
    let joined = Arc::new(AtomicBool::new(false));
    let tools = JoiningTools {
        admitted: AtomicBool::new(false),
        joined: joined.clone(),
    };
    let wakes = Arc::new(WakeCounter::default());
    let waker = Waker::from(wakes.clone());
    let mut context = Context::from_waker(&waker);
    let options = state.capture_options();
    let mut budget = TurnBudget::new(state.unlimited);
    let result = {
        let mut preparation = state.preparation();
        let mut running = std::pin::pin!(run(
            &mut state,
            &mut preparation,
            &tools,
            &provider,
            &options,
            &mut budget,
            token
        ));
        assert!(running.as_mut().poll(&mut context).is_pending());
        assert!(tools.admitted.load(Ordering::SeqCst));
        assert!(!joined.load(Ordering::SeqCst));
        source.cancel();
        assert_eq!(wakes.0.load(Ordering::SeqCst), 1);
        running.as_mut().poll(&mut context)
    };
    assert_eq!(result, Poll::Ready(Err(Error::cancelled())));
    assert!(joined.load(Ordering::SeqCst));
    assert_eq!(state.recorded_tools.len(), 1);
    assert!(state.applied_results.is_empty());
    assert_eq!(provider.requests.lock().unwrap().len(), 1);
    assert_eq!(state.trace.values().last().unwrap(), "record_tools");
}

struct CancellingTools(Arc<CancellationSource>);

impl Tools for CancellingTools {
    fn capture_catalog(&self) -> BoxFuture<'_, Result<ToolCatalog, Error>> {
        Box::pin(async { Ok(ToolCatalog::default()) })
    }

    fn execute<'a>(
        &'a self,
        calls: Vec<ToolCall>,
        _receipt: ToolCallReceipt,
        _flags: TurnFlags,
        cancellation: CancellationToken<'a>,
    ) -> BoxFuture<'a, Result<Vec<ToolResult>, Error>> {
        Box::pin(async move {
            assert!(!cancellation.is_cancelled());
            self.0.cancel();
            assert!(cancellation.is_cancelled());
            Ok(calls
                .into_iter()
                .map(|call| ToolResult {
                    call_id: call.id,
                    tool_name: call.name,
                    status: ToolStatus::Cancelled,
                    text: "tool-owned cancellation result".into(),
                    ..ToolResult::default()
                })
                .collect())
        })
    }
}

#[test]
fn core_records_tool_owned_cancellation_results_without_rewriting_or_retrying_them() {
    let mut state = MemoryState::new([Reply::tools("call")]);
    let provider = state.provider.clone();
    let tools = CancellingTools(state.cancellation.clone());
    assert_eq!(state.run_with(&tools, &provider), Err(Error::cancelled()));
    assert_eq!(state.recorded_tools.len(), 1);
    assert_eq!(state.applied_results.len(), 1);
    assert_eq!(state.applied_results[0].call_id, "call-1");
    assert_eq!(state.applied_results[0].status, ToolStatus::Cancelled);
    assert_eq!(
        state.applied_results[0].text,
        "tool-owned cancellation result"
    );
    assert_eq!(provider.requests.lock().unwrap().len(), 1);
    assert_eq!(state.trace.values().last().unwrap(), "record_results");
}
