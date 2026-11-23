//! The `codemode` tool: binds the sandbox, the declaration renderer, and the
//! nested tool runner into one runtime Tool the model calls.

use crate::declarations::{
    DEFAULT_INPUT_SCHEMA_MAX_CHARS, DeclaredTool, build_groups, ordered_groups, render_declaration,
    render_tool_section, select_catalog,
};
use crate::runner::{NestedCallOutcome, NestedCallRequest, NestedToolCapture, NestedToolRunner};
use crate::sandbox::{
    HostCallRequest, SandboxOutcome, SandboxRequest, ScriptHost, StoreSnapshot, execute_script,
};
use crate::source::parse_source;
use crate::values::{
    OutputItem, ScriptCallRecord, ScriptCallStatus, ScriptCallTarget, ScriptError, ScriptErrorKind,
    ScriptGlobal, ScriptTool, StoreWrites, ToolCallFailure,
};
use parrot_domain::contracts::async_ops::{BoxFuture, Cancellation, OperationError};
use parrot_domain::contracts::security::{AccessSnapshot, InvocationScope};
use parrot_domain::contracts::tools::{ParallelSafety, Tool};
use parrot_domain::values::tools::ConfiguredToolDefinition;
use parrot_runtime::tool_settlement::{
    SharedSecurity, ToolFailure, ToolInvocation, ToolMetadata, ToolOutput,
};
use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// The codemode tool's input schema: one `code` string.
pub const CODEMODE_PARAMETERS_JSON: &str = r#"{"type":"object","properties":{"code":{"type":"string","description":"Raw JavaScript source."}},"required":["code"]}"#;

/// The tool name the model calls and the host registers.
pub const CODEMODE_TOOL_NAME: &str = "codemode";

/// Characters one output token is estimated to take.
const OUTPUT_CHARS_PER_TOKEN: usize = 4;

/// The output token budget when the script sets none.
const DEFAULT_MAX_OUTPUT_TOKENS: u64 = 10_000;

/// The store values one session's codemode scripts share.
#[derive(Default)]
pub struct CodemodeStore {
    values: Mutex<BTreeMap<String, serde_json::Value>>,
}

impl CodemodeStore {
    pub fn new() -> Self {
        Self::default()
    }

    /// The values a script starts with.
    pub fn snapshot(&self) -> StoreSnapshot {
        self.values.lock().expect("codemode store poisoned").clone()
    }

    /// Applies one script's writes; a later script reads them.
    pub fn apply(&self, writes: &StoreWrites) {
        let mut values = self.values.lock().expect("codemode store poisoned");
        for key in &writes.delete {
            values.remove(key);
        }
        for (key, json) in &writes.set {
            if let Ok(value) = serde_json::from_str(json) {
                values.insert(key.clone(), value);
            }
        }
    }
}

/// An owned cancellation the runtime scope's cancellation propagates into.
/// The sandbox and the nested runner both observe it.
#[derive(Clone)]
pub struct ScopedCancellation {
    sender: tokio::sync::watch::Sender<bool>,
    receiver: tokio::sync::watch::Receiver<bool>,
}

impl Default for ScopedCancellation {
    fn default() -> Self {
        Self::new()
    }
}

impl ScopedCancellation {
    pub fn new() -> Self {
        let (sender, receiver) = tokio::sync::watch::channel(false);
        Self { sender, receiver }
    }

    /// Marks the script cancelled; every watcher resolves.
    pub fn fire(&self) {
        let _ = self.sender.send(true);
    }
}

impl Cancellation for ScopedCancellation {
    fn is_cancelled(&self) -> bool {
        *self.receiver.borrow()
    }

    fn cancelled(&self) -> BoxFuture<'_, ()> {
        let mut receiver = self.receiver.clone();
        Box::pin(async move {
            let _ = receiver.wait_for(|fired| *fired).await;
        })
    }
}

/// What one script call's summary keeps for the result text.
#[derive(Clone, Copy, Debug, Default)]
pub struct CallTiming {
    pub duration_ms: u64,
    pub image_artifacts: usize,
}

/// Per-execution call timings the result formatter reads.
#[derive(Default)]
pub struct CallTimings {
    timings: Mutex<BTreeMap<u32, CallTiming>>,
}

impl CallTimings {
    pub fn new() -> Self {
        Self::default()
    }

    fn record(&self, id: u32, timing: CallTiming) {
        self.timings
            .lock()
            .expect("call timings poisoned")
            .insert(id, timing);
    }

