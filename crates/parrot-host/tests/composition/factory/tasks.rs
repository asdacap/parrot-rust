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
    path::Path,
    sync::{Arc, Mutex},
};

use parrot_domain::{
    contracts::{
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
        _: &'a str,
    ) -> parrot_domain::contracts::async_ops::BoxFuture<
        'a,
        Result<(), parrot_runtime::agent_turn::services::TurnError>,
    > {
        self.deliver(execution)
    }

        fn deliver<'a>(
            &'a self,
            _: &'a parrot_runtime::agent_turn::lifecycle::Execution,
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
    let task_session: Arc<dyn parrot_runtime::task_scheduler::TaskSession> = Arc::new(
        TaskSessionAdapter::new(identity.clone(), built.adapter.session().clone()),
    );
    let mut configuration = TaskConfiguration::defaults(support::templates());
    configuration.fork_history_mode = mode;
    let controller = Controller::new(SchedulerConfig {
        owner_identity: identity.clone(),
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
    }, Clone::clone).unwrap();
    parts.services = vec![
        ServiceRegistration::new(controller.scheduler().clone()),
        ServiceRegistration::new(task_session),
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
                bytes: self.bytes.clone(),
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
fn render_accepted(count: usize) -> Result<String, ToolFailure> {
    support::templates()
        .render(
            "agent-task.set-accepted",
            &[("count".into(), serde_json::Value::String(count.to_string()))],
            &NeverCancelled,
        )
        .map_err(|error| {
            ToolFailure::new(
                parrot_tools::tasks_queues::FailureKind::Other,
                error.to_string(),
            )
        })
}
type TaskToolBridge =
    TaskTool<Controller, Reader, fn(usize) -> Result<String, ToolFailure>, TurnSelection<Security>>;
fn tool(kind: TaskToolKind, controller: &Arc<Controller>, reader: Arc<Reader>) -> TaskToolBridge {
    TaskTool::new(
        kind,
        &definitions(),
        controller.clone(),
        reader,
        Arc::new(render_accepted as fn(usize) -> Result<String, ToolFailure>),
    )
    .unwrap()
}
fn invocation(arguments: &str) -> ToolInvocation {
    ToolInvocation {
        call_id: "set-call".into(),
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
async fn state(controller: &Controller) -> Vec<parrot_domain::values::agent_tasks::Task> {
    controller
        .snapshot(TaskQuery { name: None })
        .await
        .unwrap()
        .tasks
}

#[tokio::test]
async fn actual_tool_parser_scheduler_and_runner_preserve_captured_model_and_details() {
    let h = Harness::new();
    let (root, controller) = root_with_controller(&h, "owner", ForkHistoryMode::Empty).await;
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
    assert_eq!(
        result.text,
        "AgentTask update accepted (1 tasks). State changes are delivered as notifications."
    );
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
    let (_, controller) = root_with_controller(&h, "owner", ForkHistoryMode::Parent).await;
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
            assert_ne!(tasks[0].state, ExecutionStatus::Failed, "{tasks:?}");
            if tasks[0].state == ExecutionStatus::Succeeded {
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
    let arguments = serde_json::json!({"tasks":[declaration("retained")]}).to_string();
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
async fn update_observer_cancellation_remains_live_during_final_snapshot_of_later_admission() {
    use parrot_domain::contracts::agent_tasks::TaskArtifactParser;
    use parrot_storage::conversation::HistoryForkBoundary;
    use std::{future::Future, task::Poll};
    let h = Harness::new();
    let (_, controller) = root_with_controller(&h, "owner", ForkHistoryMode::Empty).await;
    let set = tool(TaskToolKind::Set, &controller, Arc::new(Reader::default()));
    let cancellation = Signal::new();
    let selected = support::selection(&h.provider, "first", "root-mode", 0xff);
    let mut first = declaration("already-done");
    first["state"] = "succeeded".into();
    first["result"] = "recorded".into();
    let arguments = serde_json::json!({"tasks":[first]}).to_string();
    let mut update = Box::pin(invoke(&set, &arguments, selected.clone(), &cancellation));
    assert!(std::future::poll_fn(|cx| Poll::Ready(update.as_mut().poll(cx).is_pending())).await);
    let construction = support::Gate::new();
    *h.factory.construction.lock().unwrap() = Some(construction.clone());
    h.provider.script(
        "later",
        [fixture::Reply::text(
            r#"{"result":"later","verdict":"accept"}"#,
        )],
    );
    let tasks = TaskArtifactParserAdapter
        .parse_task_set(&serde_json::json!([declaration("later")]).to_string())
        .unwrap();
    let later = controller
        .scheduler()
        .set_tasks(tasks, selected, HistoryForkBoundary::AfterCompletedHistory)
        .unwrap();
    construction.entered().await;
    // FIFO proves the first operation completed; this poll advances its observer
    // into the final snapshot, which must wait for the still-held later update.
    assert!(std::future::poll_fn(|cx| Poll::Ready(update.as_mut().poll(cx).is_pending())).await);
    cancellation.cancel();
    assert!(matches!(
        bounded(update).await,
        Err(OperationError::Cancelled)
    ));
    assert!(!later.is_finished());
    construction.open();
    bounded(later.join()).await.unwrap();
    bounded(async {
        loop {
            let tasks = state(&controller).await;
            let later = tasks.iter().find(|task| task.name == "later").unwrap();
            assert_ne!(later.state, ExecutionStatus::Failed, "{tasks:?}");
            if later.state == ExecutionStatus::Succeeded {
                assert_eq!(later.result.as_deref(), Some("later"));
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
        task::{Context, Poll, Wake, Waker},
    };
    struct NoWake;
    impl Wake for NoWake {
        fn wake(self: Arc<Self>) {}
    }
    let waker = Waker::from(Arc::new(NoWake));
    assert!(matches!(
        waiting.as_mut().poll(&mut Context::from_waker(&waker)),
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
