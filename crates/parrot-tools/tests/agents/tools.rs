use std::sync::{Arc, Mutex};

use parrot_domain::contracts::async_ops::{BoxFuture, Cancellation, OperationError};
use parrot_domain::contracts::runtime::AgentRuntime;
use parrot_domain::contracts::tools::{ParallelSafety, Tool};
use parrot_domain::values::tools::ConfiguredToolDefinition;
use parrot_tools::agents::input::{ForkSelection, WaitInput};
use parrot_tools::agents::status::StatusSnapshot;
use parrot_tools::agents::*;

use super::{CANCELLED, NEVER, Security, context, invocation};

#[derive(Default)]
pub(super) struct Runtime {
    spawn: Mutex<Vec<(SpawnRequest, Security)>>,
    sends: Mutex<Vec<SendRequest>>,
    interrupts: Mutex<Vec<NamedRequest>>,
    waits: Mutex<Vec<WaitInput>>,
    failure: Mutex<Option<AgentToolError>>,
}
impl Runtime {
    fn result<T>(&self, value: T) -> Result<T, OperationError<AgentToolError>> {
        match self.failure.lock().unwrap().clone() {
            None => Ok(value),
            Some(error) => Err(OperationError::Failed(error)),
        }
    }
}
impl AgentRuntime for Runtime {
    type Security = Security;
    type SpawnRequest = SpawnRequest;
    type SpawnResult = SpawnResult;
    type SendRequest = SendRequest;
    type SendResult = SendResult;
    type InterruptRequest = NamedRequest;
    type InterruptResult = ();
    type StatusRequest = NamedRequest;
    type StatusResult = StatusSnapshot;
    type WaitRequest = WaitInput;
    type WaitResult = String;
    type Error = AgentToolError;

    fn spawn<'a>(
        &'a self,
        request: SpawnRequest,
        security: Security,
        cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<SpawnResult, OperationError<AgentToolError>>> {
        Box::pin(async move {
            cancellation
                .check()
                .map_err(|_| OperationError::Cancelled)?;
            self.spawn.lock().unwrap().push((request, security));
            self.result(SpawnResult {
                name: "child-helper".into(),
                depth: 1,
                output_path: "/out/live.dat".into(),
            })
        })
    }
    fn send<'a>(
        &'a self,
        request: SendRequest,
        cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<SendResult, OperationError<AgentToolError>>> {
        Box::pin(async move {
            cancellation
                .check()
                .map_err(|_| OperationError::Cancelled)?;
            self.sends.lock().unwrap().push(request);
            self.result(SendResult {
                name: "worker".into(),
                message_id: "msg-9".into(),
                follow_up: false,
            })
        })
    }
    fn interrupt<'a>(
        &'a self,
        request: NamedRequest,
        cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<(), OperationError<AgentToolError>>> {
        assert!(!cancellation.is_cancelled());
        Box::pin(async move {
            self.interrupts.lock().unwrap().push(request);
            self.result(())
        })
    }
    fn status(&self, request: NamedRequest) -> Result<StatusSnapshot, AgentToolError> {
        if request.name == "missing" {
            return Err(AgentToolError::Registry(
                "child agent not found: missing".into(),
            ));
        }
        Ok(super::status::snapshot())
    }
    fn wait<'a>(
        &'a self,
        request: WaitInput,
        cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<String, OperationError<AgentToolError>>> {
        Box::pin(async move {
            cancellation
                .check()
                .map_err(|_| OperationError::Cancelled)?;
            self.waits.lock().unwrap().push(request);
            self.result("wait interrupted".into())
        })
    }
}
pub(super) struct Models;
impl ModelSelector for Models {
    fn resolve(&self, selector: &str) -> Result<String, AgentToolError> {
        match selector {
            "fast" => Ok("fast".into()),
            _ => Err(AgentToolError::Argument(format!(
                "model alias: alias \"{selector}\" is not configured"
            ))),
        }
    }
}
fn definition() -> ConfiguredToolDefinition {
    ConfiguredToolDefinition::new("configured description", "{\"type\":\"object\"}")
}
fn tool(kind: AgentToolKind, runtime: Arc<Runtime>) -> AgentTool<Runtime, Models> {
    AgentTool::new(kind, definition(), runtime, Arc::new(Models), None, false)
}

