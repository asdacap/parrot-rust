//! The script sandbox: one QuickJS runtime per script on a dedicated thread,
//! the host-call bridge, and the message loop that drives both sides.

use crate::prelude::prelude_source;
use crate::values::{
    CODEMODE_MEMORY_LIMIT_BYTES, MAX_VM_STACK_SIZE, OutputItem, ScriptCallRecord, ScriptCallStatus,
    ScriptCallTarget, ScriptError, ScriptErrorKind, ScriptGlobal, ScriptResult, ScriptTool,
    StoreWrites, ToolCallFailure,
};
use parrot_domain::contracts::async_ops::BoxFuture;
use rquickjs::context::EvalOptions;
use rquickjs::{Context, Ctx, Function, Runtime, Value};
use std::cell::RefCell;
use std::collections::BTreeMap;
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender, unbounded_channel};

/// The call a script makes to the host: one tool call or one host global.
#[derive(Clone, Debug)]
pub struct HostCallRequest {
    /// The prelude's call id, 1-based in issue order.
    pub id: u32,
    pub target: ScriptCallTarget,
    pub name: String,
    /// JSON text of the arguments, `None` when the script called with none.
    pub arguments_json: Option<String>,
}

/// How the host answers the calls a script makes.
pub trait ScriptHost: Send + Sync {
    /// Runs one tool call and returns the JSON text of its result, or the
    /// failure the script's rejected promise carries.
    fn call(&self, request: HostCallRequest)
    -> BoxFuture<'static, Result<String, ToolCallFailure>>;

    /// Runs one host global. Same contract as [`ScriptHost::call`].
    fn call_global(
        &self,
        request: HostCallRequest,
    ) -> BoxFuture<'static, Result<String, ToolCallFailure>>;
}

/// What one script execution is given.
pub struct SandboxRequest {
    /// The parsed script body, options line already blanked.
    pub body: String,
    /// The script's own options, parsed from its options line.
    pub options: crate::source::ScriptOptions,
    /// The tools the script may call.
    pub tools: Vec<ScriptTool>,
    /// The host globals the script may call.
    pub globals: Vec<ScriptGlobal>,
    /// The current store values; the script's writes are reported in the outcome.
    pub store: StoreSnapshot,
    /// The host's hard deadline in milliseconds, on top of the script's own
    /// `timeout_ms`. `None` sets no host deadline.
    pub host_deadline_ms: Option<u64>,
}

/// The store values the host keeps across scripts.
pub type StoreSnapshot = BTreeMap<String, serde_json::Value>;

/// What one script execution produced.
pub struct SandboxOutcome {
    pub result: ScriptResult,
    /// The script's own output budget, for the caller's bookkeeping.
    pub max_output_tokens: Option<u64>,
    /// The values the script wrote through `store()`.
    pub store_writes: StoreWrites,
}

/// What the prelude's `done` report carries before the fragment is built.
#[derive(Clone, Debug, Default)]
struct ScriptReport {
    ok: bool,
    value_json: Option<String>,
    writes_json: Option<String>,
}

/// Why the host stopped a script before it settled.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum StopReason {
    Deadline,
    Cancelled,
}

impl StopReason {
    fn flag(self) -> u8 {
        match self {
            Self::Deadline => 1,
            Self::Cancelled => 2,
        }
    }

    fn kind(self) -> ScriptErrorKind {
        match self {
            Self::Deadline => ScriptErrorKind::Timeout,
            Self::Cancelled => ScriptErrorKind::Aborted,
        }
    }
}

fn stop_reason_flag(flag: u8) -> Option<StopReason> {
    match flag {
        1 => Some(StopReason::Deadline),
        2 => Some(StopReason::Cancelled),
        _ => None,
    }
}

enum VmToHost {
    Call(HostCallRequest),
    Output(OutputItem),
    Settled { id: u32, ok: bool },
}

enum HostToVm {
    Deliver {
        id: u32,
        result: Result<String, ToolCallFailure>,
    },
    Abort,
}

/// What the VM thread reports when the script settles.
struct OutcomeFragment {
    ok: bool,
    value_json: Option<String>,
    error: Option<ScriptError>,
    writes: StoreWrites,
}

