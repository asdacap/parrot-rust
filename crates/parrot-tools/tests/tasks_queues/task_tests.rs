use std::{
    collections::BTreeMap,
    path::Path,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    task::{Poll, Waker},
};

use parrot_domain::{
    contracts::{
        async_ops::{BoxFuture, Cancellation, OperationError, OperationResult},
        filesystem::{
            AuthorizedReadService, BoundRead, FileMetadata, FileReadError, FileReadErrorKind,
            LinkPolicy, ReadRequest,
        },
        paths::ResolvedRead,
        runtime::TaskController,
        security::{AccessSnapshot, InvocationContext},
        tools::{ParallelSafety, Tool},
    },
    values::{
        agent_tasks::{ExecutionStatus, Task, TaskPayload},
        tools::{ConfiguredToolDefinition, ToolDefinitionCatalog},
    },
};
use parrot_tools::{
    task_artifacts::{BeforeToolBatch, parse_task_set},
    tasks_queues::{
        FailureKind, ToolFailure, ToolInvocation,
        tasks::{TaskDetail, TaskQuery, TaskSnapshot, TaskTool, TaskToolKind, TaskUpdate},
    },
};

const DECLARATIONS: &str = r#"[{"name":"build","description":"Build it","payload":"Run the build","acceptance_criteria":"It builds","hidden":true},{"name":"test","dependencies":["build"],"description":"Test it","payload":"Run the tests","acceptance_criteria":"Tests pass","state":"succeeded","result":"green"}]"#;
const SNAPSHOT_TASKS: &str = r#"[
    {"name":"build","description":"Build it","payload":"Run the build","acceptance_criteria":"It builds","model":"fast","hidden":true,"state":"succeeded","result":"built ✓ \"quoted\""},
    {"name":"test","dependencies":["build"],"description":"Test it","payload":"Run the tests","acceptance_criteria":"Tests pass","state":"failed","failure":"red"},
    {"name":"ship","dependencies":["build","test"],"description":"Ship it","payload":[{"name":"tag","description":"Tag it","payload":"Tag the release","acceptance_criteria":"Tagged","hidden":true}],"acceptance_criteria":"Shipped"}
]"#;
const SUMMARIES: &str = r#"[{"name":"build","state":"succeeded","description":"Build it","hidden":true},{"name":"test","state":"failed","description":"Test it","hidden":false},{"name":"ship","state":"pending","description":"Ship it","hidden":false}]"#;
const BUILD_DETAIL: &str = r#"{"name":"build","dependencies":[],"description":"Build it","payload":"Run the build","acceptance_criteria":"It builds","model":"fast","hidden":true,"agent_name":"worker-build","state":"succeeded","result":"built ✓ \"quoted\""}"#;
const TEST_DETAIL: &str = r#"{"name":"test","dependencies":["build"],"description":"Test it","payload":"Run the tests","acceptance_criteria":"Tests pass","hidden":false,"agent_name":"worker-test","state":"failed","failure":"red"}"#;
const SHIP_DETAIL: &str = r#"{"name":"ship","dependencies":["build","test"],"description":"Ship it","payload":[{"name":"tag","dependencies":[],"description":"Tag it","payload":"Tag the release","acceptance_criteria":"Tagged","hidden":true}],"acceptance_criteria":"Shipped","hidden":false,"state":"pending"}"#;

struct Never;
impl Cancellation for Never {
    fn is_cancelled(&self) -> bool {
        false
    }

    fn cancelled(&self) -> BoxFuture<'_, ()> {
        Box::pin(std::future::pending())
    }
}

#[derive(Default)]
struct AtomicFlag {
    value: AtomicBool,
    waiter: Mutex<Option<Waker>>,
}

impl AtomicFlag {
    fn cancel(&self) {
        self.value.store(true, Ordering::SeqCst);
        if let Some(waiter) = self.waiter.lock().unwrap().take() {
            waiter.wake();
        }
    }
}

impl Cancellation for AtomicFlag {
    fn is_cancelled(&self) -> bool {
        self.value.load(Ordering::SeqCst)
    }

    fn cancelled(&self) -> BoxFuture<'_, ()> {
        Box::pin(std::future::poll_fn(|context| {
            let mut waiter = self.waiter.lock().unwrap();
            if self.is_cancelled() {
                Poll::Ready(())
            } else {
                *waiter = Some(context.waker().clone());
                Poll::Pending
            }
        }))
    }
}