    /// One call's timing, `None` when the call never settled.
    pub fn timing(&self, id: u32) -> Option<CallTiming> {
        self.timings
            .lock()
            .expect("call timings poisoned")
            .get(&id)
            .copied()
    }
}

struct BridgeState {
    /// Calls waiting to join the running batch.
    queued: Vec<HostCallRequest>,
    /// Callers awaiting their answer, by sandbox call id.
    waiters: BTreeMap<u32, tokio::sync::oneshot::Sender<Result<String, ToolCallFailure>>>,
    leading: bool,
}

/// Answers the sandbox's tool calls: concurrent calls coalesce into one
/// runner batch, so the runner's parallel-safety grouping applies.
struct ScriptBridge<S: AccessSnapshot + 'static> {
    runner: Arc<dyn NestedToolRunner<S>>,
    capture: NestedToolCapture<S>,
    /// The injected siblings, for `listTools` and `describeTool`.
    tools: Vec<DeclaredTool>,
    state: Arc<Mutex<BridgeState>>,
    timings: Arc<CallTimings>,
}

fn answer_of(outcome: NestedCallOutcome) -> Result<String, ToolCallFailure> {
    match outcome {
        NestedCallOutcome::Ok { text, .. } => Ok(serde_json::to_string(&text).unwrap_or_default()),
        NestedCallOutcome::Failed { message, .. } => Err(ToolCallFailure {
            message,
            properties_json: None,
        }),
    }
}

fn nested_request(request: &HostCallRequest) -> NestedCallRequest {
    NestedCallRequest {
        name: request.name.clone(),
        arguments_json: request
            .arguments_json
            .clone()
            .unwrap_or_else(|| "null".into()),
    }
}

fn fail_waiters(state: &mut BridgeState, message: &str) {
    let waiters = std::mem::take(&mut state.waiters);
    for (_, sender) in waiters {
        let _ = sender.send(Err(ToolCallFailure {
            message: message.to_string(),
            properties_json: None,
        }));
    }
}

/// Drives the batching loop over owned handles, so it can outlive the
/// borrow of the bridge inside its spawned task.
struct LeaderDriver<S: AccessSnapshot + 'static> {
    runner: Arc<dyn NestedToolRunner<S>>,
    capture: NestedToolCapture<S>,
    state: Arc<Mutex<BridgeState>>,
    timings: Arc<CallTimings>,
}

impl<S: AccessSnapshot + 'static> LeaderDriver<S> {
    async fn drive(&self) {
        loop {
            // Let same-burst callers land in the queue, so one Promise.all
            // group reaches the runner as one batch and its parallel-safety
            // grouping applies.
            tokio::task::yield_now().await;
            let batch =
                std::mem::take(&mut self.state.lock().expect("script bridge poisoned").queued);
            if batch.is_empty() {
                let drained = {
                    let mut state = self.state.lock().expect("script bridge poisoned");
                    if state.queued.is_empty() {
                        state.leading = false;
                        true
                    } else {
                        false
                    }
                };
                if drained {
                    break;
                }
                continue;
            }
            if self.capture.cancellation.is_cancelled() {
                fail_waiters(
                    &mut self.state.lock().expect("script bridge poisoned"),
                    "operation cancelled",
                );
                continue;
            }
            let started: Vec<(u32, Instant)> = batch
                .iter()
                .map(|request| (request.id, Instant::now()))
                .collect();
            let requests: Vec<NestedCallRequest> = batch.iter().map(nested_request).collect();
            let outcomes = self.runner.run(&self.capture, requests).await;
            for ((request, began), outcome) in batch.into_iter().zip(started).zip(outcomes) {
                let (id, started_at) = began;
                let timing = CallTiming {
                    duration_ms: u64::try_from(started_at.elapsed().as_millis())
                        .unwrap_or(u64::MAX),
                    image_artifacts: match &outcome {
                        NestedCallOutcome::Ok {
                            image_artifacts, ..
                        } => image_artifacts.len(),
                        NestedCallOutcome::Failed { .. } => 0,
                    },
                };
                self.timings.record(id, timing);
                let sender = self
                    .state
                    .lock()
                    .expect("script bridge poisoned")
                    .waiters
                    .remove(&request.id);
                if let Some(sender) = sender {
                    let _ = sender.send(answer_of(outcome));
                }
            }
        }
    }
}

