//! Real authoritative scope services through retained tools. Only the provider and
//! a naturally completing in-memory execution are synthetic, not OS/security proof.
use super::*;
use parrot_domain::contracts::runtime::TaskController;
use parrot_domain::protocol::messages::parrot::Delivery;
use parrot_domain::values::agent_tasks::ExecutionStatus;
use parrot_host::composition::agents::ScopedAgentRuntime;
use parrot_runtime::agent_turn::services::TurnProfile;
use parrot_runtime::tool_settlement::SharedSecurity;
use parrot_storage::artifacts::output_blobs::MAXIMUM_INLINE_BYTES;
use parrot_tools::{
    agents::{AgentSelection, AgentTool, AgentToolKind},
    process::ownership::{
        execution::ProcessResult,
        managed::CompletionReport,
        owner::{ProcessAgent, ProcessStart},
    },
    tasks_queues::tasks::{TaskQuery, TaskTool, TaskToolKind},
};
use std::{
    collections::VecDeque,
    sync::atomic::{AtomicBool, Ordering},
};
use tokio::sync::Semaphore;

pub(super) type Scripts = Arc<Mutex<VecDeque<Step>>>;
pub(super) struct Step {
    marker: String,
    events: VecDeque<ProviderEvent>,
    gate: Option<Arc<Semaphore>>,
}
struct ScriptStream(Option<Step>);
impl EventStream for ScriptStream {
    fn next<'a>(
        &'a mut self,
        cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<Option<ProviderEvent>, Error>> {
        Box::pin(async move {
            let Some(mut step) = self.0.take() else {
                return Ok(None);
            };
            if let Some(gate) = step.gate.take() {
                tokio::select! {
                    permit = gate.acquire() => permit.unwrap().forget(),
                    _ = cancellation.cancelled() => return Err(Error::Cancelled),
                }
            }
            cancellation.check().map_err(|_| Error::Cancelled)?;
            let event = step.events.pop_front();
            if !step.events.is_empty() {
                self.0 = Some(step);
            }
            Ok(event)
        })
    }
    fn close(&mut self) -> BoxFuture<'_, Result<(), Error>> {
        self.0 = None;
        Box::pin(async { Ok(()) })
    }
}
pub(super) fn scripted_stream(
    scripts: &Scripts,
    request: &Request,
) -> Option<Box<dyn EventStream>> {
    let mut scripts = scripts.lock().unwrap();
    let index = scripts.iter().position(|step| {
        request
            .messages
            .iter()
            .any(|message| message.content().contains(&step.marker))
    })?;
    Some(Box::new(ScriptStream(scripts.remove(index))))
}
pub(super) fn reply(text: &str) -> Completion {
    Completion {
        assistant_text: text.into(),
        provider_id: "local".into(),
        finish_reason: "stop".into(),
        input_tokens: 100,
        output_tokens: 5,
        ..Default::default()
    }
}
pub(super) fn script(
    fixture: &Fixture,
    marker: &str,
    completion: Completion,
    gate: Option<Arc<Semaphore>>,
) {
    fixture.scripts.lock().unwrap().push_back(Step {
        marker: marker.into(),
        events: VecDeque::from([ProviderEvent::Completed(completion)]),
        gate,
    });
}
pub(super) struct ChildProfiles;
impl ProfileResolver<Security> for ChildProfiles {
    fn resolve_child(&self, id: &str) -> ChildResult<ChildProfile<Security>> {
        if id != "adapter" && id != "agent-task-payload" {
            return Policies.resolve_child(id);
        }
        Ok(ChildProfile {
            profile: TurnProfile {
                id: id.into(),
                maximum_requests: 4,
                allowed_tools: None,
                disabled_tools: Vec::new(),
            },
            security: Arc::new(Security),
            recursion_limit: 4,
        })
    }
}
async fn fixture() -> Fixture {
    Fixture::with_profiles(false, false, None, None, Arc::new(ChildProfiles)).await
}
type Agent = AgentTool<ScopedAgentRuntime<Security>, ModelRouter>;
pub(super) fn agent_tool(
    fixture: &Fixture,
    root: &Arc<Scope<Security>>,
    kind: AgentToolKind,
) -> Agent {
    AgentTool::new(
        kind,
        catalog::configured_definitions(&fixture.configuration)
            .describe(kind.name())
            .unwrap()
            .clone(),
        root.get_service().unwrap(),
        fixture.router.clone(),
        Some(fixture.user().templates.clone()),
        true,
    )
}
pub(super) async fn agent_invoke(
    tool: &Agent,
    arguments: Value,
    cancellation: &dyn Cancellation,
) -> Result<String, OperationError<parrot_tools::agents::AgentToolError>> {
    tool.invoke(
        parrot_tools::agents::ToolInvocation {
            arguments_json: arguments.to_string(),
            assistant_sequence: 1,
            call_id: "adapter-agent-call".into(),
        },
        InvocationContext::new(
            SharedSecurity(Arc::new(Security)),
            AgentSelection {
                requested_model: "local/model".into(),
            },
            cancellation,
        ),
    )
    .await
}
fn queue_tool(
    fixture: &Fixture,
    root: &Arc<Scope<Security>>,
    kind: QueueToolKind,
) -> impl Tool<
    Invocation = ToolInvocation,
    Selection = (),
    Security = Security,
    Output = parrot_tools::tasks_queues::ToolOutput,
    Error = parrot_tools::tasks_queues::ToolFailure,
