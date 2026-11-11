//! Bounded real scheduler/Tool bridge tests, not the final host constructor gate.
#![allow(dead_code)]

#[path = "../../../../parrot-runtime/tests/task_scheduler/admission_publisher.rs"]
mod admission_publisher;
#[path = "../../../../parrot-runtime/tests/task_scheduler/fixture.rs"]
mod fixture;
#[path = "../../../../parrot-runtime/tests/task_scheduler/harness.rs"]
mod harness;
#[path = "../../../../parrot-runtime/tests/task_scheduler/support.rs"]
mod support;

use std::{
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
};

use parrot_domain::{
    contracts::{
        agent_state::{AgentStateError, AgentStateStore},
        async_ops::{BoxFuture, Cancellation, OperationError, OperationResult},
        filesystem::{
            AuthorizedReadService, BoundRead, FileMetadata, FileReadError, FileReadErrorKind,
            LinkPolicy, ReadRequest,
        },
        runtime::TaskController,
        security::InvocationContext,
        tools::Tool,
    },
    values::{
        agent_state::{AgentStateSnapshot, AgentStateUpdate, TaskCallReceipt},
        agent_tasks::ExecutionStatus,
        tools::{ConfiguredToolDefinition, ToolDefinitionCatalog},
    },
};
use parrot_host::composition::factory::tasks::ScopedTaskController;
use parrot_runtime::{
    agent_turn::{lifecycle::Signal, services::TurnSelection},
    children::ScopedSession,
    task_scheduler::{ForkHistoryMode, SchedulerConfig, TaskConfiguration, TaskSessionAdapter},
};
use parrot_storage::{
    artifacts::output_blobs::{MAXIMUM_INLINE_BYTES, ToolOutputBlobStore},
    events::NeverCancelled,
};
use parrot_tools::{
    task_artifacts::TaskArtifactParserAdapter,
    tasks_queues::{
        ToolFailure, ToolInvocation,
        tasks::{TaskQuery, TaskTool, TaskToolKind},
    },
};

use harness::Harness;
use support::{Security, bounded};

type Controller = ScopedTaskController<Security, TurnSelection<Security>>;

struct SealingState {
    inner: Arc<dyn AgentStateStore>,
    gate: Mutex<Option<Arc<support::Gate>>>,
}
impl AgentStateStore for SealingState {
    fn load(&self) -> BoxFuture<'_, Result<Option<AgentStateSnapshot>, AgentStateError>> {
        self.inner.load()
    }
    fn initialize(
        &self,
        snapshot: AgentStateSnapshot,
    ) -> BoxFuture<'_, Result<(), AgentStateError>> {
        self.inner.initialize(snapshot)
    }
    fn update(
        &self,
        update: AgentStateUpdate,
    ) -> BoxFuture<'_, Result<AgentStateSnapshot, AgentStateError>> {
        Box::pin(async move {
            let gate = if matches!(&update, AgentStateUpdate::Tasks(graph) if graph.accepted_calls.last().is_some_and(|call| matches!(call.receipt, TaskCallReceipt::Accepted { .. })))
            {
                self.gate.lock().unwrap().take()
            } else {
                None
            };
            if let Some(gate) = gate {
                gate.wait(&NeverCancelled).await.unwrap();
            }
            self.inner.update(update).await
        })
    }
    fn retire(&self) -> BoxFuture<'_, Result<(), AgentStateError>> {
        self.inner.retire()
    }
}

