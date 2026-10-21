//! R04 composition of nonempty skill results, not a replacement for T44's
//! discovery, mention parsing, rendering or security algorithms.
use super::*;
use parrot_runtime::prompt_context::StaticSystemPrompt;
use protobuf::Message;

struct ScriptedSkills {
    selections: Mutex<VecDeque<(String, SkillRendering)>>,
    pending: Mutex<Option<SkillRendering>>,
    beginnings: AtomicUsize,
}
impl TurnSkills for ScriptedSkills {
    fn begin_turn<'a>(&'a self, _: &'a dyn Cancellation) -> BoxFuture<'a, PromptResult<()>> {
        Box::pin(async move {
            assert!(self.pending.lock().unwrap().is_none());
            self.beginnings.fetch_add(1, Ordering::SeqCst);
            Ok(())
        })
    }
    fn select<'a>(
        &'a self,
        parts: &'a [ConversationPart],
        _: &'a dyn Cancellation,
    ) -> BoxFuture<'a, PromptResult<()>> {
        Box::pin(async move {
            let (expected, rendering) = self.selections.lock().unwrap().pop_front().unwrap();
            assert_eq!(parrot_storage::events::content(parts), expected);
            assert!(self.pending.lock().unwrap().replace(rendering).is_none());
            Ok(())
        })
    }
    fn render<'a>(
        &'a self,
        _: &'a dyn Cancellation,
    ) -> BoxFuture<'a, PromptResult<SkillRendering>> {
        Box::pin(async move {
            Ok(self
                .pending
                .lock()
                .unwrap()
                .take()
                .unwrap_or(SkillRendering {
                    content: String::new(),
                    loaded_paths: vec![],
                }))
        })
    }
    fn end_turn(&self) -> BoxFuture<'_, ()> {
        Box::pin(async {})
    }
}

/// The injected settlement port uses the real durable conversation APIs. Its
/// fixed result matches the original test's SettledTool; T47 owns real tools.
struct SettledTools;
impl TurnTools<Security> for SettledTools {
    type Snapshot = Vec<sessions::Tool>;
    fn materialize(&self) -> Result<Self::Snapshot, TurnError> {
        Ok(vec![])
    }
    fn permitted(
        &self,
        snapshot: &Self::Snapshot,
        _: &TurnSelection<Security>,
    ) -> Result<Self::Snapshot, TurnError> {
        Ok(snapshot.clone())
    }
    fn definitions<'a>(&self, snapshot: &'a Self::Snapshot) -> &'a [sessions::Tool] {
        snapshot
    }
    fn enabled_after_interruption(&self, snapshot: &Self::Snapshot) -> Self::Snapshot {
        snapshot.clone()
    }
    fn reconcile<'a>(
        &'a self,
        resources: TurnResources<'a>,
        _: &'a TurnSelection<Security>,
        _: &'a Self::Snapshot,
        _: &'a dyn SelectionSource<Security>,
        cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<(), TurnError>> {
        Box::pin(async move {
            for assistant in resources.history.conversation().await? {
                for call in assistant.message.tool_calls {
                    if !resources
                        .history
                        .tool_terminals_of_calls(vec![call.id.clone()])
                        .await?
                        .is_empty()
                    {
                        continue;
                    }
                    let mut started = Event::new();
                    started.id = id();
                    started.agent_session_id = "agent".into();
                    started.set_tool_started(protocol::ToolStarted {
                        tool_call_id: call.id.clone(),
                        tool_name: call.name.clone(),
                        ..Default::default()
                    });
                    resources.events.emit(started, cancellation).await?;
                    let mut finished = Event::new();
                    finished.id = id();
                    finished.agent_session_id = "agent".into();
                    finished.set_tool_finished(protocol::ToolFinished {
                        tool_call_id: call.id.clone(),
                        tool_name: call.name.clone(),
                        result: Some("settled".into()),
                        ..Default::default()
                    });
                    if let Some(item) = resources
                        .history
                        .append_tool_settlement(
                            finished,
                            assistant.sequence,
                            parrot_storage::events::ToolExecutionTerminal {
                                tool_call_id: call.id,
                                tool_name: call.name,
                                status: parrot_storage::events::ToolExecutionStatus::Finished,
                                result_parts: vec![ConversationPart::text("settled")],
                                message: "settled".into(),
                            },
                        )
                        .await?
                    {
                        let message = resources.conversation.materialize(item).await?;
                        resources.conversation.add(message);
                    }
                }
            }
            Ok(())
        })
    }
}

