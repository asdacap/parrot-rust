use parrot_core::{
    AgentState, BoxFuture, Cancellation, CancellationToken, CompletionBlockerFeedback, ConfigKey,
    ConfigValue, Content, ContextConfiguration, ConversationAddition, ConversationEvent,
    ConversationOrigin, ConversationPersistence, ConversationReceipt, ConversationTurn, Error,
    ErrorCode, ErrorKind, EventPublisher, FinishReason, InputAdmission, Message, Outcome,
    PreparedSkillPrompt, PromotedInput, PromptTemplate, Provider, ProviderRequest,
    ProviderResponse, Reasoning, Role, Skill, SkillContent, SkillMention, SkillProvider,
    SkillReference, States, StatusKey, StatusOffer, StatusOffering, StatusStanding, ToolCall,
    ToolCallReceipt, ToolCatalog, ToolDefinition, ToolExecutionRequest, ToolResult, ToolStatus,
    Tools, TurnFlags, TurnModel, TurnOptions, parse_skill_mentions,
};
use std::{
    collections::{BTreeMap, VecDeque},
    future::Future,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    task::{Context, Poll, Wake, Waker},
    thread,
};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Delivery {
    Steer,
    Queue,
}

#[path = "turn_loop/cancellation.rs"]
mod cancellation;
#[path = "turn_loop/compact_tools.rs"]
mod compact_tools;
#[path = "turn_loop/compaction_dependencies.rs"]
mod compaction_dependencies;
#[path = "turn_loop/completion_policy.rs"]
mod completion_policy;
#[path = "turn_loop/context_preparation.rs"]
mod context_preparation;
#[path = "turn_loop/provider_cancellation.rs"]
mod provider_cancellation;
#[path = "turn_loop/request_assembly.rs"]
mod request_assembly;
#[path = "turn_loop/skills.rs"]
mod skills;
#[path = "turn_loop/status_updates.rs"]
mod status_updates;
#[path = "turn_loop/tool_execution.rs"]
mod tool_execution;
#[path = "turn_loop/turn_state.rs"]
mod turn_state;

use cancellation::CancellationSource;
use compaction_dependencies::MemoryCompaction;

struct MemoryEventPublisher {
    events: Arc<Mutex<Vec<ConversationEvent>>>,
    started_turns: Arc<Mutex<Vec<TurnModel>>>,
}

impl EventPublisher for MemoryEventPublisher {
    fn publish<'a>(
        &'a self,
        event: ConversationEvent,
        _: CancellationToken<'a>,
    ) -> BoxFuture<'a, Result<(), Error>> {
        Box::pin(async move {
            self.events.lock().unwrap().push(event);
            Ok(())
        })
    }
    fn report_turn_started<'a>(
        &'a self,
        model: &'a TurnModel,
        _: CancellationToken<'a>,
    ) -> BoxFuture<'a, Result<(), Error>> {
        Box::pin(async move {
            self.started_turns.lock().unwrap().push(model.clone());
            Ok(())
        })
    }
}

const RUNAWAY_NOTICE: &str = "configured runaway notice";

struct MemoryTurnHooks {
    model: String,
    activations: Arc<Mutex<usize>>,
    activation_error: Option<Error>,
    fail_at: Option<&'static str>,
}

impl parrot_core::TurnHooks for MemoryTurnHooks {
    fn begin_conversation_turn(&self) -> BoxFuture<'_, Result<TurnModel, Error>> {
        Box::pin(async move {
            if self.fail_at == Some("open") {
                return Err(Error::failed("open"));
            }
            Ok(TurnModel {
                selector: self.model.clone(),
                alias_icon: None,
            })
        })
    }
    fn activate_conversation_turn(&self) -> BoxFuture<'_, Result<(), Error>> {
        *self.activations.lock().unwrap() += 1;
        let result = self.activation_error.clone().map_or(Ok(()), Err);
        Box::pin(async move { result })
    }
    fn begin_step(&self) -> BoxFuture<'_, Result<(), Error>> {
        Box::pin(async { Ok(()) })
    }
}

struct MemoryTurnLifecycle<'a> {
    states: &'a mut MemoryState,
    tools: &'a dyn Tools,
    provider: &'a dyn Provider,
    publisher: &'a MemoryEventPublisher,
    single_turn: bool,
    opened: bool,
}

impl parrot_core::TurnLifecycle for MemoryTurnLifecycle<'_> {
    fn has_scheduled_work(&self) -> bool {
        false
    }
    fn has_ready_turn(&mut self) -> BoxFuture<'_, Result<bool, Error>> {
        Box::pin(async move {
            if self.single_turn {
                return Ok(!self.opened);
            }
            let turn = self.states.get_last_turn().await?;
            Ok(turn.as_ref().is_some_and(ConversationTurn::needs_response)
                || self.states.has_pending_input().await?)
        })
    }
    fn open_turn<'a>(
        &'a mut self,
        cancellation: CancellationToken<'a>,
    ) -> BoxFuture<'a, Result<Box<dyn parrot_core::Turn + 'a>, Error>> {
        Box::pin(async move {
            let hooks = MemoryTurnHooks {
                model: self.states.model.clone(),
                activations: self.states.turn_activations.clone(),
                activation_error: self.states.activation_error.clone(),
                fail_at: self.states.fail_at,
            };
            let model = parrot_core::TurnHooks::begin_conversation_turn(&hooks).await?;
            self.publisher
                .report_turn_started(&model, cancellation)
                .await?;
            let skills = MemorySkills::new(self.states);
            self.opened = true;
            Ok(Box::new(MemoryTurn {
                states: self.states,
                tools: self.tools,
                provider: self.provider,
                publisher: self.publisher,
                hooks,
                skills,
            }) as Box<dyn parrot_core::Turn>)
        })
    }
}

struct MemoryTurn<'a> {
    states: &'a mut MemoryState,
    tools: &'a dyn Tools,
    provider: &'a dyn Provider,
    publisher: &'a MemoryEventPublisher,
    hooks: MemoryTurnHooks,
    skills: MemorySkills,
}

impl parrot_core::Turn for MemoryTurn<'_> {
    fn borrow_services(&mut self) -> parrot_core::TurnServices<'_> {
        parrot_core::TurnServices {
            states: self.states,
            skills: &self.skills,
            tools: self.tools,
            provider: self.provider,
            publisher: self.publisher,
            hooks: &self.hooks,
        }
    }
    fn dispose<'a>(
        &'a mut self,
        result: &'a Result<Outcome, Error>,
    ) -> BoxFuture<'a, Result<(), Error>> {
        Box::pin(async move {
            let mut pending = true;
            std::future::poll_fn(|cx| {
                if std::mem::take(&mut pending) {
                    cx.waker().wake_by_ref();
                    Poll::Pending
                } else {
                    Poll::Ready(())
                }
            })
            .await;
            let result = match result {
                Ok(Outcome::Completed(answer)) => Ok(Outcome::Completed(answer.clone())),
                Ok(Outcome::BudgetExhausted(notice)) => {
                    Ok(Outcome::BudgetExhausted(notice.clone()))
                }
                Err(error) => Err(error.clone()),
            };
            self.states.disposed_turns.push(result);
            self.states.disposal_error.clone().map_or(Ok(()), Err)
        })
    }
}