> {
    QueueTool::new(
        kind,
        &catalog::configured_definitions(&fixture.configuration),
        Arc::new(
            ScopedQueueBackend::new(root.clone(), fixture.user().registry().unwrap()).unwrap(),
        ),
        Arc::new(Files::<Security>(PhantomData)),
        fixture.clock.clone(),
        Arc::new(Diagnostics),
    )
    .unwrap()
}
async fn queue(fixture: &Fixture, root: &Arc<Scope<Security>>, kind: QueueToolKind, args: Value) {
    queue_tool(fixture, root, kind)
        .invoke(
            ToolInvocation {
                call_id: "adapter-queue-call".into(),
                arguments_json: args.to_string(),
                assistant_sequence: 1,
            },
            InvocationContext::new(Security, (), &NeverCancelled),
        )
        .await
        .unwrap();
}
async fn set_tasks(fixture: &Fixture, root: &Arc<Scope<Security>>, tasks: Value) {
    let controller = root
        .get_service::<ScopedTaskController<Security, ContextSelection<Security>>>()
        .unwrap();
    let templates = fixture.user().templates.clone();
    let render = Arc::new(move |count: usize| {
        templates
            .render(
                "agent-task.set-accepted",
                &[("count".into(), count.to_string().into())],
                &NeverCancelled,
            )
            .map_err(|e| {
                parrot_tools::tasks_queues::ToolFailure::new(
                    parrot_tools::tasks_queues::FailureKind::Other,
                    e.to_string(),
                )
            })
    });
    let tool = TaskTool::new(
        TaskToolKind::Set,
        &catalog::configured_definitions(&fixture.configuration),
        controller,
        Arc::new(Files::<SharedSecurity<Security>>(PhantomData)),
        render,
    )
    .unwrap();
    let turn = root.selection().capture().unwrap();
    let arguments_json = json!({"tasks":tasks}).to_string();
    // The actual TaskTool captures a durable invoking batch for history forks.
    // Establish that source input rather than using a nonexistent sequence zero.
    let mut event = parrot_domain::protocol::messages::parrot::Event::new();
    event.id = Identifier::event_id().unwrap();
    event.agent_session_id = root.identity().session.session_id.clone();
    let mut message = parrot_storage::conversation::ConversationMessage::text(
        parrot_storage::conversation::MessageRole::Assistant,
        "actual task invocation batch",
    );
    message
        .tool_calls
        .push(parrot_storage::conversation::ToolCall {
            id: "adapter-set-call".into(),
            name: "set_agent_tasks".into(),
            arguments_json: arguments_json.clone(),
        });
    root.history()
        .append_conversation(event, message)
        .await
        .unwrap();
    let assistant_sequence = root
        .history()
        .conversation()
        .await
        .unwrap()
        .last()
        .unwrap()
        .sequence;
    let output = tool
        .invoke(
            ToolInvocation {
                call_id: "adapter-set-call".into(),
                assistant_sequence,
                arguments_json,
            },
            InvocationContext::new(
                SharedSecurity(turn.security.clone()),
                ContextSelection::external(turn),
                &NeverCancelled,
            ),
        )
        .await
        .unwrap();
    assert!(!output.text.starts_with("error:"), "{}", output.text);
    let mut event = parrot_domain::protocol::messages::parrot::Event::new();
    event.id = Identifier::event_id().unwrap();
    event.agent_session_id = root.identity().session.session_id.clone();
    let mut message = parrot_storage::conversation::ConversationMessage::text(
        parrot_storage::conversation::MessageRole::Tool,
        output.text,
    );
    message.tool_call_id = "adapter-set-call".into();
    root.history()
        .append_conversation(event, message)
        .await
        .unwrap();
}
fn task(name: &str, payload: &str, result: Option<&str>) -> Value {
    let mut value = json!({"name":name,"description":format!("Complete {name}"),"payload":payload,"acceptance_criteria":"return the requested strict verdict","model":"local/model"});
    if let Some(result) = result {
        value["state"] = "succeeded".into();
        value["result"] = result.into();
    }
    value
}
async fn idle(root: &Arc<Scope<Security>>) {
    bounded(async {
        while root.session().is_active() {
            tokio::task::yield_now().await;
        }
    })
    .await;
}