async fn skill_session(
    fixture: &Fixture,
    provider: Scripted,
    skills: Arc<ScriptedSkills>,
    first_path: &str,
    second_path: &str,
) -> AgentSession<RuntimeBackend<Security, SettledTools>> {
    let broker = Arc::new(EventBroker::new());
    let trace = Arc::new(Trace::default());
    let publisher: Arc<RuntimePublisher> = Arc::new(Publisher {
        history: fixture.history.clone(),
        broker: broker.clone(),
        terminal_failure: 0,
        terminals: AtomicUsize::new(0),
        terminal_observed: AtomicBool::new(false),
        reminder_gate: Mutex::new(None),
        cancel_retry: AtomicBool::new(false),
        trace: trace.clone(),
    });
    let templates = templates();
    let reminders = Arc::new(
        ExitReminders::new(
            fixture.history.clone(),
            publisher.clone(),
            templates.clone(),
        )
        .await
        .unwrap(),
    );
    // The catalog text is an injected snapshot. This test does not discover or
    // read skill files or infer permissions from these inert path descriptions.
    let instructions =
        format!("Available Markdown skills:\n- $first ({first_path})\n- $second ({second_path})");
    let backend = RuntimeBackend::new(RuntimeConfig {
        identity: SessionIdentity {
            session_id: "agent".into(),
            name: "test".into(),
            parent_session_id: String::new(),
            depth: 0,
        },
        history: fixture.history.clone(),
        broker,
        publisher,
        selection: Arc::new(Selection(Mutex::new(TurnSelection {
            model: provider.selection(),
            profile: TurnProfile {
                id: "mode".into(),
                maximum_requests: 3,
                allowed_tools: None,
                disabled_tools: vec![],
            },
            security: Arc::new(Security),
        }))),
        tools: Arc::new(SettledTools),
        prompt: Box::new(StaticSystemPrompt(instructions)),
        skills,
        context: fixture.controller(),
        status: Arc::new(Status(trace.clone())),
        completions: vec![],
        templates,
        reminders,
        output: Arc::new(Output::default()),
        activity: Arc::new(Activity::new(trace.clone())),
        parent_completion: Arc::new(Parent(trace)),
        ancestors: vec![],
        event_id: Arc::new(id),
    })
    .await
    .unwrap();
    AgentSession::new(backend, Arc::new(Signal::new()))
}

async fn send_skill_input(
    session: &AgentSession<RuntimeBackend<Security, SettledTools>>,
    prompt: &str,
) -> parrot_runtime::agent_turn::lifecycle::SendResult {
    bounded(session.send(
        vec![ConversationPart::text(prompt)],
        id(),
        protocol::Delivery::DELIVERY_STEER,
        IncomingActivity {
            name: "test".into(),
            cause: None,
            source: protocol::InputSource::default(),
        },
        Arc::new(NeverCancelled),
    ))
    .await
    .unwrap()
}