/// Runs one script to completion against the host. Never rejects for script
/// failures; everything lands in the outcome. `cancellation` aborts the script
/// with kind `aborted` when it resolves.
pub async fn execute_script(
    request: SandboxRequest,
    host: Arc<dyn ScriptHost>,
    cancellation: tokio::sync::oneshot::Receiver<()>,
) -> SandboxOutcome {
    let (vm_sender, mut vm_receiver) = unbounded_channel::<VmToHost>();
    let (host_sender, host_receiver) = unbounded_channel::<HostToVm>();
    let interrupt = Arc::new(AtomicBool::new(false));
    let stop_reason = Arc::new(AtomicU8::new(0));

    let interrupt_for_host = interrupt.clone();
    let stop_reason_for_host = stop_reason.clone();
    let thread = std::thread::Builder::new()
        .name("parrot-codemode-vm".into())
        .stack_size(VM_THREAD_STACK_SIZE)
        .spawn(move || {
            run_vm(
                request.body,
                request.tools,
                request.globals,
                request.store,
                interrupt,
                stop_reason,
                host_receiver,
                vm_sender,
            )
        })
        .expect("the script thread spawns");

    let mut output: Vec<OutputItem> = Vec::new();
    let mut calls: BTreeMap<u32, ScriptCallRecord> = BTreeMap::new();
    let mut running: Vec<tokio::task::JoinHandle<()>> = Vec::new();

    let effective_deadline = match (request.host_deadline_ms, request.options.timeout_ms) {
        (Some(host), Some(script)) => Some(host.min(script)),
        (host, script) => host.or(script),
    };
    let deadline = tokio::time::Instant::now()
        + std::time::Duration::from_millis(effective_deadline.unwrap_or(u64::MAX));
    let deadline_sleep = tokio::time::sleep_until(deadline);
    tokio::pin!(deadline_sleep);
    let mut cancellation = cancellation;
    let mut stopped = false;

    loop {
        tokio::select! {
            biased;
            _ = &mut cancellation, if !stopped => {
                stopped = true;
                stop_reason_for_host.store(StopReason::Cancelled.flag(), Ordering::SeqCst);
                interrupt_for_host.store(true, Ordering::SeqCst);
                let _ = host_sender.send(HostToVm::Abort);
            }
            _ = &mut deadline_sleep, if effective_deadline.is_some() && !stopped => {
                stopped = true;
                stop_reason_for_host.store(StopReason::Deadline.flag(), Ordering::SeqCst);
                interrupt_for_host.store(true, Ordering::SeqCst);
                let _ = host_sender.send(HostToVm::Abort);
            }
            message = vm_receiver.recv() => {
                let Some(message) = message else {
                    break;
                };
                match message {
                    VmToHost::Call(request) => {
                        calls.insert(
                            request.id,
                            ScriptCallRecord {
                                id: request.id,
                                name: request.name.clone(),
                                target: request.target,
                                arguments_json: request.arguments_json.clone(),
                                status: ScriptCallStatus::Cancelled,
                            },
                        );
                        let host = host.clone();
                        let sender = host_sender.clone();
                        let call = request.clone();
                        running.push(tokio::spawn(async move {
                            let answer = match call.target {
                                ScriptCallTarget::Tool => host.call(call.clone()).await,
                                ScriptCallTarget::Global => host.call_global(call.clone()).await,
                            };
                            let _ = sender.send(HostToVm::Deliver {
                                id: call.id,
                                result: answer,
                            });
                        }));
                    }
                    VmToHost::Output(item) => output.push(item),
                    VmToHost::Settled { id, ok } => {
                        if let Some(record) = calls.get_mut(&id) {
                            record.status = if ok {
                                ScriptCallStatus::Ok
                            } else {
                                ScriptCallStatus::Error
                            };
                        }
                    }
                }
            }
        }
    }
    for handle in running.drain(..) {
        handle.abort();
    }
    drop(host_sender);
    let fragment = match thread.join() {
        Ok(fragment) => fragment,
        Err(_) => sandbox_failure("the script thread ended without reporting a result".into()),
    };
    let result = ScriptResult {
        ok: fragment.ok,
        output,
        value: fragment.value_json,
        error: fragment.error,
        calls: calls.into_values().collect(),
    };
    SandboxOutcome {
        result,
        max_output_tokens: request.options.max_output_tokens,
        store_writes: fragment.writes,
    }
}

