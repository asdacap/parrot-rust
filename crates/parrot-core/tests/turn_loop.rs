use parrot_core::{
    BoxFuture, CompletionDecision, Error, Outcome, Provider, ProviderError, Response, ResponseKind,
    RetryReason, State, Tools, budget::TurnRequestPhase, run,
};
use std::{
    collections::VecDeque,
    future::Future,
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
            Poll::Ready(output) => return output,
            Poll::Pending => thread::park(),
        }
    }
}

enum Reply {
    Response(ResponseKind, &'static str),
    TooLarge(&'static str),
    Failed(&'static str),
}
#[derive(Clone, Default)]
struct Trace(Arc<Mutex<Vec<String>>>);
impl Trace {
    fn step(&self, name: &'static str, fail_at: Option<&'static str>) -> Result<(), &'static str> {
        self.0.lock().unwrap().push(name.into());
        if fail_at == Some(name) {
            Err(name)
        } else {
            Ok(())
        }
    }
    fn values(&self) -> Vec<String> {
        self.0.lock().unwrap().clone()
    }
    fn clear(&self) {
        self.0.lock().unwrap().clear();
    }
}

/// Provides only tool-effect instrumentation, never turn state.
trait ToolEffects: Send {
    fn reconcile(&mut self) -> Result<(), &'static str>;
    fn execute(&mut self, snapshot: &[&'static str]) -> Result<(), &'static str>;
}
struct MemoryToolContext {
    trace: Trace,
    advertised: Vec<&'static str>,
    fail_at: Option<&'static str>,
}
impl ToolEffects for MemoryToolContext {
    fn reconcile(&mut self) -> Result<(), &'static str> {
        self.trace.step("reconcile", self.fail_at)
    }
    fn execute(&mut self, snapshot: &[&'static str]) -> Result<(), &'static str> {
        assert_eq!(self.advertised, snapshot);
        self.trace.step("execute", self.fail_at)
    }
}

/// Provides only scripted provider invocations, never turn state.
trait ProviderReplies: Send {
    fn begin_invocation(&mut self) -> Result<(), &'static str>;
    fn take_reply(&mut self) -> Reply;
}
struct MemoryProviderContext {
    trace: Trace,
    replies: VecDeque<Reply>,
    fail_at: Option<&'static str>,
}
impl ProviderReplies for MemoryProviderContext {
    fn begin_invocation(&mut self) -> Result<(), &'static str> {
        self.trace.step("invoke", self.fail_at)
    }
    fn take_reply(&mut self) -> Reply {
        self.replies
            .pop_front()
            .expect("unexpected provider request")
    }
}

struct MemoryState {
    trace: Trace,
    tools: MemoryToolContext,
    provider: MemoryProviderContext,
    decisions: VecDeque<CompletionDecision>,
    requests: Vec<(TurnRequestPhase, Vec<&'static str>)>,
    retries: Vec<(RetryReason, i32)>,
    work: bool,
    turns: usize,
    open: bool,
    scheduled: bool,
    cancelled: bool,
    interrupted: bool,
    unlimited: bool,
    maximum: i32,
    reduced: bool,
    clear_work_on_retry: bool,
    availability_restored: Arc<AtomicBool>,
    fail_at: Option<&'static str>,
}
impl MemoryState {
    fn new(replies: impl IntoIterator<Item = Reply>) -> Self {
        let trace = Trace::default();
        Self {
            trace: trace.clone(),
            tools: MemoryToolContext {
                trace: trace.clone(),
                advertised: Vec::new(),
                fail_at: None,
            },
            provider: MemoryProviderContext {
                trace,
                replies: replies.into_iter().collect(),
                fail_at: None,
            },
            decisions: VecDeque::new(),
            requests: Vec::new(),
            retries: Vec::new(),
            work: true,
            turns: 1,
            open: false,
            scheduled: false,
            cancelled: false,
            interrupted: false,
            unlimited: false,
            maximum: 10,
            reduced: true,
            clear_work_on_retry: false,
            availability_restored: Arc::new(AtomicBool::new(false)),
            fail_at: None,
        }
    }
    fn step(&mut self, name: &'static str) -> Result<(), &'static str> {
        self.trace.step(name, self.fail_at)
    }
    fn run(&mut self) -> Result<Outcome, Error<&'static str>> {
        self.tools.fail_at = self.fail_at;
        self.provider.fail_at = self.fail_at;
        let tools = MemoryTools {
            availability_restored: self.availability_restored.clone(),
        };
        finish(run(self, &tools, &MemoryProvider))
    }
}
impl State for MemoryState {
    type Selection = i32;
    type Request = ();
    type Response = &'static str;
    type ToolDefinition = &'static str;
    type Error = &'static str;
    type ToolContext = dyn ToolEffects;
    type ProviderContext = dyn ProviderReplies;

    fn borrow_tool_context(&mut self) -> &mut Self::ToolContext {
        &mut self.tools
    }
    fn borrow_provider_context(&mut self) -> &mut Self::ProviderContext {
        &mut self.provider
    }
    fn disables_request_limit(&self) -> bool {
        self.unlimited
    }
    fn is_turn_open(&self) -> bool {
        self.open
    }
    fn has_scheduled_work(&self) -> bool {
        self.scheduled
    }
    fn check_cancellation(&self) -> Result<(), Self::Error> {
        if self.cancelled {
            Err("cancelled")
        } else {
            Ok(())
        }
    }
    fn has_work(&self) -> BoxFuture<'_, Result<bool, Self::Error>> {
        Box::pin(async move { Ok(self.work || self.turns > 0) })
    }
    fn begin_turn(&mut self) -> BoxFuture<'_, Result<Self::Selection, Self::Error>> {
        Box::pin(async move {
            self.step("begin")?;
            self.open = true;
            self.work = true;
            self.availability_restored.store(false, Ordering::SeqCst);
            self.turns = self.turns.saturating_sub(1);
            Ok(self.maximum)
        })
    }
    fn restore_tool_availability(&mut self) -> BoxFuture<'_, Result<(), Self::Error>> {
        Box::pin(async move {
            self.step("restore")?;
            self.availability_restored.store(true, Ordering::SeqCst);
            Ok(())
        })
    }
    fn promote_input(&mut self) -> BoxFuture<'_, Result<(), Self::Error>> {
        Box::pin(async move { self.step("promote") })
    }
    fn maximum_requests(&self, selection: &Self::Selection) -> i32 {
        *selection
    }
    fn take_interruption(&mut self) -> bool {
        std::mem::take(&mut self.interrupted)
    }
    fn prepare_request<'a>(
        &'a mut self,
        _selection: &'a Self::Selection,
        definitions: &'a [Self::ToolDefinition],
        phase: TurnRequestPhase,
    ) -> BoxFuture<'a, Result<Self::Request, Self::Error>> {
        Box::pin(async move {
            self.step("prepare")?;
            self.requests.push((phase, definitions.to_vec()));
            self.tools.advertised = definitions.to_vec();
            Ok(())
        })
    }
    fn compact_request<'a>(
        &'a mut self,
        _selection: &'a Self::Selection,
        _definitions: &'a [Self::ToolDefinition],
    ) -> BoxFuture<'a, Result<bool, Self::Error>> {
        Box::pin(async move {
            self.step("compact")?;
            Ok(self.reduced)
        })
    }
    fn record_retry(
        &mut self,
        reason: RetryReason,
        attempt: i32,
    ) -> BoxFuture<'_, Result<(), Self::Error>> {
        Box::pin(async move {
            self.step("retry")?;
            self.retries.push((reason, attempt));
            Ok(())
        })
    }
    fn record_tool_calls<'a>(
        &'a mut self,
        _response: &'a Self::Response,
    ) -> BoxFuture<'a, Result<(), Self::Error>> {
        Box::pin(async move { self.step("record_tools") })
    }
    fn evaluate_completion<'a>(
        &'a mut self,
        _selection: &'a Self::Selection,
        _response: &'a Self::Response,
    ) -> BoxFuture<'a, Result<CompletionDecision, Self::Error>> {
        Box::pin(async move {
            self.step("evaluate")?;
            let decision = self
                .decisions
                .pop_front()
                .unwrap_or(CompletionDecision::Complete);
            if matches!(decision, CompletionDecision::Retry { .. }) && self.clear_work_on_retry {
                self.work = false;
            }
            Ok(decision)
        })
    }
    fn complete_turn<'a>(
        &'a mut self,
        response: &'a Self::Response,
    ) -> BoxFuture<'a, Result<String, Self::Error>> {
        Box::pin(async move {
            self.step("complete")?;
            self.open = false;
            self.work = false;
            Ok((*response).into())
        })
    }
    fn fail_runaway(&mut self) -> BoxFuture<'_, Result<String, Self::Error>> {
        Box::pin(async move {
            self.step("runaway")?;
            Ok("runaway".into())
        })
    }
}
struct MemoryTools {
    availability_restored: Arc<AtomicBool>,
}
impl Tools for MemoryTools {
    type Context = dyn ToolEffects;
    type Selection = i32;
    type Definition = &'static str;
    type Snapshot = Vec<&'static str>;
    type Error = &'static str;

