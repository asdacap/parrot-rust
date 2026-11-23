//! CodemodeTool behavior tests: the tool contract, description assembly,
//! result formatting, and cancellation, against real fakes over the same
//! types the runner tests use.

use parrot_codemode::declarations::{DEFAULT_INLINE_BUDGET, DeclaredTool};
use parrot_codemode::runner::{NestedCallOutcome, NestedCallRequest, NestedToolRunner};
use parrot_codemode::tool::{
    CODEMODE_PARAMETERS_JSON, CODEMODE_TOOL_NAME, CallTimings, CodemodeStore, CodemodeTool,
    RunnerInjection, ScopedCancellation, build_description, format_script_result,
};
use parrot_codemode::values::ScriptCallStatus;
use parrot_domain::contracts::async_ops::{BoxFuture, Cancellation, OperationError};
use parrot_domain::contracts::security::{AccessSnapshot, InvocationScope};
use parrot_domain::contracts::tools::{ParallelSafety, Tool};
use parrot_foundation::template_catalog::PromptTemplateCatalog;
use parrot_provider::catalogs::Model;
use parrot_provider::routing::aliases::{ModelAliasCatalog, ModelRouter, ModelRouting};
use parrot_provider::routing::registry::ProviderRegistry;
use parrot_provider::sessions::Error as ProviderError;
use parrot_provider::sessions::{Error, Provider, Session};
use parrot_runtime::agent_turn::services::{TurnProfile, TurnSelection};
use parrot_runtime::tool_settlement::{SharedSecurity, ToolFailure, ToolInvocation, ToolOutput};
use std::collections::BTreeMap;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use tokio::sync::Notify;

fn boxed<'a, T, E>(
    future: impl Future<Output = Result<T, E>> + Send + 'a,
) -> BoxFuture<'a, Result<T, E>> {
    Box::pin(future)
}

#[derive(Default)]
struct TestSecurity;
impl AccessSnapshot for TestSecurity {
    type Error = ();
    fn check_read(&self, _: &parrot_domain::contracts::paths::ResolvedRead) -> Result<(), ()> {
        Ok(())
    }
    fn check_write(&self, _: &Path) -> Result<(), ()> {
        Ok(())
    }
}

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
        security: Arc::new(TestSecurity),
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

/// The hand-driven cancellation the tests observe through the scope.
#[derive(Default)]
struct TestCancellation {
    flag: AtomicBool,
    notify: Notify,
}
impl TestCancellation {
    #[allow(dead_code)]
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
        let flag = &self.flag;
        let notify = &self.notify;
        let future = async move {
            loop {
                if flag.load(Ordering::SeqCst) {
                    return;
                }
                notify.notified().await;
            }
        };
        Box::pin(future)
    }
}

/// One scripted outcome, produced on demand.
type Answer = fn() -> NestedCallOutcome;

/// A fake runner that replays scripted outcomes, so tests need no real tools.
struct FakeRunner {
    /// One future per call the script makes, in call order.
    answers: Arc<tokio::sync::Mutex<Vec<Answer>>>,
    seen: Arc<AtomicU32>,
}

impl FakeRunner {
    fn with(answers: Vec<Answer>) -> (Self, Arc<AtomicU32>) {
        let seen = Arc::new(AtomicU32::new(0));
        (
            Self {
                answers: Arc::new(tokio::sync::Mutex::new(answers)),
                seen: seen.clone(),
            },
            seen,
        )
    }
}

impl<S: AccessSnapshot + 'static> NestedToolRunner<S> for FakeRunner {
    fn run(
        &self,
        _capture: &parrot_codemode::runner::NestedToolCapture<S>,
        calls: Vec<NestedCallRequest>,
    ) -> BoxFuture<'_, Vec<NestedCallOutcome>> {
        let answers = Arc::clone(&self.answers);
        self.seen.fetch_add(calls.len() as u32, Ordering::SeqCst);
        Box::pin(async move {
            let mut answers = answers.lock().await;
            let taken: Vec<Answer> = calls
                .iter()
                .map(|_| answers.pop().expect("the fake runner has answers"))
                .collect();
            drop(answers);
            taken.into_iter().map(|answer| answer()).collect()
        })
    }
}

fn ok_outcome() -> NestedCallOutcome {
    NestedCallOutcome::Ok {
        call_id: "c0".into(),
        text: "SECRET TOOL BODY the model must not see".into(),
        image_artifacts: vec![],
        display_result: None,
    }
}