#[tokio::test]
async fn retained_wait_wakes_from_actual_task_notifier_and_oversized_publication_uses_production_filenames()
 {
    let fixture = fixture().await;
    let hosted = fixture.open().await.unwrap();
    let root = fixture.root();
    let wait = agent_tool(&fixture, &root, AgentToolKind::Wait);
    let waiting = agent_invoke(&wait, json!({"duration_ms":10000}), &NeverCancelled);
    tokio::pin!(waiting);
    tokio::select! { result = &mut waiting => panic!("wait returned before actual task admission: {result:?}"), _ = tokio::task::yield_now() => {} }
    let tasks: Vec<_> = (0..900)
        .map(|index| {
            let mut declaration = task(
                &format!("task-{index:04}-{}", "x".repeat(70)),
                "not executed",
                None,
            );
            declaration["state"] = "canceled".into();
            declaration
        })
        .collect();
    let mut items = String::new();
    use std::fmt::Write;
    for task in &tasks {
        write!(items, "\n- {}: canceled", task["name"].as_str().unwrap()).unwrap();
    }
    let expected_update = format!("AgentTask states changed by your update:{items}");
    let expected_complete = format!("All AgentTasks are complete:{items}");
    assert!(expected_update.len() > MAXIMUM_INLINE_BYTES);
    assert!(expected_complete.len() > MAXIMUM_INLINE_BYTES);
    set_tasks(&fixture, &root, json!(tasks)).await;
    let woke = bounded(waiting).await.unwrap();
    assert_eq!(woke, "wait interrupted due to AgentTask update");
    let user = fixture.user();
    let reference = bounded(async {
        let started = Instant::now();
        loop {
            let events = user.lease.events.replay().await.unwrap();
            if started.elapsed() > Duration::from_secs(5) {
                let tasks = root.get_service::<TaskScheduler<Security, TaskArtifactParserAdapter>>().unwrap().snapshot().await;
                let states = tasks.iter().map(|task| (&task.name, task.state, task.result.as_ref().map(String::len), &task.failure)).collect::<Vec<_>>();
                let inputs = events.iter().filter(|event| event.has_input_admitted()).map(|event| (event.input_admitted().content.chars().take(180).collect::<String>(), event.input_admitted().parts.len())).collect::<Vec<_>>();
                panic!("missing spill: states={states:?}; inputs={inputs:?}; remaining scripts={} requests={}", fixture.scripts.lock().unwrap().len(), fixture.requests.lock().unwrap().len());
            }
            if let Some(text) = events
                .iter()
                .filter(|e| e.has_input_admitted())
                .flat_map(|e| e.input_admitted().parts.iter())
                .find_map(|part| {
                    part.has_text()
                        .then(|| part.text().to_owned())
                        .filter(|text| {
                            text.starts_with("Tool output exceeded 64 KiB and was saved to ")
                        })
                })
            {
                break text;
            }
            tokio::task::yield_now().await;
        }
    })
    .await;
    let path = PathBuf::from(
        reference
            .trim_start_matches("Tool output exceeded 64 KiB and was saved to ")
            .trim_end_matches('.'),
    );
    assert!(path.starts_with(user.resources.root()));
    let filename = path.file_name().unwrap().to_string_lossy();
    assert!(filename.ends_with("-arse.dat"), "{filename}");
    assert_eq!(filename.trim_end_matches("-arse.dat").split('-').count(), 2);
    let saved = std::fs::read_to_string(&path).unwrap();
    assert!(saved == expected_update || saved == expected_complete);
    let snapshot = root
        .get_service::<ScopedTaskController<Security, ContextSelection<Security>>>()
        .unwrap()
        .snapshot(TaskQuery { name: None })
        .await
        .unwrap();
    assert_eq!(snapshot.tasks.len(), 900);
    assert!(
        snapshot
            .tasks
            .iter()
            .all(|task| task.state == ExecutionStatus::Canceled)
    );
    bounded(hosted.close()).await.unwrap();
}