const VM_THREAD_STACK_SIZE: usize = 16 * 1024 * 1024;

#[allow(clippy::too_many_arguments)]
fn run_vm(
    body: String,
    tools: Vec<ScriptTool>,
    globals: Vec<ScriptGlobal>,
    store: StoreSnapshot,
    interrupt: Arc<AtomicBool>,
    stop_reason: Arc<AtomicU8>,
    host_receiver: UnboundedReceiver<HostToVm>,
    vm_sender: UnboundedSender<VmToHost>,
) -> OutcomeFragment {
    let runtime = match Runtime::new() {
        Ok(runtime) => runtime,
        Err(error) => return sandbox_failure(format!("the VM runtime failed to start: {error}")),
    };
    runtime.set_memory_limit(CODEMODE_MEMORY_LIMIT_BYTES);
    runtime.set_max_stack_size(MAX_VM_STACK_SIZE);
    let handler_interrupt = interrupt.clone();
    runtime.set_interrupt_handler(Some(Box::new(move || {
        handler_interrupt.load(Ordering::Relaxed)
    })));

    let context = match Context::full(&runtime) {
        Ok(context) => context,
        Err(error) => return sandbox_failure(format!("the VM context failed to start: {error}")),
    };
    let tools_json = match serde_json::to_string(
        &tools
            .iter()
            .map(|tool| {
                serde_json::json!({
                    "name": tool.name,
                    "jsName": tool.js_name,
                    "description": tool.description,
                })
            })
            .collect::<Vec<_>>(),
    ) {
        Ok(json) => json,
        Err(_) => return sandbox_failure("tool descriptors failed to serialize".into()),
    };
    let globals_json = match serde_json::to_string(
        &globals
            .iter()
            .map(|global| {
                serde_json::json!({
                    "name": global.name,
                    "spread": global.spread,
                })
            })
            .collect::<Vec<_>>(),
    ) {
        Ok(json) => json,
        Err(_) => return sandbox_failure("global descriptors failed to serialize".into()),
    };
    let store_json = match serde_json::to_string(&store) {
        Ok(json) => json,
        Err(_) => return sandbox_failure("the store failed to serialize".into()),
    };

    let calls: Rc<RefCell<Vec<HostCallRequest>>> = Rc::new(RefCell::new(Vec::new()));
    let settled: Rc<RefCell<Option<ScriptReport>>> = Rc::new(RefCell::new(None));

    let fragment = context.with(|ctx| {
        drive(
            ctx,
            body,
            tools_json,
            globals_json,
            store_json,
            interrupt.clone(),
            stop_reason.clone(),
            calls.clone(),
            settled.clone(),
            host_receiver,
            vm_sender,
        )
    });
    refine_out_of_memory(&runtime, fragment)
}

/// QuickJS reports a heap-limit hit as an unnameable null exception. Naming it
/// from the runtime's own accounting keeps the script error readable. Safe
/// outside the context closure: the runtime lock is free there.
fn refine_out_of_memory(runtime: &Runtime, mut fragment: OutcomeFragment) -> OutcomeFragment {
    let Some(error) = fragment.error.as_ref() else {
        return fragment;
    };
    if error.kind != ScriptErrorKind::Script
        || (error.message != "null" && !error.message.is_empty())
    {
        return fragment;
    }
    let usage = runtime.memory_usage();
    eprintln!(
        "oom check: malloc_size={} malloc_limit={} memory_used_size={} memory_used_count={} malloc_count={}",
        usage.malloc_size,
        usage.malloc_limit,
        usage.memory_used_size,
        usage.memory_used_count,
        usage.malloc_count
    );
    if usage.memory_used_size >= (CODEMODE_MEMORY_LIMIT_BYTES as i64 / 10) * 9 {
        fragment.error = Some(ScriptError {
            kind: ScriptErrorKind::Script,
            name: "InternalError".into(),
            message: "out of memory".into(),
            stack: "InternalError: out of memory".into(),
        });
    }
    fragment
}