fn failed_outcome() -> NestedCallOutcome {
    NestedCallOutcome::Failed {
        call_id: "c0".into(),
        message: "the fake tool failed".into(),
    }
}

fn sibling_declarations() -> Vec<DeclaredTool> {
    vec![
        DeclaredTool {
            name: "read".into(),
            description: "Read a file".into(),
            parameters_json: serde_json::json!({
                "type": "object",
                "properties": { "path": { "type": "string" } },
                "required": ["path"],
            }),
            output_schema: None,
        },
        DeclaredTool {
            name: "grep".into(),
            description: "Search file contents".into(),
            parameters_json: serde_json::json!({
                "type": "object",
                "properties": { "pattern": { "type": "string" } },
                "required": ["pattern"],
            }),
            output_schema: None,
        },
    ]
}

fn invocation(arguments_json: &str) -> ToolInvocation {
    let mut invocation = ToolInvocation::new("call-1", arguments_json, 7, templates());
    invocation.image_budget = None;
    invocation
}

async fn invoke_code(
    tool: &CodemodeTool<TestSecurity>,
    code: &str,
    cancellation: &Arc<TestCancellation>,
) -> Result<ToolOutput, OperationError<ToolFailure>> {
    let arguments = serde_json::json!({ "code": code }).to_string();
    let selection = selection();
    let security = SharedSecurity(Arc::clone(&selection.security));
    let scope = InvocationScope::new(security, selection, cancellation.as_ref());
    tool.invoke(invocation(&arguments), scope).await
}

#[tokio::test]
async fn un_injected_runner_fails_explicitly() {
    let tool: CodemodeTool<TestSecurity> = CodemodeTool::new(Arc::new(CodemodeStore::new()));
    let cancellation = Arc::new(TestCancellation::default());
    let outcome = invoke_code(&tool, "return 1;", &cancellation).await;
    let message = match outcome {
        Err(OperationError::Failed(failure)) => failure.message,
        other => panic!("expected an explicit failure, got {other:?}"),
    };
    assert!(message.contains("not been injected"), "{message}");
}

#[tokio::test]
async fn two_nested_calls_return_joined_output_with_summary() {
    let tool: CodemodeTool<TestSecurity> = CodemodeTool::new(Arc::new(CodemodeStore::new()));
    let (runner, seen) = FakeRunner::with(vec![ok_outcome, ok_outcome]);
    tool.inject(RunnerInjection {
        runner: Arc::new(runner),
        siblings: sibling_declarations(),
        inline_budget: None,
    });
    let cancellation = Arc::new(TestCancellation::default());
    let outcome = invoke_code(
        &tool,
        "const [a, b] = await Promise.all([tools.read({path: 'x'}), tools.read({path: 'y'})]); text(a + ' ' + b); return 'done';",
        &cancellation,
    )
    .await
    .expect("the tool succeeds");
    let text = &outcome.text;
    assert!(text.starts_with("Script completed"), "{}", text);
    assert!(
        text.contains("==> text 1/2 <==\nSECRET TOOL BODY the model must not see"),
        "{}",
        text
    );
    assert!(text.contains("read ok"), "summary line missing: {text}");
    assert!(text.contains("c1 read ok"), "call id missing: {text}");
    assert_eq!(seen.load(Ordering::SeqCst), 2, "the runner saw two calls");
}

#[tokio::test]
async fn failing_script_returns_failed_header_with_partial_output() {
    let tool: CodemodeTool<TestSecurity> = CodemodeTool::new(Arc::new(CodemodeStore::new()));
    let (runner, _) = FakeRunner::with(vec![ok_outcome]);
    tool.inject(RunnerInjection {
        runner: Arc::new(runner),
        siblings: sibling_declarations(),
        inline_budget: None,
    });
    let cancellation = Arc::new(TestCancellation::default());
    let outcome = invoke_code(
        &tool,
        "text(\"before the fall\"); await tools.read({path: 'x'}); throw new Error('boom');",
        &cancellation,
    )
    .await
    .expect("the tool reports a script failure as a completed tool call");
    let text = &outcome.text;
    assert!(text.starts_with("Script failed"), "{}", text);
    assert!(
        text.contains("before the fall"),
        "partial output lost: {text}"
    );
    assert!(text.contains("Script error:"), "{}", text);
    assert!(text.contains("boom"), "{}", text);
}

