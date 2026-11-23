//! NestedToolRunner behavior tests: grouping, ordering, failure mapping,
//! cancellation, and the fresh call-id scheme.

use parrot_codemode::runner::{
    NestedCallRequest, NestedToolCapture, NestedToolRunner, SiblingToolRunner,
};
use parrot_domain::contracts::async_ops::{BoxFuture, Cancellation, Cancelled, OperationError};
use parrot_domain::contracts::paths::ResolvedRead;
use parrot_domain::contracts::security::{AccessSnapshot, InvocationScope};
use parrot_domain::contracts::tools::{ParallelSafety, Tool};
use parrot_foundation::template_catalog::PromptTemplateCatalog;
use parrot_provider::catalogs::Model;
use parrot_provider::routing::aliases::{ModelAliasCatalog, ModelRouter, ModelRouting};
use parrot_provider::routing::registry::ProviderRegistry;
use parrot_provider::sessions::Error as ProviderError;
use parrot_provider::sessions::{Error, Provider, Session};
use parrot_runtime::agent_turn::services::{TurnProfile, TurnSelection};
use parrot_runtime::tool_settlement::{
    SharedSecurity, ToolFailure, ToolHandle, ToolInvocation, ToolMetadata, ToolOutput,
};
use std::collections::BTreeMap;
use std::path::Path;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;
use tokio::sync::Notify;

fn boxed<'a, T, E>(
    future: impl Future<Output = Result<T, E>> + Send + 'a,
) -> BoxFuture<'a, Result<T, E>> {
    Box::pin(future)
}

/// Inert authority; the runner must lend whatever capture it was given.
#[derive(Default)]
struct TestSecurity {
    version: usize,
}
impl AccessSnapshot for TestSecurity {
    type Error = ();
    fn check_read(&self, _: &ResolvedRead) -> Result<(), ()> {
        Ok(())
    }
    fn check_write(&self, _: &Path) -> Result<(), ()> {
        Ok(())
    }
}

/// A provider that only exists so model resolution builds a real selection.
#[derive(Clone)]
struct InertProvider;
impl Provider for InertProvider {
    fn id(&self) -> &str {
        "inert"
    }
    fn has_credential<'a>(&'a self, _: &'a dyn Cancellation) -> BoxFuture<'a, Result<bool, Error>> {
        boxed(async { Ok(false) })
    }
    fn seed_models(&self) -> Vec<Model> {
        vec![Model::new("model", "inert")]
    }
    fn list_models<'a>(
        &'a self,
        _: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<Vec<Model>, Error>> {
        boxed(async { Ok(vec![Model::new("model", "inert")]) })
    }
    fn open_session(&self) -> Result<Arc<dyn Session>, Error> {
        Ok(Arc::new(InertSession))
    }
}
struct InertSession;
impl Session for InertSession {
    fn begin_turn(&self) -> BoxFuture<'_, Result<(), Error>> {
        boxed(async { Ok(()) })
    }
    fn call(
        &self,
        _: parrot_provider::sessions::model::Request,
    ) -> Result<Box<dyn parrot_provider::sessions::EventStream>, Error> {
        Err(ProviderError::Provider(
            "inert session does not answer".into(),
        ))
    }
    fn try_fall_back_to_http(&self) -> BoxFuture<'_, Result<bool, Error>> {
        boxed(async { Ok(false) })
    }
    fn close(&self) -> BoxFuture<'_, Result<(), Error>> {
        boxed(async { Ok(()) })
    }
}

fn selection() -> TurnSelection<TestSecurity> {
    let registry = Arc::new(
        ProviderRegistry::new(
            vec![Arc::new(InertProvider)],
            BTreeMap::from([("inert".into(), vec![Model::new("model", "inert")])]),
        )
        .unwrap(),
    );
    let aliases = Arc::new(ModelAliasCatalog::new(registry.clone(), vec![]).unwrap());
    let router = ModelRouter::new(
        registry,
        Arc::new(ModelRouting::new(aliases, "inert/model")),
    );
    TurnSelection {
        model: router.resolve("inert/model").unwrap(),
        profile: TurnProfile {
            id: "test".into(),
            maximum_requests: 100,
            allowed_tools: None,
            disabled_tools: vec![],
        },
        security: Arc::new(TestSecurity { version: 7 }),
    }
}

