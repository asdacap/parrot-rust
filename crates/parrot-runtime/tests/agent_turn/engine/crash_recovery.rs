//! One actual-backend crash cut, not a lower-layer crash campaign.
use super::*;
use serde_json::{Value, json};
use std::{
    fs::{self, File, OpenOptions},
    io::Write,
    mem::ManuallyDrop,
    os::unix::process::ExitStatusExt,
    process::{Child, Command, Stdio},
    time::Instant,
};

const CHILD_ROOT: &str = "T46_ENGINE_CRASH_CHILD_ROOT";
const CHILD_PARENT: &str = "T46_ENGINE_CRASH_PARENT_PID";
const TEST: &str = "engine::crash_recovery::hard_kill_after_durable_terminal_before_publication_recovers_pending_input";
const PHASE_LIMIT: Duration = Duration::from_secs(20);

fn event_id() -> String {
    // Each process starts its fixture counter at zero; a restart must not reuse
    // synthetic event IDs from the killed process.
    format!(
        "t46-crash-{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::SeqCst)
    )
}

fn open_fixture(root: &Path) -> ManuallyDrop<Fixture> {
    fs::create_dir_all(root.join("scratch/blobs")).unwrap();
    let repository = Fixture::open(root);
    let history = Fixture::history_for(root, &repository);
    // Keep the real DB/artifact paths for the parent and evidence inspection.
    // In the child SIGKILL, rather than Fixture::drop, ends ownership.
    ManuallyDrop::new(Fixture {
        path: root.into(),
        repository,
        history,
    })
}

fn write_json(path: &Path, value: &Value) {
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
        .unwrap();
    serde_json::to_writer_pretty(&mut file, value).unwrap();
    file.write_all(b"\n").unwrap();
    file.sync_all().unwrap();
}

async fn observe(fixture: &Fixture) -> Value {
    let sql = fixture.repository.events().database()
        .admit("crash-observation", &NeverCancelled, |tx| {
            let mut messages = tx.prepare("SELECT role, content FROM message WHERE agent_session = 'agent' ORDER BY sequence")?;
            let history = messages.query_map([], |row| Ok(format!("{}: {}", row.get::<_, String>(0)?, row.get::<_, String>(1)?)))?
                .collect::<Result<Vec<_>, _>>()?;
            let mut inputs = tx.prepare("SELECT id, message_id, content, delivery, status FROM input WHERE agent_session = 'agent' ORDER BY sequence")?;
            let inputs = inputs.query_map([], |row| Ok(json!({
                "input_id": row.get::<_, String>(0)?, "message_id": row.get::<_, String>(1)?,
                "content": row.get::<_, String>(2)?, "delivery": row.get::<_, String>(3)?,
                "status": row.get::<_, String>(4)?,
            })))?.collect::<Result<Vec<_>, _>>()?;
            Ok(json!({"message_history": history, "inputs": inputs}))
        }).unwrap().into_completion().await.unwrap().into_value().into_output();
    let events = fixture.events().await;
    let starts: Vec<_> = events
        .iter()
        .filter(|event| event.has_turn_started())
        .map(|event| json!({"event_id": event.id, "model": event.turn_started().model}))
        .collect();
    let endings: Vec<_> = events.iter().filter(|event| event.has_turn_ended())
        .map(|event| json!({"event_id": event.id, "finish_reason": event.turn_ended().finish_reason,
            "input_tokens": event.turn_ended().input_tokens, "output_tokens": event.turn_ended().output_tokens})).collect();
    let facts: Vec<_> = events
        .iter()
        .filter(|event| event.has_request_usage_recorded())
        .map(|event| {
            let fact = event.request_usage_recorded();
            json!({
                "event_id": event.id, "request_id": fact.request_id,
                "input_tokens": fact.input_tokens, "cached_input_tokens": fact.cached_input_tokens,
                "output_tokens": fact.output_tokens,
            })
        })
        .collect();
    let promotions: Vec<_> = events.iter().filter(|event| event.has_input_promoted())
        .map(|event| json!({"input_id": event.input_promoted().input_id, "message_id": event.input_promoted().message_id})).collect();
    let statistics = fixture
        .repository
        .events()
        .get_runtime_statistics()
        .await
        .unwrap()
        .get_agent_statistics("agent")
        .capture()
        .own
        .totals;
    let raw = fs::read_to_string(fixture.path.join("agent.jsonl")).unwrap();
    let rows: Vec<Value> = raw
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    json!({
        "sql": sql, "repository_history": fixture.history.messages().await.unwrap(),
        "pending": fixture.history.has_pending_inputs().await.unwrap(),
        "turn_starts": starts, "turn_endings": endings, "usage_facts": facts,
        "promotions": promotions,
        "turn_failed_count": events.iter().filter(|event| event.has_turn_failed()).count(),
        "usage_totals": {"input_tokens": statistics.input_tokens, "cached_input_tokens": statistics.cached_input_tokens,
            "output_tokens": statistics.output_tokens, "tool_calls": statistics.tool_calls,
            "input_cost": statistics.input_cost, "output_cost": statistics.output_cost},
        "jsonl_raw": raw, "jsonl_rows": rows,
    })
}