impl<S: AccessSnapshot + 'static> ScriptHost for ScriptBridge<S> {
    fn call(
        &self,
        request: HostCallRequest,
    ) -> BoxFuture<'static, Result<String, ToolCallFailure>> {
        let (sender, receiver) = tokio::sync::oneshot::channel();
        let leader = {
            let mut state = self.state.lock().expect("script bridge poisoned");
            state.queued.push(request.clone());
            state.waiters.insert(request.id, sender);
            if state.leading {
                false
            } else {
                state.leading = true;
                true
            }
        };
        if leader {
            let runner = Arc::clone(&self.runner);
            let capture = self.capture.clone();
            let state = Arc::clone(&self.state);
            let timings = Arc::clone(&self.timings);
            tokio::spawn(async move {
                LeaderDriver {
                    runner,
                    capture,
                    state,
                    timings,
                }
                .drive()
                .await;
            });
        }
        Box::pin(async move {
            receiver.await.unwrap_or_else(|_| {
                Err(ToolCallFailure {
                    message: "the script bridge stopped before the call was answered".into(),
                    properties_json: None,
                })
            })
        })
    }

    fn call_global(
        &self,
        request: HostCallRequest,
    ) -> BoxFuture<'static, Result<String, ToolCallFailure>> {
        let answer = self.answer_global(&request);
        Box::pin(std::future::ready(answer))
    }
}

impl<S: AccessSnapshot + 'static> ScriptBridge<S> {
    /// Answers one global in-process: `listTools` lists every sibling,
    /// `describeTool(name)` renders one tool's declaration.
    fn answer_global(&self, request: &HostCallRequest) -> Result<String, ToolCallFailure> {
        match request.name.as_str() {
            "listTools" => {
                let tools: Vec<serde_json::Value> = self
                    .tools
                    .iter()
                    .map(|tool| {
                        serde_json::json!({
                            "name": tool.name,
                            "jsName": crate::identifier::to_identifier(&tool.name),
                            "description": tool.description,
                        })
                    })
                    .collect();
                Ok(serde_json::json!(tools).to_string())
            }
            "describeTool" => {
                let name = request
                    .arguments_json
                    .as_deref()
                    .and_then(|json| serde_json::from_str::<serde_json::Value>(json).ok())
                    .and_then(|value| value.as_str().map(str::to_owned))
                    .ok_or_else(|| ToolCallFailure {
                        message: "describeTool(name) expects a tool name string".into(),
                        properties_json: None,
                    })?;
                let tool = self
                    .tools
                    .iter()
                    .find(|tool| {
                        tool.name == name || crate::identifier::to_identifier(&tool.name) == name
                    })
                    .ok_or_else(|| ToolCallFailure {
                        message: format!("unknown tool {name}"),
                        properties_json: None,
                    })?;
                let rendered = render_declaration(tool, DEFAULT_INPUT_SCHEMA_MAX_CHARS);
                Ok(serde_json::json!({
                    "name": tool.name,
                    "jsName": rendered.js_name,
                    "description": tool.description,
                    "declaration": render_tool_section(&rendered),
                })
                .to_string())
            }
            other => Err(ToolCallFailure {
                message: format!("unknown global {other}"),
                properties_json: None,
            }),
        }
    }
}

/// What the host injects after materialize: the runner over this turn's
/// sibling tools, and the siblings' declarations for the description.
pub struct RunnerInjection<S: AccessSnapshot + 'static> {
    pub runner: Arc<dyn NestedToolRunner<S>>,
    pub siblings: Vec<DeclaredTool>,
    /// The declaration budget; `None` uses the default.
    pub inline_budget: Option<usize>,
}

/// The `codemode` tool. The runner slot is filled after materialize by the
/// host; until then, invoking fails explicitly.
pub struct CodemodeTool<S: AccessSnapshot + 'static> {
    metadata: ToolMetadata,
    store: Arc<CodemodeStore>,
    injection: Mutex<Option<Arc<RunnerInjection<S>>>>,
}

