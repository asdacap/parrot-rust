use parrot_core::{
    AgentState, BoxFuture, CancellationToken, CompletionBlockerFeedback, ConfigKey, ConfigValue,
    Content, ContextConfiguration, ContextModel, ContextSnapshot, ConversationAddition,
    ConversationEvent, ConversationOrigin, ConversationPersistence, ConversationReceipt,
    ConversationTurn, Delivery, DerivedContext, Error, ErrorCode, ErrorKind, EventPublisher,
    FinishReason, InputAdmission, Message, Outcome, PendingMessage, PromotedInput, PromptTemplate,
    Provider, ProviderRequest, ProviderResponse, Reasoning, Role, Skill, SkillContent,
    SkillMention, SkillReference, States, StatusKey, StatusOffer, StatusOffering, StatusStanding,
    TokenCalibration, ToolCall, ToolCallReceipt, ToolCatalog, ToolDefinition, ToolExecutionRequest,
    ToolResult, ToolStatus, Tools, TurnFlags, TurnModel, TurnOptions, parse_skill_mentions,
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

#[path = "turn_loop/cancellation.rs"]
mod cancellation;
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

#[path = "compaction/support.rs"]
mod compaction_support;

use cancellation::CancellationSource;
use compaction_dependencies::MemoryCompaction;

struct MemoryEventPublisher {
    events: Arc<Mutex<Vec<ConversationEvent>>>,
    started_turns: Arc<Mutex<Vec<TurnModel>>>,
    compacting: Arc<AtomicBool>,
    compaction: Arc<Mutex<MemoryCompaction>>,
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
    ) -> BoxFuture<'a, Result<String, Error>> {
        Box::pin(async move {
            let mut started = self.started_turns.lock().unwrap();
            let turn_id = format!("turn-start-{}", started.len());
            started.push(model.clone());
            Ok(turn_id)
        })
    }
    fn on_epoch_started(&self) -> BoxFuture<'_, Result<(), Error>> {
        Box::pin(async { Ok(()) })
    }
    fn report_compaction_started(&self) -> BoxFuture<'_, Result<(), Error>> {
        Box::pin(async move {
            self.compacting.store(true, Ordering::SeqCst);
            Ok(())
        })
    }
    fn report_compaction_failed(&self, _: String) -> BoxFuture<'_, Result<(), Error>> {
        Box::pin(async move {
            self.compacting.store(false, Ordering::SeqCst);
            Ok(())
        })
    }
    fn report_compaction_finished(&self) -> BoxFuture<'_, Result<(), Error>> {
        Box::pin(async move {
            self.compacting.store(false, Ordering::SeqCst);
            Ok(())
        })
    }
    fn report_compaction_progress<'a, 'c>(
        &'a self,
        progress: parrot_core::CompactionProgress,
        _: CancellationToken<'c>,
    ) -> BoxFuture<'a, Result<(), Error>>
    where
        'c: 'a,
    {
        Box::pin(async move {
            self.compaction
                .lock()
                .unwrap()
                .effects
                .progress
                .lock()
                .unwrap()
                .push(progress);
            Ok(())
        })
    }
}

const RUNAWAY_NOTICE: &str = "configured runaway notice";

struct MemoryTurnLifecycle<'a> {
    states: &'a mut MemoryState,
    tools: &'a dyn Tools,
    provider: &'a dyn Provider,
    publisher: &'a MemoryEventPublisher,
}