async fn root_with_controller(
    h: &Harness,
    owner: &str,
    mode: ForkHistoryMode,
) -> (
    Arc<parrot_runtime::children::Scope<Security>>,
    Arc<Controller>,
) {
    use parrot_runtime::children::*;
    struct NoParent;
    impl parrot_runtime::agent_turn::services::ParentCompletion for NoParent {
        fn deliver_correlated<'a>(
            &'a self,
            execution: &'a parrot_runtime::agent_turn::lifecycle::Execution,
            elapsed_ms: Option<i64>,
            _: &'a str,
        ) -> parrot_domain::contracts::async_ops::BoxFuture<
            'a,
            Result<(), parrot_runtime::agent_turn::services::TurnError>,
        > {
            self.deliver(execution, elapsed_ms)
        }

        fn deliver<'a>(
            &'a self,
            _: &'a parrot_runtime::agent_turn::lifecycle::Execution,
            _: Option<i64>,
        ) -> BoxFuture<'a, Result<(), parrot_runtime::agent_turn::services::TurnError>> {
            Box::pin(async { Ok(()) })
        }
    }
    let identity = AgentIdentity::root(support::id(), owner.into(), support::templates());
    let history = h.histories.history(&identity);
    let lineage = PolicyLineage::root();
    let selection = Arc::new(SelectionCell::new(
        support::selection(&h.provider, "first", "root-mode", 0xff),
        lineage.clone(),
    ));
    let agent_state = h
        .registry
        .prepare_state(
            &identity,
            &selection.current().unwrap(),
            "",
            CompletionDelivery::RetainedOnly,
        )
        .await
        .unwrap();
    let mut parts = h
        .factory
        .build(PreparedScope {
            agent_state: agent_state.clone(),
            identity: identity.clone(),
            history: history.clone(),
            selection: selection.clone(),
            lineage,
            parent_completion: Arc::new(NoParent),
            lifetime: h.registry.lifetime(),
        })
        .await
        .unwrap();
    let built = h.factory.get(owner);
    let task_session: Arc<dyn parrot_runtime::task_scheduler::TaskSession> =
        Arc::new(TaskSessionAdapter::new(
            identity.clone(),
            built.adapter.session().clone(),
            agent_state.clone(),
        ));
    let mut configuration = TaskConfiguration::defaults(support::templates());
    configuration.fork_history_mode = mode;
    let concurrency = h.factory.concurrency.lock().unwrap().clone().unwrap();
    let sealing = Arc::new(SealingState {
        inner: agent_state.clone(),
        gate: Mutex::new(None),
    });
    let controller = Controller::new(SchedulerConfig {
        owner_identity: identity.clone(),
        concurrency,
        state: sealing.clone(),
        registry: Arc::downgrade(&h.registry),
        owner_session: task_session.clone(),
        history: built.history.clone(),
        parser: Arc::new(TaskArtifactParserAdapter),
        router: h.factory.router.clone(),
        configuration,
        output_blobs: Arc::new(ToolOutputBlobStore::for_session(
            h.histories.repository.events().database().selected_root(), "scratch/task-adapter-blobs",
            Arc::new(parrot_tools::process::execution::filenames::HaikunatorNameGenerator::default()),
        ).unwrap()),
        publisher: built.publisher.clone(),
        diagnostics: h.factory.task_diagnostics.clone(),
        lifetime: h.registry.lifetime(),
    }, Clone::clone).await.unwrap();
    parts.services = vec![
        ServiceRegistration::new(controller.scheduler().clone()),
        ServiceRegistration::new(task_session),
        ServiceRegistration::new(sealing),
    ];
    parts.work = vec![controller.scheduler().work_owner()];
    let scope = bounded(
        Scope::create(ScopeConfig {
            authority: h.registry.clone(),
            identity,
            history,
            selection,
            parent: None,
            delivery: CompletionDelivery::RetainedOnly,
            retention: None,
            parts,
        })
        .join(),
    )
    .await
    .unwrap();
    h.registry.register_root(scope.clone()).unwrap();
    (scope, controller)
}

#[derive(Default)]
struct Reader {
    bytes: Vec<u8>,
    file: Option<PathBuf>,
    denied: bool,
    calls: Mutex<Vec<(ReadRequest, u8)>>,
}
impl AuthorizedReadService for Reader {
    type Security = Security;
    fn inspect<'a>(
        &'a self,
        _: &'a Path,
        _: LinkPolicy,
        _: &'a Security,
        _: &'a dyn Cancellation,
    ) -> BoxFuture<'a, OperationResult<FileMetadata, FileReadError>> {
        panic!("task artifact reader does not inspect separately")
    }
    fn list_directory<'a>(
        &'a self,
        _: &'a Path,
        _: LinkPolicy,
        _: &'a Security,
        _: &'a dyn Cancellation,
    ) -> BoxFuture<'a, OperationResult<Vec<FileMetadata>, FileReadError>> {
        panic!("task artifact reader does not enumerate directories")
    }
    fn read<'a>(
        &'a self,
        request: &'a ReadRequest,
        security: &'a Security,
        cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, OperationResult<BoundRead, FileReadError>> {
        Box::pin(async move {
            if cancellation.is_cancelled() {
                return Err(OperationError::Cancelled);
            }
            self.calls
                .lock()
                .unwrap()
                .push((request.clone(), security.permissions));
            if self.denied {
                return Err(OperationError::Failed(FileReadError::new(
                    FileReadErrorKind::Denied,
                    "synthetic artifact denied",
                )));
            }
            Ok(BoundRead {
                lexical_path: request.requested.clone(),
                physical_path: request.requested.clone(),
                bytes: match &self.file {
                    Some(path) => std::fs::read(path).map_err(|error| {
                        OperationError::Failed(FileReadError::new(
                            FileReadErrorKind::Io,
                            error.to_string(),
                        ))
                    })?,
                    None => self.bytes.clone(),
                },
            })
        })
    }
}

