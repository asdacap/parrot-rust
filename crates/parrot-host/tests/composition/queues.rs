//! T62 queue binding: actual scoped runtime queues through retained P07 Tools.
//! Fixture scope construction is the accepted T48/T51 real-session harness;
//! security below authorizes no filesystem operation and no fake queue exists.

#[allow(dead_code)]
#[path = "../../../parrot-runtime/tests/children/admission_publisher.rs"]
mod admission_publisher;
#[allow(dead_code)]
#[path = "../../../parrot-runtime/tests/agent_queues/harness.rs"]
mod harness;
#[allow(dead_code)]
#[path = "../../../parrot-runtime/tests/children/support.rs"]
mod support;

use std::{
    path::Path,
    sync::{Arc, Mutex},
    time::{Duration, Instant, SystemTime},
};

use parrot_domain::{
    contracts::{
        async_ops::{BoxFuture, Cancellation, Cancelled, Clock, OperationError, OperationResult},
        filesystem::{
            AuthorizedReadService, BoundRead, FileMetadata, FileReadError, FileReadErrorKind,
            LinkPolicy, ReadRequest,
        },
        security::InvocationContext,
        tools::Tool,
    },
    values::tools::{ConfiguredToolDefinition, ToolDefinitionCatalog},
};
use parrot_host::composition::queues::{QueueOwner, ScopedQueueBackend};
use parrot_runtime::{agent_turn::lifecycle::Signal, children::Scope};
use parrot_storage::events::NeverCancelled;
use parrot_tools::tasks_queues::{
    ToolInvocation, ToolOutput,
    queues::{QueueDiagnostic, QueueDiagnostics, QueueTool, QueueToolKind},
};
use serde_json::{Value, json};
use support::Security;
use tokio::sync::Semaphore;

/// Inline queue test input must not acquire filesystem reading authority.
struct NoReads;
fn denied<T>() -> OperationResult<T, FileReadError> {
    Err(OperationError::Failed(FileReadError::new(
        FileReadErrorKind::Denied,
        "queue binding test does not authorize reads",
    )))
}
impl AuthorizedReadService for NoReads {
    type Security = Security;
    fn inspect<'a>(
        &'a self,
        _: &'a Path,
        _: LinkPolicy,
        _: &'a Security,
        _: &'a dyn Cancellation,
    ) -> BoxFuture<'a, OperationResult<FileMetadata, FileReadError>> {
        Box::pin(async { denied() })
    }
    fn list_directory<'a>(
        &'a self,
        _: &'a Path,
        _: LinkPolicy,
        _: &'a Security,
        _: &'a dyn Cancellation,
    ) -> BoxFuture<'a, OperationResult<Vec<FileMetadata>, FileReadError>> {
        Box::pin(async { denied() })
    }
    fn read<'a>(
        &'a self,
        _: &'a ReadRequest,
        _: &'a Security,
        _: &'a dyn Cancellation,
    ) -> BoxFuture<'a, OperationResult<BoundRead, FileReadError>> {
        Box::pin(async { denied() })
    }
}
struct RealClock {
    sleeps: Mutex<Vec<Duration>>,
    sleeping: Semaphore,
}
impl Default for RealClock {
    fn default() -> Self {
        Self {
            sleeps: Mutex::new(Vec::new()),
            sleeping: Semaphore::new(0),
        }
    }
}
impl Clock for RealClock {
    fn now(&self) -> Instant {
        Instant::now()
    }
    fn utc_now(&self) -> SystemTime {
        SystemTime::now()
    }
    fn sleep_until<'a>(
        &'a self,
        deadline: Instant,
        cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<(), Cancelled>> {
        Box::pin(async move {
            self.sleeps
                .lock()
                .unwrap()
                .push(deadline.saturating_duration_since(Instant::now()));
            self.sleeping.add_permits(1);
            tokio::select! {
                _ = cancellation.cancelled() => Err(Cancelled),
                _ = tokio::time::sleep_until(tokio::time::Instant::from_std(deadline)) => cancellation.check(),
            }
        })
    }
}
#[derive(Default)]
struct Diagnostics(Mutex<Vec<QueueDiagnostic>>);
impl QueueDiagnostics for Diagnostics {
    fn record(&self, event: QueueDiagnostic) {
        self.0.lock().unwrap().push(event);
    }
}

type BoundTool = QueueTool<
    ScopedQueueBackend<Security>,
    NoReads,
    RealClock,
    Diagnostics,
    QueueOwner<Security>,
    (),