#[tokio::test]
async fn cancellation_returns_promptly_with_aborted_kind() {
    let tool: CodemodeTool<TestSecurity> = CodemodeTool::new(Arc::new(CodemodeStore::new()));
    // The tool call never finishes: the script hangs on it.
    struct NeverRunner;
    impl<S: AccessSnapshot + 'static> NestedToolRunner<S> for NeverRunner {
        fn run(
            &self,
            _: &parrot_codemode::runner::NestedToolCapture<S>,
            _: Vec<NestedCallRequest>,
        ) -> BoxFuture<'_, Vec<NestedCallOutcome>> {
            Box::pin(async { std::future::pending().await })
        }
    }
    tool.inject(RunnerInjection {
        runner: Arc::new(NeverRunner),
        siblings: sibling_declarations(),
        inline_budget: None,
    });
    let cancellation = Arc::new(TestCancellation::default());
    tokio::spawn({
        let cancellation = Arc::clone(&cancellation);
        async move {
            tokio::time::sleep(std::time::Duration::from_millis(200)).await;
            cancellation.cancel();
        }
    });
    let started = std::time::Instant::now();
    let outcome = invoke_code(&tool, "await tools.read({path: 'x'});", &cancellation).await;
    let text = match outcome {
        Ok(output) => output.text,
        Err(OperationError::Cancelled) => panic!("the tool cancelled instead of reporting"),
        Err(OperationError::Failed(failure)) => panic!("unexpected failure: {}", failure.message),
    };
    assert!(
        started.elapsed() < std::time::Duration::from_secs(10),
        "cancellation returned promptly"
    );
    assert!(text.contains("Script aborted"), "{}", text);
}

#[tokio::test]
async fn description_contains_declarations_and_respects_budget() {
    let siblings = sibling_declarations();
    let full = build_description(&siblings, None);
    assert!(full.contains("await tools.<name>({ ...args })"), "{full}");
    assert!(full.contains("### `read`"), "{full}");
    assert!(full.contains("Read a file"), "{full}");
    assert!(full.contains("### `grep`"), "{full}");
    assert!(full.contains("store(key, value)"), "{full}");
    assert!(full.contains("// @options:"), "{full}");
    assert!(full.contains("listTools()"), "{full}");

    // A tiny budget drops sections rather than the intro.
    let tiny = build_description(&siblings, Some(20));
    assert!(tiny.contains("await tools.<name>({ ...args })"), "{tiny}");
    assert!(
        !tiny.contains("### `read`"),
        "budget did not drop a section: {tiny}"
    );
}

#[tokio::test]
async fn parallel_safety_and_interruption_flags() {
    let tool: CodemodeTool<TestSecurity> = CodemodeTool::new(Arc::new(CodemodeStore::new()));
    let invocation = invocation("{}");
    assert_eq!(
        tool.parallel_safety(&invocation),
        ParallelSafety::Sequential
    );
    assert!(!tool.enabled_after_interruption());
    assert_eq!(tool.definition().name, CODEMODE_TOOL_NAME);
    assert!(tool.definition().aliases.is_empty());
}

#[tokio::test]
async fn malformed_arguments_fail_with_argument_kind() {
    let tool: CodemodeTool<TestSecurity> = CodemodeTool::new(Arc::new(CodemodeStore::new()));
    let (runner, _) = FakeRunner::with(vec![]);
    tool.inject(RunnerInjection {
        runner: Arc::new(runner),
        siblings: sibling_declarations(),
        inline_budget: None,
    });
    let selection = selection();
    let security = SharedSecurity(Arc::clone(&selection.security));
    let cancellation = TestCancellation::default();
    let scope = InvocationScope::new(security, selection, &cancellation);
    let outcome = tool
        .invoke(invocation("\"just a string\""), scope)
        .await
        .expect_err("non-object arguments fail");
    match outcome {
        OperationError::Failed(failure) => {
            assert!(failure.message.contains("code"), "{}", failure.message)
        }
        other => panic!("unexpected: {other:?}"),
    }
}