fn definitions() -> ToolDefinitionCatalog {
    use parrot_foundation::config::{
        configuration::source_predefined_content, runtime::read_tool_definitions, yaml,
    };
    let root = yaml::parse_root(&source_predefined_content().unwrap()).unwrap();
    ToolDefinitionCatalog::new(
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
    )
}
type TaskToolBridge = TaskTool<Controller, Reader, TurnSelection<Security>>;
fn tool(kind: TaskToolKind, controller: &Arc<Controller>, reader: Arc<Reader>) -> TaskToolBridge {
    TaskTool::new(kind, &definitions(), controller.clone(), reader).unwrap()
}
fn invocation(arguments: &str) -> ToolInvocation {
    ToolInvocation {
        call_id: support::id(),
        arguments_json: arguments.into(),
        assistant_sequence: 17,
    }
}
async fn invoke(
    tool: &TaskToolBridge,
    arguments: &str,
    selection: TurnSelection<Security>,
    cancel: &dyn Cancellation,
) -> OperationResult<parrot_tools::tasks_queues::ToolOutput, ToolFailure> {
    let security = (*selection.security).clone();
    tool.invoke(
        invocation(arguments),
        InvocationContext::new(security, selection, cancel),
    )
    .await
}
fn declaration(name: &str) -> serde_json::Value {
    serde_json::json!({"name":name,"description":format!("Do {name}"),"payload":"Do work","acceptance_criteria":"Done"})
}
async fn occupy_self(
    root: &Arc<parrot_runtime::children::Scope<Security>>,
    controller: &Arc<Controller>,
) {
    let set = tool(TaskToolKind::Set, controller, Arc::new(Reader::default()));
    let selected = root.selection().current().unwrap();
    bounded(set.invoke(
        ToolInvocation {
            call_id: "owner-slot-call".into(),
            assistant_sequence: 0,
            arguments_json: serde_json::json!({"tasks":[declaration("owner-slot")]}).to_string(),
        },
        InvocationContext::new((*selected.security).clone(), selected, &NeverCancelled),
    ))
    .await
    .unwrap();
}
async fn state(controller: &Controller) -> Vec<parrot_domain::values::agent_tasks::Task> {
    controller
        .snapshot(TaskQuery { name: None })
        .await
        .unwrap()
        .tasks
        .into_iter()
        .map(|view| view.task)
        .collect()
}

