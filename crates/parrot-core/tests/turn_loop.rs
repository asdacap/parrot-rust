use parrot_core::{
    BoxFuture, CompletionDecision, Error, ErrorCode, ErrorKind, Outcome, Provider, ProviderRequest,
    ProviderResponse, RetryReason, States, ToolCall, ToolCatalog, ToolContext, ToolDefinition,
    ToolPhase, Tools, TurnOptions, budget::TurnRequestPhase, run,
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

#[derive(Clone)]
enum Reply {
    Response(ProviderResponse),
    TooLarge(&'static str),
    Failed(&'static str),
    Error(Error),
}
impl Reply {
    fn final_response(text: &str) -> Self {
        Self::Response(ProviderResponse {
            assistant_text: text.into(),
            finish_reason: "stop".into(),
            ..ProviderResponse::default()
        })
    }
    fn tools(text: &str) -> Self {
        Self::Response(ProviderResponse {
            assistant_text: text.into(),
            finish_reason: "tool_calls".into(),
            tool_calls: vec![ToolCall {
                id: "call-1".into(),
                name: "read".into(),
                arguments_json: "{}".into(),
            }],
            ..ProviderResponse::default()
        })
    }
    fn invalid(text: &str) -> Self {
        let Self::Response(mut response) = Self::tools(text) else {
            unreachable!()
        };
        response.tool_calls[0].arguments_json = "{".into();
        Self::Response(response)
    }
    fn truncated(text: &str) -> Self {
        let Self::Response(mut response) = Self::invalid(text) else {
            unreachable!()
        };
        response.finish_reason = "length".into();
        Self::Response(response)
    }
}

#[derive(Clone, Default)]
struct Trace(Arc<Mutex<Vec<String>>>);
impl Trace {
    fn step(&self, name: &'static str, fail_at: Option<&'static str>) -> Result<(), Error> {
        self.0.lock().unwrap().push(name.into());
        if fail_at == Some(name) {
            Err(Error::failed(name))
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

#[derive(Clone)]
struct MemoryTools {
    trace: Trace,
    advertised: Arc<Mutex<Vec<String>>>,
    availability_restored: Arc<AtomicBool>,
    fail_at: Arc<Mutex<Option<&'static str>>>,
    phases: Arc<Mutex<Vec<ToolPhase>>>,
}
impl Tools for MemoryTools {
    fn reconcile_pending(&self) -> BoxFuture<'_, Result<(), Error>> {
        Box::pin(async move { self.trace.step("reconcile", *self.fail_at.lock().unwrap()) })
    }
    fn capture_catalog(&self) -> BoxFuture<'_, Result<ToolCatalog, Error>> {
        Box::pin(async move {
            assert!(!self.availability_restored.load(Ordering::SeqCst));
            let read = ToolDefinition {
                name: "read".into(),
                description: "read a value".into(),
                parameters_json: "{}".into(),
            };
            let write = ToolDefinition {
                name: "write".into(),
                description: "write a value".into(),
                parameters_json: "{}".into(),
            };
            Ok(ToolCatalog {
                normal: vec![read.clone(), write],
                final_phase: vec![read],
            })
        })
    }
    fn execute_pending(&self, context: ToolContext) -> BoxFuture<'_, Result<(), Error>> {
        Box::pin(async move {
            let expected: &[&str] = match context.phase {
                ToolPhase::Normal => &["read", "write"],
                ToolPhase::Final => &["read"],
            };
            assert_eq!(*self.advertised.lock().unwrap(), expected);
            self.phases.lock().unwrap().push(context.phase);
            self.trace.step("execute", *self.fail_at.lock().unwrap())
        })
    }
}

#[derive(Clone)]
struct MemoryProvider {
    trace: Trace,
    replies: Arc<Mutex<VecDeque<Reply>>>,
    fail_at: Arc<Mutex<Option<&'static str>>>,
    requests: Arc<Mutex<Vec<ProviderRequest>>>,
}
impl Provider for MemoryProvider {
    fn invoke(&self, request: ProviderRequest) -> BoxFuture<'_, Result<ProviderResponse, Error>> {
        Box::pin(async move {
            self.trace.step("invoke", *self.fail_at.lock().unwrap())?;
            self.requests.lock().unwrap().push(request);
            match self
                .replies
                .lock()
                .unwrap()
                .pop_front()
                .expect("unexpected provider request")
            {
                Reply::Response(response) => Ok(response),
                Reply::TooLarge(message) => Err(Error::request_too_large(message)),
                Reply::Failed(message) => Err(Error::failed(message)),
                Reply::Error(error) => Err(error),
            }
        })
    }
}

struct MemoryState {
    trace: Trace,
    tools: MemoryTools,
    provider: MemoryProvider,
    decisions: VecDeque<CompletionDecision>,
    requests: Vec<(TurnRequestPhase, Vec<String>)>,
    retries: Vec<(RetryReason, i32)>,
    recorded_tools: Vec<ProviderResponse>,
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
        let availability_restored = Arc::new(AtomicBool::new(false));
        Self {
            trace: trace.clone(),
            tools: MemoryTools {
                trace: trace.clone(),
                advertised: Arc::default(),
                availability_restored: availability_restored.clone(),
                fail_at: Arc::default(),
                phases: Arc::default(),
            },
            provider: MemoryProvider {
                trace,
                replies: Arc::new(Mutex::new(replies.into_iter().collect())),
                fail_at: Arc::default(),
                requests: Arc::default(),
            },
            decisions: VecDeque::new(),
            requests: Vec::new(),
            retries: Vec::new(),
            recorded_tools: Vec::new(),
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
            availability_restored,
            fail_at: None,
        }
    }
    fn step(&mut self, name: &'static str) -> Result<(), Error> {
        self.trace.step(name, self.fail_at)
    }
    fn run(&mut self) -> Result<Outcome, Error> {
        *self.tools.fail_at.lock().unwrap() = self.fail_at;
        *self.provider.fail_at.lock().unwrap() = self.fail_at;
        let tools = self.tools.clone();
        let provider = self.provider.clone();
        finish(run(self, &tools, &provider))
    }
}
impl States for MemoryState {
    fn disables_request_limit(&self) -> bool {
        self.unlimited
    }
    fn has_scheduled_work(&self) -> bool {
        self.scheduled
    }
    fn check_cancellation(&self) -> Result<(), Error> {
        if self.cancelled {
            Err(Error::cancelled())
        } else {
            Ok(())
        }
    }
    fn is_turn_open(&self) -> BoxFuture<'_, bool> {
        Box::pin(async move { self.open })
    }
    fn has_work(&self) -> BoxFuture<'_, Result<bool, Error>> {
        Box::pin(async move { Ok(self.work || self.turns > 0) })
    }
    fn begin_turn(&mut self) -> BoxFuture<'_, Result<TurnOptions, Error>> {
        Box::pin(async move {
            self.step("begin")?;
            self.open = true;
            self.work = true;
            self.availability_restored.store(false, Ordering::SeqCst);
            self.turns = self.turns.saturating_sub(1);
            Ok(TurnOptions {
                maximum_requests: self.maximum,
            })
        })
    }
    fn restore_tool_availability(&mut self) -> BoxFuture<'_, Result<(), Error>> {
        Box::pin(async move {
            self.step("restore")?;
            self.availability_restored.store(true, Ordering::SeqCst);
            Ok(())
        })
    }
    fn promote_input(&mut self) -> BoxFuture<'_, Result<(), Error>> {
        Box::pin(async move { self.step("promote") })
    }
    fn take_interruption(&mut self) -> bool {
        std::mem::take(&mut self.interrupted)
    }
    fn prepare_request<'a>(
        &'a mut self,
        definitions: &'a [ToolDefinition],
        phase: TurnRequestPhase,
    ) -> BoxFuture<'a, Result<ProviderRequest, Error>> {
        Box::pin(async move {
            self.step("prepare")?;
            let names: Vec<_> = definitions
                .iter()
                .map(|definition| definition.name.clone())
                .collect();
            self.requests.push((phase, names.clone()));
            *self.tools.advertised.lock().unwrap() = names;
            Ok(ProviderRequest {
                model: "memory/model".into(),
                instructions: "core request".into(),
                tools: definitions.to_vec(),
                ..ProviderRequest::default()
            })
        })
    }
    fn compact_request<'a>(
        &'a mut self,
        _definitions: &'a [ToolDefinition],
    ) -> BoxFuture<'a, Result<bool, Error>> {
        Box::pin(async move {
            self.step("compact")?;
            Ok(self.reduced)
        })
    }
    fn record_retry(
        &mut self,
        reason: RetryReason,
        attempt: i32,
    ) -> BoxFuture<'_, Result<(), Error>> {
        Box::pin(async move {
            self.step("retry")?;
            self.retries.push((reason, attempt));
            Ok(())
        })
    }
    fn record_tool_calls<'a>(
        &'a mut self,
        response: &'a ProviderResponse,
    ) -> BoxFuture<'a, Result<(), Error>> {
        Box::pin(async move {
            self.step("record_tools")?;
            self.recorded_tools.push(response.clone());
            Ok(())
        })
    }
    fn evaluate_completion<'a>(
        &'a mut self,
        _response: &'a ProviderResponse,
    ) -> BoxFuture<'a, Result<CompletionDecision, Error>> {
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
        response: &'a ProviderResponse,
    ) -> BoxFuture<'a, Result<String, Error>> {
        Box::pin(async move {
            self.step("complete")?;
            self.open = false;
            self.work = false;
            Ok(response.assistant_text.clone())
        })
    }
    fn fail_runaway(&mut self) -> BoxFuture<'_, Result<String, Error>> {
        Box::pin(async move {
            self.step("runaway")?;
            Ok("runaway".into())
        })
    }
}