#[tokio::test]
async fn retained_wait_wakes_from_real_child_completion_and_cleans_child_registration() {
    let fixture = fixture().await;
    let hosted = fixture.open().await.unwrap();
    let root = fixture.root();
    let gate = Arc::new(Semaphore::new(0));
    script(
        &fixture,
        "child-wake-marker",
        reply("child naturally completed"),
        Some(gate.clone()),
    );
    let spawn = agent_tool(&fixture, &root, AgentToolKind::Spawn);
    let spawned = agent_invoke(&spawn, json!({"name":"adapter-child","agent":"adapter","prompt":"child-wake-marker","fork":"empty"}), &NeverCancelled).await.unwrap();
    assert!(spawned.contains("adapter-child"), "{spawned}");
    let child = root.children().find("adapter-child").unwrap();
    assert!(fixture.user().registry().unwrap().contains(&child));
    assert!(Arc::ptr_eq(&root, &fixture.root()));
    assert!(Arc::ptr_eq(&root, &child.parent_scope().parent().unwrap()));
    assert!(!Arc::ptr_eq(&root, &child));
    assert!(child.session().is_active());
    let work = root
        .get_service::<AgentSessionServices<Security>>()
        .unwrap()
        .active_work
        .work_sections()
        .await
        .unwrap();
    assert!(
        work.contains("Running direct subagents") && work.contains("adapter-child"),
        "{work}"
    );
    let wait = agent_tool(&fixture, &root, AgentToolKind::Wait);
    let waiting = agent_invoke(&wait, json!({"duration_ms":10000}), &NeverCancelled);
    tokio::pin!(waiting);
    tokio::select! { result = &mut waiting => panic!("wait returned before child completion: {result:?}"), _ = tokio::task::yield_now() => {} }
    gate.add_permits(1);
    let result = bounded(waiting).await.unwrap();
    assert!(
        result.contains("wait interrupted due to adapter-child completion"),
        "{result}"
    );
    bounded(async {
        while child.session().is_active() {
            tokio::task::yield_now().await;
        }
    })
    .await;
    bounded(hosted.close()).await.unwrap();
}