impl<S: AccessSnapshot + 'static> CodemodeTool<S> {
    pub fn new(store: Arc<CodemodeStore>) -> Self {
        Self {
            metadata: ToolMetadata {
                name: CODEMODE_TOOL_NAME.into(),
                aliases: Vec::new(),
            },
            store,
            injection: Mutex::new(None),
        }
    }

    /// Fills the runner slot. The description budget comes from the injection.
    pub fn inject(&self, injection: RunnerInjection<S>) {
        *self.injection.lock().expect("codemode injection poisoned") = Some(Arc::new(injection));
    }

    /// The provider-visible definition once injected: the assembled
    /// description and the input schema. `None` before injection.
    pub fn configured_definition(&self) -> Option<ConfiguredToolDefinition> {
        let injection = self.injection.lock().expect("codemode injection poisoned");
        let injection = injection.as_ref()?;
        Some(ConfiguredToolDefinition {
            description: build_description(&injection.siblings, injection.inline_budget),
            parameters_json: CODEMODE_PARAMETERS_JSON.into(),
        })
    }
}

/// The tool description: the intro, the helper list, and the budgeted tool
/// declarations.
pub fn build_description(siblings: &[DeclaredTool], budget: Option<usize>) -> String {
    let intro = "Run JavaScript that calls other tools. The input is raw JavaScript (not JSON, no code fence), run as an async function body in a QuickJS sandbox: top-level `await` and `return` work. No Node, file system, network, or timers.\n\
- `await tools.<name>({ ...args })` resolves to a string and rejects with an Error on failure. Calls still running when the script ends are cancelled.\n\
- Optional first line: `// @options: {\"max_output_tokens\": 10000, \"timeout_ms\": 60000}`\n\
Globals:\n\
- `text(value)`, `console.log(...)`, and top-level `return` add output; `exit()` ends the script.\n\
- `store(key, value)` and `load(key)` keep JSON values across codemode calls.\n\
- `ALL_TOOLS`, `listTools()`, `describeTool(name)`: find unlisted tools.";
    let declarations: Vec<_> = siblings
        .iter()
        .map(|tool| render_declaration(tool, DEFAULT_INPUT_SCHEMA_MAX_CHARS))
        .collect();
    let sections: BTreeMap<String, String> = declarations
        .iter()
        .map(|declaration| (declaration.name.clone(), render_tool_section(declaration)))
        .collect();
    let groups = ordered_groups(build_groups(&declarations, sections, &BTreeMap::new()));
    let selected = select_catalog(&groups, budget);
    let mut parts = vec![intro.to_string()];
    for group in &groups {
        for (name, section, _) in &group.entries {
            if selected.contains(name) {
                parts.push(section.clone());
            }
        }
    }
    parts.join("\n\n")
}

impl<S: AccessSnapshot + 'static> Tool for CodemodeTool<S> {
    type Definition = ToolMetadata;
    type Invocation = ToolInvocation;
    type Selection = parrot_runtime::agent_turn::services::TurnSelection<S>;
    type Security = SharedSecurity<S>;
    type Output = ToolOutput;
    type Error = ToolFailure;

    fn definition(&self) -> &ToolMetadata {
        &self.metadata
    }

    fn parallel_safety(&self, _invocation: &ToolInvocation) -> ParallelSafety {
        ParallelSafety::Sequential
    }

    fn enabled_after_interruption(&self) -> bool {
        false
    }

    fn invoke<'a>(
        &'a self,
        invocation: ToolInvocation,
        scope: InvocationScope<
            'a,
            SharedSecurity<S>,
            parrot_runtime::agent_turn::services::TurnSelection<S>,
        >,
    ) -> BoxFuture<'a, Result<ToolOutput, OperationError<ToolFailure>>> {
        Box::pin(self.run(invocation, scope))
    }
}

fn argument_failure(message: String) -> OperationError<ToolFailure> {
    OperationError::Failed(ToolFailure::new(
        message,
        parrot_storage::diagnostics::FailureKind::Argument,
    ))
}