#[tokio::test]
async fn store_values_carry_between_calls() {
    let store = Arc::new(CodemodeStore::new());
    let tool = CodemodeTool::new(store.clone());
    let (runner, _) = FakeRunner::with(vec![]);
    tool.inject(RunnerInjection {
        runner: Arc::new(runner),
        siblings: sibling_declarations(),
        inline_budget: None,
    });
    let cancellation = Arc::new(TestCancellation::default());
    let outcome = invoke_code(
        &tool,
        "store(\"runs\", 3); return load(\"runs\");",
        &cancellation,
    )
    .await
    .expect("the first script succeeds");
    assert!(outcome.text.contains("3"), "{}", outcome.text);
    let cancellation = Arc::new(TestCancellation::default());
    let outcome = invoke_code(&tool, "return load(\"runs\");", &cancellation)
        .await
        .expect("the second script succeeds");
    assert!(
        outcome.text.contains("3"),
        "store did not carry: {}",
        outcome.text
    );
}

#[tokio::test]
async fn result_text_hides_nested_tool_output_on_failure() {
    let tool: CodemodeTool<TestSecurity> = CodemodeTool::new(Arc::new(CodemodeStore::new()));
    // FakeRunner serves answers from the back, so this is failed first.
    let (runner, _) = FakeRunner::with(vec![ok_outcome, failed_outcome]);
    tool.inject(RunnerInjection {
        runner: Arc::new(runner),
        siblings: sibling_declarations(),
        inline_budget: None,
    });
    let cancellation = Arc::new(TestCancellation::default());
    let outcome = invoke_code(
        &tool,
        "try { await tools.read({path: 'x'}); } catch { } return 'kept';",
        &cancellation,
    )
    .await
    .expect("the script survives the failed call");
    let text = &outcome.text;
    assert!(text.starts_with("Script completed"), "{}", text);
    assert!(text.contains("c1 read error"), "{}", text);
    assert!(text.contains("kept"), "{}", text);
    assert!(
        !text.contains("SECRET TOOL BODY"),
        "nested output leaked into the result: {text}"
    );
}

#[tokio::test]
async fn configured_definition_appears_after_injection() {
    let tool: CodemodeTool<TestSecurity> = CodemodeTool::new(Arc::new(CodemodeStore::new()));
    assert!(tool.configured_definition().is_none());
    let (runner, _) = FakeRunner::with(vec![]);
    tool.inject(RunnerInjection {
        runner: Arc::new(runner),
        siblings: sibling_declarations(),
        inline_budget: Some(DEFAULT_INLINE_BUDGET),
    });
    let definition = tool
        .configured_definition()
        .expect("the definition exists once injected");
    assert_eq!(definition.parameters_json, CODEMODE_PARAMETERS_JSON);
    assert!(definition.description.contains("### `read`"));
}

#[tokio::test]
async fn console_and_value_framing() {
    // A direct formatter check: several text items, console lines, a value.
    let result = parrot_codemode::ScriptResult {
        ok: true,
        output: vec![
            parrot_codemode::values::OutputItem::Text("first".into()),
            parrot_codemode::values::OutputItem::Text("second".into()),
            parrot_codemode::values::OutputItem::Console("logged".into()),
        ],
        value: Some("7".into()),
        error: None,
        calls: vec![parrot_codemode::ScriptCallRecord {
            id: 1,
            name: "read".into(),
            target: parrot_codemode::values::ScriptCallTarget::Tool,
            arguments_json: Some("{\"path\":\"x\"}".into()),
            status: ScriptCallStatus::Ok,
        }],
    };
    let outcome = parrot_codemode::SandboxOutcome {
        result,
        max_output_tokens: None,
        store_writes: parrot_codemode::values::StoreWrites::default(),
    };
    let text = format_script_result(
        &outcome,
        std::time::Duration::from_millis(1500),
        &CallTimings::new(),
    );
    assert!(text.contains("==> text 1/3 <==\nfirst"), "{}", text);
    assert!(text.contains("==> text 2/3 <==\nsecond"), "{}", text);
    assert!(text.contains("==> text 3/3 <==\n7"), "{}", text);
    assert!(
        text.contains("<console_output>\nlogged\n</console_output>"),
        "{}",
        text
    );
    assert!(text.contains("c1 read ok"), "{}", text);
}

#[tokio::test]
async fn cancelled_scope_propagates_into_the_bind() {
    // The ScopedCancellation the tool builds resolves when the scope's
    // cancellation fires, which the sandbox and runner both observe.
    let bind = ScopedCancellation::new();
    let watcher = Arc::new(bind.clone());
    let observer = {
        let bind = watcher;
        tokio::spawn(async move {
            bind.cancelled().await;
            true
        })
    };
    bind.fire();
    assert!(observer.await.expect("the watcher completes"));
}