/// No child process is spawned. The real managed owner, completion delivery,
/// scoped status and resource joins run around this naturally released execution.
#[derive(Default)]
pub(super) struct NaturalExecution {
    finished: AtomicBool,
    changed: tokio::sync::Notify,
}
impl NaturalExecution {
    pub(super) fn finish(&self) {
        self.finished.store(true, Ordering::SeqCst);
        self.changed.notify_waiters();
    }
    async fn completed(&self) {
        loop {
            let changed = self.changed.notified();
            tokio::pin!(changed);
            changed.as_mut().enable();
            if self.finished.load(Ordering::SeqCst) {
                return;
            }
            changed.await;
        }
    }
    fn result(&self) -> ProcessResult {
        ProcessResult {
            exit_code: 0,
            elapsed_milliseconds: 1,
            stdout: "natural synthetic output".into(),
            stderr: String::new(),
            blob_path: None,
        }
    }
}
impl Execution for NaturalExecution {
    fn is_completed(&self) -> bool {
        self.finished.load(Ordering::SeqCst)
    }
    fn is_pseudo_terminal(&self) -> bool {
        false
    }
    fn stdout_path(&self) -> Option<PathBuf> {
        None
    }
    fn stderr_path(&self) -> Option<PathBuf> {
        None
    }
    fn read_transcript(&self, offset: u64) -> io::Result<(u64, Vec<u16>)> {
        Ok((offset, Vec::new()))
    }
    fn read_result(&self, offset: u64) -> io::Result<(u64, ProcessResult)> {
        assert!(self.is_completed());
        Ok((offset, self.result()))
    }
    fn wait(&self) -> BoxFuture<'_, io::Result<ProcessResult>> {
        Box::pin(async {
            self.completed().await;
            Ok(self.result())
        })
    }
    fn write_stdin<'a>(&'a self, _: &'a str) -> BoxFuture<'a, io::Result<()>> {
        panic!("no stdin capability in this fixture")
    }
    fn send_signal(&self, _: i32) -> io::Result<()> {
        panic!("signals are prohibited")
    }
    fn request_cancel(&self) {
        assert!(
            self.is_completed(),
            "the synthetic execution must finish naturally before cleanup"
        );
    }
    fn settle(&self) -> BoxFuture<'_, io::Result<()>> {
        Box::pin(async {
            self.completed().await;
            Ok(())
        })
    }
}
pub(super) async fn start_process(
    root: &Arc<Scope<Security>>,
    name: &str,
) -> Arc<NaturalExecution> {
    let owner = root.get_service::<ShellProcessOwner>().unwrap();
    let execution = Arc::new(NaturalExecution::default());
    let launched = execution.clone();
    owner
        .start(
            ProcessStart {
                requested_name: Some(name.into()),
                command: "synthetic in-memory execution; no OS launch".into(),
                description: "natural process observation".into(),
                origin_tool_call_id: "adapter-process".into(),
                agent: ProcessAgent {
                    session_id: root.identity().session.session_id.clone(),
                    name: root.identity().session.name.clone(),
                    parent_session_id: root.identity().session.parent_session_id.clone(),
                    parent_name: root.identity().parent_name.clone(),
                    depth: root.identity().session.depth,
                },
            },
            Arc::new(CompletionReport),
            move |_| Box::pin(async move { Ok(launched as Arc<dyn Execution>) }),
        )
        .await
        .unwrap()
        .yield_now();
    execution
}

#[tokio::test]
async fn retained_wait_uses_actual_process_completion_and_live_scoped_status() {
    let fixture = fixture().await;
    let hosted = fixture.open().await.unwrap();
    let root = fixture.root();
    let execution = start_process(&root, "natural-shell").await;
    let source = root
        .get_service::<dyn parrot_domain::contracts::status::ProcessStatusSource>()
        .unwrap();
    let active = source.active();
    assert_eq!(active.len(), 1);
    assert_eq!(active[0].name, "natural-shell");
    let snapshot = source.snapshot();
    assert_eq!(
        snapshot[0].owner_session_id,
        root.identity().session.session_id
    );
    assert_eq!(
        active[0].id,
        format!("{}/natural-shell", root.identity().session.session_id)
    );
    assert!(snapshot[0].process_id.starts_with("shell-process-"));
    let status = agent_tool(&fixture, &root, AgentToolKind::Status);
    let rendered = agent_invoke(
        &status,
        json!({"name":format!("/{}", root.identity().session.name)}),
        &NeverCancelled,
    )
    .await
    .unwrap();
    assert!(rendered.contains("natural-shell"), "{rendered}");
    let wait = agent_tool(&fixture, &root, AgentToolKind::Wait);
    let waiting = agent_invoke(&wait, json!({"duration_ms":10000}), &NeverCancelled);
    tokio::pin!(waiting);
    tokio::select! { result = &mut waiting => panic!("wait returned before process completion: {result:?}"), _ = tokio::task::yield_now() => {} }
    execution.finish();
    let result = bounded(waiting).await.unwrap();
    assert!(
        result.contains("wait interrupted due to") && result.contains("natural-shell"),
        "{result}"
    );
    bounded(async {
        while !source.snapshot().is_empty() {
            tokio::task::yield_now().await;
        }
    })
    .await;
    bounded(hosted.close()).await.unwrap();
    assert!(source.active().is_empty());
}