#[test]
fn tools_are_recorded_and_executed_before_the_next_request_and_final_completion() {
    let mut state = MemoryState::new([Reply::tools("call"), Reply::final_response("answer")]);
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
        Reply::truncated("cut"),
        Reply::invalid("bad"),
        Reply::final_response("answer"),
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
    let mut state = MemoryState::new((0..5).map(|_| Reply::tools("call")));
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
    let mut state = MemoryState::new([Reply::invalid("bad")]);
    state.maximum = 1;
    assert_eq!(state.run(), Ok(Outcome::Failed("runaway".into())));
    assert_eq!(state.requests.len(), 1);
}

#[test]
fn interruption_enters_final_phase_even_when_request_limits_are_disabled() {
    let mut state = MemoryState::new([Reply::final_response("answer")]);
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
        Reply::final_response("candidate"),
        Reply::invalid("bad"),
        Reply::final_response("answer"),
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
        Reply::final_response("candidate"),
        Reply::final_response("candidate2"),
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
        Reply::final_response("candidate"),
        Reply::final_response("candidate2"),
        Reply::final_response("answer"),
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
        Reply::tools("call"),
        Reply::TooLarge("second"),
        Reply::final_response("answer"),
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
    assert_eq!(state.run(), Err(Error::request_too_large("second")));
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
    assert_eq!(state.run(), Err(Error::request_too_large("original")));
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
    assert_eq!(state.run(), Err(Error::missing_turn()));
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
    assert_eq!(state.run(), Err(Error::cancelled()));
    assert_eq!(state.trace.values(), ["reconcile"]);
}