fn templates() -> Arc<PromptTemplateCatalog> {
    Arc::new(
        PromptTemplateCatalog::from_content(
            "prompt_templates:\n  test.marker:\n    template: 'test.marker'\n    allowed_arguments: []\n    required_arguments: []\n",
            &BTreeMap::from([("HOME".into(), "/synthetic-home".into())]),
        )
        .unwrap()
        .1,
    )
}

/// A cancellation the test drives by hand.
#[derive(Default)]
struct TestCancellation {
    flag: AtomicBool,
    notify: Notify,
}
impl TestCancellation {
    fn cancel(&self) {
        self.flag.store(true, Ordering::SeqCst);
        self.notify.notify_waiters();
    }
}
impl Cancellation for TestCancellation {
    fn is_cancelled(&self) -> bool {
        self.flag.load(Ordering::SeqCst)
    }
    fn cancelled(&self) -> BoxFuture<'_, ()> {
        let future = async move {
            loop {
                if self.flag.load(Ordering::SeqCst) {
                    return;
                }
                self.notify.notified().await;
            }
        };
        Box::pin(future)
    }
}

type Effect = Arc<
    dyn Fn(ToolInvocation) -> BoxFuture<'static, Result<ToolOutput, OperationError<ToolFailure>>>
        + Send
        + Sync,
>;

/// A fake tool that records its starts and ends in a shared order log.
struct FakeTool {
    metadata: ToolMetadata,
    safety: ParallelSafety,
    effect: Effect,
}

impl FakeTool {
    fn metadata(name: &str, aliases: Vec<String>) -> ToolMetadata {
        ToolMetadata {
            name: name.into(),
            aliases,
        }
    }
}

impl Tool for FakeTool {
    type Definition = ToolMetadata;
    type Invocation = ToolInvocation;
    type Selection = TurnSelection<TestSecurity>;
    type Security = SharedSecurity<TestSecurity>;
    type Output = ToolOutput;
    type Error = ToolFailure;
    fn definition(&self) -> &ToolMetadata {
        &self.metadata
    }
    fn parallel_safety(&self, _: &ToolInvocation) -> ParallelSafety {
        self.safety
    }
    fn enabled_after_interruption(&self) -> bool {
        false
    }
    fn invoke<'a>(
        &'a self,
        invocation: ToolInvocation,
        scope: InvocationScope<'a, SharedSecurity<TestSecurity>, TurnSelection<TestSecurity>>,
    ) -> BoxFuture<'a, Result<ToolOutput, OperationError<ToolFailure>>> {
        // The scope must be observed so the runner's capture stays honest.
        let _security_version = scope.security().snapshot().version;
        let effect = self.effect.clone();
        Box::pin(async move { (effect)(invocation).await })
    }
}

fn handle(tool: FakeTool) -> ToolHandle<TestSecurity> {
    ToolHandle::Ordinary(Arc::new(tool))
}

fn capture(cancellation: Arc<dyn Cancellation>) -> NestedToolCapture<TestSecurity> {
    NestedToolCapture {
        selection: selection(),
        assistant_sequence: 1,
        prompt_templates: templates(),
        image_budget: None,
        cancellation,
    }
}

fn request(name: &str) -> NestedCallRequest {
    NestedCallRequest {
        name: name.into(),
        arguments_json: "{}".into(),
    }
}