#[tokio::test]
async fn retained_wait_timeout_lists_actual_child_process_and_task_but_not_queue_only_reminder() {
    let fixture = fixture().await;
    let hosted = fixture.open().await.unwrap();
    let root = fixture.root();
    let gate = Arc::new(Semaphore::new(0));
    script(
        &fixture,
        "timeout-child-marker",
        reply("naturally done"),
        Some(gate.clone()),
    );
    let spawn = agent_tool(&fixture, &root, AgentToolKind::Spawn);
    agent_invoke(&spawn, json!({"name":"timeout-child","agent":"adapter","prompt":"timeout-child-marker","fork":"empty"}), &NeverCancelled).await.unwrap();
    let process = start_process(&root, "timeout-process").await;
    queue(
        &fixture,
        &root,
        QueueToolKind::Create,
        json!({"name":"queue-only"}),
    )
    .await;
    queue(
        &fixture,
        &root,
        QueueToolKind::Push,
        json!({"name":"queue-only","items":["queued payload"]}),
    )
    .await;
    // A failed dependency and its pending successor are real scheduler state,
    // without admitting an unwanted worker or synthesizing notification input.
    let mut failed = task("unresolved-dependency", "not executed", None);
    failed["state"] = "failed".into();
    failed["failure"] = "synthetic retained failure".into();
    set_tasks(&fixture, &root, json!([failed, {"name":"waiting-task","dependencies":["unresolved-dependency"],"description":"blocked actual task","payload":"not executed","acceptance_criteria":"unused"}])).await;
    let mut completion = reply("");
    completion
        .tool_calls
        .push(parrot_provider::sessions::ToolCall {
            id: "actual-timeout-wait".into(),
            name: "wait".into(),
            arguments_json: json!({"duration_ms":10000}).to_string(),
        });
    script(&fixture, "timeout-root-marker", completion, None);
    let root_gate = Arc::new(Semaphore::new(0));
    script(
        &fixture,
        "timeout-root-marker",
        reply("root naturally completed"),
        Some(root_gate.clone()),
    );
    hosted
        .send(
            vec![ConversationPart::text("timeout-root-marker")],
            "timeout-root-input".into(),
            Delivery::DELIVERY_STEER,
            Arc::new(NeverCancelled),
        )
        .await
        .unwrap();
    let result = tokio::time::timeout(Duration::from_secs(15), async {
        loop {
            let events = fixture.user().lease.events.replay().await.unwrap();
            if let Some(event) = events.iter().find(|event| {
                event.has_tool_finished()
                    && event.tool_finished().tool_call_id == "actual-timeout-wait"
            }) {
                break event.tool_finished().result.as_ref().unwrap().clone();
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap();
    assert!(
        result.starts_with("Wait timed out after 10000 ms."),
        "{result}"
    );
    assert!(
        result.contains("Running direct subagents") && result.contains("timeout-child"),
        "{result}"
    );
    assert!(
        result.contains("Running processes") && result.contains("timeout-process"),
        "{result}"
    );
    assert!(
        result.contains("waiting-task [pending]")
            && result.contains("unresolved-dependency [failed]"),
        "{result}"
    );
    assert!(
        !result.contains("queue-only") && !result.contains("queued payload"),
        "{result}"
    );
    process.finish();
    gate.add_permits(1);
    root_gate.add_permits(1);
    // Resolve the actual failed/pending task graph before ordinary shutdown.
    set_tasks(
        &fixture,
        &root,
        json!([
            task("unresolved-dependency", "not executed", Some("resolved")),
            task("waiting-task", "not executed", Some("resolved"))
        ]),
    )
    .await;
    queue(
        &fixture,
        &root,
        QueueToolKind::Take,
        json!({"name":"queue-only","yield_after_ms":0}),
    )
    .await;
    bounded(async {
        while !root
            .get_service::<dyn parrot_domain::contracts::status::ProcessStatusSource>()
            .unwrap()
            .snapshot()
            .is_empty()
        {
            tokio::task::yield_now().await;
        }
    })
    .await;
    bounded(hosted.close()).await.unwrap();
}

#[tokio::test]
async fn actual_registered_queue_arc_is_retained_during_and_after_host_close_and_is_disposed() {
    let barrier = Arc::new(RefreshBarrier::default());
    let fixture = Fixture::with_refresh_barrier(false, false, Some(barrier.clone())).await;
    let hosted = fixture.open().await.unwrap();
    let root = fixture.root();
    let queues = root.get_service::<AgentQueues<Security>>().unwrap();
    queue(
        &fixture,
        &root,
        QueueToolKind::Create,
        json!({"name":"identity-queue"}),
    )
    .await;
    assert!(Arc::ptr_eq(
        &queues,
        &root.get_service::<AgentQueues<Security>>().unwrap()
    ));
    let refresh = hosted.refresh_skills(&NeverCancelled);
    tokio::pin!(refresh);
    tokio::select! { result = &mut refresh => panic!("refresh did not hold close: {result:?}"), _ = barrier.entered.notified() => {} }
    let closing = hosted.close();
    tokio::pin!(closing);
    tokio::select! { result = &mut closing => panic!("close bypassed actual refresh producer: {result:?}"), _ = tokio::task::yield_now() => {} }
    assert!(Arc::ptr_eq(
        &queues,
        &root.get_service::<AgentQueues<Security>>().unwrap()
    ));
    assert!(!queues.is_disposed());
    barrier.release.notify_one();
    let _ = bounded(refresh).await;
    bounded(closing).await.unwrap();
    assert!(Arc::ptr_eq(
        &queues,
        &root.get_service::<AgentQueues<Security>>().unwrap()
    ));
    assert!(queues.is_disposed());
    assert!(queues.snapshot().unwrap().queues.is_empty());
    assert!(queues.create("after-close", "must fail").is_err());
}

#[tokio::test]
async fn production_catalog_dispatches_queue_and_write_tools_and_settles_actual_tool_results() {
    let fixture = fixture().await;
    let hosted = fixture.open().await.unwrap();
    let root = fixture.root();
    let outside = fixture
        .user()
        .resources
        .root()
        .join("must-not-be-written.txt");
    for (id, name, arguments) in [
        (
            "catalog-create",
            "queue_create",
            json!({"name":"catalog-queue"}),
        ),
        (
            "catalog-push",
            "queue_push",
            json!({"name":"catalog-queue","items":["real retained payload"]}),
        ),
        (
            "catalog-take",
            "queue_take",
            json!({"name":"catalog-queue","yield_after_ms":0}),
        ),
        (
            "catalog-write",
            "write",
            json!({"path":outside,"content":"denied fixture capability"}),
        ),
    ] {
        let mut completion = reply("");
        completion
            .tool_calls
            .push(parrot_provider::sessions::ToolCall {
                id: id.into(),
                name: name.into(),
                arguments_json: arguments.to_string(),
            });
        script(&fixture, "catalog-dispatch-marker", completion, None);
    }
    hosted
        .send(
            vec![ConversationPart::text("catalog-dispatch-marker")],
            "catalog-dispatch-input".into(),
            Delivery::DELIVERY_STEER,
            Arc::new(NeverCancelled),
        )
        .await
        .unwrap();
    idle(&root).await;
    let history = root.history().conversation().await.unwrap();
    for id in [
        "catalog-create",
        "catalog-push",
        "catalog-take",
        "catalog-write",
    ] {
        let result = history
            .iter()
            .find(|message| message.message.tool_call_id == id)
            .unwrap_or_else(|| panic!("missing actual catalog result for {id}: {history:?}"));
        if id == "catalog-take" {
            assert!(
                parrot_storage::events::content(&result.message.parts)
                    .contains("real retained payload")
            );
        }
        if id == "catalog-write" {
            assert!(
                parrot_storage::events::content(&result.message.parts)
                    .contains("synthetic fixture authorizes no filesystem operations")
            );
        }
    }
    assert!(!outside.exists());
    assert!(
        root.get_service::<AgentQueues<Security>>()
            .unwrap()
            .snapshot()
            .unwrap()
            .queues
            .iter()
            .any(|queue| queue.name == "catalog-queue" && queue.size == 0)
    );
    assert!(fixture.scripts.lock().unwrap().is_empty());
    {
        let requests = fixture.requests.lock().unwrap();
        assert!(
            requests
                .iter()
                .any(|request| request.tools.iter().any(|tool| tool.name == "write"))
        );
    }
    bounded(hosted.close()).await.unwrap();
}

#[tokio::test]
async fn actual_factory_listener_flushes_held_text_before_encrypted_reasoning_event() {
    let fixture = fixture().await;
    let hosted = fixture.open().await.unwrap();
    let root = fixture.root();
    fixture.scripts.lock().unwrap().push_back(Step {
        marker: "fragment-order-marker".into(),
        gate: None,
        events: VecDeque::from([
            ProviderEvent::TextDelta("first-fragment".into()),
            ProviderEvent::TextDelta("held-fragment".into()),
            ProviderEvent::EncryptedReasoningReceived,
            ProviderEvent::EncryptedReasoningReceived,
            ProviderEvent::Completed(reply("first-fragmentheld-fragment")),
        ]),
    });
    let mut stream = hosted.listen(None, Arc::new(NeverCancelled)).unwrap();
    hosted
        .send(
            vec![ConversationPart::text("fragment-order-marker")],
            "fragment-order-input".into(),
            Delivery::DELIVERY_STEER,
            Arc::new(NeverCancelled),
        )
        .await
        .unwrap();
    idle(&root).await;
    let observed = bounded(async {
        let mut observed = Vec::new();
        loop {
            let event = stream.next(&NeverCancelled).await.unwrap().unwrap();
            if event.has_text_chunk() {
                observed.push(event.text_chunk().fragment.clone());
            }
            if event.has_encrypted_reasoning_received() {
                observed.push("encrypted-event".into());
                if observed
                    .iter()
                    .filter(|item| item.as_str() == "encrypted-event")
                    .count()
                    == 2
                {
                    break observed;
                }
            }
        }
    })
    .await;
    assert_eq!(
        observed,
        [
            "first-fragment",
            "held-fragment",
            "encrypted-event",
            "encrypted-event"
        ]
    );
    drop(stream);
    bounded(hosted.close()).await.unwrap();
}

#[tokio::test]
async fn actual_catalog_compaction_borrows_the_drain_and_external_control_uses_same_scope() {
    let fixture = fixture().await;
    let hosted = fixture.open().await.unwrap();
    let root = fixture.root();
    hosted
        .send(
            vec![ConversationPart::text(
                "retained compaction input ".repeat(3000),
            )],
            "compaction-prefill".into(),
            Delivery::DELIVERY_STEER,
            Arc::new(NeverCancelled),
        )
        .await
        .unwrap();
    idle(&root).await;
    let mut completion = reply("");
    completion
        .tool_calls
        .push(parrot_provider::sessions::ToolCall {
            id: "actual-inline-compact".into(),
            name: "compact_context".into(),
            arguments_json: json!({"target_context_size":"16k"}).to_string(),
        });
    script(&fixture, "catalog-compact-marker", completion, None);
    hosted
        .send(
            vec![ConversationPart::text("catalog-compact-marker")],
            "compaction-dispatch".into(),
            Delivery::DELIVERY_STEER,
            Arc::new(NeverCancelled),
        )
        .await
        .unwrap();
    idle(&root).await;
    let events = fixture.user().lease.events.replay().await.unwrap();
    let result = events
        .iter()
        .find(|event| {
            event.has_tool_finished()
                && event.tool_finished().tool_call_id == "actual-inline-compact"
        })
        .expect("retained inline compact settled through the actual drain");
    assert!(
        !result
            .tool_finished()
            .result
            .as_deref()
            .unwrap()
            .to_lowercase()
            .contains("error"),
        "{result:?}"
    );
    assert!(
        events.iter().any(|event| event.has_compaction_finished()),
        "{events:?}"
    );
    // Hosted external control schedules onto that same retained session.
    bounded(hosted.compact(
        Some(parrot_domain::values::context::ContextSize::parse("16k").unwrap()),
        Arc::new(NeverCancelled),
    ))
    .await
    .unwrap();
    assert!(Arc::ptr_eq(&root, &fixture.root()));
    bounded(hosted.close()).await.unwrap();
}