#[test]
fn provider_failure_propagates_without_recording_or_completing_a_response() {
    let mut state = MemoryState::new([Reply::Failed("provider")]);
    assert_eq!(state.run(), Err(Error::failed("provider")));
    assert_eq!(state.trace.values().last().unwrap(), "invoke");
}

#[test]
fn adapter_errors_stop_at_the_failing_effect() {
    for (stage, reply) in [
        ("reconcile", Reply::final_response("response")),
        ("begin", Reply::final_response("response")),
        ("restore", Reply::final_response("response")),
        ("promote", Reply::final_response("response")),
        ("prepare", Reply::final_response("response")),
        ("invoke", Reply::final_response("response")),
        ("retry", Reply::invalid("response")),
        ("record_tools", Reply::tools("response")),
        ("execute", Reply::tools("response")),
        ("evaluate", Reply::final_response("response")),
        ("complete", Reply::final_response("response")),
    ] {
        let mut state = MemoryState::new([reply]);
        state.fail_at = Some(stage);
        assert_eq!(state.run(), Err(Error::failed(stage)), "{stage}");
        assert_eq!(state.trace.values().last().unwrap(), stage);
    }
    let mut state = MemoryState::new([Reply::TooLarge("original")]);
    state.fail_at = Some("compact");
    assert_eq!(state.run(), Err(Error::failed("compact")));
    assert_eq!(state.trace.values().last().unwrap(), "compact");
    let mut state = MemoryState::new([]);
    state.maximum = 0;
    state.fail_at = Some("runaway");
    assert_eq!(state.run(), Err(Error::failed("runaway")));
    assert_eq!(state.trace.values().last().unwrap(), "runaway");
}