/// A tool that logs start, waits on the gate, and logs end. `gate` uses two
/// slots: two parallel tools must both arrive before either can proceed.
fn pair_tool(
    name: &str,
    arrivals: Arc<(std::sync::atomic::AtomicUsize, Notify)>,
    log: Arc<Mutex<Vec<String>>>,
) -> ToolHandle<TestSecurity> {
    let name = name.to_string();
    let fake = FakeTool {
        metadata: FakeTool::metadata(&name, vec![]),
        safety: ParallelSafety::ParallelSafe,
        effect: {
            let log = log.clone();
            let name = name.clone();
            Arc::new(move |_| {
                let log = log.clone();
                let arrivals = arrivals.clone();
                let name = name.clone();
                Box::pin(async move {
                    log.lock().unwrap().push(format!("{name}:start"));
                    let arrived = arrivals.0.fetch_add(1, Ordering::SeqCst) + 1;
                    if arrived < 2 {
                        arrivals.1.notified().await;
                    } else {
                        arrivals.1.notify_waiters();
                    }
                    log.lock().unwrap().push(format!("{name}:end"));
                    Ok(ToolOutput::text(name))
                }) as BoxFuture<'static, _>
            })
        },
    };
    handle(fake)
}

/// A parallel-safe tool that sleeps, so a non-serialized overlap shows in the log.
fn slow_parallel_tool(
    name: &str,
    log: Arc<Mutex<Vec<String>>>,
    pause: Duration,
) -> ToolHandle<TestSecurity> {
    let name = name.to_string();
    let fake = FakeTool {
        metadata: FakeTool::metadata(&name, vec![]),
        safety: ParallelSafety::ParallelSafe,
        effect: {
            let log = log.clone();
            let name = name.clone();
            Arc::new(move |_| {
                let log = log.clone();
                let name = name.clone();
                Box::pin(async move {
                    log.lock().unwrap().push(format!("{name}:start"));
                    tokio::time::sleep(pause).await;
                    log.lock().unwrap().push(format!("{name}:end"));
                    Ok(ToolOutput::text(name))
                }) as BoxFuture<'static, _>
            })
        },
    };
    handle(fake)
}

fn sequential_tool(
    name: &str,
    log: Arc<Mutex<Vec<String>>>,
    pause: Duration,
) -> ToolHandle<TestSecurity> {
    let name = name.to_string();
    let fake = FakeTool {
        metadata: FakeTool::metadata(&name, vec![]),
        safety: ParallelSafety::Sequential,
        effect: {
            let log = log.clone();
            let name = name.clone();
            Arc::new(move |_| {
                let log = log.clone();
                let name = name.clone();
                Box::pin(async move {
                    log.lock().unwrap().push(format!("{name}:start"));
                    tokio::time::sleep(pause).await;
                    log.lock().unwrap().push(format!("{name}:end"));
                    Ok(ToolOutput::text(name))
                }) as BoxFuture<'static, _>
            })
        },
    };
    handle(fake)
}

fn plain_tool(name: &str, text: &str, aliases: Vec<String>) -> ToolHandle<TestSecurity> {
    let text = text.to_string();
    let fake = FakeTool {
        metadata: FakeTool::metadata(name, aliases),
        safety: ParallelSafety::Sequential,
        effect: Arc::new(move |_| {
            let text = text.clone();
            Box::pin(async move { Ok(ToolOutput::text(text)) }) as BoxFuture<'static, _>
        }),
    };
    handle(fake)
}

fn failing_tool(name: &str, message: &str) -> ToolHandle<TestSecurity> {
    let message = message.to_string();
    let fake = FakeTool {
        metadata: FakeTool::metadata(name, vec![]),
        safety: ParallelSafety::Sequential,
        effect: Arc::new(move |_| {
            let message = message.clone();
            Box::pin(async move {
                Err(OperationError::Failed(ToolFailure::new(
                    message,
                    parrot_storage::diagnostics::FailureKind::InvalidOperation,
                )))
            }) as BoxFuture<'static, _>
        }),
    };
    handle(fake)
}