#[tokio::test]
async fn selected_skill_context_follows_original_steer_and_new_turn_history_and_event_order() {
    let fixture = Fixture::new();
    let first_path = fixture
        .path
        .join("skills/first/SKILL.md")
        .display()
        .to_string();
    let second_path = fixture
        .path
        .join("skills/second/SKILL.md")
        .display()
        .to_string();
    let third_path = fixture
        .path
        .join("skills/third/SKILL.md")
        .display()
        .to_string();
    // Fixed source-shaped results, rather than copied T44 selection/rendering.
    let first_body =
        format!("<skill>\n<name>first</name>\n<path>{first_path}</path>\nFIRST BODY\n</skill>");
    let second_body =
        format!("<skill>\n<name>second</name>\n<path>{second_path}</path>\nSECOND BODY\n</skill>");
    let third_body =
        format!("<skill>\n<name>third</name>\n<path>{third_path}</path>\nTHIRD BODY\n</skill>");
    let skills = Arc::new(ScriptedSkills {
        selections: Mutex::new(VecDeque::from([
            (
                "use $second then $second".into(),
                SkillRendering {
                    content: second_body,
                    loaded_paths: vec![second_path.clone()],
                },
            ),
            (
                "also $first".into(),
                SkillRendering {
                    content: first_body.clone(),
                    loaded_paths: vec![first_path.clone()],
                },
            ),
            (
                "next turn $first $third".into(),
                SkillRendering {
                    content: format!("{first_body}\n\n{third_body}"),
                    loaded_paths: vec![first_path.clone(), third_path.clone()],
                },
            ),
        ])),
        pending: Mutex::new(None),
        beginnings: AtomicUsize::new(0),
    });
    let first_gate = Gate::new();
    let second_gate = Gate::new();
    let third_gate = Gate::new();
    let mut first = Step::complete("");
    first.gate = Some(first_gate.clone());
    first.completion.as_mut().unwrap().tool_calls = vec![sessions::ToolCall {
        id: "call-1".into(),
        name: "settled".into(),
        arguments_json: "{}".into(),
    }];
    let mut second = Step::complete("first answer");
    second.gate = Some(second_gate.clone());
    let mut third = Step::complete("second answer");
    third.gate = Some(third_gate.clone());
    let provider = Scripted::new(vec![first, second, third]);
    let session = skill_session(
        &fixture,
        provider.clone(),
        skills.clone(),
        &first_path,
        &second_path,
    )
    .await;
    let first_send = send_skill_input(&session, "use $second then $second").await;
    first_gate.entered().await;
    let steer = send_skill_input(&session, "also $first").await;
    first_gate.open();
    second_gate.entered().await;
    {
        let requests = provider.requests.lock().unwrap();
        for catalog_item in ["$first", "$second", &first_path, &second_path] {
            assert!(requests[0].instructions.contains(catalog_item));
        }
        let first = requests[0].messages.last().unwrap();
        assert_eq!(first.role, sessions::Role::User);
        assert!(first.content().contains("SECOND BODY"));
        assert!(!first.content().contains("FIRST BODY"));
        let continued = requests[1].messages.last().unwrap();
        assert!(continued.content().contains("FIRST BODY"));
        assert!(!continued.content().contains("SECOND BODY"));
        assert_eq!(
            requests[1]
                .messages
                .iter()
                .map(|message| message.content().matches("SECOND BODY").count())
                .sum::<usize>(),
            1
        );
    }
    second_gate.open();
    assert_eq!(
        bounded(first_send.execution.join()).await.unwrap().status,
        ExecutionStatus::Succeeded
    );
    assert_eq!(
        bounded(steer.execution.join()).await.unwrap().status,
        ExecutionStatus::Succeeded
    );
    let next = send_skill_input(&session, "next turn $first $third").await;
    third_gate.entered().await;
    {
        let requests = provider.requests.lock().unwrap();
        let next = requests[2].messages.last().unwrap().content();
        assert!(next.contains("FIRST BODY"));
        assert!(next.contains("THIRD BODY"));
    }
    third_gate.open();
    assert_eq!(
        bounded(next.execution.join()).await.unwrap().status,
        ExecutionStatus::Succeeded
    );
    bounded(session.close()).await.unwrap();

    let conversation = fixture.history.conversation().await.unwrap();
    let history = conversation
        .iter()
        .map(|item| {
            let text = parrot_storage::events::content(&item.message.parts);
            if text.contains("<skill>") {
                format!(
                    "skill: {}",
                    ["FIRST", "SECOND", "THIRD"]
                        .into_iter()
                        .filter(|body| text.contains(&format!("{body} BODY")))
                        .collect::<Vec<_>>()
                        .join(" ")
                )
            } else {
                format!("{}: {text}", item.message.role.as_str())
            }
        })
        .collect::<Vec<_>>()
        .join(" | ");
    assert_eq!(
        history,
        "user: use $second then $second | skill: SECOND | assistant:  | tool: settled | user: also $first | skill: FIRST | assistant: first answer | user: next turn $first $third | skill: FIRST THIRD | assistant: second answer"
    );
    // The source-format JSONL intentionally escapes '<'. Compare decoded
    // message parts, not an invalid assumption about their wire spelling.
    let projected: Vec<_> = fixture
        .jsonl()
        .lines()
        .filter_map(|line| {
            match parrot_storage::history_files::json::deserialize_entry(line.as_bytes()).unwrap() {
                parrot_storage::conversation::AgentHistoryEntry::Message {
                    role, parts, ..
                } => Some((role, parts)),
                _ => None,
            }
        })
        .collect();
    let authoritative: Vec<_> = conversation
        .iter()
        .map(|item| {
            (
                item.message.role.as_str().to_owned(),
                item.message.parts.clone(),
            )
        })
        .collect();
    assert_eq!(projected, authoritative);
    let events = fixture.events().await;
    let loaded: Vec<_> = events
        .iter()
        .filter(|event| event.has_skill_loaded())
        .collect();
    assert_eq!(
        loaded
            .iter()
            .map(|event| event.skill_loaded().path.as_str())
            .collect::<Vec<_>>()
            .join("|"),
        format!("{second_path}|{first_path}|{first_path}|{third_path}")
    );
    assert_eq!(events.iter().filter(|event| event.has_agent_notice() && event.agent_notice().kind == "skill_context").count(), 3);
    assert!(
        events
            .iter()
            .position(|event| event.has_skill_loaded())
            .unwrap()
            < events
                .iter()
                .position(|event| event.has_tool_started())
                .unwrap()
    );
    for event in &events {
        assert!(!event.to_string().contains("<skill>"));
        assert!(
            !event
                .write_to_bytes()
                .unwrap()
                .windows(b"<skill>".len())
                .any(|bytes| bytes == b"<skill>")
        );
    }
    assert_eq!(provider.requests.lock().unwrap().len(), 3);
    assert_eq!(skills.beginnings.load(Ordering::SeqCst), 2);
    assert!(skills.selections.lock().unwrap().is_empty());
}