>;
struct Fixture {
    runtime: harness::Harness,
    root: Arc<Scope<Security>>,
    clock: Arc<RealClock>,
    log: Arc<Diagnostics>,
    definitions: ToolDefinitionCatalog,
}
impl Fixture {
    async fn new() -> Self {
        use parrot_foundation::config::{
            configuration::source_predefined_content, runtime::read_tool_definitions, yaml,
        };
        let runtime = harness::Harness::new();
        let root = runtime.root().await;
        let predefined = yaml::parse_root(&source_predefined_content().unwrap()).unwrap();
        let definitions = ToolDefinitionCatalog::new(
            &read_tool_definitions(&predefined)
                .unwrap()
                .into_iter()
                .map(|(name, definition)| {
                    (
                        name,
                        ConfiguredToolDefinition::new(
                            definition.description,
                            definition.parameters_json,
                        ),
                    )
                })
                .collect(),
        );
        Self {
            runtime,
            root,
            clock: Arc::new(RealClock::default()),
            log: Arc::new(Diagnostics::default()),
            definitions,
        }
    }
    fn tool(&self, scope: &Arc<Scope<Security>>, kind: QueueToolKind) -> BoundTool {
        QueueTool::new(
            kind,
            &self.definitions,
            Arc::new(
                ScopedQueueBackend::new(scope.clone(), self.runtime.registry.clone()).unwrap(),
            ),
            Arc::new(NoReads),
            self.clock.clone(),
            self.log.clone(),
        )
        .unwrap()
    }
    async fn call(
        &self,
        scope: &Arc<Scope<Security>>,
        kind: QueueToolKind,
        input: Value,
    ) -> ToolOutput {
        self.tool(scope, kind)
            .invoke(invocation(input), context(&NeverCancelled))
            .await
            .unwrap()
    }
    async fn close(&self) {
        self.runtime.close().await;
    }
}
fn invocation(input: Value) -> ToolInvocation {
    ToolInvocation {
        call_id: "bound-queue-call".into(),
        arguments_json: input.to_string(),
        assistant_sequence: 1,
    }
}
fn context(cancellation: &dyn Cancellation) -> InvocationContext<'_, Security, ()> {
    InvocationContext::new(
        Security {
            permissions: 0,
            read_only: false,
        },
        (),
        cancellation,
    )
}
fn wire(output: ToolOutput) -> Value {
    serde_json::from_str(&output.text)
        .unwrap_or_else(|error| panic!("expected queue JSON, got {:?}: {error}", output.text))
}
use QueueToolKind::{Create, Info, Push, Take};

// Ledger585: test/Parrot.Core.Tests/AgentQueueTests.cs::AgentQueueTests.An_agent_accesses_its_own_and_direct_parents_queues@24
#[tokio::test]
async fn bound_tool_accesses_only_own_and_direct_parent_bare_queues() {
    let f = Fixture::new().await;
    let child = f.runtime.spawn(&f.root, "child").await;
    let grandchild = f.runtime.spawn(&child, "grandchild").await;
    let parent = wire(f.call(&f.root, Create, json!({"name":"parent-work"})).await);
    let own = wire(f.call(&child, Create, json!({"name":"own-work"})).await);
    assert_eq!(
        wire(f.call(&child, Info, json!({"name":"parent-work"})).await),
        parent
    );
    assert_eq!(
        wire(f.call(&child, Info, json!({"name":"own-work"})).await),
        own
    );
    assert_ne!(parent["path"], own["path"]);
    f.call(
        &f.root,
        Push,
        json!({"name":"parent-work","items":["parent-item"]}),
    )
    .await;
    f.call(
        &child,
        Push,
        json!({"name":"parent-work","items":["from-child"]}),
    )
    .await;
    assert_eq!(
        wire(f.call(&child, Info, json!({"name":"parent-work"})).await)["size"],
        2
    );
    assert_eq!(
        f.runtime
            .q(&child)
            .list()
            .unwrap()
            .into_iter()
            .map(|queue| queue.name)
            .collect::<Vec<_>>(),
        ["own-work", "parent-work"]
    );
    f.call(
        &child,
        Push,
        json!({"name":"own-work","items":["own-item"]}),
    )
    .await;
    assert_eq!(
        wire(
            f.call(
                &child,
                Take,
                json!({"name":"parent-work","count":2,"yield_after_ms":0})
            )
            .await
        )["items"],
        json!(["parent-item", "from-child"])
    );
    assert_eq!(
        wire(
            f.call(&child, Take, json!({"name":"own-work","yield_after_ms":0}))
                .await
        )["items"],
        json!(["own-item"])
    );
    assert_eq!(
        wire(f.call(&f.root, Info, json!({"name":"parent-work"})).await)["size"],
        0
    );
    for (scope, name) in [(&grandchild, "parent-work"), (&f.root, "own-work")] {
        assert_eq!(
            f.call(scope, Info, json!({"name":name})).await.text,
            format!("error: queue: '{name}' was not found")
        );
    }
    f.close().await;
}