    fn reconcile<'a>(
        &'a self,
        context: &'a mut Self::Context,
    ) -> BoxFuture<'a, Result<(), &'static str>> {
        Box::pin(async move { context.reconcile() })
    }
    fn snapshot(&self, _selection: &i32) -> Result<Self::Snapshot, &'static str> {
        assert!(!self.availability_restored.load(Ordering::SeqCst));
        Ok(vec!["read", "write"])
    }
    fn definitions<'a>(&self, snapshot: &'a Self::Snapshot) -> &'a [&'static str] {
        snapshot
    }
    fn restrict_for_final_phase(&self, _snapshot: &Self::Snapshot) -> Self::Snapshot {
        vec!["read"]
    }
    fn execute_pending<'a>(
        &'a self,
        context: &'a mut Self::Context,
        _selection: &'a i32,
        snapshot: &'a Self::Snapshot,
    ) -> BoxFuture<'a, Result<(), &'static str>> {
        Box::pin(async move { context.execute(snapshot) })
    }
}
struct MemoryProvider;
impl Provider for MemoryProvider {
    type Context = dyn ProviderReplies;
    type Request = ();
    type Response = &'static str;
    type Error = &'static str;

    fn invoke<'a>(
        &'a self,
        (): (),
        context: &'a mut Self::Context,
    ) -> BoxFuture<'a, Result<Response<&'static str>, ProviderError<&'static str>>> {
        Box::pin(async move {
            context.begin_invocation().map_err(ProviderError::Failed)?;
            match context.take_reply() {
                Reply::Response(kind, value) => Ok(Response { value, kind }),
                Reply::TooLarge(error) => Err(ProviderError::RequestTooLarge(error)),
                Reply::Failed(error) => Err(ProviderError::Failed(error)),
            }
        })
    }
}