async fn run(
    states: &mut MemoryState,
    tools: &dyn Tools,
    provider: &dyn Provider,
    cancellation: CancellationToken<'_>,
) -> Result<Outcome, Error> {
    let publisher = MemoryEventPublisher {
        events: states.events.clone(),
        started_turns: states.started_turns.clone(),
    };
    let mut lifecycle = MemoryTurnLifecycle {
        states,
        tools,
        provider,
        publisher: &publisher,
        single_turn: false,
        opened: false,
    };
    parrot_core::TurnEngine::run(&mut lifecycle, cancellation).await
}

async fn run_turn(
    states: &mut MemoryState,
    tools: &dyn Tools,
    provider: &dyn Provider,
    cancellation: CancellationToken<'_>,
) -> Result<Outcome, Error> {
    let publisher = MemoryEventPublisher {
        events: states.events.clone(),
        started_turns: states.started_turns.clone(),
    };
    let mut lifecycle = MemoryTurnLifecycle {
        states,
        tools,
        provider,
        publisher: &publisher,
        single_turn: true,
        opened: false,
    };
    parrot_core::TurnEngine::run(&mut lifecycle, cancellation).await
}

struct ThreadWake(thread::Thread);
impl Wake for ThreadWake {
    fn wake(self: Arc<Self>) {
        self.0.unpark();
    }
}
fn request_too_large(message: &str) -> Error {
    Error {
        kind: ErrorKind::RequestTooLarge,
        code: ErrorCode::RequestTooLarge,
        message: message.into(),
        response_body: String::new(),
        detail: None,
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
    UsageThenError { error: Error, input_tokens: i32 },
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

type ScriptedReplies = Arc<Mutex<VecDeque<Vec<ToolResult>>>>;

#[derive(Clone)]
struct MemoryTools {
    trace: Trace,
    fail_at: Arc<Mutex<Option<&'static str>>>,
    requests: Arc<Mutex<Vec<Vec<ToolCall>>>>,
    replies: ScriptedReplies,
    receipts: Arc<Mutex<Vec<ToolCallReceipt>>>,
    flags: Arc<Mutex<Vec<TurnFlags>>>,
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
        request: ToolExecutionRequest,
        _cancellation: CancellationToken<'a>,
    ) -> BoxFuture<'a, Result<Vec<ToolResult>, Error>> {
        Box::pin(async move {
            let ToolExecutionRequest {
                calls,
                receipt,
                flags,
                ..
            } = request;
            assert!(!calls.is_empty());
            self.requests.lock().unwrap().push(calls.clone());
            self.receipts.lock().unwrap().push(receipt);
            self.flags.lock().unwrap().push(flags);
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
    request: Arc<Mutex<MemoryRequestData>>,
    compaction: Arc<Mutex<MemoryCompaction>>,
    cancellation: Arc<CancellationSource>,
}
impl Provider for MemoryProvider {
    fn estimate_input_tokens(
        &self,
        instructions: &str,
        definitions: &[ToolDefinition],
        messages: &[Message],
    ) -> Result<i64, Error> {
        self.trace.step("estimate", *self.fail_at.lock().unwrap())?;
        let mut request = self.request.lock().unwrap();
        let tokens = request
            .estimate_script
            .pop_front()
            .unwrap_or(request.estimated_input_tokens);
        let request_index = request.rendered_instructions.len().saturating_sub(1);
        request.estimates.push(InputEstimate {
            request_index,
            instructions: instructions.into(),
            definitions: definitions.to_vec(),
            messages: messages.to_vec(),
            tokens,
        });
        Ok(tokens)
    }

    fn compact<'a, 'c>(
        &'a self,
        request: parrot_core::CompactionRequest,
        cancellation: CancellationToken<'c>,
    ) -> BoxFuture<'a, Result<Option<Message>, Error>>
    where
        'c: 'a,
    {
        Box::pin(async move {
            cancellation.check().map_err(|_| Error::cancelled())?;
            self.trace.step("compact", *self.fail_at.lock().unwrap())?;
            let mut compaction = self.compaction.lock().unwrap();
            compaction.requests.push(request);
            let result = compaction.replies.pop_front().unwrap_or(Ok(None));
            if let Some(context) = compaction.contexts.pop_front() {
                let mut request = self.request.lock().unwrap();
                request.messages = context.messages;
                request.model = context.state;
            }
            if result.as_ref().is_ok_and(Option::is_some) {
                let mut state = compaction.saved_state.clone();
                state.last_request_statistic.reset();
                compaction.effect_state = Some(state);
            }
            compaction.results.push(result.clone());
            result
        })
    }

    fn invoke<'a, 'c>(
        &'a self,
        request: ProviderRequest,
        _cancellation: CancellationToken<'c>,
    ) -> BoxFuture<'a, Result<ProviderResponse, parrot_core::ProviderInvokeError>>
    where
        'c: 'a,
    {
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
                Reply::TooLarge(message) => Err(request_too_large(message).into()),
                Reply::Failed(message) => Err(Error::failed(message).into()),
                Reply::Error(error) => Err(error.into()),
                Reply::UsageThenError {
                    error,
                    input_tokens,
                } => Err(parrot_core::ProviderInvokeError {
                    error,
                    input_tokens: Some(input_tokens),
                }),
            }
        })
    }
}

struct RenderedRequest {
    instructions: String,
    maximum_output_tokens: i32,
    messages: Vec<Message>,
}

#[derive(Clone)]
struct InputEstimate {
    request_index: usize,
    instructions: String,
    definitions: Vec<ToolDefinition>,
    messages: Vec<Message>,
    tokens: i64,
}

#[derive(Clone)]
struct MemoryModel {
    configuration: ContextConfiguration,
}

struct CompactedContext {
    messages: Vec<Message>,
    state: MemoryModel,
}

struct MemoryRequestData {
    rendered_instructions: Vec<String>,
    estimates: Vec<InputEstimate>,
    estimate_script: VecDeque<i64>,
    estimated_input_tokens: i64,
    model: MemoryModel,
    instructions: String,
    messages: Vec<Message>,
    renderings: VecDeque<RenderedRequest>,
}

#[derive(Default)]
struct MemoryPromptData {
    exit_blockers: VecDeque<Vec<String>>,
}

struct MemoryState {
    events: Arc<Mutex<Vec<ConversationEvent>>>,
    started_turns: Arc<Mutex<Vec<TurnModel>>>,
    turn_activations: Arc<Mutex<usize>>,
    closed_turns: Vec<ConversationAddition>,
    disposed_turns: Vec<Result<Outcome, Error>>,
    activation_error: Option<Error>,
    disposal_error: Option<Error>,
    usage_totals: parrot_core::UsageTotals,
    skill_rows: Vec<(Skill, SkillContent)>,
    steers_after_results: VecDeque<String>,
    skill_lookups: Arc<Mutex<Vec<SkillReference>>>,
    skill_reads: Arc<Mutex<Vec<String>>>,
    trace: Trace,
    request: Arc<Mutex<MemoryRequestData>>,
    prompts: Arc<Mutex<MemoryPromptData>>,
    skill_templates: Arc<Mutex<BTreeMap<ConfigKey, String>>>,
    recorded_skill_prompts: Vec<PreparedSkillPrompt>,
    tools: MemoryTools,
    provider: MemoryProvider,
    completion_blocker_feedback: Vec<CompletionBlockerFeedback>,
    final_requests: Vec<usize>,
    retries: Vec<(String, i32)>,
    recorded_tools: Vec<Message>,
    applied_results: Vec<ToolResult>,
    recorded_catalogs: Vec<Vec<String>>,
    model: String,
    reasoning: Option<Reasoning>,
    provider_preferences: String,
    include_router_metadata: bool,
    compactions: VecDeque<CompactedContext>,
    last_turn: Option<ConversationTurn>,
    pending_messages: VecDeque<(Delivery, Message)>,
    turn_reads: AtomicUsize,
    admissions: Vec<InputAdmission>,
    cancellation: Arc<CancellationSource>,
    maximum: i32,
    status_offering: StatusOffering,
    next_status_offering: Option<StatusOffering>,
    status_steered: Vec<String>,
    status_reads: AtomicUsize,
    last_capture_request: Option<usize>,
    fail_at: Option<&'static str>,
    budget: AgentState,
    budget_read_error: Option<Error>,
    budget_write_error: Option<Error>,
    budget_write_error_after: usize,
    budget_writes: usize,
    compaction: Arc<Mutex<MemoryCompaction>>,
}
impl MemoryState {
    fn compact_with(&mut self, text: &str) {
        self.compaction
            .lock()
            .unwrap()
            .replies
            .push_back(Ok(Some(Message {
                role: Role::System,
                contents: vec![Content::Text(text.into())],
                ..Message::default()
            })));
    }