#[tokio::test]
async fn spawn_uses_captured_model_security_and_exact_before_batch_metadata() {
    let runtime = Arc::new(Runtime::default());
    let tool = tool(AgentToolKind::Spawn, runtime.clone());
    let result = tool.invoke(invocation(r#"{"prompt":"work","agent":"worker","name":"  Child Helper!  ","scope":"bounded","fork":"full"}"#), context(&NEVER)).await.unwrap();
    assert_eq!(
        result,
        r#"{"name":"child-helper","kind":"agent","status":"running","depth":1,"output":"/out/live.dat"}"#
    );
    assert!(!result.contains("session_id"));
    let requests = runtime.spawn.lock().unwrap();
    let (request, security) = &requests[0];
    assert_eq!(request.input.model, "captured/model");
    assert_eq!(request.input.name, "  Child Helper!  ");
    assert_eq!(request.input.scope, "bounded");
    assert_eq!(request.input.fork, ForkSelection::Full);
    assert_eq!(request.assistant_sequence, 17);
    assert_eq!(request.call_id, "call-9");
    assert_eq!(security, &Security(42));
}

#[tokio::test]
async fn explicit_model_resolves_and_invalid_alias_prevents_spawn() {
    let runtime = Arc::new(Runtime::default());
    let tool = tool(AgentToolKind::Spawn, runtime.clone());
    tool.invoke(
        invocation(r#"{"prompt":"p","agent":"worker","name":"n","model":"fast"}"#),
        context(&NEVER),
    )
    .await
    .unwrap();
    assert_eq!(runtime.spawn.lock().unwrap()[0].0.input.model, "fast");
    let result = tool
        .invoke(
            invocation(r#"{"prompt":"p","agent":"worker","name":"n","model":"broken"}"#),
            context(&NEVER),
        )
        .await
        .unwrap();
    assert_eq!(
        result,
        "error: model alias: alias \"broken\" is not configured"
    );
    assert_eq!(runtime.spawn.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn spawn_maps_profile_depth_provider_failures_but_does_not_swallow_service_failures() {
    for error in [
        AgentToolError::Registry("subagent depth limit reached".into()),
        AgentToolError::Argument("agent profile build cannot be spawned".into()),
        AgentToolError::Provider("no model".into()),
    ] {
        let runtime = Arc::new(Runtime::default());
        *runtime.failure.lock().unwrap() = Some(error.clone());
        let result = tool(AgentToolKind::Spawn, runtime)
            .invoke(
                invocation(r#"{"prompt":"p","agent":"worker","name":"n"}"#),
                context(&NEVER),
            )
            .await
            .unwrap();
        assert_eq!(result, format!("error: {error}"));
    }
    let runtime = Arc::new(Runtime::default());
    *runtime.failure.lock().unwrap() = Some(AgentToolError::Service("storage failed".into()));
    let result = tool(AgentToolKind::Spawn, runtime)
        .invoke(
            invocation(r#"{"prompt":"p","agent":"worker","name":"n"}"#),
            context(&NEVER),
        )
        .await;
    assert_eq!(
        result,
        Err(OperationError::Failed(AgentToolError::Service(
            "storage failed".into()
        )))
    );
}

#[tokio::test]
async fn malformed_and_oversized_input_never_reaches_services() {
    let runtime = Arc::new(Runtime::default());
    for kind in [
        AgentToolKind::Spawn,
        AgentToolKind::Send,
        AgentToolKind::Interrupt,
        AgentToolKind::Status,
    ] {
        assert!(
            tool(kind, runtime.clone())
                .invoke(invocation("{}"), context(&NEVER))
                .await
                .unwrap()
                .starts_with("error: ")
        );
    }
    let arguments = serde_json::json!({"name":"worker","message":"界".repeat(10923)}).to_string();
    assert_eq!(
        tool(AgentToolKind::Send, runtime.clone())
            .invoke(invocation(&arguments), context(&NEVER))
            .await
            .unwrap(),
        "error: agent message exceeds 32768 UTF-8 bytes; split it into smaller messages"
    );
    assert!(runtime.spawn.lock().unwrap().is_empty());
    assert!(runtime.sends.lock().unwrap().is_empty());
    assert!(runtime.interrupts.lock().unwrap().is_empty());
}

#[tokio::test]
async fn send_preserves_parent_policy_and_exact_utf8_boundary() {
    let runtime = Arc::new(Runtime::default());
    let args = serde_json::json!({"name":"parent/parent", "message":"x".repeat(32768)}).to_string();
    let result = tool(AgentToolKind::Send, runtime.clone())
        .invoke(invocation(&args), context(&NEVER))
        .await
        .unwrap();
    assert_eq!(result, r#"{"name":"worker","message_id":"msg-9"}"#);
    let requests = runtime.sends.lock().unwrap();
    assert_eq!(requests[0].input.name, "parent/parent");
    assert_eq!(requests[0].input.message.len(), 32768);
    assert!(!requests[0].allow_parent);
    drop(requests);
    assert_eq!(
        SendResult {
            name: "parent".into(),
            message_id: "ignored".into(),
            follow_up: true
        }
        .format(),
        r#"{"name":"parent","status":"running"}"#
    );
}

#[tokio::test]
async fn interrupt_direct_child_request_is_uncancelled_and_returns_acknowledgement() {
    let runtime = Arc::new(Runtime::default());
    let tool = tool(AgentToolKind::Interrupt, runtime.clone());
    let result = tool
        .invoke(invocation(r#"{"name":"  worker  "}"#), context(&CANCELLED))
        .await
        .unwrap();
    assert_eq!(
        result,
        "Interrupt requested for child agent 'worker'. Its stop is reported through its completion event."
    );
    assert_eq!(
        runtime.interrupts.lock().unwrap().as_slice(),
        &[NamedRequest {
            name: "worker".into()
        }]
    );
    for name in ["", "   "] {
        let args = serde_json::json!({"name":name}).to_string();
        assert_eq!(
            tool.invoke(invocation(&args), context(&NEVER))
                .await
                .unwrap(),
            "error: no child agent given"
        );
    }
    *runtime.failure.lock().unwrap() = Some(AgentToolError::Registry(
        "direct child not found: missing".into(),
    ));
    assert_eq!(
        tool.invoke(invocation(r#"{"name":"missing"}"#), context(&NEVER))
            .await
            .unwrap(),
        "error: direct child not found: missing"
    );
    let before = runtime.interrupts.lock().unwrap().len();
    assert!(
        tool.invoke(
            invocation(r#"{"name":"worker","extra":1}"#),
            context(&NEVER)
        )
        .await
        .unwrap()
        .starts_with("error: ")
    );
    assert_eq!(runtime.interrupts.lock().unwrap().len(), before);
}

#[tokio::test]
async fn wait_passes_duration_and_cancellation_without_text_error_conversion() {
    let runtime = Arc::new(Runtime::default());
    let tool = tool(AgentToolKind::Wait, runtime.clone());
    assert_eq!(
        tool.invoke(invocation("{}"), context(&NEVER))
            .await
            .unwrap(),
        "wait interrupted"
    );
    assert_eq!(
        runtime.waits.lock().unwrap().as_slice(),
        &[WaitInput {
            duration_ms: 300000
        }]
    );
    assert_eq!(
        tool.invoke(invocation("{}"), context(&CANCELLED)).await,
        Err(OperationError::Cancelled)
    );
    assert_eq!(runtime.waits.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn status_keeps_retained_cursor_and_registry_errors() {
    let runtime = Arc::new(Runtime::default());
    let tool = tool(AgentToolKind::Status, runtime);
    let first = tool
        .invoke(invocation(r#"{"name":"worker"}"#), context(&CANCELLED))
        .await
        .unwrap();
    assert!(first.starts_with("Agent status\nName: worker"));
    assert_eq!(
        tool.invoke(invocation(r#"{"name":"   "}"#), context(&NEVER))
            .await
            .unwrap(),
        "error: no child agent given"
    );
    assert_eq!(
        tool.invoke(invocation(r#"{"name":"missing"}"#), context(&NEVER))
            .await
            .unwrap(),
        "error: child agent not found: missing"
    );
}

struct Templates;
impl ResultTemplates for Templates {
    fn render(&self, name: &str, args: &[(&str, &str)]) -> Result<String, AgentToolError> {
        Ok(format!(
            "{name}:{}",
            args.iter()
                .map(|(key, value)| format!("{key}={value}"))
                .collect::<Vec<_>>()
                .join("|")
        ))
    }
}

#[test]
fn metadata_and_dynamic_send_description_preserve_configured_parameters() {
    let runtime = Arc::new(Runtime::default());
    for (kind, expected_name) in [
        (AgentToolKind::Spawn, "agent_spawn"),
        (AgentToolKind::Send, "agent_send"),
        (AgentToolKind::Interrupt, "agent_interrupt"),
        (AgentToolKind::Status, "agent_status"),
        (AgentToolKind::Wait, "wait"),
    ] {
        let tool = tool(kind, runtime.clone());
        assert_eq!(tool.name(), expected_name);
        assert_eq!(tool.definition(), &definition());
        assert_eq!(
            tool.enabled_after_interruption(),
            kind != AgentToolKind::Spawn
        );
        assert_eq!(
            tool.parallel_safety(&invocation("{}")),
            ParallelSafety::Sequential
        );
    }
    let configured = definition();
    assert_eq!(
        send_definition(configured.clone(), true, &Templates).unwrap(),
        configured
    );
    let restricted = send_definition(configured.clone(), false, &Templates).unwrap();
    assert_eq!(
        restricted.description,
        "agent-send-tool.description-without-parent:"
    );
    assert_eq!(restricted.parameters_json, configured.parameters_json);
}

#[tokio::test]
async fn configured_error_and_interrupt_templates_are_used_without_touching_json_results() {
    let runtime = Arc::new(Runtime::default());
    let tool = AgentTool::new(
        AgentToolKind::Interrupt,
        definition(),
        runtime,
        Arc::new(Models),
        Some(Arc::new(Templates)),
        false,
    );
    assert_eq!(
        tool.invoke(invocation("{}"), context(&NEVER))
            .await
            .unwrap(),
        "tool-result.error:message=Tool arguments require a string 'name'."
    );
    assert!(
        tool.invoke(invocation(r#"{"name":"worker"}"#), context(&NEVER))
            .await
            .unwrap()
            .starts_with("tool-result.text:value=Interrupt requested")
    );
}

#[test]
fn actual_source_formatter_capture_matches_all_native_result_bytes() {
    let mut native = Vec::new();
    for name in ["worker", "a\"<&😀", "plus+quote'backtick`", "العربية界"] {
        native.push(
            SpawnResult {
                name: name.into(),
                depth: 2,
                output_path: "x\\y\n".into(),
            }
            .format(),
        );
        native.push(
            SendResult {
                name: name.into(),
                message_id: "msg-9".into(),
                follow_up: false,
            }
            .format(),
        );
        native.push(
            SendResult {
                name: name.into(),
                message_id: "unused".into(),
                follow_up: true,
            }
            .format(),
        );
    }
    assert_eq!(
        format!("{}\n", native.join("\n")),
        include_str!("fixtures/formatting/source.stdout")
    );
}

#[test]
fn json_result_escaping_preserves_source_writer_shape() {
    let value = SpawnResult {
        name: "a\"<&😀".into(),
        depth: 2,
        output_path: "x\\y\n".into(),
    }
    .format();
    assert_eq!(
        value,
        r#"{"name":"a\u0022\u003C\u0026\uD83D\uDE00","kind":"agent","status":"running","depth":2,"output":"x\\y\n"}"#
    );
    let decoded: serde_json::Value = serde_json::from_str(&value).unwrap();
    assert_eq!(decoded["name"], "a\"<&😀");
}