#[test]
fn tools_are_recorded_and_executed_before_the_next_request_and_final_completion() {
    let mut state = MemoryState::new([
        Reply::Response(ResponseKind::ToolCalls, "call"),
        Reply::Response(ResponseKind::Final, "answer"),
    ]);
    assert_eq!(state.run(), Ok(Outcome::Completed("answer".into())));
    assert_eq!(
        state.trace.values(),
        [
            "reconcile",
            "begin",
            "restore",
            "promote",
            "prepare",
            "invoke",
            "record_tools",
            "execute",
            "reconcile",
            "promote",
            "prepare",
            "invoke",
            "evaluate",
            "complete",
            "reconcile",
        ]
    );
}

#[test]
fn invalid_and_truncated_tool_responses_retry_without_recording_or_executing_calls() {
    let mut state = MemoryState::new([
        Reply::Response(ResponseKind::TruncatedToolCalls, "cut"),
        Reply::Response(ResponseKind::InvalidToolCalls, "bad"),
        Reply::Response(ResponseKind::Final, "answer"),
    ]);
    assert_eq!(state.run(), Ok(Outcome::Completed("answer".into())));
    assert_eq!(
        state.retries,
        [
            (RetryReason::TruncatedToolCalls, 1),
            (RetryReason::InvalidToolCalls, 2),
        ]
    );
    assert!(
        !state
            .trace
            .values()
            .iter()
            .any(|entry| entry == "record_tools" || entry == "execute")
    );
}

