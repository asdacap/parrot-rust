//! Skill preparation through the real core loop with raw native records.
use super::*;
use parrot_runtime::prompt_context::StaticSystemPrompt;
use protobuf::Message;

struct StoredSkills {
    rows: Vec<(parrot_core::Skill, String)>,
    reads: Mutex<Vec<String>>,
    beginnings: AtomicUsize,
}
impl TurnSkills for StoredSkills {
    fn begin_turn<'a>(&'a self, _: &'a dyn Cancellation) -> BoxFuture<'a, PromptResult<()>> {
        Box::pin(async move {
            self.beginnings.fetch_add(1, Ordering::SeqCst);
            Ok(())
        })
    }
    fn get_skills<'a>(
        &'a self,
        references: &'a [parrot_core::SkillReference],
        _: &'a dyn Cancellation,
    ) -> BoxFuture<'a, PromptResult<Vec<Option<parrot_core::Skill>>>> {
        Box::pin(async move {
            Ok(references
                .iter()
                .map(|reference| {
                    self.rows
                        .iter()
                        .find(|(skill, _)| {
                            skill.name == reference.name
                                && reference
                                    .path
                                    .as_ref()
                                    .is_none_or(|path| path == &skill.path || path == &skill.id)
                        })
                        .map(|(skill, _)| skill.clone())
                })
                .collect())
        })
    }
    fn read_skill_content<'a>(
        &'a self,
        skill: &'a parrot_core::Skill,
        _: &'a dyn Cancellation,
    ) -> BoxFuture<'a, PromptResult<parrot_core::SkillContent>> {
        Box::pin(async move {
            let (_, content) = self
                .rows
                .iter()
                .find(|(row, _)| row.id == skill.id)
                .expect("skill body belongs to the stored record");
            self.reads.lock().unwrap().push(skill.id.clone());
            Ok(parrot_core::SkillContent::Text(content.clone()))
        })
    }
    fn end_turn(&self) -> BoxFuture<'_, ()> {
        Box::pin(async {})
    }
}

/// The injected executor returns the original test's fixed SettledTool result.
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
    fn execute<'a>(
        &'a self,
        request: parrot_runtime::agent_turn::services::ToolExecutionChunk,
        resources: TurnResources<'a>,
        _: &'a TurnSelection<Security>,
        _: &'a Self::Snapshot,
        _: &'a dyn SelectionSource<Security>,
        cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<Vec<parrot_core::ToolResult>, TurnError>> {
        Box::pin(async move {
            let mut results = Vec::new();
            for call in request.calls {
                let mut started = Event::new();
                started.id = id();
                started.agent_session_id = "agent".into();
                started.set_tool_started(protocol::ToolStarted {
                    tool_call_id: call.id.clone(),
                    tool_name: call.name.clone(),
                    ..Default::default()
                });
                resources.events.emit(started, cancellation).await?;
                results.push(parrot_core::ToolResult {
                    call_id: call.id,
                    tool_name: call.name,
                    status: parrot_core::ToolStatus::Finished,
                    text: "settled".into(),
                    error_message: String::new(),
                    images: Vec::new(),
                    metadata: None,
                });
            }
            Ok(results)
        })
    }
}

async fn skill_session(
    fixture: &Fixture,
    provider: Scripted,
    skills: Arc<StoredSkills>,
    first_path: &str,
    second_path: &str,
    templates: Arc<PromptTemplateCatalog>,
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
        agent_limits: parrot_foundation::config::runtime::AgentLimitsConfig {
            disable_max_turns: false,
            ..Default::default()
        },
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
        completion_sources: vec![],
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
    let skills = Arc::new(StoredSkills {
        rows: [
            ("first", first_path.clone(), "FIRST BODY"),
            ("second", second_path.clone(), "SECOND BODY"),
            ("third", third_path.clone(), "THIRD BODY"),
        ]
        .into_iter()
        .map(|(name, path, body)| {
            (
                parrot_core::Skill {
                    id: path.clone(),
                    name: name.into(),
                    path,
                },
                body.into(),
            )
        })
        .collect(),
        reads: Mutex::new(Vec::new()),
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
        templates(),
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
    assert_eq!(
        *skills.reads.lock().unwrap(),
        [second_path, first_path.clone(), first_path, third_path]
    );
}

#[tokio::test]
async fn core_skill_context_uses_configured_minijinja_without_interpreting_skill_body() {
    let fixture = Fixture::new();
    let environment = [
        ("HOME", "/skill-test/home"),
        ("XDG_CONFIG_HOME", "/skill-test/config"),
        ("XDG_STATE_HOME", "/skill-test/state"),
        ("XDG_DATA_HOME", "/skill-test/data"),
        ("WORKDIR", "/skill-test/work"),
    ]
    .into_iter()
    .map(|(name, value)| (name.into(), value.into()))
    .collect();
    let templates = Arc::new(
        PromptTemplateCatalog::from_content(
            "prompt_templates:\n  context.skill-selected:\n    engine: minijinja\n    template: 'configured {{ name }} | {{ path }} | {{ content }}'\n    allowed_arguments: [name, path, content]\n    required_arguments: [name, path, content]\n",
            &environment,
        )
        .unwrap()
        .1,
    );
    let skills = Arc::new(StoredSkills {
        rows: vec![(
            parrot_core::Skill {
                id: "physical-skill".into(),
                name: "first".into(),
                path: "logical<&>/SKILL.md".into(),
            },
            "body {{ untouched }}".into(),
        )],
        reads: Mutex::new(Vec::new()),
        beginnings: AtomicUsize::new(0),
    });
    let provider = Scripted::new(vec![Step::complete("answer")]);
    let session = skill_session(
        &fixture,
        provider.clone(),
        skills.clone(),
        "logical<&>/SKILL.md",
        "unused",
        templates,
    )
    .await;
    let sent = send_skill_input(&session, "use $first").await;
    assert_eq!(
        bounded(sent.execution.join()).await.unwrap().status,
        ExecutionStatus::Succeeded
    );
    bounded(session.close()).await.unwrap();

    let expected = "configured first | logical&lt;&amp;&gt;/SKILL.md | body {{ untouched }}";
    {
        let requests = provider.requests.lock().unwrap();
        assert_eq!(requests.len(), 1);
        assert_eq!(requests[0].messages.last().unwrap().content(), expected);
    }
    assert!(
        fixture
            .history
            .conversation()
            .await
            .unwrap()
            .iter()
            .any(|item| {
                item.message.role == MessageRole::User
                    && parrot_storage::events::content(&item.message.parts) == expected
            })
    );
    assert_eq!(*skills.reads.lock().unwrap(), ["physical-skill"]);
}