#[allow(clippy::too_many_arguments)]
fn drive(
    ctx: Ctx<'_>,
    body: String,
    tools_json: String,
    globals_json: String,
    store_json: String,
    interrupt: Arc<AtomicBool>,
    stop_reason: Arc<AtomicU8>,
    calls: Rc<RefCell<Vec<HostCallRequest>>>,
    settled: Rc<RefCell<Option<ScriptReport>>>,
    mut host_receiver: UnboundedReceiver<HostToVm>,
    vm_sender: UnboundedSender<VmToHost>,
) -> OutcomeFragment {
    let vm_sender_for_bridge = vm_sender.clone();
    let calls_for_bridge = calls.clone();
    let settled_for_bridge = settled.clone();
    let bridge = match Function::new(
        ctx.clone(),
        move |kind: String, a: Value, b: Value, c: Value| -> rquickjs::Result<()> {
            match kind.as_str() {
                "call" | "global" => {
                    let Some(id) = value_as_u32(&a) else {
                        return Ok(());
                    };
                    let name = value_as_string(&b).unwrap_or_default();
                    let arguments_json = value_as_string(&c);
                    let request = HostCallRequest {
                        id,
                        target: if kind == "call" {
                            ScriptCallTarget::Tool
                        } else {
                            ScriptCallTarget::Global
                        },
                        name,
                        arguments_json,
                    };
                    calls_for_bridge.borrow_mut().push(request.clone());
                    let _ = vm_sender_for_bridge.send(VmToHost::Call(request));
                }
                "output" => {
                    let kind = value_as_string(&a).unwrap_or_default();
                    let text = value_as_string(&b).unwrap_or_default();
                    let item = if kind == "console" {
                        OutputItem::Console(text)
                    } else {
                        OutputItem::Text(text)
                    };
                    let _ = vm_sender_for_bridge.send(VmToHost::Output(item));
                }
                "done" => {
                    let ok = value_as_string(&a).as_deref() == Some("1");
                    let payload = value_as_string(&b);
                    let writes = value_as_string(&c);
                    *settled_for_bridge.borrow_mut() = Some(ScriptReport {
                        ok,
                        value_json: payload,
                        writes_json: writes,
                    });
                }
                _ => {}
            }
            Ok(())
        },
    ) {
        Ok(bridge) => bridge,
        Err(error) => {
            return sandbox_failure(format!("the host bridge failed to register: {error}"));
        }
    };

    let api_value = eval_prelude(&ctx, bridge, tools_json, globals_json, store_json);
    let api = match api_value {
        Ok(api) => api,
        Err(_) => return described_failure(ctx, ScriptErrorKind::Sandbox, None),
    };
    let Some(api_object) = api.as_object() else {
        return sandbox_failure("the prelude did not return its api object".into());
    };
    let Ok(settle) = api_object.get::<_, Function>("settle") else {
        return sandbox_failure("the prelude api is missing settle".into());
    };
    let Ok(run) = api_object.get::<_, Function>("run") else {
        return sandbox_failure("the prelude api is missing run".into());
    };
    let Ok(stalled) = api_object.get::<_, Function>("stalled") else {
        return sandbox_failure("the prelude api is missing stalled".into());
    };
    let Ok(describe) = api_object.get::<_, Function>("describe") else {
        return sandbox_failure("the prelude api is missing describe".into());
    };

    let wrapper = format!("(async (tools, console) => {{{body}\n}})");
    let mut eval_options = EvalOptions::default();
    eval_options.global = true;
    eval_options.filename = Some("codemode.js".into());
    let script_fn = match ctx.eval_with_options::<Function, _>(wrapper, eval_options) {
        Ok(script_fn) => script_fn,
        Err(_) => return described_failure(ctx, ScriptErrorKind::Script, Some(&describe)),
    };

    if run.call::<_, ()>((script_fn,)).is_err() {
        // The interrupt throws mid-script; report the stop, not the throw.
        let kind = if interrupt.load(Ordering::Relaxed) {
            stopped_kind(&stop_reason)
        } else {
            ScriptErrorKind::Script
        };
        return described_failure(ctx, kind, Some(&describe));
    }
    if let Err(failure) = drain_jobs(&ctx, &interrupt, &stop_reason, &describe) {
        return failure_fragment(failure);
    }
    if settled.borrow().is_none() {
        if stalled.call::<_, bool>(()).is_err() {
            return described_failure(ctx, ScriptErrorKind::Sandbox, Some(&describe));
        }
        if let Err(failure) = drain_jobs(&ctx, &interrupt, &stop_reason, &describe) {
            return failure_fragment(failure);
        }
    }

    loop {
        if interrupt.load(Ordering::Relaxed) {
            break;
        }
        if settled.borrow().is_some() {
            break;
        }
        match host_receiver.blocking_recv() {
            Some(HostToVm::Abort) => break,
            Some(HostToVm::Deliver { id, result }) => {
                let ok = result.is_ok();
                let _ = vm_sender.send(VmToHost::Settled { id, ok });
                let (payload, properties) = match result {
                    Ok(value) => (Some(value), None),
                    Err(failure) => (Some(failure.message), failure.properties_json),
                };
                let payload = optional_value(&ctx, payload);
                let properties = optional_value(&ctx, properties);
                if let (Ok(payload), Ok(properties)) = (payload, properties) {
                    let _ = settle.call::<_, ()>((id, ok, payload, properties));
                }
                if let Err(failure) = drain_jobs(&ctx, &interrupt, &stop_reason, &describe) {
                    return failure_fragment(failure);
                }
                if settled.borrow().is_none() {
                    if stalled.call::<_, bool>(()).is_err() {
                        return described_failure(ctx, ScriptErrorKind::Sandbox, Some(&describe));
                    }
                    if let Err(failure) = drain_jobs(&ctx, &interrupt, &stop_reason, &describe) {
                        return failure_fragment(failure);
                    }
                }
            }
            None => break,
        }
    }

    let report = settled.borrow().clone();
    let Some(ScriptReport {
        ok,
        value_json,
        writes_json,
    }) = report
    else {
        return failure_fragment(Stop {
            kind: stopped_kind(&stop_reason),
            stack: None,
        });
    };
    let mut error = None;
    if !ok {
        error = Some(parse_error_json(
            value_json.as_deref(),
            stopped_kind(&stop_reason),
        ));
    }
    let writes = parse_writes(writes_json.as_deref());
    OutcomeFragment {
        ok,
        value_json: if ok { value_json } else { None },
        error,
        writes,
    }
}