impl parrot_core::TurnLifecycle for MemoryTurnLifecycle<'_> {
    fn has_scheduled_work(&self) -> bool {
        false
    }
    fn has_ready_turn(&mut self) -> BoxFuture<'_, Result<bool, Error>> {
        Box::pin(async move { self.states.has_ready_turn().await })
    }
    fn run_turn<'a>(
        &'a mut self,
        cancellation: CancellationToken<'a>,
    ) -> BoxFuture<'a, Result<Outcome, Error>> {
        Box::pin(async move {
            parrot_core::open_turn(self.states, self.publisher, cancellation).await?;
            parrot_core::run_turn(
                self.states,
                self.tools,
                self.provider,
                self.publisher,
                cancellation,
            )
            .await
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
        compacting: states.compacting.clone(),
        compaction: states.compaction.clone(),
    };
    let mut lifecycle = MemoryTurnLifecycle {
        states,
        tools,
        provider,
        publisher: &publisher,
    };
    parrot_core::run(&mut lifecycle, cancellation).await
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
        compacting: states.compacting.clone(),
        compaction: states.compaction.clone(),
    };
    parrot_core::open_turn(states, &publisher, cancellation).await?;
    parrot_core::run_turn(states, tools, provider, &publisher, cancellation).await
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
    compacting: Arc<AtomicBool>,
    cancellation: Arc<CancellationSource>,
}
impl Provider for MemoryProvider {
    fn estimate_input_tokens(
        &self,
        instructions: &str,
        definitions: &[ToolDefinition],
        messages: &[Message],
    ) -> Result<i64, Error> {
        if self.compacting.load(Ordering::SeqCst) {
            return self
                .compaction
                .lock()
                .unwrap()
                .effects
                .estimate_context_tokens(instructions, definitions, messages);
        }
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
    fn summarize<'a, 'c>(
        &'a self,
        request: ProviderRequest,
        cancellation: CancellationToken<'c>,
    ) -> BoxFuture<'a, Result<parrot_core::SummaryResponse, Error>>
    where
        'c: 'a,
    {
        let provider = self.compaction.lock().unwrap().provider.clone();
        Box::pin(
            async move { parrot_core::Provider::summarize(&provider, request, cancellation).await },
        )
    }

    fn invoke<'a, 'c>(
        &'a self,
        request: ProviderRequest,
        _cancellation: CancellationToken<'c>,
    ) -> BoxFuture<'a, Result<ProviderResponse, Error>>
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
                Reply::TooLarge(message) => Err(request_too_large(message)),
                Reply::Failed(message) => Err(Error::failed(message)),
                Reply::Error(error) => Err(error),
            }
        })
    }
}