struct DomainAccessSnapshot {
    denied: bool,
    witness: Arc<()>,
}

impl DomainAccessSnapshot {
    fn new(denied: bool) -> Self {
        Self {
            denied,
            witness: Arc::new(()),
        }
    }
}

impl AccessSnapshot for DomainAccessSnapshot {
    type Error = FileReadError;

    fn check_read(&self, _: &ResolvedRead) -> Result<(), Self::Error> {
        if self.denied {
            Err(FileReadError::new(
                FileReadErrorKind::Denied,
                "fixture denied",
            ))
        } else {
            Ok(())
        }
    }

    fn check_write(&self, _: &Path) -> Result<(), Self::Error> {
        Err(FileReadError::new(
            FileReadErrorKind::Denied,
            "fixture is read-only",
        ))
    }
}

// A port double, not a filesystem resolver or evidence of real security approval.
struct FixtureReader {
    artifact: Vec<u8>,
    failure: Option<OperationError<FileReadError>>,
    reads: Mutex<Vec<(ReadRequest, Arc<()>)>>,
    cancel_on_read: Option<Arc<AtomicFlag>>,
}

impl Default for FixtureReader {
    fn default() -> Self {
        Self {
            artifact: format!(r#"{{"schema_version":1,"tasks":{DECLARATIONS}}}"#).into_bytes(),
            failure: None,
            reads: Mutex::new(Vec::new()),
            cancel_on_read: None,
        }
    }
}

impl AuthorizedReadService for FixtureReader {
    type Security = DomainAccessSnapshot;

    fn inspect<'a>(
        &'a self,
        _: &'a Path,
        _: LinkPolicy,
        _: &'a Self::Security,
        _: &'a dyn Cancellation,
    ) -> BoxFuture<'a, OperationResult<FileMetadata, FileReadError>> {
        panic!("task tools must use the single bound read, not inspect")
    }

    fn list_directory<'a>(
        &'a self,
        _: &'a Path,
        _: LinkPolicy,
        _: &'a Self::Security,
        _: &'a dyn Cancellation,
    ) -> BoxFuture<'a, OperationResult<Vec<FileMetadata>, FileReadError>> {
        panic!("task tools must not list directories")
    }

    fn read<'a>(
        &'a self,
        request: &'a ReadRequest,
        snapshot: &'a Self::Security,
        cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, OperationResult<BoundRead, FileReadError>> {
        self.reads
            .lock()
            .unwrap()
            .push((request.clone(), snapshot.witness.clone()));
        Box::pin(async move {
            cancellation
                .check()
                .map_err(|_| OperationError::Cancelled)?;
            if let Some(flag) = &self.cancel_on_read {
                flag.cancel();
            }
            cancellation
                .check()
                .map_err(|_| OperationError::Cancelled)?;
            if snapshot.denied {
                return Err(OperationError::Failed(FileReadError::new(
                    FileReadErrorKind::Denied,
                    "fixture policy denies this artifact",
                )));
            }
            if let Some(failure) = &self.failure {
                return Err(failure.clone());
            }
            let bytes = match request.requested.to_str().unwrap() {
                "artifact.json" => self.artifact.clone(),
                "malformed.json" => b"{}".to_vec(),
                "missing.json" => {
                    return Err(OperationError::Failed(FileReadError::new(
                        FileReadErrorKind::NotFound,
                        "Source 'missing.json' is missing.",
                    )));
                }
                "alias.json" => {
                    return Err(OperationError::Failed(FileReadError::new(
                        FileReadErrorKind::InvalidOperation,
                        "Path 'alias.json' traverses a symbolic link.",
                    )));
                }
                path => panic!("unexpected fixture path: {path}"),
            };
            Ok(BoundRead {
                lexical_path: request.requested.clone(),
                physical_path: request.requested.clone(),
                bytes,
            })
        })
    }
}

#[derive(Debug)]
struct Selection;

#[derive(Default)]
struct RecordingController {
    updates: Mutex<Vec<TaskUpdate<Arc<Selection>>>>,
    attempts: AtomicUsize,
    queries: Mutex<Vec<TaskQuery>>,
    tasks: Vec<Task>,
    update_failure: Option<OperationError<ToolFailure>>,
    snapshot_failure: Option<ToolFailure>,
    snapshot_gate: Option<Arc<tokio::sync::Notify>>,
}

