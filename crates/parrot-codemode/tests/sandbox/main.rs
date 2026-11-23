//! Sandbox behavior tests: the bridge, deadlines, memory, store, options, and
//! cancellation, each against a fake host.

use parrot_codemode::source::parse_source;
use parrot_codemode::values::{ScriptCallStatus, ScriptErrorKind, ToolCallFailure};
use parrot_codemode::{HostCallRequest, SandboxOutcome, SandboxRequest, ScriptHost};
use parrot_domain::contracts::async_ops::BoxFuture;
use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::oneshot;

fn boxed<'a, T, E>(
    future: impl Future<Output = Result<T, E>> + Send + 'a,
) -> BoxFuture<'a, Result<T, E>> {
    Box::pin(future)
}

/// A host whose tool returns a fixed string, optionally delaying or failing.
struct FakeHost {
    reply: Arc<str>,
    delay: Option<Duration>,
    fail: Option<&'static str>,
}

impl FakeHost {
    fn answer(&self, _request: HostCallRequest) -> Result<String, ToolCallFailure> {
        if let Some(fail) = self.fail {
            return Err(ToolCallFailure {
                message: fail.into(),
                properties_json: None,
            });
        }
        Ok(format!("\"{}\"", self.reply))
    }
}

impl ScriptHost for FakeHost {
    fn call(
        &self,
        request: HostCallRequest,
    ) -> BoxFuture<'static, Result<String, ToolCallFailure>> {
        let reply = self.answer(request);
        let delay = self.delay;
        boxed(async move {
            if let Some(delay) = delay {
                tokio::time::sleep(delay).await;
            }
            reply
        })
    }

    fn call_global(
        &self,
        request: HostCallRequest,
    ) -> BoxFuture<'static, Result<String, ToolCallFailure>> {
        self.call(request)
    }
}

fn request(body: &str) -> SandboxRequest {
    let (parsed_body, options) = parse_source(body).expect("the source parses");
    SandboxRequest {
        body: parsed_body,
        options,
        tools: vec![parrot_codemode::ScriptTool {
            name: "read".into(),
            js_name: "read".into(),
            description: "Read a file".into(),
        }],
        globals: vec![],
        store: BTreeMap::new(),
        host_deadline_ms: None,
    }
}

async fn run(body: &str) -> SandboxOutcome {
    let host = Arc::new(FakeHost {
        reply: "hi".into(),
        delay: None,
        fail: None,
    });
    parrot_codemode::execute_script(request(body), host, oneshot::channel().1).await
}

#[tokio::test]
async fn concurrent_awaits_resolve() {
    let outcome =
        run("const [a, b] = await Promise.all([tools.read({}), tools.read({})]); return a + b;")
            .await;
    let result = &outcome.result;
    assert!(result.ok, "the script failed: {:?}", result.error);
    assert_eq!(result.value.as_deref(), Some("\"hihi\""));
    assert_eq!(result.calls.len(), 2);
    assert!(
        result
            .calls
            .iter()
            .all(|call| call.status == ScriptCallStatus::Ok)
    );
    assert_eq!(result.calls[0].id, 1);
    assert_eq!(result.calls[1].id, 2);
}

#[tokio::test]
async fn deadline_kills_spinning_script() {
    let (parsed_body, options) = parse_source("while (true) {}").unwrap();
    let host = Arc::new(FakeHost {
        reply: "hi".into(),
        delay: None,
        fail: None,
    });
    let outcome = parrot_codemode::execute_script(
        SandboxRequest {
            body: parsed_body,
            options,
            host_deadline_ms: Some(300),
            ..request("while (true) {}")
        },
        host,
        oneshot::channel().1,
    )
    .await;
    let result = &outcome.result;
    assert!(!result.ok);
    assert_eq!(
        result.error.as_ref().map(|error| error.kind),
        Some(ScriptErrorKind::Timeout)
    );
}

#[tokio::test]
async fn memory_limit_fails_allocations() {
    let outcome =
        run("const array = []; while (true) { array.push(new Array(1024 * 1024)); }").await;
    let result = &outcome.result;
    assert!(!result.ok);
    let error = result.error.as_ref().expect("the failure reports");
    assert_eq!(error.kind, ScriptErrorKind::Script);
    println!("memory test error: {:?}", error);
    assert!(
        error.message.to_lowercase().contains("memory"),
        "{}",
        error.message
    );
}

#[tokio::test]
async fn store_limits_enforce() {
    // One value beyond the per-value cap fails.
    let oversized = "x".repeat(parrot_codemode::MAX_STORE_VALUE_CHARS + 1);
    let outcome = run(&format!("store(\"big\", \"{oversized}\");")).await;
    assert!(!outcome.result.ok);
    let error = outcome.result.error.as_ref().expect("the failure reports");
    assert!(
        error.message.contains("characters of JSON"),
        "{}",
        error.message
    );

    // A normal write is reported.
    let outcome = run("store(\"kept\", 41); return load(\"kept\");").await;
    assert!(outcome.result.ok);
    assert_eq!(outcome.store_writes.set.len(), 1);
    assert_eq!(outcome.store_writes.set[0].0, "kept");
    assert_eq!(outcome.result.value.as_deref(), Some("41"));

    // Storing undefined deletes the key and is reported as a delete.
    let outcome = run("store(\"gone\", undefined);").await;
    assert!(outcome.result.ok);
    assert_eq!(outcome.store_writes.delete, vec!["gone".to_string()]);
}