#[derive(Debug)]
struct Stop {
    kind: ScriptErrorKind,
    stack: Option<String>,
}

#[allow(clippy::type_complexity)]
fn eval_prelude<'js>(
    ctx: &Ctx<'js>,
    bridge: Function<'js>,
    tools_json: String,
    globals_json: String,
    store_json: String,
) -> Result<Value<'js>, rquickjs::Error> {
    let mut eval_options = EvalOptions::default();
    eval_options.global = true;
    eval_options.filename = Some("codemode-prelude.js".into());
    let prelude_fn: Function = ctx.eval_with_options(prelude_source(), eval_options)?;
    prelude_fn.call::<_, Value>((bridge, tools_json, globals_json, store_json))
}

fn described_failure<'js>(
    ctx: Ctx<'js>,
    kind: ScriptErrorKind,
    describe: Option<&Function<'js>>,
) -> OutcomeFragment {
    let exception = if ctx.has_exception() {
        ctx.catch()
    } else {
        Value::new_undefined(ctx.clone())
    };
    let stack = describe
        .and_then(|describe| describe.call::<_, String>((exception.clone(),)).ok())
        .unwrap_or_else(|| "the script failed in the VM".into());
    failure_fragment(Stop {
        kind,
        stack: Some(stack),
    })
}

fn failure_fragment(failure: Stop) -> OutcomeFragment {
    let error = parse_error_json(failure.stack.as_deref(), failure.kind);
    OutcomeFragment {
        ok: false,
        value_json: None,
        error: Some(error),
        writes: StoreWrites::default(),
    }
}

fn stopped_kind(stop_reason: &Arc<AtomicU8>) -> ScriptErrorKind {
    stop_reason_flag(stop_reason.load(Ordering::Relaxed))
        .map(|reason| reason.kind())
        .unwrap_or(ScriptErrorKind::Script)
}