impl TaskController for RecordingController {
    type Update = TaskUpdate<Arc<Selection>>;
    type Query = TaskQuery;
    type Snapshot = TaskSnapshot;
    type Error = ToolFailure;

    fn update<'a>(
        &'a self,
        update: Self::Update,
        cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, OperationResult<Self::Snapshot, Self::Error>> {
        self.attempts.fetch_add(1, Ordering::SeqCst);
        Box::pin(async move {
            cancellation
                .check()
                .map_err(|_| OperationError::Cancelled)?;
            if let Some(failure) = &self.update_failure {
                return Err(failure.clone());
            }
            self.updates.lock().unwrap().push(update);
            Ok(TaskSnapshot::default())
        })
    }

    fn snapshot(&self, query: Self::Query) -> BoxFuture<'_, Result<Self::Snapshot, Self::Error>> {
        self.queries.lock().unwrap().push(query.clone());
        Box::pin(async move {
            if let Some(gate) = &self.snapshot_gate {
                gate.notified().await;
            }
            if let Some(failure) = &self.snapshot_failure {
                return Err(failure.clone());
            }
            let detail = query.name.as_ref().and_then(|name| {
                self.tasks
                    .iter()
                    .find(|task| &task.name == name)
                    .map(|task| TaskDetail {
                        task: task.clone(),
                        agent_name: (task.state != ExecutionStatus::Pending)
                            .then(|| format!("worker-{name}")),
                    })
            });
            Ok(TaskSnapshot {
                tasks: self.tasks.clone(),
                detail,
            })
        })
    }
}

type Renderer = Box<dyn Fn(usize) -> Result<String, ToolFailure> + Send + Sync>;
type LocalTaskTool = TaskTool<RecordingController, FixtureReader, Renderer, Arc<Selection>>;

#[derive(Default)]
struct Fixture {
    controller: Arc<RecordingController>,
    reader: Arc<FixtureReader>,
    rendered: Arc<Mutex<Vec<usize>>>,
}

impl Fixture {
    fn tool(&self, kind: TaskToolKind) -> LocalTaskTool {
        let definitions = ToolDefinitionCatalog::new(&BTreeMap::from([
            (
                "set_agent_tasks".into(),
                ConfiguredToolDefinition::new("Set tasks", "{}"),
            ),
            (
                "get_agent_tasks".into(),
                ConfiguredToolDefinition::new("Get tasks", "{}"),
            ),
        ]));
        let rendered = self.rendered.clone();
        let renderer: Renderer = Box::new(move |count| {
            rendered.lock().unwrap().push(count);
            Ok(format!(
                "AgentTask update accepted ({count} tasks). State changes are delivered as notifications."
            ))
        });
        TaskTool::new(
            kind,
            &definitions,
            self.controller.clone(),
            self.reader.clone(),
            Arc::new(renderer),
        )
        .unwrap()
    }

    fn snapshot() -> Self {
        Self {
            controller: Arc::new(RecordingController {
                tasks: parse_task_set(SNAPSHOT_TASKS).unwrap(),
                ..Default::default()
            }),
            ..Default::default()
        }
    }

    fn assert_no_update(&self) {
        assert!(self.controller.updates.lock().unwrap().is_empty());
        assert!(self.rendered.lock().unwrap().is_empty());
    }
}

fn invocation(arguments: &str) -> ToolInvocation {
    ToolInvocation {
        call_id: "call-7".into(),
        arguments_json: arguments.into(),
        assistant_sequence: 7,
    }
}

fn context(
    cancellation: &dyn Cancellation,
) -> InvocationContext<'_, DomainAccessSnapshot, Arc<Selection>> {
    InvocationContext::new(
        DomainAccessSnapshot::new(false),
        Arc::new(Selection),
        cancellation,
    )
}

async fn assert_invalid_set(arguments: &str, expected: &str, expected_reads: usize) {
    let fixture = Fixture::default();
    let result = fixture
        .tool(TaskToolKind::Set)
        .invoke(invocation(arguments), context(&Never))
        .await
        .unwrap();
    assert_eq!(result.text, expected);
    fixture.assert_no_update();
    assert_eq!(fixture.controller.attempts.load(Ordering::SeqCst), 0);
    let reads = fixture.reader.reads.lock().unwrap();
    assert_eq!(reads.len(), expected_reads);
    for (request, _) in reads.iter() {
        assert_eq!(request.links, LinkPolicy::RejectLinks);
        assert_eq!(request.expected_physical, None);
        assert_eq!(request.max_bytes, None);
    }
}