#[test]
fn new_turns_reset_request_count_but_retain_final_phase_and_settlement_extensions() {
    let mut state = MemoryState::new([
        Reply::tools("call1"),
        Reply::final_response("answer1"),
        Reply::tools("call2"),
        Reply::tools("call3"),
        Reply::tools("call4"),
        Reply::tools("call5"),
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
        Reply::tools("call1"),
        Reply::final_response("candidate"),
        Reply::tools("call2"),
        Reply::tools("call3"),
        Reply::tools("call4"),
        Reply::tools("call5"),
        Reply::tools("call6"),
        Reply::tools("call7"),
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
fn tools_and_provider_operate_without_states() {
    let state = MemoryState::new([Reply::final_response("standalone")]);
    let tools: &dyn Tools = &state.tools;
    finish(tools.reconcile_pending()).unwrap();
    let catalog = finish(tools.capture_catalog()).unwrap();
    *state.tools.advertised.lock().unwrap() = catalog
        .normal
        .iter()
        .map(|tool| tool.name.clone())
        .collect();
    finish(tools.execute_pending(ToolContext {
        phase: ToolPhase::Normal,
    }))
    .unwrap();
    assert_eq!(
        catalog
            .normal
            .iter()
            .map(|tool| tool.name.as_str())
            .collect::<Vec<_>>(),
        ["read", "write"]
    );
    assert_eq!(
        catalog
            .final_phase
            .iter()
            .map(|tool| tool.name.as_str())
            .collect::<Vec<_>>(),
        ["read"]
    );
    let provider: &dyn Provider = &state.provider;
    let response = finish(provider.invoke(ProviderRequest::default())).unwrap();
    assert_eq!(response.assistant_text, "standalone");
    assert!(response.tool_calls.is_empty());
    assert!(state.provider.replies.lock().unwrap().is_empty());
    assert_eq!(state.trace.values(), ["reconcile", "execute", "invoke"]);
}

struct EchoProvider;
impl Provider for EchoProvider {
    fn invoke(&self, request: ProviderRequest) -> BoxFuture<'_, Result<ProviderResponse, Error>> {
        Box::pin(async move {
            assert_eq!(request.model, "memory/model");
            assert_eq!(
                request
                    .tools
                    .iter()
                    .map(|tool| tool.name.as_str())
                    .collect::<Vec<_>>(),
                ["read", "write"]
            );
            Ok(ProviderResponse {
                assistant_text: request.instructions,
                finish_reason: "stop".into(),
                ..ProviderResponse::default()
            })
        })
    }
}

#[test]
fn different_provider_implementations_share_the_same_states_tools_and_protocol() {
    let mut states = MemoryState::new([Reply::final_response("scripted answer")]);
    let tools = states.tools.clone();
    let provider = states.provider.clone();
    assert_eq!(
        finish(run(&mut states, &tools, &provider)),
        Ok(Outcome::Completed("scripted answer".into()))
    );
    states.turns = 1;
    states.work = true;
    assert_eq!(
        finish(run(&mut states, &tools, &EchoProvider)),
        Ok(Outcome::Completed("core request".into()))
    );
    assert!(states.provider.replies.lock().unwrap().is_empty());
    assert_eq!(states.requests.len(), 2);
}

#[test]
fn state_preparation_receives_phase_and_provider_receives_advertised_tools() {
    let mut states = MemoryState::new([Reply::tools("call"), Reply::final_response("answer")]);
    states.maximum = 2;
    assert_eq!(states.run(), Ok(Outcome::Completed("answer".into())));
    assert_eq!(states.requests.len(), 2);
    assert!(!states.requests[0].0.final_phase);
    assert!(states.requests[1].0.final_phase && states.requests[1].0.enters_final_phase);
    let requests = states.provider.requests.lock().unwrap();
    assert_eq!(
        requests[0]
            .tools
            .iter()
            .map(|tool| tool.name.as_str())
            .collect::<Vec<_>>(),
        ["read", "write"]
    );
    assert_eq!(
        requests[1]
            .tools
            .iter()
            .map(|tool| tool.name.as_str())
            .collect::<Vec<_>>(),
        ["read"]
    );
    assert_eq!(*states.tools.phases.lock().unwrap(), [ToolPhase::Normal]);
}

#[test]
fn core_validates_json_syntax_without_rewriting_arguments_or_provider_content() {
    for arguments in [
        "null",
        "true",
        "42",
        "1.20",
        "\"text\"",
        "[]",
        "{}",
        " { \"unknown\" : [1.20, false] } ",
    ] {
        let response = ProviderResponse {
            assistant_text: "call".into(),
            finish_reason: "tool_calls".into(),
            provider_id: "future-provider".into(),
            provider_content: " { \"unknown\" : 1.20 } ".into(),
            tool_calls: vec![ToolCall {
                id: "opaque-call".into(),
                name: "read".into(),
                arguments_json: arguments.into(),
            }],
            ..ProviderResponse::default()
        };
        let mut states = MemoryState::new([
            Reply::Response(response.clone()),
            Reply::final_response("answer"),
        ]);
        assert_eq!(
            states.run(),
            Ok(Outcome::Completed("answer".into())),
            "{arguments}"
        );
        assert!(states.retries.is_empty());
        assert_eq!(states.recorded_tools, [response]);
    }
    let mut states = MemoryState::new([Reply::Response(ProviderResponse {
        assistant_text: "partial answer".into(),
        finish_reason: "length".into(),
        ..ProviderResponse::default()
    })]);
    assert_eq!(
        states.run(),
        Ok(Outcome::Completed("partial answer".into()))
    );
    assert!(states.retries.is_empty());
}

#[test]
fn fixed_error_metadata_and_original_oversized_error_survive_core_recovery() {
    let error = Error {
        kind: ErrorKind::RequestTooLarge,
        code: ErrorCode::Http,
        message: "request rejected".into(),
        response_body: "unredacted top-level body".into(),
    };
    let mut states = MemoryState::new([Reply::Error(error.clone())]);
    states.reduced = false;
    assert_eq!(states.run(), Err(error.clone()));
    assert_eq!(error.to_string(), "request rejected");
    assert_eq!(error.code.code(), "http");
    assert!(!error.to_string().contains(&error.response_body));
    let cancelled = Error::cancelled();
    let mut states = MemoryState::new([Reply::Error(cancelled.clone())]);
    assert_eq!(states.run(), Err(cancelled.clone()));
    assert_eq!(cancelled.kind, ErrorKind::Cancelled);
    assert_eq!(cancelled.code.code(), "cancelled");
    assert_eq!(states.trace.values().last().unwrap(), "invoke");
}