    fn prompts(&self) -> std::sync::MutexGuard<'_, MemoryPromptData> {
        self.prompts.lock().unwrap()
    }
    fn request(&self) -> std::sync::MutexGuard<'_, MemoryRequestData> {
        self.request.lock().unwrap()
    }
    fn new(replies: impl IntoIterator<Item = Reply>) -> Self {
        let trace = Trace::default();
        let request = Arc::new(Mutex::new(MemoryRequestData {
            rendered_instructions: Vec::new(),
            estimates: Vec::new(),
            estimate_script: VecDeque::new(),
            estimated_input_tokens: 0,
            model: MemoryModel {
                configuration: ContextConfiguration {
                    canonical_model: "memory/model".into(),
                    context_window: 0,
                    maximum_input_tokens: 0,
                    maximum_output_tokens: 0,
                    trigger_percent: 90,
                    context_limit: None,
                },
            },
            instructions: "core request".into(),
            messages: Vec::new(),
            renderings: VecDeque::new(),
        }));
        let cancellation = Arc::new(CancellationSource::default());
        let compaction = Arc::new(Mutex::new(MemoryCompaction::default()));
        let mut state = Self {
            events: Arc::default(),
            started_turns: Arc::default(),
            turn_activations: Arc::new(Mutex::new(0)),
            closed_turns: Vec::new(),
            disposed_turns: Vec::new(),
            activation_error: None,
            disposal_error: None,
            usage_totals: parrot_core::UsageTotals::default(),
            skill_rows: Vec::new(),
            steers_after_results: VecDeque::new(),
            skill_lookups: Arc::default(),
            skill_reads: Arc::default(),
            request: request.clone(),
            prompts: Arc::default(),
            skill_templates: Arc::new(Mutex::new([
                (
                    ConfigKey::TruncatedToolCallTemplate,
                    "truncated tool calls".into(),
                ),
                (
                    ConfigKey::InvalidToolCallTemplate,
                    "invalid tool calls".into(),
                ),
                (
                    ConfigKey::FinalRequestTemplate,
                    "Finish the current turn.".into(),
                ),
                (
                    ConfigKey::RestoredToolsTemplate,
                    "Tools are available again.".into(),
                ),
                (ConfigKey::RunawayTemplate, RUNAWAY_NOTICE.into()),
                (
                    ConfigKey::SkillSelectedTemplate,
                    "<skill>\n<name>{name}</name>\n<path>{path}</path>\n{content}\n</skill>".into(),
                ),
                (
                    ConfigKey::SkillUnavailableTemplate,
                    "<skill-unavailable>\n<name>{name}</name>\n<message>{message}</message>\n</skill-unavailable>".into(),
                ),
                (
                    ConfigKey::SkillTooLargeTemplate,
                    "<skill-too-large>\n<name>{name}</name>\n<path>{path}</path>\n<message>The skill instructions exceed the {limit_bytes}-byte injection limit and were not loaded.</message>\n</skill-too-large>".into(),
                ),
            ].into())),
            recorded_skill_prompts: Vec::new(),
            trace: trace.clone(),
            tools: MemoryTools {
                trace: trace.clone(),
                fail_at: Arc::default(),
                requests: Arc::default(),
                replies: Arc::default(),
                receipts: Arc::default(),
                flags: Arc::default(),
            },
            provider: MemoryProvider {
                trace,
                replies: Arc::new(Mutex::new(replies.into_iter().collect())),
                fail_at: Arc::default(),
                requests: Arc::default(),
                request,
                compaction: compaction.clone(),
                cancellation: cancellation.clone(),
            },
            completion_blocker_feedback: Vec::new(),
            final_requests: Vec::new(),
            retries: Vec::new(),
            recorded_tools: Vec::new(),
            applied_results: Vec::new(),
            recorded_catalogs: Vec::new(),
            model: "memory/model".into(),
            reasoning: None,
            provider_preferences: String::new(),
            include_router_metadata: false,
            compactions: VecDeque::new(),
            last_turn: None,
            pending_messages: VecDeque::new(),
            turn_reads: AtomicUsize::new(0),
            admissions: Vec::new(),
            cancellation,
            maximum: 10,
            status_offering: StatusOffering::default(),
            next_status_offering: None,
            status_steered: Vec::new(),
            status_reads: AtomicUsize::new(0),
            last_capture_request: None,
            fail_at: None,
            budget: AgentState::default(),
            budget_read_error: None,
            budget_write_error: None,
            budget_write_error_after: 0,
            budget_writes: 0,
            compaction,
        };
        state.enqueue_message(Delivery::Queue, "question");
        state
    }
    fn enqueue_message(&mut self, delivery: Delivery, content: &str) {
        self.pending_messages.push_back((
            delivery,
            Message {
                role: Role::User,
                contents: vec![Content::Text(content.into())],
                ..Message::default()
            },
        ));
    }
    fn loaded_skill_paths(&self) -> Vec<String> {
        self.events
            .lock()
            .unwrap()
            .iter()
            .filter_map(|event| match event {
                ConversationEvent::SkillLoaded { path } => Some(path.clone()),
                _ => None,
            })
            .collect()
    }
    fn append_turn_message(&mut self, message: Message) {
        if message.role == Role::User {
            self.last_turn = None;
        }
        let turn = self.last_turn.get_or_insert_with(|| ConversationTurn {
            messages: Vec::new(),
        });
        turn.messages.push(message);
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
    fn has_pending_input(&self) -> BoxFuture<'_, Result<bool, Error>> {
        Box::pin(async move {
            if self.fail_at == Some("pending_input") {
                self.trace.step("pending_input", self.fail_at)?;
            }
            Ok(!self.pending_messages.is_empty())
        })
    }
    fn capture_options(&self) -> TurnOptions {
        TurnOptions {
            maximum_requests: self.maximum,
            model: self.model.clone(),
            reasoning: self.reasoning.clone(),
            provider_preferences: self.provider_preferences.clone(),
            include_router_metadata: self.include_router_metadata,
        }
    }
    fn prime_context(&mut self, estimate: i64) {
        let mut request = self.request();
        request.model.configuration.context_window = 1000;
        request.model.configuration.maximum_output_tokens = 1;
        request.messages = vec![Message {
            role: Role::User,
            contents: vec![Content::Text("question".into())],
            ..Message::default()
        }];
        request.estimated_input_tokens = estimate;
    }
    fn capture_catalogs(&self) -> Vec<Vec<String>> {
        self.provider
            .requests
            .lock()
            .unwrap()
            .iter()
            .map(|request| request.tools.iter().map(|tool| tool.name.clone()).collect())
            .collect()
    }
    fn capture_request_estimates(&self) -> Vec<InputEstimate> {
        let request = self.request();
        let estimates = &request.estimates;
        (0..request.rendered_instructions.len())
            .map(|request_index| {
                estimates
                    .iter()
                    .rev()
                    .find(|estimate| estimate.request_index == request_index)
                    .unwrap()
                    .clone()
            })
            .collect()
    }
    fn run(&mut self) -> Result<Outcome, Error> {
        *self.tools.fail_at.lock().unwrap() = self.fail_at;
        *self.provider.fail_at.lock().unwrap() = self.fail_at;
        let tools = self.tools.clone();
        let provider = self.provider.clone();
        self.run_with(&tools, &provider)
    }
    fn apply_options(&mut self, options: &TurnOptions) {
        self.maximum = options.maximum_requests;
        self.model = options.model.clone();
        self.reasoning = options.reasoning.clone();
        self.provider_preferences = options.provider_preferences.clone();
        self.include_router_metadata = options.include_router_metadata;
    }
    fn run_with(&mut self, tools: &dyn Tools, provider: &dyn Provider) -> Result<Outcome, Error> {
        self.compaction
            .lock()
            .unwrap()
            .contexts
            .extend(self.compactions.drain(..));
        self.budget.restart();
        let source = self.cancellation.clone();
        finish(run(
            self,
            tools,
            provider,
            CancellationToken::new(source.as_ref()),
        ))
    }
}
impl States for MemoryState {
    fn read_status_offering(&self) -> BoxFuture<'_, Result<StatusOffering, Error>> {
        if self.status_reads.fetch_add(1, Ordering::SeqCst) == 0 {
            return Box::pin(async { Ok(self.status_offering.clone()) });
        }
        Box::pin(async move { finish(self.compaction.lock().unwrap().status.offering()) })
    }
    fn add_conversation_messages(
        &mut self,
        addition: ConversationAddition,
    ) -> BoxFuture<'_, Result<ConversationReceipt, Error>> {
        Box::pin(async move {
            let mut sequence = 0;
            match &addition.persistence {
                ConversationPersistence::Append { origin, event } => match event {
                    ConversationEvent::Notice { kind, .. }
                        if kind == "tool_availability_restored_prompt" =>
                    {
                        return Ok(ConversationReceipt::default());
                    }
                    ConversationEvent::Notice { kind, .. }
                        if kind == "final_provider_request_prompt" =>
                    {
                        self.step("final_request")?;
                        let count = self.request().rendered_instructions.len();
                        self.final_requests.push(count);
                    }
                    ConversationEvent::Notice { kind, notice } if kind == "status" => {
                        self.step("steer_status")?;
                        assert!(notice.starts_with("Status prompt injected\n"));
                        let content = addition.messages[0]
                            .contents
                            .iter()
                            .map(|part| match part {
                                Content::Text(text) => text.as_str(),
                                _ => "",
                            })
                            .collect::<String>();
                        self.status_steered.push(content);
                    }
                    ConversationEvent::Notice { kind, .. } if kind == "skill_prompt" => {
                        self.step("record_skill_prompt")?;
                        let content = addition.messages[0]
                            .contents
                            .iter()
                            .filter_map(|content| match content {
                                Content::Text(text) => Some(text.as_str()),
                                _ => None,
                            })
                            .collect();
                        self.recorded_skill_prompts.push(PreparedSkillPrompt {
                            content,
                            loaded_paths: Vec::new(),
                        });
                    }
                    ConversationEvent::Retry { reason, attempt } => {
                        self.step("retry")?;
                        self.retries.push((reason.clone(), *attempt));
                        if let Some(next) = self.next_status_offering.take() {
                            self.status_offering = next;
                        }
                    }
                    ConversationEvent::Message if *origin == ConversationOrigin::Model => {
                        self.step("record_tools")?;
                        self.recorded_tools.extend(addition.messages.clone());
                        let catalog = self
                            .request()
                            .estimates
                            .last()
                            .expect("estimated provider request")
                            .definitions
                            .iter()
                            .map(|definition| definition.name.clone())
                            .collect();
                        self.recorded_catalogs.push(catalog);
                        sequence = i64::try_from(self.recorded_tools.len()).unwrap();
                    }
                    _ => {}
                },
                ConversationPersistence::ToolResult { result, .. } => {
                    self.step("record_results")?;
                    self.applied_results.push(result.clone());
                    if let Some(steer) = self.steers_after_results.pop_front() {
                        self.enqueue_message(Delivery::Steer, &steer);
                    }
                    if let Some(next) = self.next_status_offering.take() {
                        self.status_offering = next;
                    }
                }
                ConversationPersistence::Committed(_) => {}
                ConversationPersistence::CompletionBlockerFeedback(feedback) => {
                    self.step("completion_blocker_feedback")?;
                    self.completion_blocker_feedback.push(feedback.clone());
                }
                ConversationPersistence::CloseTurn { .. } => {
                    self.step("complete")?;
                    self.closed_turns.push(addition.clone());
                }
            }
            for message in addition.messages {
                if !matches!(
                    &addition.persistence,
                    ConversationPersistence::Committed(_)
                        | ConversationPersistence::ToolResult { .. }
                        | ConversationPersistence::CloseTurn { .. }
                        | ConversationPersistence::CompletionBlockerFeedback(_)
                        | ConversationPersistence::Append {
                            origin: ConversationOrigin::Model,
                            ..
                        }
                ) {
                    self.request().messages.push(message.clone());
                }
                if !matches!(
                    &addition.persistence,
                    ConversationPersistence::Append {
                        event: ConversationEvent::Notice { kind, .. },
                        ..
                    } if kind == "final_provider_request_prompt"
                ) {
                    self.append_turn_message(message);
                }
            }
            Ok(ConversationReceipt {
                inserted: true,
                sequence,
            })
        })
    }

    fn get_config(&self, key: ConfigKey) -> BoxFuture<'_, Result<ConfigValue, Error>> {
        Box::pin(async move {
            match key {
                ConfigKey::Turn => Ok(ConfigValue::Turn(self.capture_options())),
                ConfigKey::Context => {
                    self.trace.step("context_configuration", self.fail_at)?;
                    Ok(ConfigValue::Context(
                        self.request().model.configuration.clone(),
                    ))
                }
                key => {
                    self.trace.step("render_template", self.fail_at)?;
                    let templates = self.skill_templates.lock().unwrap();
                    let text = templates
                        .get(&key)
                        .ok_or_else(|| Error::failed("unknown template"))?;
                    let allowed = [
                        "name",
                        "path",
                        "content",
                        "message",
                        "limit_bytes",
                        "percentage",
                        "estimated_tokens",
                        "context_limit",
                        "notification_interval",
                        "trigger",
                        "trigger_tokens",
                        "instruction",
                        "skills",
                        "depth",
                        "session_id",
                        "parent_session_id",
                        "parent_session_name",
                        "agent_name",
                    ]
                    .map(str::to_owned);
                    Ok(ConfigValue::PromptTemplate(Arc::new(
                        PromptTemplate::compile(
                            &format!("test.{key:?}"),
                            text,
                            "scalar",
                            &allowed,
                            &[],
                        )?,
                    )))
                }
            }
        })
    }
    fn get_agent_state(&self) -> BoxFuture<'_, Result<AgentState, Error>> {
        Box::pin(async move {
            if let Some(error) = &self.budget_read_error {
                return Err(error.clone());
            }
            Ok(self
                .compaction
                .lock()
                .unwrap()
                .effect_state
                .clone()
                .unwrap_or_else(|| self.budget.clone()))
        })
    }
    fn set_agent_state(&mut self, state: AgentState) -> BoxFuture<'_, Result<(), Error>> {
        Box::pin(async move {
            self.budget_writes += 1;
            if self.budget_writes > self.budget_write_error_after
                && let Some(error) = &self.budget_write_error
            {
                return Err(error.clone());
            }
            let mut compaction = self.compaction.lock().unwrap();
            compaction.saved_state = state.clone();
            compaction.effect_state = None;
            self.budget = state;
            Ok(())
        })
    }
    fn collect_completion_blockers(&mut self) -> BoxFuture<'_, Result<Vec<String>, Error>> {
        Box::pin(async move {
            self.step("exit_blockers")?;
            Ok(self.prompts().exit_blockers.pop_front().unwrap_or_default())
        })
    }

    fn capture_system_prompt(&mut self) -> BoxFuture<'_, Result<String, Error>> {
        Box::pin(async move {
            self.step("instruction_state")?;
            self.trace.step("render", self.fail_at)?;
            let request_index = self.provider.requests.lock().unwrap().len();
            let fresh = self.last_capture_request != Some(request_index);
            let instructions = if fresh {
                let rendered_instructions = {
                    let mut request = self.request();
                    if let Some(rendered) = request.renderings.pop_front() {
                        request.instructions = rendered.instructions;
                        request.model.configuration.maximum_output_tokens =
                            rendered.maximum_output_tokens;
                        request.messages = rendered.messages;
                    }
                    let instructions = request.instructions.clone();
                    request.rendered_instructions.push(instructions.clone());
                    request.instructions.clone()
                };
                self.last_capture_request = Some(request_index);
                rendered_instructions
            } else {
                self.request().instructions.clone()
            };
            Ok(instructions)
        })
    }
    fn get_last_turn(&self) -> BoxFuture<'_, Result<Option<ConversationTurn>, Error>> {
        Box::pin(async move {
            self.turn_reads.fetch_add(1, Ordering::SeqCst);
            if self.fail_at == Some("last_turn") {
                self.trace.step("last_turn", self.fail_at)?;
            }
            Ok(self.last_turn.clone())
        })
    }
    fn promote_pending_inputs(
        &mut self,
        admission: InputAdmission,
    ) -> BoxFuture<'_, Result<Vec<PromotedInput>, Error>> {
        Box::pin(async move {
            self.step("promote")?;
            self.status_reads.store(0, Ordering::SeqCst);
            self.admissions.push(admission);
            let mut promoted = Vec::new();
            while let Some(index) = self
                .pending_messages
                .iter()
                .position(|(delivery, _)| *delivery == Delivery::Steer)
            {
                promoted.push(self.pending_messages.remove(index).unwrap());
            }
            if promoted.is_empty() && admission == InputAdmission::SteersOrNextQueued {
                promoted.extend(self.pending_messages.pop_front());
            }
            Ok(promoted
                .into_iter()
                .map(|(_, message)| PromotedInput {
                    message,
                    publication: Default::default(),
                })
                .collect())
        })
    }
    fn read_usage_totals(&self) -> BoxFuture<'_, Result<parrot_core::UsageTotals, Error>> {
        Box::pin(async move { Ok(self.usage_totals) })
    }
    fn read_conversation_state(
        &self,
    ) -> BoxFuture<'_, Result<parrot_core::ConversationState, Error>> {
        Box::pin(async move {
            self.trace.step("conversation", self.fail_at)?;
            let messages = self.request().messages.clone();
            let first_sequence = 1;
            let mut owners = std::collections::BTreeMap::new();
            let history = messages
                .into_iter()
                .enumerate()
                .map(|(index, message)| {
                    let sequence = first_sequence + index as i64;
                    let tool_owner_sequence = owners.get(&message.tool_call_id).copied();
                    for call in &message.tool_calls {
                        owners.insert(call.id.clone(), sequence);
                    }
                    parrot_core::ConversationHistoryItem {
                        sequence,
                        tool_owner_sequence,
                        is_compaction_status: false,
                        message,
                    }
                })
                .collect();
            Ok(parrot_core::ConversationState {
                summary: None,
                history,
            })
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
            "catalog",
            "promote",
            "instruction_state",
            "render",
            "conversation",
            "context_configuration",
            "estimate",
            "conversation",
            "context_configuration",
            "estimate",
            "invoke",
            "record_tools",
            "execute",
            "record_results",
            "catalog",
            "promote",
            "instruction_state",
            "render",
            "conversation",
            "context_configuration",
            "estimate",
            "conversation",
            "context_configuration",
            "estimate",
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
            ("truncated tool calls".into(), 1),
            ("invalid tool calls".into(), 2),
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
    assert_eq!(
        state.run(),
        Ok(Outcome::BudgetExhausted(RUNAWAY_NOTICE.into()))
    );
    assert_eq!(state.request().rendered_instructions.len(), 5);
    assert!(
        state
            .tools
            .flags
            .lock()
            .unwrap()
            .iter()
            .all(|flags| flags.final_phase)
    );
    assert!(
        state
            .capture_catalogs()
            .iter()
            .all(|tools| tools == &["read"])
    );
    assert!(state.final_requests.contains(&0));
    assert_eq!(state.final_requests, [0]);
    assert_eq!(
        state
            .trace
            .values()
            .iter()
            .filter(|entry| *entry == "execute")
            .count(),
        5
    );
    assert_eq!(
        &state.trace.values()[state.trace.values().len() - 2..],
        ["record_results", "render_template"]
    );
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
    assert_eq!(
        state.run(),
        Ok(Outcome::BudgetExhausted(RUNAWAY_NOTICE.into()))
    );
    assert_eq!(state.request().rendered_instructions.len(), 1);
}

#[test]
fn caller_budget_configuration_controls_request_limits_and_final_phase() {
    for maximum in [1, 1_000_000] {
        let mut state = MemoryState::new([Reply::invalid("bad"), Reply::final_response("answer")]);
        state.maximum = maximum;
        let tools = state.tools.clone();
        let provider = state.provider.clone();

        assert_eq!(
            finish(run(
                &mut state,
                &tools,
                &provider,
                CancellationToken::new(provider.cancellation.as_ref())
            )),
            if maximum == 1_000_000 {
                Ok(Outcome::Completed("answer".into()))
            } else {
                Ok(Outcome::BudgetExhausted(RUNAWAY_NOTICE.into()))
            }
        );
        assert_eq!(state.retries, [("invalid tool calls".into(), 1)]);
        assert_eq!(
            state.request().rendered_instructions.len(),
            if maximum == 1_000_000 { 2 } else { 1 }
        );
        assert!(state.capture_catalogs().iter().all(|tools| {
            if maximum == 1_000_000 {
                tools == &["read", "write"]
            } else {
                tools == &["read"]
            }
        }));
        assert_eq!(
            state.final_requests,
            if maximum == 1_000_000 {
                vec![]
            } else {
                vec![0]
            }
        );
        assert!(state.recorded_tools.is_empty());
        assert!(state.tools.requests.lock().unwrap().is_empty());
        assert_eq!(
            state.trace.values().last().unwrap(),
            if maximum == 1_000_000 {
                "complete"
            } else {
                "render_template"
            }
        );
    }
}

#[test]
fn exhausted_budget_returns_without_provider_or_terminal_state_effects() {
    let mut state = MemoryState::new([]);
    state.maximum = 0;
    assert_eq!(
        state.run(),
        Ok(Outcome::BudgetExhausted(RUNAWAY_NOTICE.into()))
    );
    assert_eq!(
        state.trace.values(),
        ["catalog", "promote", "render_template"]
    );
    assert!(state.provider.requests.lock().unwrap().is_empty());
    assert!(state.tools.requests.lock().unwrap().is_empty());
    assert!(state.pending_messages.is_empty());
    assert_eq!(state.admissions, [InputAdmission::SteersOrNextQueued]);
    assert_eq!(
        state
            .trace
            .values()
            .iter()
            .filter(|step| *step == "catalog")
            .count(),
        1
    );
    assert_eq!(
        state.last_turn.as_ref().unwrap().messages[0].role,
        Role::User
    );
}

#[test]
fn large_request_budget_keeps_early_requests_in_normal_phase() {
    let mut state = MemoryState::new([
        Reply::tools("call1"),
        Reply::tools("call2"),
        Reply::final_response("answer"),
    ]);
    state.maximum = 1_000_000;
    assert_eq!(state.run(), Ok(Outcome::Completed("answer".into())));
    assert_eq!(state.request().rendered_instructions.len(), 3);
    assert!(
        state
            .capture_catalogs()
            .iter()
            .all(|tools| tools == &["read", "write"])
    );
    assert!(state.final_requests.is_empty());
    assert!(
        state
            .tools
            .flags
            .lock()
            .unwrap()
            .iter()
            .all(|flags| !flags.final_phase)
    );
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
        .prompts()
        .exit_blockers
        .push_back(vec!["unfinished goal".into()]);
    assert_eq!(state.run(), Ok(Outcome::Completed("answer".into())));
    assert_eq!(state.retries, [("invalid tool calls".into(), 1)]);
    assert_eq!(
        state
            .trace
            .values()
            .iter()
            .filter(|entry| *entry == "catalog")
            .count(),
        3
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
    assert_eq!(state.capture_catalogs()[1], ["read", "write"]);
    assert_eq!(state.capture_catalogs()[2], ["read"]);
    assert_eq!(state.final_requests, [2]);
}

#[test]
fn oversized_requests_compact_once_until_a_successful_response_resets_the_guard() {
    let mut state = MemoryState::new([
        Reply::TooLarge("first"),
        Reply::tools("call"),
        Reply::TooLarge("second"),
        Reply::final_response("answer"),
    ]);
    state.compact_with("first summary");
    state.compact_with("second summary");
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
    assert_eq!(state.request().rendered_instructions.len(), 4);
}

#[test]
fn a_second_oversized_failure_propagates_without_repeating_compaction() {
    let mut state = MemoryState::new([Reply::TooLarge("first"), Reply::TooLarge("second")]);
    state.compact_with("core summary");
    assert_eq!(state.run(), Err(request_too_large("second")));
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
    assert_eq!(state.run(), Err(request_too_large("original")));
    assert_eq!(state.compaction.lock().unwrap().requests.len(), 1);
    assert_eq!(state.provider.requests.lock().unwrap().len(), 1);
}

#[test]
fn fresh_invocations_use_supplied_settings_and_refresh_tools_each_step() {
    let mut state = MemoryState::new([
        Reply::invalid("invalid"),
        Reply::TooLarge("oversized"),
        Reply::tools("call"),
        Reply::final_response("candidate"),
        Reply::final_response("first answer"),
        Reply::final_response("second answer"),
    ]);
    state.maximum = 0;
    state.model = "unused/state-model".into();
    state
        .prompts()
        .exit_blockers
        .push_back(vec!["work remains".into()]);
    state.compact_with("core summary");
    let tools = state.tools.clone();
    let provider = state.provider.clone();
    let options = TurnOptions {
        maximum_requests: 10,
        model: "first/model".into(),
        reasoning: Some(Reasoning {
            effort: "first effort".into(),
            summary: "first summary".into(),
        }),
        provider_preferences: " { \"raw\" : [1.2300] } ".into(),
        include_router_metadata: true,
    };
    state.budget = AgentState::default();

    assert_eq!(
        finish({
            state.apply_options(&options);
            run_turn(
                &mut state,
                &tools,
                &provider,
                CancellationToken::new(provider.cancellation.as_ref()),
            )
        }),
        Ok(Outcome::Completed("first answer".into()))
    );
    assert_eq!(state.request().rendered_instructions.len(), 5);
    assert_eq!(state.turn_reads.load(Ordering::SeqCst), 5);
    assert_eq!(state.retries, [("invalid tool calls".into(), 1)]);
    assert_eq!(state.applied_results.len(), 1);
    assert_eq!(state.completion_blocker_feedback.len(), 1);
    assert!(provider.requests.lock().unwrap().iter().all(|request| {
        request.model == options.model
            && request.reasoning == options.reasoning
            && request.provider_preferences == options.provider_preferences
            && request.include_router_metadata == options.include_router_metadata
    }));
    let trace = state.trace.values();
    assert_eq!(trace.iter().filter(|step| *step == "catalog").count(), 5);
    assert_eq!(trace.iter().filter(|step| *step == "compact").count(), 1);
    assert_eq!(trace.iter().filter(|step| *step == "complete").count(), 1);

    let options = TurnOptions {
        maximum_requests: 1,
        model: "second/model".into(),
        reasoning: None,
        provider_preferences: "second preferences".into(),
        include_router_metadata: false,
    };
    state.enqueue_message(Delivery::Queue, "second question");

    assert_eq!(
        finish({
            state.apply_options(&options);
            run_turn(
                &mut state,
                &tools,
                &provider,
                CancellationToken::new(provider.cancellation.as_ref()),
            )
        }),
        Ok(Outcome::Completed("second answer".into()))
    );
    {
        let requests = provider.requests.lock().unwrap();
        assert_eq!(requests[5].model, options.model);
        assert_eq!(requests[5].reasoning, options.reasoning);
        assert_eq!(
            requests[5].provider_preferences,
            options.provider_preferences
        );
        assert_eq!(
            requests[5].include_router_metadata,
            options.include_router_metadata
        );
    }
    assert!(state.final_requests.contains(&5));
    assert_eq!(state.turn_reads.load(Ordering::SeqCst), 6);
    let trace = state.trace.values();
    assert_eq!(trace.iter().filter(|step| *step == "catalog").count(), 6);
    assert_eq!(trace.iter().filter(|step| *step == "complete").count(), 2);

    *tools.fail_at.lock().unwrap() = Some("catalog");

    assert_eq!(
        finish({
            state.apply_options(&options);
            run(
                &mut state,
                &tools,
                &provider,
                CancellationToken::new(provider.cancellation.as_ref()),
            )
        }),
        Ok(Outcome::Completed(String::new()))
    );
    assert_eq!(state.turn_reads.load(Ordering::SeqCst), 7);
    assert_eq!(state.trace.values(), trace);
    assert_eq!(provider.requests.lock().unwrap().len(), 6);
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
    let tools = state.tools.clone();
    let provider = state.provider.clone();

    state.budget = AgentState::default();

    assert_eq!(
        finish(run_turn(
            &mut state,
            &tools,
            &provider,
            CancellationToken::new(provider.cancellation.as_ref())
        )),
        Err(Error::cancelled())
    );
    assert!(state.trace.values().is_empty());
    assert!(provider.requests.lock().unwrap().is_empty());
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
        ("catalog", Reply::final_response("response")),
        ("promote", Reply::final_response("response")),
        ("render", Reply::final_response("response")),
        ("conversation", Reply::final_response("response")),
        ("invoke", Reply::final_response("response")),
        ("retry", Reply::invalid("response")),
        ("record_tools", Reply::tools("response")),
        ("execute", Reply::tools("response")),
        ("record_results", Reply::tools("response")),
        ("exit_blockers", Reply::final_response("response")),
        (
            "completion_blocker_feedback",
            Reply::final_response("response"),
        ),
        ("complete", Reply::final_response("response")),
    ] {
        let mut state = MemoryState::new([reply]);
        state.fail_at = Some(stage);
        if stage == "completion_blocker_feedback" {
            state
                .prompts()
                .exit_blockers
                .push_back(vec!["answer the ask".into()]);
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
    assert_eq!(
        state.run(),
        Ok(Outcome::BudgetExhausted(RUNAWAY_NOTICE.into()))
    );
    assert_eq!(state.request().rendered_instructions.len(), 6);
    assert!(state.final_requests.contains(&0));
    assert_eq!(state.final_requests, [0, 2]);
    assert_eq!(
        state
            .trace
            .values()
            .iter()
            .filter(|entry| *entry == "catalog")
            .count(),
        6
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
    state
        .prompts()
        .exit_blockers
        .push_back(vec!["work remains".into()]);
    assert_eq!(
        state.run(),
        Ok(Outcome::BudgetExhausted(RUNAWAY_NOTICE.into()))
    );
    assert_eq!(state.request().rendered_instructions.len(), 9);
    let catalogs = state.capture_catalogs();
    assert_eq!(catalogs[0], ["read", "write"]);
    assert_eq!(catalogs[1], ["read"]);
    assert_eq!(catalogs[2], ["read"]);
    assert_eq!(catalogs[3], ["read", "write"]);
    assert_eq!(state.final_requests, [1]);
    assert!(catalogs[4..].iter().all(|tools| tools == &["read"]));
    assert!(state.tools.flags.lock().unwrap()[3].final_phase);
    assert!(
        state.tools.flags.lock().unwrap()[4..]
            .iter()
            .all(|flags| flags.final_phase)
    );
    assert_eq!(state.completion_blocker_feedback.len(), 1);
    assert_eq!(
        state.completion_blocker_feedback[0]
            .candidate
            .assistant_text,
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
    assert_eq!(
        &state.trace.values()[state.trace.values().len() - 2..],
        ["record_results", "render_template"]
    );
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
    let cancellation = CancellationToken::new(&source);
    let results = finish(tools.execute(
        ToolExecutionRequest {
            calls: calls.clone(),
            receipt: ToolCallReceipt {
                assistant_sequence: 41,
            },
            flags: TurnFlags { final_phase: false },
            starts_batch: true,
            cancel_remaining: false,
        },
        cancellation,
    ))
    .unwrap();
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
    let response = finish(provider.invoke(
        ProviderRequest::default(),
        parrot_core::CancellationToken::uncancelled(),
    ))
    .unwrap();
    assert_eq!(response.assistant_text, "standalone");
    assert!(response.tool_calls.is_empty());
    assert!(state.provider.replies.lock().unwrap().is_empty());
    assert_eq!(state.trace.values(), ["catalog", "execute", "invoke"]);
}

struct EchoProvider(MemoryProvider);
impl Provider for EchoProvider {
    fn estimate_input_tokens(
        &self,
        instructions: &str,
        definitions: &[ToolDefinition],
        messages: &[Message],
    ) -> Result<i64, Error> {
        self.0
            .estimate_input_tokens(instructions, definitions, messages)
    }
    fn compact<'a, 'c>(
        &'a self,
        request: parrot_core::CompactionRequest,
        cancellation: CancellationToken<'c>,
    ) -> BoxFuture<'a, Result<Option<Message>, Error>>
    where
        'c: 'a,
    {
        self.0.compact(request, cancellation)
    }

    fn invoke<'a, 'c>(
        &'a self,
        request: ProviderRequest,
        _cancellation: CancellationToken<'c>,
    ) -> BoxFuture<'a, Result<ProviderResponse, parrot_core::ProviderInvokeError>>
    where
        'c: 'a,
    {
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
        states.run_with(&tools, &EchoProvider(provider.clone())),
        Ok(Outcome::Completed("core request".into()))
    );
    assert!(states.provider.replies.lock().unwrap().is_empty());
    assert_eq!(states.request().rendered_instructions.len(), 2);
}

#[test]
fn core_selects_phase_and_estimates_the_exact_advertised_tools() {
    let mut states = MemoryState::new([Reply::tools("call"), Reply::final_response("answer")]);
    states.maximum = 2;
    assert_eq!(states.run(), Ok(Outcome::Completed("answer".into())));
    assert_eq!(states.request().rendered_instructions.len(), 2);
    assert!(!states.tools.flags.lock().unwrap()[0].final_phase);
    assert_eq!(states.final_requests, [1]);
    let requests = states.provider.requests.lock().unwrap();
    let estimates = states.capture_request_estimates();
    assert_eq!(requests.len(), estimates.len());
    for (request, estimate) in requests.iter().zip(&estimates) {
        assert_eq!(request.tools, estimate.definitions);
        assert_eq!(request.instructions, estimate.instructions);
        assert_eq!(request.messages, estimate.messages);
        assert_eq!(estimate.tokens, 0);
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
        assert_eq!(
            states.recorded_tools,
            [Message {
                role: Role::Assistant,
                contents: vec![Content::Text(response.assistant_text)],
                tool_calls: response.tool_calls,
                provider_id: response.provider_id,
                provider_content: response.provider_content,
                ..Message::default()
            }]
        );
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
    assert_eq!(
        states.recorded_tools,
        [Message {
            role: Role::Assistant,
            contents: vec![Content::Text(response.assistant_text)],
            tool_calls: response.tool_calls,
            provider_id: response.provider_id,
            provider_content: response.provider_content,
            ..Message::default()
        }]
    );
    assert_eq!(states.tools.requests.lock().unwrap().len(), 1);
    assert_eq!(states.applied_results.len(), 1);
}

#[test]
fn fixed_error_metadata_and_original_oversized_error_survive_core_recovery() {
    let error = Error {
        detail: None,
        kind: ErrorKind::RequestTooLarge,
        code: ErrorCode::Http,
        message: "request rejected".into(),
        response_body: "unredacted top-level body".into(),
    };
    let mut states = MemoryState::new([Reply::Error(error.clone())]);
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

#[test]
fn oversized_recovery_requests_complete_compaction_before_retrying() {
    let mut state = MemoryState::new([
        Reply::TooLarge("oversized"),
        Reply::final_response("answer"),
    ]);
    state.compact_with("committed summary");
    assert_eq!(state.run(), Ok(Outcome::Completed("answer".into())));
    assert_eq!(state.compaction.lock().unwrap().requests.len(), 1);
    assert_eq!(state.provider.requests.lock().unwrap().len(), 2);
    let trace = state.trace.values();
    let compacted = trace.iter().position(|step| step == "compact").unwrap();
    assert_eq!(
        trace[compacted + 1..]
            .iter()
            .filter(|step| *step == "invoke")
            .count(),
        1
    );
}

#[test]
fn failed_compaction_propagates_without_retrying_the_turn_request() {
    let mut state = MemoryState::new([Reply::TooLarge("oversized")]);
    let error = Error {
        detail: None,
        kind: ErrorKind::Failed,
        code: ErrorCode::Http,
        message: "compaction provider failed".into(),
        response_body: "opaque response body".into(),
    };
    state
        .compaction
        .lock()
        .unwrap()
        .replies
        .push_back(Err(error.clone()));
    assert_eq!(state.run(), Err(error.clone()));
    assert_eq!(state.compaction.lock().unwrap().results, [Err(error)]);
    assert_eq!(state.provider.requests.lock().unwrap().len(), 1);
    assert_eq!(state.trace.values().last().unwrap(), "compact");
}

fn status_offer(entries: &[(&str, bool, &str)]) -> StatusOffering {
    entries
        .iter()
        .map(|(key, is_auto_update, hash)| {
            (
                StatusKey::new(*key),
                StatusOffer {
                    hash: (*hash).into(),
                    text: (*key).into(),
                    is_auto_update: *is_auto_update,
                },
            )
        })
        .collect()
}

fn status_standing(entries: &[(&str, &str)]) -> StatusStanding {
    entries
        .iter()
        .map(|(key, hash)| (StatusKey::new(*key), (*hash).into()))
        .collect()
}

#[test]
fn steering_commits_the_initial_assembly_once_before_the_first_request() {
    let mut state = MemoryState::new([Reply::final_response("answer")]);
    state.status_offering = status_offer(&[("selection", true, "h1"), ("time", false, "t1")]);
    assert_eq!(state.run(), Ok(Outcome::Completed("answer".into())));
    assert_eq!(state.status_steered, ["selection\n\ntime"]);
    assert_eq!(
        state.budget.standing_statuses,
        status_standing(&[("selection", "h1"), ("time", "t1")])
    );
    let request_messages = state.request().messages.clone();
    let steered = request_messages
        .iter()
        .filter(|message| message.role == Role::System)
        .map(|message| match &message.contents[0] {
            Content::Text(text) => text.clone(),
            _ => String::new(),
        })
        .collect::<Vec<_>>();
    assert_eq!(steered, ["selection\n\ntime".to_string()]);
    assert!(
        state
            .trace
            .values()
            .iter()
            .filter(|step| *step == "steer_status")
            .count()
            == 1
    );
}

#[test]
fn unchanged_hashes_never_steer_again_across_turns() {
    let mut state = MemoryState::new([
        Reply::final_response("answer"),
        Reply::final_response("second answer"),
    ]);
    state.status_offering = status_offer(&[("selection", true, "h1"), ("time", false, "t1")]);
    assert_eq!(state.run(), Ok(Outcome::Completed("answer".into())));
    state.enqueue_message(Delivery::Queue, "follow up");
    assert_eq!(state.run(), Ok(Outcome::Completed("second answer".into())));
    assert_eq!(state.status_steered, ["selection\n\ntime"]);
    assert_eq!(
        state.budget.standing_statuses,
        status_standing(&[("selection", "h1"), ("time", "t1")])
    );
    let steered = state
        .trace
        .values()
        .iter()
        .filter(|step| *step == "steer_status")
        .count();
    assert_eq!(steered, 1);
}

#[test]
fn changed_auto_hash_steers_again_at_the_next_safe_point() {
    let mut state = MemoryState::new([Reply::tools("call"), Reply::final_response("answer")]);
    state.status_offering = status_offer(&[("selection", true, "h1")]);
    state.next_status_offering = Some(status_offer(&[("selection", true, "h2")]));
    assert_eq!(state.run(), Ok(Outcome::Completed("answer".into())));
    assert_eq!(state.status_steered, ["selection", "selection"]);
    assert_eq!(
        state.budget.standing_statuses,
        status_standing(&[("selection", "h2")])
    );
    assert_eq!(
        state
            .trace
            .values()
            .iter()
            .filter(|step| *step == "steer_status")
            .count(),
        2
    );
}

#[test]
fn changed_auto_hash_steers_at_a_retry_continuation() {
    let mut state = MemoryState::new([Reply::truncated("cut"), Reply::final_response("answer")]);
    state.status_offering = status_offer(&[("selection", true, "h1")]);
    state.next_status_offering = Some(status_offer(&[("selection", true, "h2")]));
    assert_eq!(state.run(), Ok(Outcome::Completed("answer".into())));
    assert_eq!(state.status_steered, ["selection", "selection"]);
    assert_eq!(
        state.budget.standing_statuses,
        status_standing(&[("selection", "h2")])
    );
    assert_eq!(
        state
            .trace
            .values()
            .iter()
            .filter(|step| *step == "steer_status")
            .count(),
        2
    );
}

struct MemorySkills {
    skill_rows: Vec<(Skill, SkillContent)>,
    skill_lookups: Arc<Mutex<Vec<SkillReference>>>,
    skill_reads: Arc<Mutex<Vec<String>>>,
    trace: Trace,
    fail_at: Option<&'static str>,
    cancellation: Arc<CancellationSource>,
}

impl MemorySkills {
    fn new(state: &MemoryState) -> Self {
        Self {
            skill_rows: state.skill_rows.clone(),
            skill_lookups: state.skill_lookups.clone(),
            skill_reads: state.skill_reads.clone(),
            trace: state.trace.clone(),
            fail_at: state.fail_at,
            cancellation: state.cancellation.clone(),
        }
    }
}

impl SkillProvider for MemorySkills {
    fn resolve_skill_references<'a>(
        &'a self,
        references: &'a [SkillReference],
    ) -> BoxFuture<'a, Result<Vec<Option<Skill>>, Error>> {
        Box::pin(async move {
            self.trace.step("resolve_skill_references", self.fail_at)?;
            self.skill_lookups
                .lock()
                .unwrap()
                .extend_from_slice(references);
            if self.fail_at == Some("cancel_get_skills") {
                self.cancellation.cancel();
            }
            Ok(references
                .iter()
                .map(|reference| {
                    self.skill_rows
                        .iter()
                        .find(|(skill, _)| {
                            skill.name == reference.name
                                && reference
                                    .path
                                    .as_ref()
                                    .is_none_or(|path| path == &skill.path || path == &skill.id)
                        })
                        .map(|(skill, _)| skill.clone())
                })
                .collect())
        })
    }
    fn read_skill_content<'a>(
        &'a self,
        skill: &'a Skill,
    ) -> BoxFuture<'a, Result<SkillContent, Error>> {
        Box::pin(async move {
            self.trace.step("read_skill_content", self.fail_at)?;
            self.skill_reads.lock().unwrap().push(skill.id.clone());
            Ok(self
                .skill_rows
                .iter()
                .find(|(row, _)| row.id == skill.id)
                .unwrap()
                .1
                .clone())
        })
    }
}