struct CrashPublisher {
    history: AgentHistoryView,
    broker: Arc<EventBroker>,
    root: PathBuf,
    park_terminal: bool,
    published_terminals: Mutex<Vec<String>>,
}
impl EventPublisher for CrashPublisher {
    type Event = Event;
    type Error = BrokerError;
    fn publish_committed<'a>(
        &'a self,
        event: &'a Committed<Event>,
        cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<(), OperationError<BrokerError>>> {
        Box::pin(async move {
            if event.value().has_turn_ended() && self.park_terminal {
                let fixture = open_fixture_view(&self.root, self.history.clone());
                let observation = observe(&fixture).await;
                assert_eq!(observation["turn_endings"].as_array().unwrap().len(), 1);
                assert_eq!(observation["turn_endings"][0]["event_id"], event.value().id);
                assert_eq!(observation["pending"], true);
                let ready = json!({"phase": "durable-terminal-before-publication",
                    "child_pid": std::process::id(),
                    "parent_pid": std::env::var(CHILD_PARENT).unwrap().parse::<u32>().unwrap(),
                    "terminal_event_id": event.value().id,
                    "publication_forwarded": false, "observation": observation});
                let pending = self.root.join("terminal-ready.pending");
                write_json(&pending, &ready);
                fs::rename(pending, self.root.join("terminal-ready.json")).unwrap();
                println!(
                    "T46_ENGINE_CRASH_BARRIER {}",
                    serde_json::to_string(&ready).unwrap()
                );
                std::io::stdout().flush().unwrap();
                // No normal release/cancellation path: the owning parent must
                // SIGKILL this child before the broker publication below.
                std::future::pending::<()>().await;
            }
            self.broker.publish_committed(event, cancellation).await?;
            if event.value().has_turn_ended() {
                self.published_terminals
                    .lock()
                    .unwrap()
                    .push(event.value().id.clone());
            }
            Ok(())
        })
    }
    fn publish_transient<'a>(
        &'a self,
        event: Event,
        cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<(), OperationError<BrokerError>>> {
        self.broker.publish_transient(event, cancellation)
    }
}

fn open_fixture_view(root: &Path, history: AgentHistoryView) -> ManuallyDrop<Fixture> {
    ManuallyDrop::new(Fixture {
        path: root.into(),
        repository: history.repository().clone(),
        history,
    })
}