macro_rules! invalid_set {
    ($name:ident, $arguments:expr, $expected:expr, $reads:expr) => {
        #[tokio::test]
        async fn $name() {
            assert_invalid_set($arguments, $expected, $reads).await;
        }
    };
}

invalid_set!(
    set_rejects_missing_input,
    "{}",
    "error: Tool arguments require exactly one of a nonblank string 'path' or a 'tasks' array.",
    0
);
invalid_set!(
    set_rejects_path_and_inline_tasks,
    r#"{"path":"artifact.json","tasks":[]}"#,
    "error: Tool arguments require exactly one of a nonblank string 'path' or a 'tasks' array.",
    0
);
invalid_set!(
    set_rejects_empty_path,
    r#"{"path":""}"#,
    "error: Tool arguments require a nonblank string 'path'.",
    0
);
invalid_set!(
    set_rejects_null_path,
    r#"{"path":null}"#,
    "error: Tool arguments require a nonblank string 'path'.",
    0
);
invalid_set!(
    set_rejects_nonstring_path,
    r#"{"path":7}"#,
    "error: Tool arguments require a nonblank string 'path'.",
    0
);
invalid_set!(
    set_rejects_missing_artifact,
    r#"{"path":"missing.json"}"#,
    "error: Source 'missing.json' is missing.",
    1
);
invalid_set!(
    set_rejects_malformed_artifact,
    r#"{"path":"malformed.json"}"#,
    "error: schema_version is required.",
    1
);
invalid_set!(
    set_rejects_symbolic_link_artifact,
    r#"{"path":"alias.json"}"#,
    "error: Path 'alias.json' traverses a symbolic link.",
    1
);
invalid_set!(
    set_rejects_empty_tasks,
    r#"{"tasks":[]}"#,
    "error: tasks must not be empty.",
    0
);
invalid_set!(
    set_rejects_blocked_state,
    r#"{"tasks":[{"name":"a","description":"d","payload":"p","acceptance_criteria":"c","state":"blocked"}]}"#,
    "error: tasks[0] state must be pending, running, succeeded, failed, or canceled.",
    0
);
invalid_set!(
    set_rejects_unknown_input,
    r#"{"unexpected":true}"#,
    "error: The JSON property 'unexpected' could not be mapped to the tool input.",
    0
);