#[tokio::test]
async fn sequential_tool_serializes_its_neighbors() {
    let log: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(vec![]));
    let tools = vec![
        slow_parallel_tool("left", log.clone(), Duration::from_millis(150)),
        sequential_tool("middle", log.clone(), Duration::from_millis(50)),
        slow_parallel_tool("right", log.clone(), Duration::from_millis(150)),
    ];
    let runner = SiblingToolRunner::new(tools);
    let capture = capture(Arc::new(TestCancellation::default()));
    let outcome = tokio::time::timeout(
        Duration::from_secs(10),
        NestedToolRunner::<TestSecurity>::run(
            &runner,
            &capture,
            vec![request("left"), request("middle"), request("right")],
        ),
    )
    .await
    .expect("the run finishes");
    assert_eq!(outcome.len(), 3);
    assert_eq!(outcome[0].call_id(), "c1");
    assert_eq!(outcome[1].call_id(), "c2");
    assert_eq!(outcome[2].call_id(), "c3");
    let events = log.lock().unwrap().clone();
    let position = |name: &str| events.iter().position(|event| event == name).unwrap();
    assert!(
        position("left:end") < position("middle:start"),
        "left must finish before middle starts: {events:?}"
    );
    assert!(
        position("middle:end") < position("right:start"),
        "middle must finish before right starts: {events:?}"
    );
    // A fully serialized run would place right:start after left:end too.
    assert!(
        position("left:start") < position("right:start"),
        "{events:?}"
    );
}

/// Two parallel-safe calls must overlap: each pair tool waits for the other to
/// start, so a serialized run deadlocks and the timeout fails the test.
#[tokio::test]
async fn parallel_safe_calls_run_concurrently() {
    let log: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(vec![]));
    let arrivals: Arc<(std::sync::atomic::AtomicUsize, Notify)> =
        Arc::new((std::sync::atomic::AtomicUsize::new(0), Notify::new()));
    let tools = vec![
        pair_tool("left", arrivals.clone(), log.clone()),
        pair_tool("right", arrivals.clone(), log.clone()),
    ];
    let runner = SiblingToolRunner::new(tools);
    let capture = capture(Arc::new(TestCancellation::default()));
    let outcome = tokio::time::timeout(
        Duration::from_secs(10),
        NestedToolRunner::<TestSecurity>::run(
            &runner,
            &capture,
            vec![request("left"), request("right")],
        ),
    )
    .await
    .expect("both parallel tools finish, which proves they overlap");
    assert_eq!(outcome.len(), 2);
    let events = log.lock().unwrap().clone();
    let left_start = events
        .iter()
        .position(|event| event == "left:start")
        .unwrap();
    let right_start = events
        .iter()
        .position(|event| event == "right:start")
        .unwrap();
    let left_end = events.iter().position(|event| event == "left:end").unwrap();
    let right_end = events
        .iter()
        .position(|event| event == "right:end")
        .unwrap();
    assert!(
        left_start < right_end && right_start < left_end,
        "both start before either ends: {events:?}"
    );
}

#[tokio::test]
async fn failures_map_with_their_message_and_order_holds() {
    let tools = vec![
        plain_tool("read", "file body", vec![]),
        failing_tool("write", "the write failed"),
        plain_tool("list", "a, b", vec!["ls".into()]),
    ];
    let runner = SiblingToolRunner::new(tools);
    let capture = capture(Arc::new(TestCancellation::default()));
    let outcome = NestedToolRunner::<TestSecurity>::run(
        &runner,
        &capture,
        vec![request("read"), request("write"), request("ls")],
    )
    .await;
    assert_eq!(outcome.len(), 3);
    let NestedCallOutcomeProbe::Ok { call_id, text, .. } = probe(&outcome[0]) else {
        panic!("read must succeed");
    };
    assert_eq!(call_id, "c1");
    assert_eq!(text, "file body");
    let NestedCallOutcomeProbe::Failed { call_id, message } = probe(&outcome[1]) else {
        panic!("write must fail");
    };
    assert_eq!(call_id, "c2");
    assert_eq!(message, "the write failed");
    let NestedCallOutcomeProbe::Ok { call_id, text, .. } = probe(&outcome[2]) else {
        panic!("alias lookup must succeed");
    };
    assert_eq!(call_id, "c3");
    assert_eq!(text, "a, b");
}