async fn runtime(
    fixture: &Fixture,
    provider: Scripted,
    park_terminal: bool,
) -> (AgentSession<Backend>, Arc<CrashPublisher>) {
    let tools = Arc::new(Tools::default());
    let trace = tools.trace.clone();
    let broker = Arc::new(EventBroker::new());
    let publisher = Arc::new(CrashPublisher {
        history: fixture.history.clone(),
        broker: broker.clone(),
        root: fixture.path.clone(),
        park_terminal,
        published_terminals: Mutex::new(vec![]),
    });
    let erased: Arc<RuntimePublisher> = publisher.clone();
    let templates = templates();
    let reminders = Arc::new(
        ExitReminders::new(fixture.history.clone(), erased.clone(), templates.clone())
            .await
            .unwrap(),
    );
    let backend = RuntimeBackend::new(RuntimeConfig {
        agent_limits: parrot_foundation::config::runtime::AgentLimitsConfig {
            disable_max_turns: false,
            ..Default::default()
        },
        identity: SessionIdentity {
            session_id: "agent".into(),
            name: "main".into(),
            parent_session_id: String::new(),
            depth: 0,
        },
        history: fixture.history.clone(),
        broker,
        publisher: erased,
        selection: Arc::new(Selection(Mutex::new(TurnSelection {
            model: provider.selection(),
            profile: TurnProfile {
                id: "mode".into(),
                maximum_requests: 8,
                allowed_tools: None,
                disabled_tools: vec![],
            },
            security: Arc::new(Security),
        }))),
        tools,
        prompt: Box::new(Prompt {
            trace: trace.clone(),
            sampled: false,
            cancel_sample: false,
        }),
        skills: Arc::new(Skills(trace.clone())),
        context: fixture.controller(),
        status: Arc::new(Status(trace.clone())),
        completions: vec![],
        templates,
        reminders,
        output: Arc::new(Output::default()),
        activity: Arc::new(Activity::new(trace.clone())),
        parent_completion: Arc::new(Parent(trace)),
        ancestors: vec![],
        event_id: Arc::new(event_id),
    })
    .await
    .unwrap();
    (
        AgentSession::new(backend, Arc::new(Signal::new())),
        publisher,
    )
}

async fn run_child(root: &Path) {
    let fixture = open_fixture(root);
    let provider_gate = Gate::new();
    let mut first = Step::complete("first durable answer");
    first.completion.as_mut().unwrap().cached_input_tokens = 2;
    first.gate = Some(provider_gate.clone());
    let (session, _) = bounded(runtime(&fixture, Scripted::new(vec![first]), true)).await;
    let sent = bounded(session.send(
        vec![ConversationPart::text("first prompt")],
        "first-message".into(),
        protocol::Delivery::DELIVERY_STEER,
        IncomingActivity {
            name: "crash test".into(),
            cause: None,
            source: protocol::InputSource::default(),
        },
        Arc::new(NeverCancelled),
    ))
    .await
    .unwrap();
    provider_gate.entered().await;
    let queued = bounded(session.record(
        vec![ConversationPart::text("queued prompt")],
        "queued-message".into(),
        protocol::Delivery::DELIVERY_QUEUE,
        Arc::new(NeverCancelled),
    ))
    .await
    .unwrap();
    assert_eq!(queued.input.message_id, "queued-message");
    assert!(fixture.history.has_pending_inputs().await.unwrap());
    provider_gate.open();
    // The parent owns the shorter phase deadline and kills/reaps this process.
    // This watchdog makes even a lost parent unable to strand the child forever.
    let result = tokio::time::timeout(Duration::from_secs(45), sent.execution.join()).await;
    panic!("child was not SIGKILLed at its terminal publication barrier: {result:?}");
}