#[tokio::test]
async fn actual_tool_parser_scheduler_and_runner_preserve_captured_model_and_details() {
    let h = Harness::new();
    let (root, controller) = root_with_controller(&h, "owner", ForkHistoryMode::Empty).await;
    occupy_self(&root, &controller).await;
    let set = tool(TaskToolKind::Set, &controller, Arc::new(Reader::default()));
    let selected = support::selection(&h.provider, "second", "root-mode", 0x3f);
    h.provider.script(
        "build",
        [fixture::Reply::text(
            r#"{"result":"built","verdict":"accept"}"#,
        )],
    );
    let result = invoke(
        &set,
        &serde_json::json!({"tasks":[declaration("build")]}).to_string(),
        selected.clone(),
        &NeverCancelled,
    )
    .await
    .unwrap();
    assert!(result.text.contains("owner-slot"));
    assert!(result.text.contains("/owner/build"));
    bounded(async {
        loop {
            let tasks = state(&controller).await;
            let task = tasks.iter().find(|task| task.name == "build").unwrap();
            assert_ne!(task.state, ExecutionStatus::Failed, "{tasks:?}");
            if task.state == ExecutionStatus::Succeeded {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await;
    let child = h.factory.get("build");
    let captured = child.selection.capture().unwrap();
    assert_eq!(
        captured.model.canonical_base(),
        selected.model.canonical_base()
    );
    assert_eq!(
        captured.security.permissions,
        root.selection().current().unwrap().security.permissions
    );
    assert_eq!(
        root.selection().current().unwrap().security.permissions,
        0xff
    );
    let get = tool(TaskToolKind::Get, &controller, Arc::new(Reader::default()));
    let detail = invoke(&get, r#"{"name":"build"}"#, selected, &NeverCancelled)
        .await
        .unwrap()
        .text;
    let detail: serde_json::Value = serde_json::from_str(&detail).unwrap();
    assert_eq!(detail["result"], "built");
    assert_eq!(detail["state"], "succeeded");
    assert_eq!(detail["agent_name"], child.adapter.identity().session.name);
    assert_eq!(detail["assigned_to"], "/owner/build");
    bounded(controller.scheduler().settle().join())
        .await
        .unwrap();
    h.close().await;
}

#[tokio::test]
async fn actual_child_history_fork_stops_before_the_invoking_tool_batch() {
    use parrot_domain::protocol::messages::parrot::Event;
    use parrot_storage::conversation::{ConversationMessage, MessageRole, ToolCall};
    let h = Harness::new();
    let (root, controller) = root_with_controller(&h, "owner", ForkHistoryMode::Parent).await;
    occupy_self(&root, &controller).await;
    let history = &h.factory.get("owner").history;
    h.histories.append(history, "safe-prefix").await;
    let mut event = Event::new();
    event.id = support::id();
    event.agent_session_id = history.agent_session_id().into();
    let mut invoking =
        ConversationMessage::text(MessageRole::Assistant, "invoking-batch-must-not-fork");
    invoking.tool_calls.push(ToolCall {
        id: "current-call".into(),
        name: "set_agent_tasks".into(),
        arguments_json: "{}".into(),
    });
    history.append_conversation(event, invoking).await.unwrap();
    let sequence = history
        .conversation()
        .await
        .unwrap()
        .last()
        .unwrap()
        .sequence;
    h.histories
        .append(history, "later-history-must-not-fork")
        .await;
    let set = tool(TaskToolKind::Set, &controller, Arc::new(Reader::default()));
    h.provider.script(
        "forked",
        [fixture::Reply::text(
            r#"{"result":"forked","verdict":"accept"}"#,
        )],
    );
    let selection = support::selection(&h.provider, "first", "root-mode", 0xff);
    let security = (*selection.security).clone();
    set.invoke(
        ToolInvocation {
            call_id: "current-call".into(),
            assistant_sequence: sequence,
            arguments_json: serde_json::json!({"tasks":[declaration("forked")]}).to_string(),
        },
        InvocationContext::new(security, selection, &NeverCancelled),
    )
    .await
    .unwrap();
    bounded(async {
        loop {
            let tasks = state(&controller).await;
            let task = tasks.iter().find(|task| task.name == "forked").unwrap();
            assert_ne!(task.state, ExecutionStatus::Failed, "{tasks:?}");
            if task.state == ExecutionStatus::Succeeded {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await;
    let requests = fixture::task_requests(&h.provider, "forked");
    let conversation = fixture::conversation(&requests[0]);
    assert!(conversation.contains("safe-prefix"));
    assert!(!conversation.contains("invoking-batch-must-not-fork"));
    assert!(!conversation.contains("later-history-must-not-fork"));
    bounded(controller.scheduler().settle().join())
        .await
        .unwrap();
    h.close().await;
}

#[tokio::test]
async fn artifact_loading_uses_only_the_injected_reader_and_preserves_invocation_security() {
    let h = Harness::new();
    let (_, controller) = root_with_controller(&h, "owner", ForkHistoryMode::Empty).await;
    let mut task = declaration("already-done");
    task["state"] = "succeeded".into();
    task["result"] = "recorded".into();
    let reader = Arc::new(Reader {
        bytes: serde_json::json!({"schema_version":1,"tasks":[task]})
            .to_string()
            .into_bytes(),
        ..Reader::default()
    });
    let set = tool(TaskToolKind::Set, &controller, reader.clone());
    invoke(
        &set,
        r#"{"path":"/synthetic/tasks.json"}"#,
        support::selection(&h.provider, "first", "root-mode", 0x42),
        &NeverCancelled,
    )
    .await
    .unwrap();
    let calls = reader.calls.lock().unwrap().clone();
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].0.requested, Path::new("/synthetic/tasks.json"));
    assert_eq!(calls[0].0.links, LinkPolicy::RejectLinks);
    assert_eq!(calls[0].0.max_bytes, None);
    assert_eq!(calls[0].1, 0x42);
    assert_eq!(
        state(&controller).await[0].result.as_deref(),
        Some("recorded")
    );
    let denied = tool(
        TaskToolKind::Set,
        &controller,
        Arc::new(Reader {
            denied: true,
            ..Reader::default()
        }),
    );
    let result = invoke(
        &denied,
        r#"{"path":"/synthetic/denied.json"}"#,
        support::selection(&h.provider, "first", "root-mode", 0x42),
        &NeverCancelled,
    )
    .await
    .unwrap();
    assert_eq!(result.text, "error: access denied");
    assert_eq!(state(&controller).await.len(), 1);
    bounded(controller.scheduler().settle().join())
        .await
        .unwrap();
    h.close().await;
}

#[tokio::test]
async fn canceled_update_and_invalid_merged_graph_do_not_mutate_the_actual_scheduler() {
    let h = Harness::new();
    let (_, controller) = root_with_controller(&h, "owner", ForkHistoryMode::Empty).await;
    let set = tool(TaskToolKind::Set, &controller, Arc::new(Reader::default()));
    let cancellation = Signal::new();
    cancellation.cancel();
    let input = serde_json::json!({"tasks":[declaration("must-not-start")]}).to_string();
    assert!(matches!(
        invoke(
            &set,
            &input,
            support::selection(&h.provider, "first", "root-mode", 0xff),
            &cancellation
        )
        .await,
        Err(OperationError::Cancelled)
    ));
    assert!(state(&controller).await.is_empty());
    let mut invalid = declaration("missing");
    invalid["dependencies"] = serde_json::json!(["absent"]);
    let result = invoke(
        &set,
        &serde_json::json!({"tasks":[invalid]}).to_string(),
        support::selection(&h.provider, "first", "root-mode", 0xff),
        &NeverCancelled,
    )
    .await
    .unwrap();
    assert!(result.text.starts_with("error: "));
    assert!(result.text.contains("missing sibling dependency 'absent'"));
    assert!(state(&controller).await.is_empty());
    bounded(controller.scheduler().settle().join())
        .await
        .unwrap();
    h.close().await;
}

#[tokio::test]
async fn admitted_update_survives_observer_cancellation_and_real_get_waits_for_spawn_tail() {
    use std::{future::Future, task::Poll};
    let h = Harness::new();
    let (_, controller) = root_with_controller(&h, "owner", ForkHistoryMode::Empty).await;
    let construction = support::Gate::new();
    let provider = support::Gate::new();
    *h.factory.construction.lock().unwrap() = Some(construction.clone());
    h.provider.script(
        "retained",
        [fixture::Reply::held(
            r#"{"result":"retained","verdict":"accept"}"#,
            &provider,
        )],
    );
    let selected = support::selection(&h.provider, "first", "root-mode", 0xff);
    let set = tool(TaskToolKind::Set, &controller, Arc::new(Reader::default()));
    let cancellation = Signal::new();
    let mut retained = declaration("retained");
    retained["model"] = "first".into();
    let arguments = serde_json::json!({"tasks":[retained]}).to_string();
    let mut update = Box::pin(invoke(&set, &arguments, selected.clone(), &cancellation));
    tokio::select! {
        result = &mut update => panic!("update completed before held admission: {result:?}"),
        _ = construction.entered() => {}
    }
    cancellation.cancel();
    assert!(matches!(
        bounded(update).await,
        Err(OperationError::Cancelled)
    ));
    let get = tool(TaskToolKind::Get, &controller, Arc::new(Reader::default()));
    let mut all = Box::pin(invoke(&get, "{}", selected.clone(), &NeverCancelled));
    let mut detail = Box::pin(invoke(
        &get,
        r#"{"name":"retained"}"#,
        selected,
        &NeverCancelled,
    ));
    assert!(std::future::poll_fn(|cx| Poll::Ready(all.as_mut().poll(cx).is_pending())).await);
    assert!(std::future::poll_fn(|cx| Poll::Ready(detail.as_mut().poll(cx).is_pending())).await);
    construction.open();
    provider.entered().await;
    let all: serde_json::Value = serde_json::from_str(&bounded(all).await.unwrap().text).unwrap();
    assert_eq!(all[0]["name"], "retained");
    assert_eq!(all[0]["state"], "running");
    let detail: serde_json::Value =
        serde_json::from_str(&bounded(detail).await.unwrap().text).unwrap();
    assert_eq!(detail["name"], "retained");
    assert_eq!(detail["agent_name"], "retained");
    assert_eq!(detail["state"], "running");
    provider.open();
    bounded(async {
        loop {
            let tasks = state(&controller).await;
            assert_ne!(tasks[0].state, ExecutionStatus::Failed, "{tasks:?}");
            if tasks[0].state == ExecutionStatus::Succeeded {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await;
    bounded(controller.scheduler().settle().join())
        .await
        .unwrap();
    h.close().await;
}

#[tokio::test]
async fn update_observer_cancellation_remains_live_during_accepted_response_sealing() {
    use parrot_storage::conversation::HistoryForkBoundary;
    use std::{future::Future, task::Poll};

    let h = Harness::new();
    let (root, controller) = root_with_controller(&h, "owner", ForkHistoryMode::Empty).await;
    let seal = support::Gate::new();
    *root
        .get_service::<SealingState>()
        .unwrap()
        .gate
        .lock()
        .unwrap() = Some(seal.clone());
    let path = h.histories.path.join("held-seal-tasks.json");
    let reader = Arc::new(Reader {
        file: Some(path.clone()),
        ..Reader::default()
    });
    let set = tool(TaskToolKind::Set, &controller, reader.clone());
    let cancellation = Signal::new();
    let selected = support::selection(&h.provider, "first", "root-mode", 0xff);
    let mut first = declaration("already-done");
    first["state"] = "succeeded".into();
    first["result"] = "recorded".into();
    std::fs::write(
        &path,
        serde_json::json!({"schema_version":1,"tasks":[first]}).to_string(),
    )
    .unwrap();
    let arguments = serde_json::json!({"path":path}).to_string();
    let call_id = support::id();
    let invoke_set = |arguments_json: String, cancellation| {
        set.invoke(
            ToolInvocation {
                call_id: call_id.clone(),
                assistant_sequence: 17,
                arguments_json,
            },
            InvocationContext::new((*selected.security).clone(), selected.clone(), cancellation),
        )
    };
    let mut update = Box::pin(invoke_set(arguments.clone(), &cancellation));
    tokio::select! {
        result = &mut update => panic!("update completed before held response seal: {result:?}"),
        _ = seal.entered() => {}
    }
    let saved = root.agent_state().load().await.unwrap().unwrap();
    assert!(matches!(
        saved.tasks.accepted_calls[0].receipt,
        TaskCallReceipt::Preparing { .. }
    ));
    cancellation.cancel();
    assert!(matches!(
        bounded(update).await,
        Err(OperationError::Cancelled)
    ));
    std::fs::write(&path, "changed and invalid artifact bytes").unwrap();
    let mut preparing_replay = Box::pin(invoke_set(arguments, &NeverCancelled));
    assert!(
        std::future::poll_fn(|cx| Poll::Ready(preparing_replay.as_mut().poll(cx).is_pending()))
            .await
    );
    std::fs::remove_file(&path).unwrap();
    assert_eq!(reader.calls.lock().unwrap().len(), 1);
    let later = controller
        .scheduler()
        .set_tasks(
            vec![fixture::state("later", ExecutionStatus::Canceled)],
            selected.clone(),
            HistoryForkBoundary::AfterCompletedHistory,
        )
        .unwrap();
    assert!(!later.is_finished());
    let get = tool(TaskToolKind::Get, &controller, Arc::new(Reader::default()));
    let mut snapshot = Box::pin(invoke(&get, "{}", selected.clone(), &NeverCancelled));
    assert!(std::future::poll_fn(|cx| Poll::Ready(snapshot.as_mut().poll(cx).is_pending())).await);
    assert_eq!(
        root.agent_state()
            .load()
            .await
            .unwrap()
            .unwrap()
            .tasks
            .entries
            .len(),
        1
    );
    seal.open();
    bounded(later.join()).await.unwrap();
    let snapshot: serde_json::Value =
        serde_json::from_str(&bounded(snapshot).await.unwrap().text).unwrap();
    assert_eq!(snapshot.as_array().unwrap().len(), 2);
    let saved = root.agent_state().load().await.unwrap().unwrap();
    let TaskCallReceipt::Accepted { response_text, .. } = &saved.tasks.accepted_calls[0].receipt
    else {
        panic!("accepted response must seal after its observer cancels");
    };
    let replay = bounded(preparing_replay).await.unwrap();
    assert_eq!(&replay.text, response_text);
    let malformed = bounded(invoke_set("not JSON".into(), &NeverCancelled))
        .await
        .unwrap();
    assert_eq!(malformed.text, replay.text);
    assert_eq!(reader.calls.lock().unwrap().len(), 1);
    assert_eq!(state(&controller).await.len(), 2);
    bounded(controller.scheduler().settle().join())
        .await
        .unwrap();
    h.close().await;
}

#[tokio::test]
async fn settled_scheduler_failure_reaches_real_tool_without_admitting_work() {
    let h = Harness::new();
    let (_, controller) = root_with_controller(&h, "owner", ForkHistoryMode::Empty).await;
    bounded(controller.scheduler().settle().join())
        .await
        .unwrap();
    let set = tool(TaskToolKind::Set, &controller, Arc::new(Reader::default()));
    let result = invoke(
        &set,
        &serde_json::json!({"tasks":[declaration("must-not-start")]}).to_string(),
        support::selection(&h.provider, "first", "root-mode", 0xff),
        &NeverCancelled,
    )
    .await
    .unwrap();
    assert_eq!(result.text, "error: The agent session is shutting down.");
    assert!(state(&controller).await.is_empty());
    assert_eq!(
        h.factory
            .build_calls
            .load(std::sync::atomic::Ordering::SeqCst),
        1
    );
    h.close().await;
}

#[tokio::test]
async fn actual_scoped_notifier_spills_exact_bytes_and_wakes_only_its_owning_session() {
    let h = Harness::new();
    let (_, controller) = root_with_controller(&h, "owner", ForkHistoryMode::Empty).await;
    h.root("unrelated").await;
    let set = tool(TaskToolKind::Set, &controller, Arc::new(Reader::default()));
    let tasks: Vec<_> = (0..900)
        .map(|index| {
            let mut task = declaration(&format!("task-{index:04}-{}", "x".repeat(70)));
            task["state"] = "canceled".into();
            task
        })
        .collect();
    let mut items = String::new();
    use std::fmt::Write;
    for task in &tasks {
        write!(items, "\n- {}: canceled", task["name"].as_str().unwrap()).unwrap();
    }
    let expected_update = format!("AgentTask states changed by your update:{items}");
    let expected_complete = format!("All AgentTasks are complete:{items}");
    assert!(expected_complete.len() > MAXIMUM_INLINE_BYTES);
    let wait = parrot_runtime::active_work::wait::WaitService {
        session: h.factory.get("owner").adapter.session().clone(),
        active_work: Arc::new(parrot_runtime::active_work::blockers::ActiveWorkReminder {
            blockers: vec![],
            templates: support::templates(),
        }),
    };
    let mut waiting = Box::pin(wait.wait(
        parrot_runtime::active_work::wait::WaitDuration::new(10000).unwrap(),
        &NeverCancelled,
    ));
    use std::{
        future::Future,
        task::{Context, Poll, Waker},
    };
    assert!(matches!(
        waiting
            .as_mut()
            .poll(&mut Context::from_waker(Waker::noop())),
        Poll::Pending
    ));
    invoke(
        &set,
        &serde_json::json!({"tasks":tasks}).to_string(),
        support::selection(&h.provider, "first", "root-mode", 0xff),
        &NeverCancelled,
    )
    .await
    .unwrap();
    assert_eq!(
        bounded(waiting).await.unwrap(),
        "wait interrupted due to AgentTask update"
    );
    h.provider.wait_requests("owner", 1).await;
    let requests = fixture::requests(&h.provider, "owner");
    let conversation = fixture::conversation(&requests[0]);
    assert!(conversation.contains("Tool output exceeded 64 KiB and was saved to "));
    assert!(fixture::requests(&h.provider, "unrelated").is_empty());
    let paths: Vec<_> = std::fs::read_dir(h.histories.path.join("scratch/task-adapter-blobs"))
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .collect();
    assert_eq!(paths.len(), 2);
    let mut texts: Vec<_> = paths
        .iter()
        .map(|path| {
            assert!(
                path.file_name()
                    .unwrap()
                    .to_str()
                    .unwrap()
                    .ends_with("-arse.dat")
            );
            std::fs::read_to_string(path).unwrap()
        })
        .collect();
    texts.sort();
    let mut expected = vec![expected_update, expected_complete];
    expected.sort();
    assert_eq!(texts, expected);
    bounded(controller.scheduler().settle().join())
        .await
        .unwrap();
    h.close().await;
}

#[tokio::test]
async fn actual_bridge_returns_actionable_self_work_and_registered_dedicated_assignment() {
    let h = Harness::new();
    let (_, controller) = root_with_controller(&h, "owner", ForkHistoryMode::Empty).await;
    let provider = support::Gate::new();
    h.provider.script(
        "self",
        [fixture::Reply::held(
            r#"{"result":"delegated result","verdict":"accept"}"#,
            &provider,
        )],
    );
    let mut own = declaration("owner-todo");
    own["payload"] = "Apply the unique owner instruction".into();
    own["acceptance_criteria"] = "The unique owner regression passes".into();
    let mut delegated = declaration("self");
    delegated["model"] = "first".into();
    let set = tool(TaskToolKind::Set, &controller, Arc::new(Reader::default()));
    let response = invoke(
        &set,
        &serde_json::json!({"tasks":[own, delegated]}).to_string(),
        support::selection(&h.provider, "first", "root-mode", 0xff),
        &NeverCancelled,
    )
    .await
    .unwrap();
    assert!(response.text.contains("Apply the unique owner instruction"));
    assert!(response.text.contains("The unique owner regression passes"));
    assert!(response.text.contains("/owner/self"));
    let get = tool(TaskToolKind::Get, &controller, Arc::new(Reader::default()));
    let selected = support::selection(&h.provider, "first", "root-mode", 0xff);
    let summaries: serde_json::Value = serde_json::from_str(
        &invoke(&get, "{}", selected.clone(), &NeverCancelled)
            .await
            .unwrap()
            .text,
    )
    .unwrap();
    assert_eq!(summaries[0]["assigned_to"], "self");
    assert_eq!(summaries[1]["assigned_to"], "/owner/self");
    let own: serde_json::Value = serde_json::from_str(
        &invoke(
            &get,
            r#"{"name":"owner-todo"}"#,
            selected.clone(),
            &NeverCancelled,
        )
        .await
        .unwrap()
        .text,
    )
    .unwrap();
    assert_eq!(own["assigned_to"], "self");
    assert!(own.get("agent_name").is_none());
    let dedicated: serde_json::Value = serde_json::from_str(
        &invoke(&get, r#"{"name":"self"}"#, selected, &NeverCancelled)
            .await
            .unwrap()
            .text,
    )
    .unwrap();
    assert_eq!(dedicated["assigned_to"], "/owner/self");
    assert_eq!(dedicated["agent_name"], "self");
    provider.open();
    bounded(async {
        loop {
            let tasks = state(&controller).await;
            let task = tasks.iter().find(|task| task.name == "self").unwrap();
            assert_ne!(task.state, ExecutionStatus::Failed, "{tasks:?}");
            if task.state == ExecutionStatus::Succeeded {
                assert_eq!(task.result.as_deref(), Some("delegated result"));
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await;
    bounded(controller.scheduler().settle().join())
        .await
        .unwrap();
    h.close().await;
}

#[tokio::test]
async fn actual_bridge_replays_exact_response_without_reading_changed_or_removed_artifact() {
    use parrot_storage::conversation::HistoryForkBoundary;

    let h = Harness::new();
    let (_, controller) = root_with_controller(&h, "owner", ForkHistoryMode::Empty).await;
    let path = h.histories.path.join("accepted-tasks.json");
    let mut task = declaration("artifact-owner");
    task["payload"] = "Implement the original artifact instruction".into();
    task["acceptance_criteria"] = "The original artifact criteria pass".into();
    std::fs::write(
        &path,
        serde_json::json!({"schema_version":1,"tasks":[task]}).to_string(),
    )
    .unwrap();
    let reader = Arc::new(Reader {
        file: Some(path.clone()),
        ..Reader::default()
    });
    let set = tool(TaskToolKind::Set, &controller, reader.clone());
    let selected = support::selection(&h.provider, "first", "root-mode", 0xff);
    let arguments = serde_json::json!({"path":path}).to_string();
    let invoke_set = |arguments_json: String| {
        set.invoke(
            ToolInvocation {
                call_id: "artifact-call".into(),
                assistant_sequence: 17,
                arguments_json,
            },
            InvocationContext::new(
                (*selected.security).clone(),
                selected.clone(),
                &NeverCancelled,
            ),
        )
    };
    let accepted = bounded(invoke_set(arguments.clone())).await.unwrap();
    assert!(
        accepted
            .text
            .contains("Implement the original artifact instruction")
    );
    assert!(
        accepted
            .text
            .contains("The original artifact criteria pass")
    );
    assert_eq!(reader.calls.lock().unwrap().len(), 1);
    let mut completed = controller
        .scheduler()
        .capture_detail("artifact-owner")
        .await
        .unwrap()
        .task;
    completed.state = ExecutionStatus::Succeeded;
    completed.result = Some("completed after accepting the original response".into());
    bounded(
        controller
            .scheduler()
            .set_tasks(
                vec![completed],
                selected.clone(),
                HistoryForkBoundary::AfterCompletedHistory,
            )
            .unwrap()
            .join(),
    )
    .await
    .unwrap();
    std::fs::write(&path, "changed and invalid artifact bytes").unwrap();
    let changed = bounded(invoke_set(arguments.clone())).await.unwrap();
    assert_eq!(changed.text, accepted.text);
    std::fs::remove_file(&path).unwrap();
    let removed = bounded(invoke_set(arguments)).await.unwrap();
    assert_eq!(removed.text, accepted.text);
    let malformed = bounded(invoke_set("not JSON".into())).await.unwrap();
    assert_eq!(malformed.text, accepted.text);
    assert_eq!(reader.calls.lock().unwrap().len(), 1);
    assert_eq!(
        state(&controller).await[0].state,
        ExecutionStatus::Succeeded
    );
    bounded(controller.scheduler().settle().join())
        .await
        .unwrap();
    h.close().await;
}