#[test]
fn maximum_requests_restricts_final_tools_and_bounds_settlement_extensions() {
    let mut state =
        MemoryState::new((0..5).map(|_| Reply::Response(ResponseKind::ToolCalls, "call")));
    state.maximum = 1;
    assert_eq!(state.run(), Ok(Outcome::Failed("runaway".into())));
    assert_eq!(state.requests.len(), 5);
    assert!(
        state
            .requests
            .iter()
            .all(|(phase, tools)| phase.final_phase && tools == &["read"])
    );
    assert!(state.requests[0].0.enters_final_phase);
    assert!(
        state.requests[1..]
            .iter()
            .all(|(phase, _)| !phase.enters_final_phase)
    );
    assert_eq!(
        state
            .trace
            .values()
            .iter()
            .filter(|entry| *entry == "execute")
            .count(),
        5
    );
    assert_eq!(state.trace.values().last().unwrap(), "runaway");
}

#[test]
fn malformed_final_response_exhausts_the_request_limit_without_a_settlement_extension() {
    let mut state = MemoryState::new([Reply::Response(ResponseKind::InvalidToolCalls, "bad")]);
    state.maximum = 1;
    assert_eq!(state.run(), Ok(Outcome::Failed("runaway".into())));
    assert_eq!(state.requests.len(), 1);
}

#[test]
fn interruption_enters_final_phase_even_when_request_limits_are_disabled() {
    let mut state = MemoryState::new([Reply::Response(ResponseKind::Final, "answer")]);
    state.unlimited = true;
    state.interrupted = true;
    assert_eq!(state.run(), Ok(Outcome::Completed("answer".into())));
    let (phase, tools) = &state.requests[0];
    assert!(phase.final_phase && phase.enters_final_phase && phase.interrupted);
    assert_eq!(tools, &["read"]);
}

#[test]
fn completion_retry_resets_budget_and_keeps_pending_work_alive() {
    let mut state = MemoryState::new([
        Reply::Response(ResponseKind::Final, "candidate"),
        Reply::Response(ResponseKind::InvalidToolCalls, "bad"),
        Reply::Response(ResponseKind::Final, "answer"),
    ]);
    state.maximum = 2;
    state.clear_work_on_retry = true;
    state.decisions.push_back(CompletionDecision::Retry {
        pending: Some(true),
    });
    assert_eq!(state.run(), Ok(Outcome::Completed("answer".into())));
    assert_eq!(state.retries, [(RetryReason::InvalidToolCalls, 1)]);
    assert_eq!(
        state
            .trace
            .values()
            .iter()
            .filter(|entry| *entry == "begin")
            .count(),
        1
    );
    assert_eq!(
        state
            .trace
            .values()
            .iter()
            .filter(|entry| *entry == "complete")
            .count(),
        1
    );
    assert!(!state.requests[1].0.final_phase);
    assert!(state.requests[2].0.final_phase);
}

#[test]
fn completion_retry_can_clear_pending_and_stop_without_closing_the_candidate() {
    let mut state = MemoryState::new([
        Reply::Response(ResponseKind::Final, "candidate"),
        Reply::Response(ResponseKind::Final, "candidate2"),
    ]);
    state.clear_work_on_retry = true;
    state.decisions.extend([
        CompletionDecision::Retry {
            pending: Some(true),
        },
        CompletionDecision::Retry {
            pending: Some(false),
        },
    ]);
    assert_eq!(state.run(), Ok(Outcome::Completed(String::new())));
    assert_eq!(state.requests.len(), 2);
    assert!(!state.trace.values().iter().any(|entry| entry == "complete"));
}

#[test]
fn completion_retry_without_a_pending_override_retains_the_existing_pending_flag() {
    let mut state = MemoryState::new([
        Reply::Response(ResponseKind::Final, "candidate"),
        Reply::Response(ResponseKind::Final, "candidate2"),
        Reply::Response(ResponseKind::Final, "answer"),
    ]);
    state.clear_work_on_retry = true;
    state.decisions.extend([
        CompletionDecision::Retry {
            pending: Some(true),
        },
        CompletionDecision::Retry { pending: None },
    ]);
    assert_eq!(state.run(), Ok(Outcome::Completed("answer".into())));
    assert_eq!(state.requests.len(), 3);
}