impl<S: AccessSnapshot + 'static> CodemodeTool<S> {
    async fn run<'a>(
        &'a self,
        invocation: ToolInvocation,
        scope: InvocationScope<
            'a,
            SharedSecurity<S>,
            parrot_runtime::agent_turn::services::TurnSelection<S>,
        >,
    ) -> Result<ToolOutput, OperationError<ToolFailure>> {
        let injection = self
            .injection
            .lock()
            .expect("codemode injection poisoned")
            .clone()
            .ok_or_else(|| {
                OperationError::Failed(ToolFailure::new(
                    "the codemode tool's nested runner has not been injected; the host must inject the sibling tools after materialize",
                    parrot_storage::diagnostics::FailureKind::InvalidOperation,
                ))
            })?;
        let code = serde_json::from_str::<serde_json::Value>(&invocation.arguments_json)
            .map_err(|error| argument_failure(format!("codemode arguments are not JSON: {error}")))?
            .get("code")
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| {
                argument_failure("codemode arguments must carry a string `code` field".into())
            })?
            .to_owned();
        let (body, options) = parse_source(&code)
            .map_err(|error| argument_failure(format!("invalid codemode source: {error:?}")))?;
        let script_tools: Vec<ScriptTool> = injection
            .siblings
            .iter()
            .map(|tool| ScriptTool {
                name: tool.name.clone(),
                js_name: crate::identifier::to_identifier(&tool.name),
                description: tool.description.clone(),
            })
            .collect();
        let script_globals = vec![
            ScriptGlobal {
                name: "listTools".into(),
                spread: false,
            },
            ScriptGlobal {
                name: "describeTool".into(),
                spread: false,
            },
        ];
        let timings = Arc::new(CallTimings::new());
        let bind = Arc::new(ScopedCancellation::new());
        let capture = NestedToolCapture {
            selection: scope.selection().clone(),
            assistant_sequence: invocation.assistant_sequence,
            prompt_templates: invocation.prompt_templates.clone(),
            image_budget: invocation.image_budget.clone(),
            cancellation: bind.clone(),
        };
        let bridge = ScriptBridge {
            runner: injection.runner.clone(),
            capture,
            tools: injection.siblings.clone(),
            state: Arc::new(Mutex::new(BridgeState {
                queued: Vec::new(),
                waiters: BTreeMap::new(),
                leading: false,
            })),
            timings: timings.clone(),
        };
        let host: Arc<dyn ScriptHost> = Arc::new(bridge);
        let request = SandboxRequest {
            body,
            options,
            tools: script_tools,
            globals: script_globals,
            store: self.store.snapshot(),
            host_deadline_ms: None,
        };
        let (abort_sender, abort_receiver) = tokio::sync::oneshot::channel();
        let began = Instant::now();
        let execution = execute_script(request, host, abort_receiver);
        tokio::pin!(execution);
        tokio::select! {
            _ = propagate(bind.clone(), scope.cancellation()) => {}
            outcome = &mut execution => {
                return self.finish(outcome, began, &timings);
            }
        }
        bind.fire();
        let _ = abort_sender.send(());
        let outcome = execution.await;
        self.finish(outcome, began, &timings)
    }

    fn finish(
        &self,
        outcome: SandboxOutcome,
        began: Instant,
        timings: &Arc<CallTimings>,
    ) -> Result<ToolOutput, OperationError<ToolFailure>> {
        self.store.apply(&outcome.store_writes);
        let text = format_script_result(&outcome, began.elapsed(), timings);
        Ok(ToolOutput::text(text))
    }
}

async fn propagate(bind: Arc<ScopedCancellation>, cancellation: &dyn Cancellation) {
    cancellation.cancelled().await;
    bind.fire();
}

/// Formats one script execution as the model reads it: a `Script
/// completed/failed` header with the wall time, the script's text items
/// (`==> text N/M <==` when there are several), its console lines in one
/// `<console_output>` block, the returned value, one summary line per nested
/// call, and on failure the error with the calls already made. Nested tool
/// output never appears here; the summary is names, statuses, and counts.
pub fn format_script_result(
    outcome: &SandboxOutcome,
    wall: Duration,
    timings: &CallTimings,
) -> String {
    let result = &outcome.result;
    let mut text = String::new();
    text.push_str(if result.ok {
        "Script completed"
    } else {
        "Script failed"
    });
    text.push_str(&format!(
        "\nWall time {:.1} seconds\nOutput:\n",
        wall.as_secs_f32()
    ));
    let mut total_text = result
        .output
        .iter()
        .filter(|item| matches!(item, OutputItem::Text(_)))
        .count();
    if result.ok && value_text(result.value.as_deref()).is_some() {
        total_text += 1;
    }
    let mut index = 0;
    let mut console_lines: Vec<&str> = Vec::new();
    for item in &result.output {
        match item {
            OutputItem::Console(line) => console_lines.push(line),
            OutputItem::Text(line) => {
                index += 1;
                if total_text > 1 {
                    text.push_str(&format!("==> text {index}/{total_text} <==\n{line}\n"));
                } else {
                    text.push_str(line);
                    text.push('\n');
                }
            }
        }
    }
    if let Some(value) = result
        .ok
        .then(|| value_text(result.value.as_deref()))
        .flatten()
    {
        index += 1;
        if total_text > 1 {
            text.push_str(&format!("==> text {index}/{total_text} <==\n{value}\n"));
        } else {
            text.push_str(&value);
            text.push('\n');
        }
    }
    if !console_lines.is_empty() {
        text.push_str(&format!(
            "<console_output>\n{}\n</console_output>\n",
            console_lines.join("\n")
        ));
    }
    for record in &result.calls {
        text.push_str(&summary_line(record, timings));
    }
    if !result.ok {
        let error = result
            .error
            .as_ref()
            .expect("a failed result carries an error");
        text.push_str(&format!("Script error:\n{}\n", error_head(error)));
        text.push_str(&calls_summary(&result.calls));
    }
    truncate_output(text, outcome.max_output_tokens)
}