// Ledger1637: test/Parrot.Tools.Tests/QueueToolTests.cs::QueueToolTests.Qualified_queue_operations_use_target_store_and_bare_names_stay_local@128
#[tokio::test]
async fn explicit_target_is_local_and_retains_exact_resolved_owner() {
    let f = Fixture::new().await;
    let local = f.runtime.spawn(&f.root, "local").await;
    let target = f.runtime.spawn(&f.root, "target").await;
    f.call(&local, Create, json!({"name":"work","description":"local"}))
        .await;
    f.call(
        &target,
        Create,
        json!({"name":"work","description":"target"}),
    )
    .await;
    f.call(&f.root, Create, json!({"name":"parent-only"})).await;
    assert_eq!(
        wire(f.call(&local, Info, json!({"name":"work"})).await)["description"],
        "local"
    );
    for name in ["parent/target/work", "../target/work", "/root/target/work"] {
        assert_eq!(
            wire(f.call(&local, Info, json!({"name":name})).await)["description"],
            "target"
        );
    }
    let pushed = wire(
        f.call(
            &local,
            Push,
            json!({"name":"parent/target/work","items":["target-item"],"close":true}),
        )
        .await,
    );
    assert_eq!(pushed["size"], 1);
    assert_eq!(pushed["closed"], true);
    assert!(f.runtime.q(&target).get("work").unwrap().closed);
    assert_eq!(
        wire(
            f.call(
                &local,
                Take,
                json!({"name":"/root/target/work","yield_after_ms":0})
            )
            .await
        )["items"],
        json!(["target-item"])
    );
    assert_eq!(f.runtime.q(&local).get("work").unwrap().size, 0);
    assert!(!f.runtime.q(&local).get("work").unwrap().closed);
    assert_eq!(
        wire(f.call(&target, Info, json!({"name":"parent-only"})).await)["size"],
        0
    );
    for (kind, input) in [
        (Info, json!({"name":"target/parent-only"})),
        (
            Push,
            json!({"name":"target/parent-only","items":["leak"],"close":true}),
        ),
        (
            Take,
            json!({"name":"target/parent-only","yield_after_ms":0}),
        ),
    ] {
        assert_eq!(
            f.call(&f.root, kind, input).await.text,
            "error: queue: 'parent-only' was not found"
        );
    }
    let original = f.runtime.q(&f.root).get("parent-only").unwrap();
    assert_eq!(original.size, 0);
    assert!(!original.closed);
    let closed = f
        .log
        .0
        .lock()
        .unwrap()
        .iter()
        .find(|event| event.event == "closed")
        .unwrap()
        .clone();
    assert_eq!(
        closed.agent_session_id,
        target.identity().session.session_id
    );
    f.close().await;
}

// Ledger1638: test/Parrot.Tools.Tests/QueueToolTests.cs::QueueToolTests.Qualified_queue_operations_return_resolution_errors@162
#[tokio::test]
async fn resolver_errors_do_not_fall_back_or_leak_ownership() {
    let f = Fixture::new().await;
    f.call(&f.root, Create, json!({"name":"work"})).await;
    for (kind, input) in [
        (Info, json!({"name":"missing/work"})),
        (
            Push,
            json!({"name":"missing/work","items":["leak"],"close":true}),
        ),
        (Take, json!({"name":"missing/work","yield_after_ms":0})),
    ] {
        assert_eq!(
            f.call(&f.root, kind, input).await.text,
            "error: child agent not found: missing"
        );
    }
    for name in ["/absent/work", "./work", "parent/work", "missing/"] {
        let output = f.call(&f.root, Info, json!({"name":name})).await;
        assert!(output.text.starts_with("error: "));
        assert!(!output.text.contains(&f.root.identity().session.session_id));
    }
    let state = f.runtime.q(&f.root).get("work").unwrap();
    assert_eq!(state.size, 0);
    assert!(!state.closed);
    f.close().await;
}