#[test]
fn oversized_requests_compact_once_until_a_successful_response_resets_the_guard() {
    let mut state = MemoryState::new([
        Reply::TooLarge("first"),
        Reply::Response(ResponseKind::ToolCalls, "call"),
        Reply::TooLarge("second"),
        Reply::Response(ResponseKind::Final, "answer"),
    ]);
    assert_eq!(state.run(), Ok(Outcome::Completed("answer".into())));
    assert_eq!(
        state
            .trace
            .values()
            .iter()
            .filter(|entry| *entry == "compact")
            .count(),
        2
    );
    assert_eq!(state.requests.len(), 4);
}

#[test]
fn a_second_oversized_failure_propagates_without_repeating_compaction() {
    let mut state = MemoryState::new([Reply::TooLarge("first"), Reply::TooLarge("second")]);
    assert_eq!(state.run(), Err(Error::Adapter("second")));
    assert_eq!(
        state
            .trace
            .values()
            .iter()
            .filter(|entry| *entry == "compact")
            .count(),
        1
    );
    assert_eq!(state.trace.values().last().unwrap(), "invoke");
}

#[test]
fn compaction_that_does_not_reduce_context_returns_the_original_provider_error() {
    let mut state = MemoryState::new([Reply::TooLarge("original")]);
    state.reduced = false;
    assert_eq!(state.run(), Err(Error::Adapter("original")));
    assert_eq!(state.requests.len(), 1);
    assert_eq!(state.trace.values().last().unwrap(), "compact");
}

#[test]
fn scheduled_work_yields_before_any_effect_but_not_during_an_open_turn() {
    let mut state = MemoryState::new([]);
    state.scheduled = true;
    assert_eq!(state.run(), Ok(Outcome::Completed(String::new())));
    assert!(state.trace.values().is_empty());
    state.open = true;
    assert_eq!(state.run(), Err(Error::MissingTurn));
    assert_eq!(state.trace.values(), ["reconcile", "promote"]);
}

#[test]
fn no_work_and_cancellation_never_open_a_turn_or_invoke_the_provider() {
    let mut state = MemoryState::new([]);
    state.turns = 0;
    state.work = false;
    assert_eq!(state.run(), Ok(Outcome::Completed(String::new())));
    assert_eq!(state.trace.values(), ["reconcile"]);
    state.trace.clear();
    state.cancelled = true;
    assert_eq!(state.run(), Err(Error::Adapter("cancelled")));
    assert_eq!(state.trace.values(), ["reconcile"]);
}

#[test]
fn provider_failure_propagates_without_recording_or_completing_a_response() {
    let mut state = MemoryState::new([Reply::Failed("provider")]);
    assert_eq!(state.run(), Err(Error::Adapter("provider")));
    assert_eq!(state.trace.values().last().unwrap(), "invoke");
}

#[test]
fn adapter_errors_stop_at_the_failing_effect() {
    for (stage, kind) in [
        ("reconcile", ResponseKind::Final),
        ("begin", ResponseKind::Final),
        ("restore", ResponseKind::Final),
        ("promote", ResponseKind::Final),
        ("prepare", ResponseKind::Final),
        ("invoke", ResponseKind::Final),
        ("retry", ResponseKind::InvalidToolCalls),
        ("record_tools", ResponseKind::ToolCalls),
        ("execute", ResponseKind::ToolCalls),
        ("evaluate", ResponseKind::Final),
        ("complete", ResponseKind::Final),
    ] {
        let mut state = MemoryState::new([Reply::Response(kind, "response")]);
        state.fail_at = Some(stage);
        assert_eq!(state.run(), Err(Error::Adapter(stage)), "{stage}");
        assert_eq!(state.trace.values().last().unwrap(), stage);
    }
    let mut state = MemoryState::new([Reply::TooLarge("original")]);
    state.fail_at = Some("compact");
    assert_eq!(state.run(), Err(Error::Adapter("compact")));
    assert_eq!(state.trace.values().last().unwrap(), "compact");
    let mut state = MemoryState::new([]);
    state.maximum = 0;
    state.fail_at = Some("runaway");
    assert_eq!(state.run(), Err(Error::Adapter("runaway")));
    assert_eq!(state.trace.values().last().unwrap(), "runaway");
}