struct OwnedChild(Option<Child>);
impl Drop for OwnedChild {
    fn drop(&mut self) {
        if let Some(child) = self.0.as_mut() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

async fn parent() {
    let root = std::env::current_dir()
        .unwrap()
        .join(".tmp")
        .join(event_id());
    fs::create_dir_all(&root).unwrap();
    let command = json!({"executable": std::env::current_exe().unwrap(), "args": ["--exact", TEST, "--nocapture"],
        "child_env": {(CHILD_ROOT): root, (CHILD_PARENT): std::process::id().to_string()}});
    write_json(&root.join("child-command.json"), &command);
    let child = Command::new(std::env::current_exe().unwrap())
        .args(["--exact", TEST, "--nocapture"])
        .env(CHILD_ROOT, &root)
        .env(CHILD_PARENT, std::process::id().to_string())
        .stdin(Stdio::null())
        .stdout(Stdio::from(
            File::create(root.join("child.stdout")).unwrap(),
        ))
        .stderr(Stdio::from(
            File::create(root.join("child.stderr")).unwrap(),
        ))
        .spawn()
        .unwrap();
    let pid = child.id();
    let mut child = OwnedChild(Some(child));
    let deadline = Instant::now() + PHASE_LIMIT;
    let ready_path = root.join("terminal-ready.json");
    loop {
        assert!(
            child.0.as_mut().unwrap().try_wait().unwrap().is_none(),
            "child exited before SIGKILL; stderr={}",
            fs::read_to_string(root.join("child.stderr")).unwrap()
        );
        if ready_path.exists() {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "terminal commit barrier timed out"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let ready: Value = serde_json::from_slice(&fs::read(ready_path).unwrap()).unwrap();
    assert_eq!(ready["child_pid"], pid);
    assert_eq!(ready["parent_pid"], std::process::id());
    assert_eq!(ready["phase"], "durable-terminal-before-publication");
    assert_eq!(ready["publication_forwarded"], false);
    let before = &ready["observation"];
    assert_eq!(
        before["sql"]["message_history"],
        json!(["user: first prompt", "assistant: first durable answer"])
    );
    assert_eq!(before["pending"], true);
    assert_eq!(before["sql"]["inputs"][1]["message_id"], "queued-message");
    assert_eq!(before["sql"]["inputs"][1]["status"], "pending");
    assert_eq!(before["turn_starts"].as_array().unwrap().len(), 1);
    assert_eq!(before["turn_endings"].as_array().unwrap().len(), 1);
    assert_eq!(before["turn_endings"][0]["finish_reason"], "stop");
    assert_eq!(before["usage_facts"].as_array().unwrap().len(), 1);
    assert_eq!(before["usage_totals"]["input_tokens"], 17);
    assert_eq!(before["usage_totals"]["output_tokens"], 9);

    child.0.as_mut().unwrap().kill().unwrap();
    let deadline = Instant::now() + PHASE_LIMIT;
    loop {
        if child.0.as_mut().unwrap().try_wait().unwrap().is_some() {
            break;
        }
        assert!(Instant::now() < deadline, "SIGKILLed child was not reaped");
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let status = child.0.as_mut().unwrap().wait().unwrap();
    child.0 = None;
    let signal = json!({"pid": pid, "signal": status.signal(), "code": status.code(), "success": status.success(), "waited_and_reaped": true});
    write_json(&root.join("child-signal.json"), &signal);
    assert_eq!(status.signal(), Some(9));
    assert_eq!(status.code(), None);
    println!(
        "T46_ENGINE_CRASH_SIGNAL {}",
        serde_json::to_string(&signal).unwrap()
    );
    println!(
        "T46_ENGINE_CRASH_BEFORE_KILL {}",
        serde_json::to_string(&ready).unwrap()
    );

    let fixture = open_fixture(&root);
    let reopened = bounded(observe(&fixture)).await;
    write_json(&root.join("reopened-before-recovery.json"), &reopened);
    assert_eq!(reopened["sql"], before["sql"]);
    assert_eq!(reopened["turn_endings"], before["turn_endings"]);
    assert_eq!(reopened["usage_facts"], before["usage_facts"]);
    assert_eq!(reopened["usage_totals"], before["usage_totals"]);
    assert_eq!(reopened["pending"], true);
    println!(
        "T46_ENGINE_CRASH_REOPENED {}",
        serde_json::to_string(&reopened).unwrap()
    );

    let mut second = Step::complete("queued recovered answer");
    let completion = second.completion.as_mut().unwrap();
    completion.input_tokens = 6;
    completion.cached_input_tokens = 1;
    completion.output_tokens = 2;
    let provider = Scripted::new(vec![second]);
    let (session, publisher) = bounded(runtime(&fixture, provider.clone(), false)).await;
    assert_eq!(
        session.backend().usage().capture().own.totals.input_tokens,
        17
    );
    assert_eq!(
        bounded(session.recover().join()).await.unwrap().status,
        ExecutionStatus::Succeeded
    );
    let recovered = bounded(observe(&fixture)).await;
    write_json(&root.join("after-recovery.json"), &recovered);
    let history = json!([
        "user: first prompt",
        "assistant: first durable answer",
        "user: queued prompt",
        "assistant: queued recovered answer"
    ]);
    assert_eq!(recovered["sql"]["message_history"], history);
    assert_eq!(recovered["repository_history"], history);
    assert_eq!(recovered["pending"], false);
    assert_eq!(recovered["sql"]["inputs"].as_array().unwrap().len(), 2);
    assert_eq!(
        recovered["sql"]["inputs"][1]["input_id"],
        before["sql"]["inputs"][1]["input_id"]
    );
    assert_eq!(recovered["sql"]["inputs"][1]["status"], "promoted");
    assert_eq!(recovered["turn_starts"].as_array().unwrap().len(), 2);
    let endings = recovered["turn_endings"].as_array().unwrap();
    assert_eq!(endings.len(), 2);
    assert_eq!(endings[0], before["turn_endings"][0]);
    assert_eq!(endings[1]["finish_reason"], "stop");
    assert_eq!(
        (
            endings[0]["input_tokens"].as_i64(),
            endings[0]["output_tokens"].as_i64()
        ),
        (Some(17), Some(9))
    );
    assert_eq!(
        (
            endings[1]["input_tokens"].as_i64(),
            endings[1]["output_tokens"].as_i64()
        ),
        (Some(23), Some(11))
    );
    assert_eq!(recovered["turn_failed_count"], 0);
    let facts = recovered["usage_facts"].as_array().unwrap();
    assert_eq!(facts.len(), 2);
    assert_eq!(facts[0], before["usage_facts"][0]);
    let usage: Vec<_> = facts
        .iter()
        .map(|fact| {
            (
                fact["input_tokens"].as_i64().unwrap(),
                fact["cached_input_tokens"].as_i64().unwrap(),
                fact["output_tokens"].as_i64().unwrap(),
            )
        })
        .collect();
    assert_eq!(usage, [(17, 2, 9), (6, 1, 2)]);
    assert_eq!(
        recovered["usage_totals"],
        json!({"input_tokens":23,"cached_input_tokens":3,"output_tokens":11,"tool_calls":0,"input_cost":0.0,"output_cost":0.0})
    );
    assert_eq!(
        session.backend().usage().capture().own.totals.input_tokens,
        23
    );
    let rows = recovered["jsonl_rows"].as_array().unwrap();
    let projected: Vec<_> = rows
        .iter()
        .filter(|row| row["type"] == "message")
        .map(|row| {
            format!(
                "{}: {}",
                row["role"].as_str().unwrap(),
                row["parts"][0]["text"].as_str().unwrap()
            )
        })
        .collect();
    assert_eq!(json!(projected), history);
    let request_rows: Vec<_> = rows
        .iter()
        .filter(|row| row["type"] == "request")
        .map(|row| {
            (
                row["input_tokens"].as_i64().unwrap(),
                row["cached_input_tokens"].as_i64().unwrap(),
                row["output_tokens"].as_i64().unwrap(),
            )
        })
        .collect();
    assert_eq!(request_rows, usage);
    assert_eq!(recovered["promotions"].as_array().unwrap().len(), 2);
    assert_eq!(recovered["promotions"][0], before["promotions"][0]);
    assert_eq!(
        recovered["promotions"][1]["input_id"],
        before["sql"]["inputs"][1]["input_id"]
    );
    assert_eq!(provider.requests.lock().unwrap().len(), 1);
    assert_eq!(
        *publisher.published_terminals.lock().unwrap(),
        vec![endings[1]["event_id"].as_str().unwrap().to_owned()]
    );
    println!(
        "T46_ENGINE_CRASH_RECOVERED {}",
        serde_json::to_string(&recovered).unwrap()
    );
    println!("T46_ENGINE_CRASH_ARTIFACT_ROOT={}", root.display());
    bounded(session.close()).await.unwrap();
    bounded(fixture.repository.events().database().close())
        .await
        .unwrap();
}

#[tokio::test]
async fn hard_kill_after_durable_terminal_before_publication_recovers_pending_input() {
    if let Some(root) = std::env::var_os(CHILD_ROOT) {
        run_child(Path::new(&root)).await;
    } else {
        parent().await;
    }
}