fn value_text(value_json: Option<&str>) -> Option<String> {
    let json = value_json?;
    let value: serde_json::Value = serde_json::from_str(json).ok()?;
    Some(match value {
        serde_json::Value::String(text) => text,
        other => other.to_string(),
    })
}

fn summary_line(record: &ScriptCallRecord, timings: &CallTimings) -> String {
    let status = match record.status {
        ScriptCallStatus::Ok => "ok",
        ScriptCallStatus::Error => "error",
        ScriptCallStatus::Cancelled => "cancelled",
    };
    let target = match record.target {
        ScriptCallTarget::Tool => "",
        ScriptCallTarget::Global => " (host global)",
    };
    let duration = timings
        .timing(record.id)
        .map(|timing| format!(" {}ms", timing.duration_ms))
        .unwrap_or_default();
    let images = timings
        .timing(record.id)
        .filter(|timing| timing.image_artifacts > 0)
        .map(|timing| format!(", {} image artifacts (not shown)", timing.image_artifacts))
        .unwrap_or_default();
    let arguments = record
        .arguments_json
        .as_deref()
        .map(|args| truncate_display(args, 80))
        .unwrap_or_default();
    format!(
        "c{} {}{} {}{}{}{}\n",
        record.id, record.name, target, status, duration, images, arguments
    )
}

fn truncate_display(text: &str, limit: usize) -> String {
    if text.chars().count() <= limit {
        format!(" {text}")
    } else {
        let head: String = text.chars().take(limit).collect();
        format!(" {head}...")
    }
}

fn error_head(error: &ScriptError) -> String {
    match error.kind {
        ScriptErrorKind::Script => {
            if error.stack.is_empty() {
                format!("{}: {}", error.name, error.message)
            } else {
                error.stack.clone()
            }
        }
        ScriptErrorKind::Timeout => format!("Script timed out: {}", error.message),
        ScriptErrorKind::Aborted => format!("Script aborted: {}", error.message),
        ScriptErrorKind::Sandbox => format!("Script sandbox failed: {}", error.message),
    }
}

fn calls_summary(calls: &[ScriptCallRecord]) -> String {
    if calls.is_empty() {
        return "No tool calls were made.\n".into();
    }
    let listed = calls
        .iter()
        .map(|call| {
            let status = match call.status {
                ScriptCallStatus::Ok => "ok",
                ScriptCallStatus::Error => "error",
                ScriptCallStatus::Cancelled => "cancelled",
            };
            format!("{} ({})", call.name, status)
        })
        .collect::<Vec<_>>()
        .join(", ");
    format!("Tool calls made before the failure (they are not undone): {listed}\n")
}

/// Applies the script's output token budget: past it, the middle of the text
/// is cut with head and tail kept and a warning naming the cut.
fn truncate_output(text: String, max_output_tokens: Option<u64>) -> String {
    let budget = (max_output_tokens.unwrap_or(DEFAULT_MAX_OUTPUT_TOKENS) as usize)
        .saturating_mul(OUTPUT_CHARS_PER_TOKEN);
    if text.chars().count() <= budget {
        return text;
    }
    let chars: Vec<char> = text.chars().collect();
    let head_count = budget / 2;
    let tail_count = budget - head_count;
    let head: String = chars[..head_count].iter().collect();
    let tail: String = chars[chars.len() - tail_count..].iter().collect();
    let removed_tokens = (chars.len() - head_count - tail_count).div_ceil(OUTPUT_CHARS_PER_TOKEN);
    format!(
        "{head}[Middle truncated: {removed_tokens} tokens dropped; codemode keeps no spill file in this build]{tail}"
    )
}
