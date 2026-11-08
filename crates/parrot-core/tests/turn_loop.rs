use parrot_core::{
    BoxFuture, CancellationToken, CompletionFeedback, Content, ContextSnapshot, ConversationTurn,
    Delivery, Error, ErrorCode, ErrorKind, FinishReason, InputAdmission, Message, Outcome,
    PendingMessage, Provider, ProviderRequest, ProviderResponse, Reasoning, RetryReason, Role,
    States, ToolCall, ToolCatalog, ToolDefinition, ToolImage, ToolResult, ToolStatus, Tools,
    TurnOptions,
    budget::{TurnBudget, TurnRequestPhase},
    run, run_turn,
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

#[path = "turn_loop/cancellation.rs"]
mod cancellation;
#[path = "turn_loop/completion_policy.rs"]
mod completion_policy;
#[path = "turn_loop/request_assembly.rs"]
mod request_assembly;
#[path = "turn_loop/tool_execution.rs"]
mod tool_execution;
#[path = "turn_loop/turn_state.rs"]
mod turn_state;

use cancellation::CancellationSource;

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
            finish_reason: FinishReason::Stop,
            ..ProviderResponse::default()
        })
    }
    fn tools(text: &str) -> Self {
        Self::Response(ProviderResponse {
            assistant_text: text.into(),
            finish_reason: FinishReason::ToolCalls,
            tool_calls: vec![ToolCall {
                id: "call-1".into(),
                name: "read".into(),
                arguments_json: "{}".into(),
            }],
            ..ProviderResponse::default()
        })
    }
    fn with_tool_calls(calls: Vec<ToolCall>) -> Self {
        Self::Response(ProviderResponse {
            finish_reason: FinishReason::ToolCalls,
            tool_calls: calls,
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
        response.finish_reason = FinishReason::Length;
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
    fail_at: Arc<Mutex<Option<&'static str>>>,
    requests: Arc<Mutex<Vec<Vec<ToolCall>>>>,
    replies: Arc<Mutex<VecDeque<Vec<ToolResult>>>>,
}
impl Tools for MemoryTools {
    fn capture_catalog(&self) -> BoxFuture<'_, Result<ToolCatalog, Error>> {
        Box::pin(async move {
            self.trace.step("catalog", *self.fail_at.lock().unwrap())?;
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
    fn execute<'a>(
        &'a self,
        calls: Vec<ToolCall>,
        _cancellation: CancellationToken<'a>,
    ) -> BoxFuture<'a, Result<Vec<ToolResult>, Error>> {
        Box::pin(async move {
            assert!(!calls.is_empty());
            self.requests.lock().unwrap().push(calls.clone());
            self.trace.step("execute", *self.fail_at.lock().unwrap())?;
            if let Some(reply) = self.replies.lock().unwrap().pop_front() {
                return Ok(reply);
            }
            Ok(calls
                .into_iter()
                .map(|call| ToolResult {
                    text: format!("result for {}", call.id),
                    call_id: call.id,
                    tool_name: call.name,
                    ..ToolResult::default()
                })
                .collect())
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
    exit_blockers: VecDeque<Vec<String>>,
    completion_feedback: Vec<CompletionFeedback>,
    requests: Vec<(TurnRequestPhase, Vec<String>)>,
    prepared_definitions: Vec<Vec<ToolDefinition>>,
    retries: Vec<(RetryReason, i32)>,
    recorded_tools: Vec<ProviderResponse>,
    applied_results: Vec<ToolResult>,
    recorded_catalogs: Vec<Vec<String>>,
    model: String,
    reasoning: Option<Reasoning>,
    provider_preferences: String,
    include_router_metadata: bool,
    context: ContextSnapshot,
    messages: Vec<Message>,
    preparations: VecDeque<(ContextSnapshot, Vec<Message>)>,
    last_turn: Option<ConversationTurn>,
    pending_messages: VecDeque<(PendingMessage, Message)>,
    next_input: usize,
    open: bool,
    admissions: Vec<InputAdmission>,
    begin_message: Option<Message>,
    cancellation: Arc<CancellationSource>,
    unlimited: bool,
    maximum: i32,
    reduced: bool,
    fail_at: Option<&'static str>,
}
impl MemoryState {
    fn new(replies: impl IntoIterator<Item = Reply>) -> Self {
        let trace = Trace::default();
        let mut state = Self {
            trace: trace.clone(),
            tools: MemoryTools {
                trace: trace.clone(),
                fail_at: Arc::default(),
                requests: Arc::default(),
                replies: Arc::default(),
            },
            provider: MemoryProvider {
                trace,
                replies: Arc::new(Mutex::new(replies.into_iter().collect())),
                fail_at: Arc::default(),
                requests: Arc::default(),
            },
            exit_blockers: VecDeque::new(),
            completion_feedback: Vec::new(),
            requests: Vec::new(),
            prepared_definitions: Vec::new(),
            retries: Vec::new(),
            recorded_tools: Vec::new(),
            applied_results: Vec::new(),
            recorded_catalogs: Vec::new(),
            model: "memory/model".into(),
            reasoning: None,
            provider_preferences: String::new(),
            include_router_metadata: false,
            context: ContextSnapshot {
                instructions: "core request".into(),
                max_output_tokens: 0,
            },
            messages: Vec::new(),
            preparations: VecDeque::new(),
            last_turn: None,
            pending_messages: VecDeque::new(),
            next_input: 0,
            open: false,
            admissions: Vec::new(),
            begin_message: None,
            cancellation: Arc::default(),
            unlimited: false,
            maximum: 10,
            reduced: true,
            fail_at: None,
        };
        state.enqueue_message(Delivery::Queue, "question");
        state
    }
    fn enqueue_message(&mut self, delivery: Delivery, content: &str) {
        let input = self.next_input;
        self.next_input += 1;
        self.pending_messages.push_back((
            PendingMessage {
                input_id: format!("input-{input}"),
                message_id: format!("message-{input}"),
                delivery,
            },
            Message {
                role: Role::User,
                contents: vec![Content::Text(content.into())],
                ..Message::default()
            },
        ));
    }
    fn append_turn_message(&mut self, message: Message) {
        if message.role == Role::User {
            self.last_turn = None;
        }
        let turn = self.last_turn.get_or_insert_with(|| ConversationTurn {
            messages: Vec::new(),
            result: None,
        });
        turn.messages.push(message);
        turn.result = turn
            .messages
            .iter()
            .rev()
            .find(|message| message.role != Role::System)
            .filter(|message| message.role == Role::Assistant && message.tool_calls.is_empty())
            .cloned();
    }
    fn append_response(&mut self, response: &ProviderResponse) {
        self.append_turn_message(Message {
            role: Role::Assistant,
            contents: vec![Content::Text(response.assistant_text.clone())],
            tool_calls: response.tool_calls.clone(),
            provider_id: response.provider_id.clone(),
            provider_content: response.provider_content.clone(),
            ..Message::default()
        });
    }
    fn step(&mut self, name: &'static str) -> Result<(), Error> {
        self.trace.step(name, self.fail_at)
    }
    fn run(&mut self) -> Result<Outcome, Error> {
        *self.tools.fail_at.lock().unwrap() = self.fail_at;
        *self.provider.fail_at.lock().unwrap() = self.fail_at;
        let tools = self.tools.clone();
        let provider = self.provider.clone();
        self.run_with(&tools, &provider)
    }
    fn run_with(&mut self, tools: &dyn Tools, provider: &dyn Provider) -> Result<Outcome, Error> {
        let cancellation = self.cancellation.clone();
        let is_cancelled = || cancellation.is_cancelled();
        let cancelled = || cancellation.cancelled();
        let mut budget = TurnBudget::new(self.unlimited);
        finish(run(
            self,
            tools,
            provider,
            &mut budget,
            CancellationToken::new(&is_cancelled, &cancelled),
        ))
    }
}
impl States for MemoryState {
    fn record_tool_results<'a>(
        &'a mut self,
        results: &'a [ToolResult],
    ) -> BoxFuture<'a, Result<(), Error>> {
        Box::pin(async move {
            self.step("record_results")?;
            let calls = &self
                .recorded_tools
                .last()
                .expect("recorded tool batch")
                .tool_calls;
            assert_eq!(results.len(), calls.len());
            for (result, call) in results.iter().zip(calls) {
                assert_eq!(result.call_id, call.id);
                assert_eq!(result.tool_name, call.name);
            }
            self.applied_results.extend_from_slice(results);
            for result in results {
                self.append_turn_message(Message {
                    role: Role::Tool,
                    contents: vec![Content::Text(result.text.clone())],
                    tool_call_id: result.call_id.clone(),
                    ..Message::default()
                });
            }
            Ok(())
        })
    }
    fn get_last_turn(&self) -> BoxFuture<'_, Result<Option<ConversationTurn>, Error>> {
        Box::pin(async move {
            if self.fail_at == Some("last_turn") {
                self.trace.step("last_turn", self.fail_at)?;
            }
            Ok(self.last_turn.clone())
        })
    }
    fn get_pending_message(&self) -> BoxFuture<'_, Result<Option<PendingMessage>, Error>> {
        Box::pin(async move {
            if self.fail_at == Some("pending_message") {
                self.trace.step("pending_message", self.fail_at)?;
            }
            Ok(self
                .pending_messages
                .iter()
                .find(|(message, _)| message.delivery == Delivery::Steer)
                .or_else(|| self.pending_messages.front())
                .map(|(message, _)| message.clone()))
        })
    }
    fn is_turn_open(&self) -> BoxFuture<'_, bool> {
        Box::pin(async move { self.open })
    }
    fn begin_turn(&mut self) -> BoxFuture<'_, Result<TurnOptions, Error>> {
        Box::pin(async move {
            self.step("begin")?;
            self.open = true;
            if let Some(message) = self.begin_message.take() {
                self.append_turn_message(message);
            }
            Ok(TurnOptions {
                maximum_requests: self.maximum,
                model: self.model.clone(),
                reasoning: self.reasoning.clone(),
                provider_preferences: self.provider_preferences.clone(),
                include_router_metadata: self.include_router_metadata,
            })
        })
    }
    fn promote_input(&mut self, admission: InputAdmission) -> BoxFuture<'_, Result<(), Error>> {
        Box::pin(async move {
            self.step("promote")?;
            self.admissions.push(admission);
            let mut promoted = Vec::new();
            while let Some(index) = self
                .pending_messages
                .iter()
                .position(|(message, _)| message.delivery == Delivery::Steer)
            {
                promoted.push(self.pending_messages.remove(index).unwrap().1);
            }
            if promoted.is_empty() && admission == InputAdmission::SteersOrNextQueued {
                promoted.extend(
                    self.pending_messages
                        .pop_front()
                        .map(|(_, message)| message),
                );
            }
            for message in promoted {
                self.append_turn_message(message);
            }
            Ok(())
        })
    }
    fn prepare_context<'a>(
        &'a mut self,
        definitions: &'a [ToolDefinition],
        phase: TurnRequestPhase,
    ) -> BoxFuture<'a, Result<ContextSnapshot, Error>> {
        Box::pin(async move {
            self.step("prepare")?;
            let names: Vec<_> = definitions
                .iter()
                .map(|definition| definition.name.clone())
                .collect();
            self.requests.push((phase, names));
            self.prepared_definitions.push(definitions.to_vec());
            if let Some((context, messages)) = self.preparations.pop_front() {
                self.context = context;
                self.messages = messages;
            }
            Ok(self.context.clone())
        })
    }
    fn read_messages(&self) -> BoxFuture<'_, Result<Vec<Message>, Error>> {
        Box::pin(async move {
            self.trace.step("messages", self.fail_at)?;
            Ok(self.messages.clone())
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
            self.append_response(response);
            self.recorded_catalogs.push(
                self.requests
                    .last()
                    .expect("prepared provider request")
                    .1
                    .clone(),
            );
            Ok(())
        })
    }
    fn get_exit_blockers(&mut self) -> BoxFuture<'_, Result<Vec<String>, Error>> {
        Box::pin(async move {
            self.step("exit_blockers")?;
            Ok(self.exit_blockers.pop_front().unwrap_or_default())
        })
    }
    fn record_completion_feedback(
        &mut self,
        feedback: CompletionFeedback,
    ) -> BoxFuture<'_, Result<(), Error>> {
        Box::pin(async move {
            self.step("completion_feedback")?;
            self.append_response(&feedback.candidate);
            for message in &feedback.messages {
                self.append_turn_message(Message {
                    role: Role::System,
                    contents: vec![Content::Text(message.clone())],
                    ..Message::default()
                });
            }
            self.completion_feedback.push(feedback);
            Ok(())
        })
    }
    fn complete_turn<'a>(
        &'a mut self,
        response: &'a ProviderResponse,
    ) -> BoxFuture<'a, Result<String, Error>> {
        Box::pin(async move {
            self.step("complete")?;
            self.open = false;
            self.append_response(response);
            Ok(response.assistant_text.clone())
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
            "begin",
            "catalog",
            "promote",
            "prepare",
            "messages",
            "invoke",
            "record_tools",
            "execute",
            "record_results",
            "promote",
            "prepare",
            "messages",
            "invoke",
            "exit_blockers",
            "complete",
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
    assert_eq!(state.run(), Ok(Outcome::BudgetExhausted));
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
    assert_eq!(state.trace.values().last().unwrap(), "record_results");
    assert_eq!(state.applied_results.len(), 5);
    assert!(!state.trace.values().iter().any(|entry| entry == "complete"));
    assert_eq!(state.recorded_catalogs.len(), 5);
    assert!(
        state
            .recorded_catalogs
            .iter()
            .all(|tools| tools == &["read"])
    );
}

#[test]
fn malformed_final_response_exhausts_the_request_limit_without_a_settlement_extension() {
    let mut state = MemoryState::new([Reply::invalid("bad")]);
    state.maximum = 1;
    assert_eq!(state.run(), Ok(Outcome::BudgetExhausted));
    assert_eq!(state.requests.len(), 1);
}

#[test]
fn caller_budget_configuration_controls_request_limits_and_final_phase() {
    for disable_limits in [false, true] {
        let mut state = MemoryState::new([Reply::invalid("bad"), Reply::final_response("answer")]);
        state.maximum = 1;
        let tools = state.tools.clone();
        let provider = state.provider.clone();
        let source = state.cancellation.clone();
        let is_cancelled = || source.is_cancelled();
        let cancelled = || source.cancelled();
        let mut budget = TurnBudget::new(disable_limits);
        assert_eq!(
            finish(run(
                &mut state,
                &tools,
                &provider,
                &mut budget,
                CancellationToken::new(&is_cancelled, &cancelled),
            )),
            if disable_limits {
                Ok(Outcome::Completed("answer".into()))
            } else {
                Ok(Outcome::BudgetExhausted)
            }
        );
        assert_eq!(state.retries, [(RetryReason::InvalidToolCalls, 1)]);
        assert_eq!(state.requests.len(), if disable_limits { 2 } else { 1 });
        assert!(state.requests.iter().all(|(phase, tools)| {
            phase.final_phase != disable_limits
                && if disable_limits {
                    tools == &["read", "write"]
                } else {
                    tools == &["read"]
                }
        }));
        assert!(state.recorded_tools.is_empty());
        assert!(state.tools.requests.lock().unwrap().is_empty());
        assert_eq!(
            state.trace.values().last().unwrap(),
            if disable_limits {
                "complete"
            } else {
                "promote"
            }
        );
    }
}

#[test]
fn exhausted_budget_returns_without_provider_or_terminal_state_effects() {
    let mut state = MemoryState::new([]);
    state.maximum = 0;
    assert_eq!(state.run(), Ok(Outcome::BudgetExhausted));
    assert_eq!(state.trace.values(), ["begin", "catalog", "promote"]);
    assert!(state.provider.requests.lock().unwrap().is_empty());
    assert!(state.tools.requests.lock().unwrap().is_empty());
    assert!(state.pending_messages.is_empty());
    assert_eq!(state.admissions, [InputAdmission::SteersOrNextQueued]);
    assert!(state.open);
    assert_eq!(
        state.last_turn.as_ref().unwrap().messages[0].role,
        Role::User
    );
}

#[test]
fn disabled_request_limits_keep_every_request_in_normal_phase() {
    let mut state = MemoryState::new([
        Reply::tools("call1"),
        Reply::tools("call2"),
        Reply::final_response("answer"),
    ]);
    state.unlimited = true;
    state.maximum = 1;
    assert_eq!(state.run(), Ok(Outcome::Completed("answer".into())));
    assert_eq!(state.requests.len(), 3);
    assert!(state.requests.iter().all(|(phase, tools)| {
        !phase.final_phase && !phase.enters_final_phase && tools == &["read", "write"]
    }));
    assert_eq!(state.applied_results.len(), 2);
    assert!(
        state
            .recorded_catalogs
            .iter()
            .all(|tools| tools == &["read", "write"])
    );
}

#[test]
fn completion_retry_resets_budget_and_keeps_pending_work_alive() {
    let mut state = MemoryState::new([
        Reply::final_response("candidate"),
        Reply::invalid("bad"),
        Reply::final_response("answer"),
    ]);
    state.maximum = 2;
    state
        .exit_blockers
        .push_back(vec!["unfinished goal".into()]);
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
fn cleared_completion_blockers_close_only_the_final_candidate() {
    let mut state = MemoryState::new([
        Reply::final_response("candidate"),
        Reply::final_response("candidate2"),
    ]);
    state.exit_blockers.push_back(vec!["answer the ask".into()]);
    assert_eq!(state.run(), Ok(Outcome::Completed("candidate2".into())));
    assert_eq!(state.requests.len(), 2);
    assert_eq!(state.completion_feedback.len(), 1);
    assert_eq!(
        state.completion_feedback[0].candidate.assistant_text,
        "candidate"
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
    assert_eq!(state.trace.values().last().unwrap(), "complete");
    assert!(!state.open);
}

#[test]
fn successive_blocker_lists_retain_each_candidate_and_preserve_continuation() {
    let mut state = MemoryState::new([
        Reply::final_response("candidate"),
        Reply::final_response("candidate2"),
        Reply::final_response("answer"),
    ]);
    state.exit_blockers.push_back(vec!["answer the ask".into()]);
    state.exit_blockers.push_back(vec!["work remains".into()]);
    assert_eq!(state.run(), Ok(Outcome::Completed("answer".into())));
    assert_eq!(state.requests.len(), 3);
    assert_eq!(state.completion_feedback.len(), 2);
    assert_eq!(
        state.completion_feedback[0].candidate.assistant_text,
        "candidate"
    );
    assert_eq!(
        state.completion_feedback[1].candidate.assistant_text,
        "candidate2"
    );
    assert_eq!(state.completion_feedback[0].messages, ["answer the ask"]);
    assert_eq!(state.completion_feedback[1].messages, ["work remains"]);
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
fn open_turn_without_captured_settings_fails_before_provider_invocation() {
    let mut state = MemoryState::new([]);
    state.open = true;
    assert_eq!(state.run(), Err(Error::missing_turn()));
    assert_eq!(state.trace.values(), ["promote"]);
    assert!(state.provider.requests.lock().unwrap().is_empty());
}

#[test]
fn no_work_and_cancellation_never_open_a_turn_or_invoke_the_provider() {
    let mut state = MemoryState::new([]);
    state.pending_messages.clear();
    assert_eq!(state.run(), Ok(Outcome::Completed(String::new())));
    assert!(state.trace.values().is_empty());
    state.trace.clear();
    state.cancellation.cancel();
    assert_eq!(state.run(), Err(Error::cancelled()));
    assert!(state.trace.values().is_empty());
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
        ("begin", Reply::final_response("response")),
        ("catalog", Reply::final_response("response")),
        ("promote", Reply::final_response("response")),
        ("prepare", Reply::final_response("response")),
        ("messages", Reply::final_response("response")),
        ("invoke", Reply::final_response("response")),
        ("retry", Reply::invalid("response")),
        ("record_tools", Reply::tools("response")),
        ("execute", Reply::tools("response")),
        ("record_results", Reply::tools("response")),
        ("exit_blockers", Reply::final_response("response")),
        ("completion_feedback", Reply::final_response("response")),
        ("complete", Reply::final_response("response")),
    ] {
        let mut state = MemoryState::new([reply]);
        state.fail_at = Some(stage);
        if stage == "completion_feedback" {
            state.exit_blockers.push_back(vec!["answer the ask".into()]);
        }
        assert_eq!(state.run(), Err(Error::failed(stage)), "{stage}");
        assert_eq!(state.trace.values().last().unwrap(), stage);
    }
    let mut state = MemoryState::new([Reply::TooLarge("original")]);
    state.fail_at = Some("compact");
    assert_eq!(state.run(), Err(Error::failed("compact")));
    assert_eq!(state.trace.values().last().unwrap(), "compact");
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
    state.enqueue_message(Delivery::Queue, "second question");
    assert_eq!(state.run(), Ok(Outcome::BudgetExhausted));
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
fn completion_retry_resets_quota_final_phase_and_settlement_extensions() {
    let mut state = MemoryState::new([
        Reply::tools("call0"),
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
    state.exit_blockers.push_back(vec!["work remains".into()]);
    assert_eq!(state.run(), Ok(Outcome::BudgetExhausted));
    assert_eq!(state.requests.len(), 9);
    assert!(!state.requests[0].0.final_phase);
    assert!(state.requests[1].0.enters_final_phase);
    assert!(state.requests[2].0.final_phase);
    assert!(!state.requests[2].0.enters_final_phase);
    assert!(!state.requests[3].0.final_phase);
    assert_eq!(state.requests[3].1, ["read", "write"]);
    assert!(state.requests[4].0.enters_final_phase);
    assert!(
        state.requests[5..]
            .iter()
            .all(|(phase, _)| phase.final_phase && !phase.enters_final_phase)
    );
    assert_eq!(state.completion_feedback.len(), 1);
    assert_eq!(
        state.completion_feedback[0].candidate.assistant_text,
        "candidate"
    );
    assert_eq!(
        state
            .trace
            .values()
            .iter()
            .filter(|entry| *entry == "execute")
            .count(),
        8
    );
    assert_eq!(state.applied_results.len(), 8);
    assert_eq!(state.trace.values().last().unwrap(), "record_results");
}

#[test]
fn tools_and_provider_operate_without_states() {
    let state = MemoryState::new([Reply::final_response("standalone")]);
    let tools: &dyn Tools = &state.tools;
    let catalog = finish(tools.capture_catalog()).unwrap();
    let calls = vec![ToolCall {
        id: "explicit-call".into(),
        name: "read".into(),
        arguments_json: " { \"a\" : 1 } ".into(),
    }];
    let source = CancellationSource::default();
    let is_cancelled = || source.is_cancelled();
    let cancelled = || source.cancelled();
    let cancellation = CancellationToken::new(&is_cancelled, &cancelled);
    let results = finish(tools.execute(calls.clone(), cancellation)).unwrap();
    assert_eq!(*state.tools.requests.lock().unwrap(), [calls]);
    assert_eq!(
        results,
        [ToolResult {
            call_id: "explicit-call".into(),
            tool_name: "read".into(),
            text: "result for explicit-call".into(),
            ..ToolResult::default()
        }]
    );
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
    assert_eq!(state.trace.values(), ["catalog", "execute", "invoke"]);
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
                finish_reason: FinishReason::Stop,
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
        states.run_with(&tools, &provider),
        Ok(Outcome::Completed("scripted answer".into()))
    );
    states.enqueue_message(Delivery::Queue, "next question");
    assert_eq!(
        states.run_with(&tools, &EchoProvider),
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
    assert_eq!(requests.len(), states.prepared_definitions.len());
    for (request, definitions) in requests.iter().zip(&states.prepared_definitions) {
        assert_eq!(&request.tools, definitions);
    }
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
    assert_eq!(
        states.recorded_catalogs,
        [vec!["read".to_owned(), "write".to_owned()]]
    );
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
            finish_reason: FinishReason::ToolCalls,
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
        finish_reason: FinishReason::Length,
        ..ProviderResponse::default()
    })]);
    assert_eq!(
        states.run(),
        Ok(Outcome::Completed("partial answer".into()))
    );
    assert!(states.retries.is_empty());
}

#[test]
fn unknown_finish_reasons_remain_lossless_without_triggering_truncation_retries() {
    let Reply::Response(mut response) = Reply::tools("call") else {
        unreachable!()
    };
    response.finish_reason = FinishReason::Other("future finish reason".into());
    let mut states = MemoryState::new([
        Reply::Response(response.clone()),
        Reply::final_response("answer"),
    ]);
    assert_eq!(states.run(), Ok(Outcome::Completed("answer".into())));
    assert!(states.retries.is_empty());
    assert_eq!(states.recorded_tools, [response]);
    assert_eq!(states.tools.requests.lock().unwrap().len(), 1);
    assert_eq!(states.applied_results.len(), 1);
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