// Ledger596: test/Parrot.Core.Tests/AgentQueueTests.cs::AgentQueueTests.A_canceled_push_does_not_mutate_the_queue@306
#[tokio::test]
async fn cancelled_bare_and_explicit_push_do_not_mutate_or_close() {
    let f = Fixture::new().await;
    let child = f.runtime.spawn(&f.root, "child").await;
    f.call(&f.root, Create, json!({"name":"work"})).await;
    f.call(&f.root, Push, json!({"name":"work","items":["before"]}))
        .await;
    let before = f.runtime.q(&f.root).get("work").unwrap();
    let cancellation = Signal::new();
    cancellation.cancel();
    for name in ["work", "parent/work"] {
        for close in [false, true] {
            assert_eq!(
                f.tool(&child, Push)
                    .invoke(
                        invocation(json!({"name":name,"items":["not-added"],"close":close})),
                        context(&cancellation)
                    )
                    .await,
                Err(OperationError::Cancelled)
            );
            assert_eq!(f.runtime.q(&f.root).get("work").unwrap(), before);
        }
    }
    assert_eq!(
        wire(
            f.call(
                &f.root,
                Take,
                json!({"name":"work","count":10,"yield_after_ms":0})
            )
            .await
        )["items"],
        json!(["before"])
    );
    f.close().await;
}

// Ledger586: test/Parrot.Core.Tests/AgentQueueTests.cs::AgentQueueTests.A_direct_child_can_close_and_drain_its_parents_queue@52
#[tokio::test]
async fn child_closes_and_drains_parent_and_late_push_fails() {
    let f = Fixture::new().await;
    let child = f.runtime.spawn(&f.root, "child").await;
    f.call(&f.root, Create, json!({"name":"work"})).await;
    f.call(&f.root, Push, json!({"name":"work","items":["final"]}))
        .await;
    assert_eq!(
        wire(
            f.call(&child, Push, json!({"name":"work","items":[],"close":true}))
                .await
        )["closed"],
        true
    );
    assert!(f.runtime.q(&f.root).get("work").unwrap().closed);
    assert_eq!(
        f.call(&f.root, Push, json!({"name":"work","items":["late"]}))
            .await
            .text,
        "error: queue: 'work' is closed"
    );
    let item = wire(
        f.call(&child, Take, json!({"name":"work","yield_after_ms":0}))
            .await,
    );
    assert_eq!(item["items"], json!(["final"]));
    assert_eq!(item["closed"], true);
    let drained = wire(
        f.call(&child, Take, json!({"name":"work","yield_after_ms":30000}))
            .await,
    );
    assert_eq!(drained["items"], json!([]));
    assert_eq!(drained["closed"], true);
    assert_eq!(drained["size"], 0);
    f.close().await;
}

// Ledger1632: test/Parrot.Tools.Tests/QueueToolTests.cs::QueueToolTests.Queue_take_reports_open_empty_timeout_explicitly@11
#[tokio::test]
async fn open_empty_timeout_has_explicit_wire_result_and_real_polling() {
    let f = Fixture::new().await;
    f.call(&f.root, Create, json!({"name":"work"})).await;
    let started = Instant::now();
    let output = f
        .call(&f.root, Take, json!({"name":"work","yield_after_ms":20}))
        .await;
    assert!(started.elapsed() >= Duration::from_millis(20));
    let path = f.runtime.q(&f.root).get("work").unwrap().path;
    assert_eq!(
        output.text,
        format!(
            r#"{{"path":{},"name":"work","size":0,"closed":false,"items":[]}}"#,
            serde_json::to_string(path.to_str().unwrap()).unwrap()
        )
    );
    let sleeps = f.clock.sleeps.lock().unwrap().clone();
    assert!(!sleeps.is_empty());
    assert!(
        sleeps
            .iter()
            .all(|duration| *duration <= Duration::from_millis(10))
    );
    let records = f.log.0.lock().unwrap().clone();
    assert_eq!(records.len(), 2);
    assert_eq!(records[0].event, "wait_started");
    assert_eq!(records[1].event, "wait_completed");
    assert_eq!(records[1].outcome, Some("timeout"));
    assert_eq!(records[1].error_code, None);
    assert!(records[1].duration_milliseconds.unwrap() >= 20);
    f.close().await;
}