#[test]
fn new_turns_reset_request_count_but_retain_final_phase_and_settlement_extensions() {
    let mut state = MemoryState::new([
        Reply::Response(ResponseKind::ToolCalls, "call1"),
        Reply::Response(ResponseKind::Final, "answer1"),
        Reply::Response(ResponseKind::ToolCalls, "call2"),
        Reply::Response(ResponseKind::ToolCalls, "call3"),
        Reply::Response(ResponseKind::ToolCalls, "call4"),
        Reply::Response(ResponseKind::ToolCalls, "call5"),
    ]);
    state.maximum = 1;
    state.turns = 2;
    assert_eq!(state.run(), Ok(Outcome::Failed("runaway".into())));
    assert_eq!(state.requests.len(), 6);
    assert!(state.requests[0].0.enters_final_phase);
    assert!(
        state.requests[1..]
            .iter()
            .all(|(phase, _)| !phase.enters_final_phase)
    );
    assert_eq!(
        state
            .trace
            .values()
            .iter()
            .filter(|entry| *entry == "begin")
            .count(),
        2
    );
    assert_eq!(
        state
            .trace
            .values()
            .iter()
            .filter(|entry| *entry == "execute")
            .count(),
        5
    );
}

#[test]
fn completion_retry_resets_interruption_final_phase_and_settlement_extensions() {
    let mut state = MemoryState::new([
        Reply::Response(ResponseKind::ToolCalls, "call1"),
        Reply::Response(ResponseKind::Final, "candidate"),
        Reply::Response(ResponseKind::ToolCalls, "call2"),
        Reply::Response(ResponseKind::ToolCalls, "call3"),
        Reply::Response(ResponseKind::ToolCalls, "call4"),
        Reply::Response(ResponseKind::ToolCalls, "call5"),
        Reply::Response(ResponseKind::ToolCalls, "call6"),
        Reply::Response(ResponseKind::ToolCalls, "call7"),
    ]);
    state.maximum = 2;
    state.interrupted = true;
    state
        .decisions
        .push_back(CompletionDecision::Retry { pending: None });
    assert_eq!(state.run(), Ok(Outcome::Failed("runaway".into())));
    assert!(!state.requests[2].0.final_phase);
    assert!(!state.requests[2].0.interrupted);
    assert!(state.requests[3].0.enters_final_phase);
    assert_eq!(
        state
            .trace
            .values()
            .iter()
            .filter(|entry| *entry == "execute")
            .count(),
        7
    );
}

#[test]
fn tools_and_provider_operate_with_standalone_unsized_capabilities_without_state() {
    let trace = Trace::default();
    let tools = MemoryTools {
        availability_restored: Arc::new(AtomicBool::new(false)),
    };
    let selection = 3;
    let snapshot = tools.snapshot(&selection).unwrap();
    let mut tool_context = MemoryToolContext {
        trace: trace.clone(),
        advertised: snapshot.clone(),
        fail_at: None,
    };
    let capability: &mut dyn ToolEffects = &mut tool_context;
    finish(tools.reconcile(capability)).unwrap();
    finish(tools.execute_pending(capability, &selection, &snapshot)).unwrap();
    assert_eq!(tools.definitions(&snapshot), &["read", "write"]);
    assert_eq!(tools.restrict_for_final_phase(&snapshot), ["read"]);

    let mut provider_context = MemoryProviderContext {
        trace: trace.clone(),
        replies: VecDeque::from([Reply::Response(ResponseKind::Final, "standalone")]),
        fail_at: None,
    };
    let capability: &mut dyn ProviderReplies = &mut provider_context;
    let response = finish(MemoryProvider.invoke((), capability)).unwrap();
    assert_eq!(response.kind, ResponseKind::Final);
    assert_eq!(response.value, "standalone");
    assert!(provider_context.replies.is_empty());
    assert_eq!(trace.values(), ["reconcile", "execute", "invoke"]);
}