struct RenderedContext {
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
struct MemoryContext {
    configuration: ContextConfiguration,
    calibration: TokenCalibration,
}

struct CompactedContext {
    instructions: String,
    messages: Vec<Message>,
    state: MemoryContext,
}

struct MemoryRequestData {
    rendered_instructions: Vec<String>,
    estimates: Vec<InputEstimate>,
    estimate_script: VecDeque<i64>,
    estimated_input_tokens: i64,
    context: MemoryContext,
    instructions: String,
    messages: Vec<Message>,
    renderings: VecDeque<RenderedContext>,
}

#[derive(Default)]
struct MemoryPromptData {
    exit_blockers: VecDeque<Vec<String>>,
}

struct MemoryState {
    events: Arc<Mutex<Vec<ConversationEvent>>>,
    started_turns: Arc<Mutex<Vec<TurnModel>>>,
    turn_ids: Vec<String>,
    skill_rows: Vec<(Skill, SkillContent)>,
    steers_after_results: VecDeque<String>,
    skill_lookups: Mutex<Vec<SkillReference>>,
    skill_reads: Mutex<Vec<String>>,
    trace: Trace,
    request: Arc<Mutex<MemoryRequestData>>,
    prompts: Arc<Mutex<MemoryPromptData>>,
    skill_templates: Arc<Mutex<BTreeMap<ConfigKey, String>>>,
    recorded_input_contexts: Vec<DerivedContext>,
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
    compacted_definitions: Vec<Vec<ToolDefinition>>,
    last_turn: Option<ConversationTurn>,
    pending_messages: VecDeque<(PendingMessage, Message)>,
    next_input: usize,
    turn_reads: AtomicUsize,
    admissions: Vec<InputAdmission>,
    cancellation: Arc<CancellationSource>,
    unlimited: bool,
    maximum: i32,
    compaction_plan: Option<parrot_core::CompactionPlan>,
    summary_provider: compaction_support::Provider,
    recorded_compactions: Vec<Result<Option<parrot_core::CompactionResult>, Error>>,
    fitted_statuses: Vec<String>,
    status_offering: StatusOffering,
    next_status_offering: Option<StatusOffering>,
    status_standing: StatusStanding,
    status_steered: Vec<(String, StatusStanding)>,
    status_reads: AtomicUsize,
    last_capture_request: Option<usize>,
    fail_at: Option<&'static str>,
    budget: AgentState,
    budget_read_error: Option<Error>,
    budget_write_error: Option<Error>,
    budget_writes: usize,
    compaction: Arc<Mutex<MemoryCompaction>>,
    compacting: Arc<AtomicBool>,
}
impl MemoryState {
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
            context: MemoryContext {
                configuration: ContextConfiguration {
                    model: ContextModel {
                        canonical_model: "memory/model".into(),
                        context_window: 0,
                        maximum_input_tokens: 0,
                        maximum_output_tokens: 0,
                    },
                    trigger_percent: 90,
                    context_limit: None,
                },
                calibration: TokenCalibration::default(),
            },
            instructions: "core request".into(),
            messages: Vec::new(),
            renderings: VecDeque::new(),
        }));
        let cancellation = Arc::new(CancellationSource::default());
        let compaction = Arc::new(Mutex::new(MemoryCompaction::default()));
        let compacting = Arc::new(AtomicBool::new(false));
        let mut state = Self {
            events: Arc::default(),
            started_turns: Arc::default(),
            turn_ids: Vec::new(),
            skill_rows: Vec::new(),
            steers_after_results: VecDeque::new(),
            skill_lookups: Mutex::default(),
            skill_reads: Mutex::default(),
            request: request.clone(),
            prompts: Arc::default(),
            skill_templates: Arc::new(Mutex::new([
                (ConfigKey::OversizedToolGroupTemplate, "{path}".into()),
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
            recorded_input_contexts: Vec::new(),
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
                compacting: compacting.clone(),
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
            compacted_definitions: Vec::new(),
            last_turn: None,
            pending_messages: VecDeque::new(),
            next_input: 0,
            turn_reads: AtomicUsize::new(0),
            admissions: Vec::new(),
            cancellation,
            unlimited: false,
            maximum: 10,
            compaction_plan: None,
            summary_provider: compaction_support::Provider::new([]),
            recorded_compactions: Vec::new(),
            fitted_statuses: Vec::new(),
            status_offering: StatusOffering::default(),
            next_status_offering: None,
            status_standing: StatusStanding::default(),
            status_steered: Vec::new(),
            status_reads: AtomicUsize::new(0),
            last_capture_request: None,
            fail_at: None,
            budget: AgentState::new(false),
            budget_read_error: None,
            budget_write_error: None,
            budget_writes: 0,
            compaction,
            compacting,
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
        request.context.configuration.model.context_window = 1000;
        request.context.configuration.model.maximum_output_tokens = 1;
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
        *self.compaction.lock().unwrap() =
            MemoryCompaction::new(self.compaction_plan.clone(), self.summary_provider.clone());
        self.budget = AgentState::new(self.unlimited);
        let source = self.cancellation.clone();
        finish(run(
            self,
            tools,
            provider,
            CancellationToken::new(source.as_ref()),
        ))
    }
    fn run_with_compaction(&mut self, compaction: &mut MemoryCompaction) -> Result<Outcome, Error> {
        std::mem::swap(&mut *self.compaction.lock().unwrap(), compaction);
        self.budget = AgentState::new(self.unlimited);
        let tools = self.tools.clone();
        let provider = self.provider.clone();
        let result = finish(run(
            self,
            &tools,
            &provider,
            CancellationToken::new(provider.cancellation.as_ref()),
        ));
        std::mem::swap(&mut *self.compaction.lock().unwrap(), compaction);
        result
    }
}
impl States for MemoryState {
    fn read_status_offering(
        &self,
        context: ContextSnapshot,
    ) -> BoxFuture<'_, Result<StatusOffering, Error>> {
        if self.status_reads.fetch_add(1, Ordering::SeqCst) == 0 {
            return Box::pin(async { Ok(self.status_offering.clone()) });
        }
        Box::pin(async move { finish(self.compaction.lock().unwrap().status.offering(context)) })
    }
    fn read_standing_statuses(&self) -> BoxFuture<'_, Result<StatusStanding, Error>> {
        Box::pin(async { Ok(self.status_standing.clone()) })
    }
    fn add_conversation_messages(
        &mut self,
        addition: ConversationAddition,
    ) -> BoxFuture<'_, Result<ConversationReceipt, Error>> {
        Box::pin(async move {
            let mut sequence = 0;
            match &addition.persistence {
                ConversationPersistence::Append { origin, event } => match event {
                    ConversationEvent::Notice { kind, .. } if kind == "skill_context" => {
                        self.step("record_input_context")?;
                        let content = addition.messages[0]
                            .contents
                            .iter()
                            .filter_map(|content| match content {
                                Content::Text(text) => Some(text.as_str()),
                                _ => None,
                            })
                            .collect();
                        self.recorded_input_contexts.push(DerivedContext {
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
                ConversationPersistence::Phase { final_phase, .. } => {
                    if !final_phase {
                        return Ok(ConversationReceipt::default());
                    }
                    self.step("final_request")?;
                    let count = self.request().rendered_instructions.len();
                    self.final_requests.push(count);
                }
                ConversationPersistence::StatusSteering { standing, event } => {
                    self.step("steer_status")?;
                    assert!(matches!(
                        event,
                        ConversationEvent::Notice { kind, notice }
                            if kind == "status"
                                && notice.starts_with("Status prompt injected\n")
                    ));
                    let content = addition.messages[0]
                        .contents
                        .iter()
                        .map(|part| match part {
                            Content::Text(text) => text.as_str(),
                            _ => "",
                        })
                        .collect::<String>();
                    self.status_steered.push((content, standing.clone()));
                    self.status_standing = standing.clone();
                }
                ConversationPersistence::CompletionBlockerFeedback(feedback) => {
                    self.step("completion_blocker_feedback")?;
                    self.completion_blocker_feedback.push(feedback.clone());
                }
                ConversationPersistence::CloseTurn { .. } => {
                    self.step("complete")?;
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
                if !matches!(&addition.persistence, ConversationPersistence::Phase { .. }) {
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
                        self.request().context.configuration.clone(),
                    ))
                }
                ConfigKey::Compaction => Ok(ConfigValue::Compaction(
                    self.compaction.lock().unwrap().configuration.clone(),
                )),
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
            let mut state = self.budget.clone();
            state.final_phase = false;
            Ok(state)
        })
    }
    fn set_agent_state(&mut self, state: AgentState) -> BoxFuture<'_, Result<(), Error>> {
        Box::pin(async move {
            self.budget_writes += 1;
            if let Some(error) = &self.budget_write_error {
                return Err(error.clone());
            }
            self.budget = state;
            Ok(())
        })
    }
    fn begin_turn(&mut self) -> BoxFuture<'_, Result<TurnModel, Error>> {
        Box::pin(async move {
            Ok(TurnModel {
                selector: self.model.clone(),
                alias_icon: None,
            })
        })
    }
    fn record_turn_started(&mut self, turn_id: &str) -> BoxFuture<'_, Result<(), Error>> {
        self.turn_ids.push(turn_id.to_owned());
        Box::pin(async { Ok(()) })
    }
    fn activate_turn(&mut self) -> BoxFuture<'_, Result<(), Error>> {
        Box::pin(async { Ok(()) })
    }
    fn collect_completion_blockers(&mut self) -> BoxFuture<'_, Result<Vec<String>, Error>> {
        Box::pin(async move {
            self.step("exit_blockers")?;
            Ok(self.prompts().exit_blockers.pop_front().unwrap_or_default())
        })
    }
    fn write_to_scratch<'a, 'c>(
        &'a self,
        contents: &'a [u8],
        cancellation: CancellationToken<'c>,
    ) -> BoxFuture<'a, Result<String, Error>>
    where
        'c: 'a,
    {
        Box::pin(async move {
            finish(
                self.compaction
                    .lock()
                    .unwrap()
                    .effects
                    .write_to_scratch(contents, cancellation),
            )
        })
    }

    fn capture_system_prompt(&mut self) -> BoxFuture<'_, Result<String, Error>> {
        Box::pin(async move {
            self.step("instruction_state")?;
            let compacting = self.compacting.load(Ordering::SeqCst);
            if !compacting {
                self.trace.step("render", self.fail_at)?;
            }
            let request_index = self.provider.requests.lock().unwrap().len();
            let fresh = self.last_capture_request != Some(request_index);
            let instructions = if fresh {
                let rendered_instructions = {
                    let mut request = self.request();
                    if let Some(rendered) = (!compacting)
                        .then(|| request.renderings.pop_front())
                        .flatten()
                    {
                        request.instructions = rendered.instructions;
                        request.context.configuration.model.maximum_output_tokens =
                            rendered.maximum_output_tokens;
                        request.messages = rendered.messages;
                    }
                    if !compacting {
                        let instructions = request.instructions.clone();
                        request.rendered_instructions.push(instructions.clone());
                    }
                    request.instructions.clone()
                };
                if !compacting {
                    self.last_capture_request = Some(request_index);
                }
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
    fn peek_pending_input_header(&self) -> BoxFuture<'_, Result<Option<PendingMessage>, Error>> {
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
                .position(|(message, _)| message.delivery == Delivery::Steer)
            {
                promoted.push(self.pending_messages.remove(index).unwrap());
            }
            if promoted.is_empty() && admission == InputAdmission::SteersOrNextQueued {
                promoted.extend(self.pending_messages.pop_front());
            }
            Ok(promoted
                .into_iter()
                .map(|(input, message)| PromotedInput {
                    input_id: input.input_id,
                    message,
                    publication: Default::default(),
                })
                .collect())
        })
    }
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
    fn read_token_calibration(&self) -> BoxFuture<'_, Result<TokenCalibration, Error>> {
        Box::pin(async move {
            self.trace.step("token_calibration", self.fail_at)?;
            Ok(self.request().context.calibration.clone())
        })
    }
    fn read_provider_messages(&self) -> BoxFuture<'_, Result<Vec<Message>, Error>> {
        Box::pin(async move {
            self.trace.step("messages", self.fail_at)?;
            Ok(self.request().messages.clone())
        })
    }
    fn read_usage_totals(&self) -> BoxFuture<'_, Result<parrot_core::UsageTotals, Error>> {
        Box::pin(async move { Ok(parrot_core::UsageTotals::default()) })
    }
    fn read_conversation_summary(
        &self,
    ) -> BoxFuture<'_, Result<Option<parrot_core::ConversationSummary>, Error>> {
        Box::pin(async move {
            self.trace.step("conversation_summary", self.fail_at)?;
            Ok(None)
        })
    }
    fn read_checkpoint_sequences(&self) -> BoxFuture<'_, Result<Vec<i64>, Error>> {
        Box::pin(async move {
            self.trace.step("checkpoint_sequences", self.fail_at)?;
            Ok(Vec::new())
        })
    }
    fn read_conversation_tail(
        &self,
        after: i64,
    ) -> BoxFuture<'_, Result<Vec<parrot_core::ConversationHistoryItem>, Error>> {
        Box::pin(async move {
            self.trace.step("conversation_tail", self.fail_at)?;
            let (messages, first_sequence) = if let Some(plan) = &self.compaction_plan {
                (
                    plan.groups
                        .iter()
                        .flat_map(|group| group.messages.clone())
                        .collect::<Vec<_>>(),
                    plan.watermark - plan.keep_group_from as i64 + 1,
                )
            } else {
                (self.request().messages.clone(), 1)
            };
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
                .filter(|item| item.sequence > after)
                .collect();
            Ok(history)
        })
    }
    fn finalize_compaction<'a>(
        &'a mut self,
        instructions: &'a str,
        definitions: &'a [ToolDefinition],
        result: Result<Option<parrot_core::FittedCompaction>, Error>,
    ) -> BoxFuture<'a, Result<String, Error>> {
        Box::pin(async move {
            self.compacting.store(false, Ordering::SeqCst);
            self.step("compact")?;
            self.compacted_definitions.push(definitions.to_vec());
            self.request().instructions = instructions.into();
            self.recorded_compactions.push(
                result
                    .clone()
                    .map(|result| result.map(|fitted| fitted.result)),
            );
            if let Some(fitted) = result? {
                self.fitted_statuses.push(fitted.status);
            }
            if let Some(compacted) = self.compactions.pop_front() {
                self.request().instructions = compacted.instructions;
                self.request().messages = compacted.messages;
                self.request().context = compacted.state;
            }
            Ok(self.request().instructions.clone())
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
            "messages",
            "context_configuration",
            "estimate",
            "instruction_state",
            "render",
            "messages",
            "context_configuration",
            "token_calibration",
            "estimate",
            "messages",
            "context_configuration",
            "token_calibration",
            "estimate",
            "invoke",
            "record_tools",
            "execute",
            "record_results",
            "promote",
            "instruction_state",
            "render",
            "messages",
            "context_configuration",
            "estimate",
            "instruction_state",
            "render",
            "messages",
            "context_configuration",
            "token_calibration",
            "estimate",
            "messages",
            "context_configuration",
            "token_calibration",
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
    assert!(state.final_requests.contains(&1));
    assert_eq!(state.final_requests, [1, 2, 3, 4, 5]);
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
    for disable_limits in [false, true] {
        let mut state = MemoryState::new([Reply::invalid("bad"), Reply::final_response("answer")]);
        state.maximum = 1;
        let tools = state.tools.clone();
        let provider = state.provider.clone();

        state.budget = AgentState::new(disable_limits);

        assert_eq!(
            finish(run(
                &mut state,
                &tools,
                &provider,
                CancellationToken::new(provider.cancellation.as_ref())
            )),
            if disable_limits {
                Ok(Outcome::Completed("answer".into()))
            } else {
                Ok(Outcome::BudgetExhausted(RUNAWAY_NOTICE.into()))
            }
        );
        assert_eq!(state.retries, [("invalid tool calls".into(), 1)]);
        assert_eq!(
            state.request().rendered_instructions.len(),
            if disable_limits { 2 } else { 1 }
        );
        assert!(state.capture_catalogs().iter().all(|tools| {
            if disable_limits {
                tools == &["read", "write"]
            } else {
                tools == &["read"]
            }
        }));
        assert_eq!(
            state.final_requests,
            if disable_limits { vec![] } else { vec![1] }
        );
        assert!(state.recorded_tools.is_empty());
        assert!(state.tools.requests.lock().unwrap().is_empty());
        assert_eq!(
            state.trace.values().last().unwrap(),
            if disable_limits {
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
fn disabled_request_limits_keep_every_request_in_normal_phase() {
    let mut state = MemoryState::new([
        Reply::tools("call1"),
        Reply::tools("call2"),
        Reply::final_response("answer"),
    ]);
    state.unlimited = true;
    state.maximum = 1;
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
    assert_eq!(state.capture_catalogs()[1], ["read", "write"]);
    assert_eq!(state.capture_catalogs()[2], ["read"]);
    assert_eq!(state.final_requests, [3]);
}

#[test]
fn oversized_requests_compact_once_until_a_successful_response_resets_the_guard() {
    let mut state = MemoryState::new([
        Reply::TooLarge("first"),
        Reply::tools("call"),
        Reply::TooLarge("second"),
        Reply::final_response("answer"),
    ]);
    state.compaction_plan = Some(compaction_support::plan(&["original group", "tail"]));
    state.summary_provider.replies.lock().unwrap().push_back(Ok(
        parrot_core::SummaryResponse::Summary("first summary".into()),
    ));
    state.summary_provider.replies.lock().unwrap().push_back(Ok(
        parrot_core::SummaryResponse::Summary("second summary".into()),
    ));
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
    state.compaction_plan = Some(compaction_support::plan(&["original group", "tail"]));
    state.summary_provider.replies.lock().unwrap().push_back(Ok(
        parrot_core::SummaryResponse::Summary("core summary".into()),
    ));
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
    assert_eq!(state.request().rendered_instructions.len(), 1);
    assert_eq!(state.trace.values().last().unwrap(), "compact");
}

#[test]
fn fresh_invocations_use_supplied_settings_and_capture_tools_once() {
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
    state.compaction_plan = Some(compaction_support::plan(&["original group", "tail"]));
    state.summary_provider.replies.lock().unwrap().push_back(Ok(
        parrot_core::SummaryResponse::Summary("core summary".into()),
    ));
    *state.compaction.lock().unwrap() = MemoryCompaction::new(
        state.compaction_plan.clone(),
        state.summary_provider.clone(),
    );
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
    state.budget = AgentState::new(false);

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
    assert_eq!(trace.iter().filter(|step| *step == "catalog").count(), 1);
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
    assert!(state.final_requests.contains(&6));
    assert_eq!(state.turn_reads.load(Ordering::SeqCst), 6);
    let trace = state.trace.values();
    assert_eq!(trace.iter().filter(|step| *step == "catalog").count(), 2);
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

    state.budget = AgentState::new(false);

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
        ("messages", Reply::final_response("response")),
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
    assert!(state.final_requests.contains(&1));
    assert_eq!(state.final_requests, [1, 2, 3, 4, 5, 6]);
    assert_eq!(
        state
            .trace
            .values()
            .iter()
            .filter(|entry| *entry == "catalog")
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
    assert_eq!(state.final_requests, [2, 3, 5, 6, 7, 8, 9]);
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
    fn summarize<'a, 'c>(
        &'a self,
        request: ProviderRequest,
        cancellation: CancellationToken<'c>,
    ) -> BoxFuture<'a, Result<parrot_core::SummaryResponse, Error>>
    where
        'c: 'a,
    {
        self.0.summarize(request, cancellation)
    }

    fn invoke<'a, 'c>(
        &'a self,
        request: ProviderRequest,
        _cancellation: CancellationToken<'c>,
    ) -> BoxFuture<'a, Result<ProviderResponse, Error>>
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
    assert_eq!(states.final_requests, [2]);
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
fn oversized_recovery_invokes_core_summary_requests_and_records_the_result_before_retrying() {
    let mut state = MemoryState::new([
        Reply::TooLarge("oversized"),
        Reply::final_response("answer"),
    ]);
    state.compaction_plan = Some(compaction_support::plan(&[
        "original group",
        "retained tail",
    ]));
    state.summary_provider.replies.lock().unwrap().push_back(Ok(
        parrot_core::SummaryResponse::Summary("core summary".into()),
    ));
    assert_eq!(state.run(), Ok(Outcome::Completed("answer".into())));
    let requests = state.summary_provider.requests.lock().unwrap();
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].model, "summary-model");
    assert_eq!(
        requests[0].messages[1],
        compaction_support::message("original group")
    );
    assert!(requests[0].tools.is_empty());
    let result = state.recorded_compactions[0]
        .as_ref()
        .unwrap()
        .as_ref()
        .unwrap();
    assert_eq!(
        result.summary.contents,
        [Content::Text("prefix:core summary".into())]
    );
    assert_eq!(state.provider.requests.lock().unwrap().len(), 2);
    let trace = state.trace.values();
    let recorded = trace.iter().position(|step| step == "compact").unwrap();
    assert_eq!(
        trace[recorded + 1..]
            .iter()
            .filter(|step| *step == "invoke")
            .count(),
        1
    );
}

#[test]
fn failed_core_summary_is_recorded_before_propagation_without_retrying_the_turn_request() {
    let mut state = MemoryState::new([Reply::TooLarge("oversized")]);
    state.compaction_plan = Some(compaction_support::plan(&["original group", "tail"]));
    let error = Error {
        detail: None,
        kind: ErrorKind::Failed,
        code: ErrorCode::Http,
        message: "summary provider failed".into(),
        response_body: "opaque response body".into(),
    };
    state
        .summary_provider
        .replies
        .lock()
        .unwrap()
        .push_back(Err(error.clone()));
    assert_eq!(state.run(), Err(error.clone()));
    assert_eq!(state.recorded_compactions, [Err(error)]);
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
    assert_eq!(
        state.status_steered,
        [(
            "fitted status".into(),
            status_standing(&[("selection", "h1"), ("time", "t1")])
        )]
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
    assert_eq!(steered, ["fitted status".to_string()]);
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
    assert_eq!(
        state.status_steered,
        [(
            "fitted status".into(),
            status_standing(&[("selection", "h1"), ("time", "t1")])
        )]
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
    assert_eq!(
        state.status_steered,
        [
            (
                "fitted status".into(),
                status_standing(&[("selection", "h1")])
            ),
            ("selection".into(), status_standing(&[("selection", "h2")])),
        ]
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
    assert_eq!(
        state.status_steered,
        [
            (
                "fitted status".into(),
                status_standing(&[("selection", "h1")])
            ),
            ("selection".into(), status_standing(&[("selection", "h2")])),
        ]
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