#[tokio::test]
async fn unknown_tool_fails_with_its_name() {
    let tools = vec![plain_tool("read", "body", vec![])];
    let runner = SiblingToolRunner::new(tools);
    let capture = capture(Arc::new(TestCancellation::default()));
    let outcome =
        NestedToolRunner::<TestSecurity>::run(&runner, &capture, vec![request("missing")]).await;
    let NestedCallOutcomeProbe::Failed { call_id, message } = probe(&outcome[0]) else {
        panic!("unknown tool must fail");
    };
    assert_eq!(call_id, "c1");
    assert_eq!(message, "unknown tool missing");
}

/// Drives the cancellation mid-call: the runner must return promptly with a
/// cancelled outcome while the tool is still waiting.
#[tokio::test]
async fn cancellation_mid_call_returns_promptly() {
    let held = Arc::new(tokio::sync::Semaphore::new(0));
    let cancel = Arc::new(TestCancellation::default());
    let log: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(vec![]));
    let tool = {
        let held = held.clone();
        let effect_log = log.clone();
        FakeTool {
            metadata: FakeTool::metadata("slow", vec![]),
            safety: ParallelSafety::Sequential,
            effect: Arc::new(move |_| {
                let held = held.clone();
                let log = effect_log.clone();
                Box::pin(async move {
                    log.lock().unwrap().push("slow:start".into());
                    let _ = held.acquire().await;
                    log.lock().unwrap().push("slow:end".into());
                    Ok(ToolOutput::text("never"))
                }) as BoxFuture<'static, _>
            }),
        }
    };
    let runner = SiblingToolRunner::new(vec![handle(tool)]);
    let capture = capture(cancel.clone() as Arc<dyn Cancellation>);
    let run_task = tokio::spawn(async move {
        NestedToolRunner::<TestSecurity>::run(&runner, &capture, vec![request("slow")]).await
    });
    // Wait until the tool has started, then cancel.
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if log
                .lock()
                .unwrap()
                .iter()
                .any(|event| event == "slow:start")
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("the tool starts");
    cancel.cancel();
    let outcomes = tokio::time::timeout(Duration::from_secs(5), run_task)
        .await
        .expect("the run returns promptly after cancellation")
        .expect("the task joins");
    let NestedCallOutcomeProbe::Failed { call_id, message } = probe(&outcomes[0]) else {
        panic!("a cancelled call must not succeed");
    };
    assert_eq!(call_id, "c1");
    assert_eq!(message, "operation cancelled");
    let events = log.lock().unwrap().clone();
    assert!(
        !events.contains(&"slow:end".to_string()),
        "the held tool never completed: {events:?}"
    );
    held.add_permits(1);
}

/// Watchdog variant: the tool releases, the run finishes normally.
#[tokio::test]
async fn run_completes_when_never_cancelled() {
    let tools = vec![plain_tool("read", "body", vec![])];
    let runner = SiblingToolRunner::new(tools);
    let capture = capture(Arc::new(TestCancellation::default()));
    let outcome =
        NestedToolRunner::<TestSecurity>::run(&runner, &capture, vec![request("read")]).await;
    assert_eq!(outcome.len(), 1);
    assert!(matches!(
        probe(&outcome[0]),
        NestedCallOutcomeProbe::Ok { .. }
    ));
}

enum NestedCallOutcomeProbe {
    Ok { call_id: String, text: String },
    Failed { call_id: String, message: String },
}

fn probe(outcome: &parrot_codemode::runner::NestedCallOutcome) -> NestedCallOutcomeProbe {
    match outcome {
        parrot_codemode::runner::NestedCallOutcome::Ok { call_id, text, .. } => {
            NestedCallOutcomeProbe::Ok {
                call_id: call_id.clone(),
                text: text.clone(),
            }
        }
        parrot_codemode::runner::NestedCallOutcome::Failed { call_id, message } => {
            NestedCallOutcomeProbe::Failed {
                call_id: call_id.clone(),
                message: message.clone(),
            }
        }
    }
}

#[allow(dead_code)]
fn cancelled_witness(_: Cancelled) {}