#[tokio::test]
async fn set_denied_artifact_does_not_mutate_controller() {
    let fixture = Fixture::default();
    let security = DomainAccessSnapshot::new(true);
    let witness = security.witness.clone();
    let result = fixture
        .tool(TaskToolKind::Set)
        .invoke(
            invocation(r#"{"path":"artifact.json"}"#),
            InvocationContext::new(security, Arc::new(Selection), &Never),
        )
        .await
        .unwrap();
    assert_eq!(result.text, "error: access denied");
    fixture.assert_no_update();
    assert_eq!(fixture.controller.attempts.load(Ordering::SeqCst), 0);
    let reads = fixture.reader.reads.lock().unwrap();
    assert_eq!(reads.len(), 1);
    assert!(Arc::ptr_eq(&reads[0].1, &witness));
}

async fn assert_set_success(arguments: &str, expected_reads: usize) {
    let fixture = Fixture::default();
    let selection = Arc::new(Selection);
    let security = DomainAccessSnapshot::new(false);
    let witness = security.witness.clone();
    let result = fixture
        .tool(TaskToolKind::Set)
        .invoke(
            invocation(arguments),
            InvocationContext::new(security, selection.clone(), &Never),
        )
        .await
        .unwrap();
    assert_eq!(
        result.text,
        "AgentTask update accepted (2 tasks). State changes are delivered as notifications."
    );
    let calls = fixture.controller.updates.lock().unwrap();
    assert_eq!(calls.len(), 1);
    let call = &calls[0];
    assert_eq!(call.tasks, parse_task_set(DECLARATIONS).unwrap());
    assert!(call.tasks[0].hidden);
    assert!(!call.tasks[1].hidden);
    assert_eq!(call.tasks[0].state, ExecutionStatus::Pending);
    assert_eq!(call.tasks[1].state, ExecutionStatus::Succeeded);
    assert_eq!(call.tasks[1].result.as_deref(), Some("green"));
    assert_eq!(call.tasks[1].dependencies, ["build"]);
    assert!(Arc::ptr_eq(&call.selection, &selection));
    assert_eq!(
        call.history_boundary,
        BeforeToolBatch {
            assistant_sequence: 7,
            call_id: "call-7".into()
        }
    );
    assert_eq!(*fixture.rendered.lock().unwrap(), [2]);
    let reads = fixture.reader.reads.lock().unwrap();
    assert_eq!(reads.len(), expected_reads);
    if let Some((request, actual_witness)) = reads.first() {
        assert_eq!(request.requested, Path::new("artifact.json"));
        assert_eq!(request.links, LinkPolicy::RejectLinks);
        assert_eq!(request.expected_physical, None);
        assert_eq!(request.max_bytes, None);
        assert!(Arc::ptr_eq(actual_witness, &witness));
    }
}

#[tokio::test]
async fn set_artifact_preserves_tasks_selection_and_before_tool_batch() {
    assert_set_success(r#"{"path":"artifact.json"}"#, 1).await;
}

#[tokio::test]
async fn set_inline_preserves_tasks_selection_and_before_tool_batch() {
    assert_set_success(&format!(r#"{{"tasks":{DECLARATIONS}}}"#), 0).await;
}

#[tokio::test]
async fn set_preserves_optional_model_and_failure_for_both_input_forms() {
    let declarations = DECLARATIONS
        .replace(r#""hidden":true"#, r#""model":"fast","hidden":true"#)
        .replace(
            r#""state":"succeeded","result":"green""#,
            r#""state":"failed","result":"green","failure":"red""#,
        );
    for arguments in [
        r#"{"path":"artifact.json"}"#.to_owned(),
        format!(r#"{{"tasks":{declarations}}}"#),
    ] {
        let fixture = Fixture {
            reader: Arc::new(FixtureReader {
                artifact: format!(r#"{{"schema_version":1,"tasks":{declarations}}}"#).into_bytes(),
                ..Default::default()
            }),
            ..Default::default()
        };
        fixture
            .tool(TaskToolKind::Set)
            .invoke(invocation(&arguments), context(&Never))
            .await
            .unwrap();
        let updates = fixture.controller.updates.lock().unwrap();
        assert_eq!(updates.len(), 1);
        assert_eq!(updates[0].tasks, parse_task_set(&declarations).unwrap());
        assert_eq!(updates[0].tasks[0].model.as_deref(), Some("fast"));
        assert_eq!(updates[0].tasks[1].failure.as_deref(), Some("red"));
    }
}

async fn assert_rejected_set(kind: FailureKind, message: &str) {
    let fixture = Fixture {
        controller: Arc::new(RecordingController {
            update_failure: Some(OperationError::Failed(ToolFailure::new(kind, message))),
            ..Default::default()
        }),
        ..Default::default()
    };
    let result = fixture
        .tool(TaskToolKind::Set)
        .invoke(
            invocation(&format!(r#"{{"tasks":{DECLARATIONS}}}"#)),
            context(&Never),
        )
        .await
        .unwrap();
    assert_eq!(result.text, format!("error: {message}"));
    assert_eq!(fixture.controller.attempts.load(Ordering::SeqCst), 1);
    fixture.assert_no_update();
}

#[tokio::test]
async fn set_reports_controller_dependency_cycle_as_tool_error() {
    assert_rejected_set(FailureKind::Argument, "tasks contains a dependency cycle.").await;
}

#[tokio::test]
async fn set_reports_controller_shutdown_as_tool_error() {
    assert_rejected_set(
        FailureKind::InvalidOperation,
        "The agent session is shutting down.",
    )
    .await;
}

#[tokio::test]
async fn set_propagates_cancellation_observed_during_artifact_read_without_update() {
    let cancellation = Arc::new(AtomicFlag::default());
    let fixture = Fixture {
        reader: Arc::new(FixtureReader {
            cancel_on_read: Some(cancellation.clone()),
            ..Default::default()
        }),
        ..Default::default()
    };
    let result = fixture
        .tool(TaskToolKind::Set)
        .invoke(
            invocation(r#"{"path":"artifact.json"}"#),
            context(cancellation.as_ref()),
        )
        .await;
    assert_eq!(result, Err(OperationError::Cancelled));
    assert!(cancellation.is_cancelled());
    assert_eq!(fixture.controller.attempts.load(Ordering::SeqCst), 0);
    fixture.assert_no_update();
}

#[tokio::test]
async fn set_forwards_cancelled_inline_invocation_to_controller_without_mutation() {
    let fixture = Fixture::default();
    let cancellation = AtomicFlag::default();
    cancellation.cancel();
    let result = fixture
        .tool(TaskToolKind::Set)
        .invoke(
            invocation(&format!(r#"{{"tasks":{DECLARATIONS}}}"#)),
            context(&cancellation),
        )
        .await;
    assert_eq!(result, Err(OperationError::Cancelled));
    assert_eq!(fixture.controller.attempts.load(Ordering::SeqCst), 1);
    assert!(fixture.reader.reads.lock().unwrap().is_empty());
    fixture.assert_no_update();
}

#[tokio::test]
async fn set_propagates_controller_cancellation_without_rendering_or_update() {
    let fixture = Fixture {
        controller: Arc::new(RecordingController {
            update_failure: Some(OperationError::Cancelled),
            ..Default::default()
        }),
        ..Default::default()
    };
    let result = fixture
        .tool(TaskToolKind::Set)
        .invoke(
            invocation(&format!(r#"{{"tasks":{DECLARATIONS}}}"#)),
            context(&Never),
        )
        .await;
    assert_eq!(result, Err(OperationError::Cancelled));
    assert_eq!(fixture.controller.attempts.load(Ordering::SeqCst), 1);
    fixture.assert_no_update();
}

#[tokio::test]
async fn set_propagates_unexpected_reader_failure_instead_of_claiming_access_denied() {
    let fixture = Fixture {
        reader: Arc::new(FixtureReader {
            failure: Some(OperationError::Failed(FileReadError::new(
                FileReadErrorKind::NotSupported,
                "backend unsupported",
            ))),
            ..Default::default()
        }),
        ..Default::default()
    };
    let result = fixture
        .tool(TaskToolKind::Set)
        .invoke(invocation(r#"{"path":"artifact.json"}"#), context(&Never))
        .await;
    assert_eq!(
        result,
        Err(OperationError::Failed(ToolFailure::new(
            FailureKind::Other,
            "backend unsupported"
        )))
    );
    assert_eq!(fixture.controller.attempts.load(Ordering::SeqCst), 0);
    fixture.assert_no_update();
}

async fn assert_get(arguments: &str, expected: &str, query: Option<Option<&str>>) {
    let fixture = Fixture::snapshot();
    let result = fixture
        .tool(TaskToolKind::Get)
        .invoke(invocation(arguments), context(&Never))
        .await
        .unwrap();
    assert_eq!(result.text.as_bytes(), expected.as_bytes());
    let queries = fixture.controller.queries.lock().unwrap();
    match query {
        Some(name) => assert_eq!(
            *queries,
            [TaskQuery {
                name: name.map(str::to_owned)
            }]
        ),
        None => assert!(queries.is_empty()),
    }
    assert!(fixture.reader.reads.lock().unwrap().is_empty());
    fixture.assert_no_update();
}

macro_rules! get_case {
    ($name:ident, $arguments:expr, $expected:expr, $query:expr) => {
        #[tokio::test]
        async fn $name() {
            assert_get($arguments, $expected, $query).await;
        }
    };
}

get_case!(get_lists_all_tasks_by_default, "{}", SUMMARIES, Some(None));
get_case!(
    get_lists_all_tasks_for_null_name,
    r#"{"name":null}"#,
    SUMMARIES,
    Some(None)
);
get_case!(
    get_build_detail_has_utf8_result_model_and_agent_name_in_source_order,
    r#"{"name":"build"}"#,
    BUILD_DETAIL,
    Some(Some("build"))
);
get_case!(
    get_failed_detail_has_failure_but_no_model_or_result,
    r#"{"name":"test"}"#,
    TEST_DETAIL,
    Some(Some("test"))
);
get_case!(
    get_composite_detail_has_declarations_only_in_nested_payload,
    r#"{"name":"ship"}"#,
    SHIP_DETAIL,
    Some(Some("ship"))
);
get_case!(
    get_reports_missing_task,
    r#"{"name":"missing"}"#,
    "error: AgentTask 'missing' does not exist.",
    Some(Some("missing"))
);
get_case!(
    get_rejects_blank_name_without_querying,
    r#"{"name":" "}"#,
    "error: Tool arguments require a nonblank string 'name'.",
    None
);
get_case!(
    get_rejects_unknown_input_without_querying,
    r#"{"unexpected":true}"#,
    "error: The JSON property 'unexpected' could not be mapped to the tool input.",
    None
);

#[tokio::test]
async fn get_waits_for_snapshot_future_before_rendering_list_or_detail() {
    for (arguments, expected, name) in [
        ("{}", SUMMARIES, None),
        (r#"{"name":"build"}"#, BUILD_DETAIL, Some("build")),
    ] {
        let gate = Arc::new(tokio::sync::Notify::new());
        let fixture = Fixture {
            controller: Arc::new(RecordingController {
                tasks: parse_task_set(SNAPSHOT_TASKS).unwrap(),
                snapshot_gate: Some(gate.clone()),
                ..Default::default()
            }),
            ..Default::default()
        };
        let tool = fixture.tool(TaskToolKind::Get);
        let mut result = tool.invoke(invocation(arguments), context(&Never));
        std::future::poll_fn(|cx| {
            assert!(result.as_mut().poll(cx).is_pending());
            Poll::Ready(())
        })
        .await;
        assert_eq!(
            *fixture.controller.queries.lock().unwrap(),
            [TaskQuery {
                name: name.map(str::to_owned)
            }]
        );
        gate.notify_one();
        assert_eq!(result.await.unwrap().text.as_bytes(), expected.as_bytes());
        assert!(fixture.reader.reads.lock().unwrap().is_empty());
        fixture.assert_no_update();
    }
}

#[tokio::test]
async fn get_validates_arguments_before_snapshot_and_waits_to_propagate_its_error() {
    let gate = Arc::new(tokio::sync::Notify::new());
    let error = ToolFailure::new(FailureKind::Other, "gated snapshot backend broke");
    let fixture = Fixture {
        controller: Arc::new(RecordingController {
            snapshot_failure: Some(error.clone()),
            snapshot_gate: Some(gate.clone()),
            ..Default::default()
        }),
        ..Default::default()
    };
    let tool = fixture.tool(TaskToolKind::Get);
    for (arguments, expected) in [
        (
            r#"{"name":" "}"#,
            "error: Tool arguments require a nonblank string 'name'.",
        ),
        (
            r#"{"unexpected":true}"#,
            "error: The JSON property 'unexpected' could not be mapped to the tool input.",
        ),
    ] {
        let mut result = tool.invoke(invocation(arguments), context(&Never));
        std::future::poll_fn(|cx| {
            let Poll::Ready(output) = result.as_mut().poll(cx) else {
                panic!("argument rejection must not wait for the snapshot gate")
            };
            assert_eq!(output.unwrap().text, expected);
            Poll::Ready(())
        })
        .await;
        assert!(fixture.controller.queries.lock().unwrap().is_empty());
    }
    let mut result = tool.invoke(invocation(r#"{"name":"build"}"#), context(&Never));
    std::future::poll_fn(|cx| {
        assert!(result.as_mut().poll(cx).is_pending());
        Poll::Ready(())
    })
    .await;
    assert_eq!(
        *fixture.controller.queries.lock().unwrap(),
        [TaskQuery {
            name: Some("build".into())
        }]
    );
    gate.notify_one();
    assert_eq!(result.await, Err(OperationError::Failed(error)));
    assert!(fixture.reader.reads.lock().unwrap().is_empty());
    fixture.assert_no_update();
}

#[tokio::test]
async fn get_empty_snapshot_is_exact_empty_json_array() {
    let fixture = Fixture::default();
    let result = fixture
        .tool(TaskToolKind::Get)
        .invoke(invocation("{}"), context(&Never))
        .await
        .unwrap();
    assert_eq!(result.text.as_bytes(), b"[]");
    assert_eq!(
        *fixture.controller.queries.lock().unwrap(),
        [TaskQuery { name: None }]
    );
    fixture.assert_no_update();
}

#[tokio::test]
async fn get_omits_nested_execution_fields_even_when_child_has_state_result_and_failure() {
    let mut tasks = parse_task_set(SNAPSHOT_TASKS).unwrap();
    let TaskPayload::Tasks(children) = &mut tasks[2].payload else {
        panic!("ship must be composite")
    };
    children[0].state = ExecutionStatus::Failed;
    children[0].result = Some("nested result must not leak".into());
    children[0].failure = Some("nested failure must not leak".into());
    let fixture = Fixture {
        controller: Arc::new(RecordingController {
            tasks,
            ..Default::default()
        }),
        ..Default::default()
    };
    let result = fixture
        .tool(TaskToolKind::Get)
        .invoke(invocation(r#"{"name":"ship"}"#), context(&Never))
        .await
        .unwrap();
    assert_eq!(result.text.as_bytes(), SHIP_DETAIL.as_bytes());
    let json: serde_json::Value = serde_json::from_str(&result.text).unwrap();
    let child = &json["payload"][0];
    for omitted in ["state", "result", "failure", "agent_name"] {
        assert!(child.get(omitted).is_none(), "nested {omitted} leaked");
    }
}

#[tokio::test]
async fn get_propagates_unexpected_snapshot_backend_failure() {
    let error = ToolFailure::new(FailureKind::Other, "snapshot backend broke");
    let fixture = Fixture {
        controller: Arc::new(RecordingController {
            snapshot_failure: Some(error.clone()),
            ..Default::default()
        }),
        ..Default::default()
    };
    let result = fixture
        .tool(TaskToolKind::Get)
        .invoke(invocation("{}"), context(&Never))
        .await;
    assert_eq!(result, Err(OperationError::Failed(error)));
    assert_eq!(
        *fixture.controller.queries.lock().unwrap(),
        [TaskQuery { name: None }]
    );
    fixture.assert_no_update();
}

#[test]
fn task_factories_preserve_the_actual_recursive_configured_schema() {
    use parrot_foundation::config::{
        configuration::source_predefined_content, runtime::read_tool_definitions, yaml,
    };
    let root = yaml::parse_root(&source_predefined_content().unwrap()).unwrap();
    let definitions = ToolDefinitionCatalog::new(
        &read_tool_definitions(&root)
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
    let fixture = Fixture::default();
    for kind in [TaskToolKind::Set, TaskToolKind::Get] {
        let render: Renderer = Box::new(|_| Ok(String::new()));
        let tool: LocalTaskTool = TaskTool::new(
            kind,
            &definitions,
            fixture.controller.clone(),
            fixture.reader.clone(),
            Arc::new(render),
        )
        .unwrap();
        assert_eq!(
            tool.definition(),
            definitions.describe(kind.name()).unwrap()
        );
        let schema: serde_json::Value =
            serde_json::from_str(&tool.definition().parameters_json).unwrap();
        assert_eq!(schema["type"], "object");
        assert_eq!(schema["additionalProperties"], false);
        match kind {
            TaskToolKind::Set => {
                assert_eq!(
                    schema["oneOf"],
                    serde_json::json!([{"required":["path"]},{"required":["tasks"]}])
                );
                assert_eq!(
                    schema["properties"]["tasks"]["items"]["$ref"],
                    "#/$defs/task"
                );
                assert_eq!(
                    schema["$defs"]["task"]["properties"]["description"]["maxLength"],
                    300
                );
                assert_eq!(
                    schema["$defs"]["task"]["properties"]["payload"]["oneOf"][1]["items"]["$ref"],
                    "#/$defs/task"
                );
            }
            TaskToolKind::Get => {
                assert!(schema.get("required").is_none());
                assert_eq!(schema["properties"]["name"]["minLength"], 1);
            }
        }
    }
}

#[test]
fn both_task_tools_use_configured_definitions_are_sequential_and_disabled_after_interruption() {
    let fixture = Fixture::default();
    for (kind, name, description) in [
        (TaskToolKind::Set, "set_agent_tasks", "Set tasks"),
        (TaskToolKind::Get, "get_agent_tasks", "Get tasks"),
    ] {
        let tool = fixture.tool(kind);
        assert_eq!(tool.name(), name);
        assert_eq!(
            tool.definition(),
            &ConfiguredToolDefinition::new(description, "{}")
        );
        assert_eq!(
            tool.parallel_safety(&invocation("{}")),
            ParallelSafety::Sequential
        );
        assert!(!tool.enabled_after_interruption());
    }
}