/// Drains promise jobs one at a time, checking the interrupt flag between
/// them, so a script spinning through jobs cannot outrun its stop. A failing
/// job leaves its exception in the context, which `catch` then drains.
fn drain_jobs<'js>(
    ctx: &Ctx<'js>,
    interrupt: &AtomicBool,
    stop_reason: &Arc<AtomicU8>,
    describe: &Function<'js>,
) -> Result<(), Stop> {
    loop {
        if interrupt.load(Ordering::Relaxed) {
            return Err(Stop {
                kind: stopped_kind(stop_reason),
                stack: None,
            });
        }
        if !ctx.execute_pending_job() {
            if !ctx.has_exception() {
                return Ok(());
            }
            let exception = ctx.catch();
            if interrupt.load(Ordering::Relaxed) {
                return Err(Stop {
                    kind: stopped_kind(stop_reason),
                    stack: None,
                });
            }
            let stack = describe
                .call::<_, String>((exception,))
                .unwrap_or_else(|_| "the script failed in the VM".into());
            return Err(Stop {
                kind: ScriptErrorKind::Script,
                stack: Some(stack),
            });
        }
    }
}

/// Builds a JS argument for an optional string: `undefined` when absent.
fn optional_value<'js>(ctx: &Ctx<'js>, value: Option<String>) -> rquickjs::Result<Value<'js>> {
    match value {
        Some(text) => Ok(rquickjs::String::from_str(ctx.clone(), &text)?.into_value()),
        None => Ok(Value::new_undefined(ctx.clone())),
    }
}

fn parse_error_json(stack: Option<&str>, base_kind: ScriptErrorKind) -> ScriptError {
    let fallback = ScriptError {
        kind: base_kind,
        name: "Error".into(),
        message: stack.unwrap_or("the script failed").to_string(),
        stack: stack.unwrap_or_default().to_string(),
    };
    let Some(stack) = stack else {
        return fallback;
    };
    let Ok(parsed) = serde_json::from_str::<serde_json::Value>(stack) else {
        return fallback;
    };
    if !parsed.is_object() {
        return fallback;
    }
    let message = parsed
        .get("message")
        .and_then(serde_json::Value::as_str)
        .unwrap_or_default();
    if message.is_empty() {
        return fallback;
    }
    // QuickJS throws a bare null when even the error object cannot allocate,
    // which only happens under heap pressure. Name it for the caller.
    if message == "null" {
        return ScriptError {
            kind: base_kind,
            name: "InternalError".into(),
            message: "out of memory".into(),
            stack: "InternalError: out of memory".into(),
        };
    }
    ScriptError {
        kind: base_kind,
        name: parsed
            .get("name")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("Error")
            .to_string(),
        message: message.to_string(),
        stack: parsed
            .get("stack")
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default()
            .to_string(),
    }
}

fn parse_writes(writes_json: Option<&str>) -> StoreWrites {
    let mut writes = StoreWrites::default();
    let Some(json) = writes_json else {
        return writes;
    };
    let Ok(entries) = serde_json::from_str::<serde_json::Value>(json) else {
        return writes;
    };
    let Some(entries) = entries.as_array() else {
        return writes;
    };
    for entry in entries {
        let Some(entry) = entry.as_array() else {
            continue;
        };
        let Some(key) = entry.first().and_then(serde_json::Value::as_str) else {
            continue;
        };
        match entry.get(1).and_then(serde_json::Value::as_str) {
            Some(json) => writes.set.push((key.to_string(), json.to_string())),
            None => writes.delete.push(key.to_string()),
        }
    }
    writes
}

fn sandbox_failure(message: String) -> OutcomeFragment {
    OutcomeFragment {
        ok: false,
        value_json: None,
        error: Some(ScriptError {
            kind: ScriptErrorKind::Sandbox,
            name: "Error".into(),
            message,
            stack: String::new(),
        }),
        writes: StoreWrites::default(),
    }
}

fn value_as_u32(value: &Value<'_>) -> Option<u32> {
    if let Some(int) = value.as_int() {
        return u32::try_from(int).ok();
    }
    value.as_number().and_then(|number| {
        if number >= 0.0 && number <= u32::MAX as f64 && number.fract() == 0.0 {
            Some(number as u32)
        } else {
            None
        }
    })
}

fn value_as_string(value: &Value<'_>) -> Option<String> {
    if value.is_undefined() || value.is_null() {
        return None;
    }
    if let Some(text) = value.as_string() {
        return text.to_string().ok().map(|borrowed| borrowed.to_string());
    }
    if let Some(int) = value.as_int() {
        return Some(int.to_string());
    }
    if let Some(number) = value.as_number() {
        return Some(number.to_string());
    }
    if let Some(boolean) = value.as_bool() {
        return Some(boolean.to_string());
    }
    None
}