// Ledger1633: test/Parrot.Tools.Tests/QueueToolTests.cs::QueueToolTests.Queue_push_close_allows_prompt_drain_completion@27
#[tokio::test]
async fn close_wakes_waiting_thirty_second_take_promptly() {
    let f = Fixture::new().await;
    f.call(&f.root, Create, json!({"name":"work"})).await;
    let tool = f.tool(&f.root, Take);
    let take = tool.invoke(
        invocation(json!({"name":"work","yield_after_ms":30000})),
        context(&NeverCancelled),
    );
    let close = async {
        f.clock.sleeping.acquire().await.unwrap().forget();
        let closed_at = Instant::now();
        assert_eq!(
            wire(
                f.call(
                    &f.root,
                    Push,
                    json!({"name":"work","items":[],"close":true})
                )
                .await
            )["closed"],
            true
        );
        closed_at
    };
    let (result, closed_at) =
        tokio::time::timeout(Duration::from_secs(1), async { tokio::join!(take, close) })
            .await
            .expect("close must not wait the 30-second deadline");
    assert!(closed_at.elapsed() < Duration::from_secs(1));
    let drained = wire(result.unwrap());
    assert_eq!(drained["closed"], true);
    assert_eq!(drained["items"], json!([]));
    assert_eq!(
        f.log.0.lock().unwrap().last().unwrap().outcome,
        Some("closed")
    );
    f.close().await;
}

// Ledger1634: test/Parrot.Tools.Tests/QueueToolTests.cs::QueueToolTests.Queue_push_adds_final_items_closes_idempotently_and_rejects_late_items@53
#[tokio::test]
async fn final_items_close_is_idempotent_and_ordered_drain_rejects_late_items() {
    let f = Fixture::new().await;
    f.call(&f.root, Create, json!({"name":"work"})).await;
    f.call(&f.root, Push, json!({"name":"work","items":["first"]}))
        .await;
    let closed = wire(
        f.call(
            &f.root,
            Push,
            json!({"name":"work","items":["final-a","final-b"],"close":true}),
        )
        .await,
    );
    assert_eq!(closed["closed"], true);
    assert_eq!(closed["size"], 3);
    assert_eq!(
        wire(
            f.call(
                &f.root,
                Push,
                json!({"name":"work","items":[],"close":true})
            )
            .await
        ),
        closed
    );
    assert_eq!(
        f.call(&f.root, Push, json!({"name":"work","items":["late"]}))
            .await
            .text,
        "error: queue: 'work' is closed"
    );
    let drained = wire(
        f.call(
            &f.root,
            Take,
            json!({"name":"work","count":10,"yield_after_ms":0}),
        )
        .await,
    );
    assert_eq!(drained["items"], json!(["first", "final-a", "final-b"]));
    assert_eq!(drained["closed"], true);
    assert_eq!(drained["size"], 0);
    f.close().await;
}

#[tokio::test]
async fn retained_backend_has_no_scope_cycle_but_resolved_target_retains_its_owner() {
    use parrot_tools::tasks_queues::queues::QueueToolBackend;

    let f = Fixture::new().await;
    let child = f.runtime.spawn(&f.root, "child").await;
    let root_refs = Arc::strong_count(&f.root);
    let registry_refs = Arc::strong_count(&f.runtime.registry);
    let backend = ScopedQueueBackend::new(f.root.clone(), f.runtime.registry.clone()).unwrap();
    assert_eq!(Arc::strong_count(&f.root), root_refs);
    assert_eq!(Arc::strong_count(&f.runtime.registry), registry_refs);
    let child_refs = Arc::strong_count(&child);
    let resolved = backend.resolve("child/work").unwrap();
    assert_eq!(Arc::strong_count(&child), child_refs + 1);
    assert_eq!(
        backend.resolved_session_id(&resolved.owner),
        child.identity().session.session_id
    );
    let clone = resolved.owner.clone();
    assert_eq!(Arc::strong_count(&child), child_refs + 2);
    drop(resolved);
    drop(clone);
    assert_eq!(Arc::strong_count(&child), child_refs);
    f.close().await;
}

#[tokio::test]
async fn nullable_items_preserve_store_lookup_closed_validation_order() {
    let f = Fixture::new().await;
    f.call(&f.root, Create, json!({"name":"work"})).await;
    assert_eq!(
        f.call(
            &f.root,
            Push,
            json!({"name":"work","items":[null],"close":true})
        )
        .await
        .text,
        "error: queue: items cannot contain null"
    );
    assert_eq!(
        f.call(&f.root, Push, json!({"name":"absent","items":[null]}))
            .await
            .text,
        "error: queue: 'absent' was not found"
    );
    f.call(
        &f.root,
        Push,
        json!({"name":"work","items":[],"close":true}),
    )
    .await;
    for name in ["work", "/root/work"] {
        assert_eq!(
            f.call(&f.root, Push, json!({"name":name,"items":[null]}))
                .await
                .text,
            "error: queue: 'work' is closed"
        );
    }
    f.close().await;
}