#[tokio::test]
async fn options_line_shapes_deadline() {
    let body = "// @options: {\"timeout_ms\": 300}\nwhile (true) {}";
    let (parsed_body, options) = parse_source(body).unwrap();
    assert_eq!(options.timeout_ms, Some(300));
    let host = Arc::new(FakeHost {
        reply: "hi".into(),
        delay: None,
        fail: None,
    });
    let outcome = parrot_codemode::execute_script(
        SandboxRequest {
            body: parsed_body,
            options,
            host_deadline_ms: None,
            ..request(body)
        },
        host,
        oneshot::channel().1,
    )
    .await;
    assert_eq!(
        outcome.result.error.as_ref().map(|error| error.kind),
        Some(ScriptErrorKind::Timeout)
    );
}

#[tokio::test]
async fn failed_tool_rejects_in_script() {
    let host = Arc::new(FakeHost {
        reply: "hi".into(),
        delay: None,
        fail: Some("the tool failed"),
    });
    let outcome = parrot_codemode::execute_script(
        request("try { await tools.read({}); } catch (error) { return String(error.message); }"),
        host,
        oneshot::channel().1,
    )
    .await;
    let result = &outcome.result;
    assert!(result.ok);
    assert_eq!(result.value.as_deref(), Some("\"the tool failed\""));
}

#[tokio::test]
async fn cancellation_aborts_pending_call() {
    let host = Arc::new(FakeHost {
        reply: "hi".into(),
        delay: Some(Duration::from_secs(30)),
        fail: None,
    });
    let (cancel, cancel_receiver) = oneshot::channel::<()>();
    let handle = tokio::spawn(parrot_codemode::execute_script(
        request("await tools.read({});"),
        host,
        cancel_receiver,
    ));
    tokio::time::sleep(Duration::from_millis(200)).await;
    cancel.send(()).expect("the cancellation sends");
    let started = std::time::Instant::now();
    let outcome = handle.await.expect("the execution finishes");
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "cancellation returned promptly"
    );
    let result = &outcome.result;
    assert!(!result.ok);
    assert_eq!(
        result.error.as_ref().map(|error| error.kind),
        Some(ScriptErrorKind::Aborted)
    );
    assert_eq!(result.calls.len(), 1);
    assert_eq!(result.calls[0].status, ScriptCallStatus::Cancelled);
}

#[tokio::test]
async fn script_without_pending_work_fails_fast() {
    let started = std::time::Instant::now();
    let outcome = run("await new Promise(() => {});").await;
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "the stalled script fails fast"
    );
    assert!(!outcome.result.ok);
    assert!(
        outcome
            .result
            .error
            .as_ref()
            .expect("the failure reports")
            .message
            .contains("never settle")
    );
}

#[tokio::test]
async fn output_helpers_record_items() {
    let outcome = run("text(\"first\"); console.log(\"logged\"); text({a: 1}); return 7;").await;
    let result = &outcome.result;
    assert!(result.ok);
    assert_eq!(result.output.len(), 3);
    assert_eq!(result.output[0].text(), "first");
    assert_eq!(result.output[1].text(), "logged");
    assert_eq!(result.output[2].text(), "{\"a\":1}");
    assert_eq!(result.value.as_deref(), Some("7"));
}

#[tokio::test]
async fn unknown_tool_member_errors() {
    let outcome = run("await tools.write({});").await;
    assert!(!outcome.result.ok);
    let error = outcome.result.error.as_ref().expect("the failure reports");
    assert!(
        error.message.contains("tools.write does not exist"),
        "{}",
        error.message
    );
}

#[tokio::test]
async fn store_survives_across_executions() {
    let mut store: BTreeMap<String, serde_json::Value> = BTreeMap::new();
    let host: Arc<dyn ScriptHost> = Arc::new(FakeHost {
        reply: "hi".into(),
        delay: None,
        fail: None,
    });
    let mut first = request("store(\"runs\", (load(\"runs\") ?? 0) + 1);");
    first.store = store.clone();
    let outcome = parrot_codemode::execute_script(first, host.clone(), oneshot::channel().1).await;
    assert!(outcome.result.ok);
    for (key, value) in &outcome.store_writes.set {
        store.insert(key.clone(), serde_json::from_str(value).unwrap());
    }
    let mut second = request("return load(\"runs\");");
    second.store = store.clone();
    let outcome = parrot_codemode::execute_script(second, host, oneshot::channel().1).await;
    assert_eq!(outcome.result.value.as_deref(), Some("1"));
}

#[test]
fn identifier_mapping_matches_pi() {
    assert_eq!(parrot_codemode::to_identifier("my-tool"), "my_tool");
    assert_eq!(
        parrot_codemode::to_identifier("mcp__docs__search"),
        "mcp__docs__search"
    );
    assert_eq!(parrot_codemode::to_identifier("2fa"), "_fa");
}

#[tokio::test]
async fn tool_error_status_recorded() {
    let host = Arc::new(FakeHost {
        reply: "hi".into(),
        delay: None,
        fail: Some("nope"),
    });
    let outcome = parrot_codemode::execute_script(
        request("try { await tools.read({}); } catch { } return null;"),
        host,
        oneshot::channel().1,
    )
    .await;
    assert!(outcome.result.ok);
    assert_eq!(outcome.result.calls[0].status, ScriptCallStatus::Error);
    assert!(
        outcome
            .result
            .calls
            .iter()
            .any(|call| matches!(call.target, parrot_codemode::ScriptCallTarget::Tool))
    );
}

#[tokio::test]
async fn minimal_text_only() {
    let outcome = run("text(\"hi\"); return 1;").await;
    assert!(outcome.result.ok, "{:?}", outcome.result.error);
}

#[tokio::test]
async fn minimal_single_call() {
    let outcome = run("const v = await tools.read({}); return v;").await;
    assert!(outcome.result.ok, "{:?}", outcome.result.error);
    assert_eq!(outcome.result.value.as_deref(), Some("\"hi\""));
}
