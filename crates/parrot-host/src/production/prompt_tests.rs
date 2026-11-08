use super::*;
use parrot_provider::sessions::Role;
use std::sync::atomic::AtomicBool;

#[derive(Clone, Default)]
struct CaptureProvider {
    requests: Arc<Mutex<Vec<Request>>>,
    root_spawned: Arc<AtomicBool>,
    child_spawned: Arc<AtomicBool>,
}
impl CaptureProvider {
    fn stream(&self, request: Request) -> Result<Box<dyn EventStream>, Error> {
        let child = request
            .instructions
            .contains("Child agent name: prompt-child\n");
        let grandchild = request
            .instructions
            .contains("Child agent name: prompt-grandchild\n");
        self.requests.lock().unwrap().push(request);
        let tool = if child && !self.child_spawned.swap(true, Ordering::AcqRel) {
            Some((
                "agent_spawn",
                serde_json::json!({
                    "agent": "worker", "name": "prompt-grandchild", "model": "local/test",
                    "prompt": "Capture inherited context.", "fork": "empty"
                }),
            ))
        } else if !child && !grandchild && !self.root_spawned.swap(true, Ordering::AcqRel) {
            Some((
                "agent_spawn",
                serde_json::json!({
                    "agent": "worker", "name": "prompt-child", "model": "local/test",
                    "prompt": "Capture explicit context.", "scope": "PROMPT-EXPLICIT-SCOPE",
                    "fork": "empty"
                }),
            ))
        } else {
            None
        };
        Ok(Box::new(OneCompletion(Some(reply(tool)))))
    }
}
impl Provider for CaptureProvider {
    fn id(&self) -> &str {
        "local"
    }
    fn call(&self, request: Request) -> Result<Box<dyn EventStream>, Error> {
        self.stream(request)
    }
    fn has_credential<'a>(&'a self, _: &'a dyn Cancellation) -> BoxFuture<'a, Result<bool, Error>> {
        Box::pin(async { Ok(true) })
    }
    fn seed_models(&self) -> Vec<Model> {
        NoCallProvider::models()
    }
    fn list_models<'a>(
        &'a self,
        _: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<Vec<Model>, Error>> {
        Box::pin(async { Ok(NoCallProvider::models()) })
    }
    fn open_session(&self) -> Result<Arc<dyn Session>, Error> {
        Ok(Arc::new(self.clone()))
    }
}
impl Session for CaptureProvider {
    fn begin_turn(&self) -> BoxFuture<'_, Result<(), Error>> {
        Box::pin(async { Ok(()) })
    }
    fn call(&self, request: Request) -> Result<Box<dyn EventStream>, Error> {
        self.stream(request)
    }
    fn try_fall_back_to_http(&self) -> BoxFuture<'_, Result<bool, Error>> {
        Box::pin(async { Ok(false) })
    }
    fn close(&self) -> BoxFuture<'_, Result<(), Error>> {
        Box::pin(async { Ok(()) })
    }
}

#[tokio::test]
async fn production_requests_include_host_context_and_explicit_or_inherited_scope() {
    let mut fixture = Fixture::new();
    let global = fixture.roots.current().config().join("AGENTS.md");
    std::fs::create_dir_all(global.parent().unwrap()).unwrap();
    std::fs::write(&global, "PROMPT-GLOBAL-INSTRUCTION").unwrap();
    std::fs::write(
        fixture.root.join("AGENTS.md"),
        "PROMPT-ANCESTOR-INSTRUCTION",
    )
    .unwrap();
    std::fs::write(
        fixture.root.join("workspace/AGENTS.md"),
        "PROMPT-WORKSPACE-INSTRUCTION",
    )
    .unwrap();
    fixture.configuration.runtime.sandbox_rules.push(
        parrot_foundation::config::runtime::SandboxRule {
            path: global.to_string_lossy().into_owned(),
            action: SandboxRuleAction::DenyRead,
        },
    );
    assert!(
        !ProductionProfiles::new(&fixture.configuration)
            .unwrap()
            .profiles["build"]
            .1
            .allows_read(&global)
    );
    let executable = std::env::current_exe()
        .unwrap()
        .to_string_lossy()
        .into_owned();
    fixture.configuration.runtime.cli_utilities.expected = vec![executable.clone()];
    fixture.configuration.runtime.cli_utilities.optional = vec![
        fixture
            .root
            .join("not-a-command")
            .to_string_lossy()
            .into_owned(),
    ];
    fixture
        .configuration
        .runtime
        .profiles
        .get_mut("worker")
        .unwrap()
        .usage = "PROMPT-WORKER-USAGE".into();
    let capture = CaptureProvider::default();
    let host = fixture.host_with(Arc::new(capture.clone()));
    let session = bounded(host.service().create_session(
        &wire::CreateSessionRequest {
            model: "local/test".into(),
            mode: "build".into(),
            ..Default::default()
        },
        Arc::new(NeverCancelled),
    ))
    .await
    .unwrap();
    bounded(host.service().send_message(
        &wire::SendMessageRequest {
            user_session_id: session.id,
            text: "Capture the production prompts.".into(),
            delivery: wire::Delivery::DELIVERY_QUEUE.into(),
            ..Default::default()
        },
        Arc::new(NeverCancelled),
    ))
    .await
    .unwrap();
    bounded(async {
        loop {
            if capture.requests.lock().unwrap().iter().any(|request| {
                request
                    .instructions
                    .contains("Child agent name: prompt-grandchild\n")
            }) {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await;
    bounded(host.close()).await.unwrap();
    let requests = capture.requests.lock().unwrap();
    let root = requests
        .iter()
        .find(|request| !request.instructions.contains("Child agent session:"))
        .unwrap();
    let child = requests
        .iter()
        .find(|request| {
            request
                .instructions
                .contains("Child agent name: prompt-child\n")
        })
        .unwrap();
    let grandchild = requests
        .iter()
        .find(|request| {
            request
                .instructions
                .contains("Child agent name: prompt-grandchild\n")
        })
        .unwrap();
    let static_sections = fixture
        .configuration
        .system_prompts
        .values()
        .cloned()
        .collect::<Vec<_>>()
        .join("\n\n");
    for request in [root, child, grandchild] {
        let prompt = &request.instructions;
        assert!(prompt.contains(&static_sections));
        assert!(prompt.contains("## Agent Tasks"));
        assert!(!prompt.contains("@builtin:system-prompts/"));
        assert!(!prompt.contains("@builtin:profile-prompts/"));
        for marker in [
            "PROMPT-GLOBAL-INSTRUCTION",
            "PROMPT-ANCESTOR-INSTRUCTION",
            "PROMPT-WORKSPACE-INSTRUCTION",
            "Available optional CLI utilities: none",
            "Available CLI utilities:",
            "Platform:",
            "Working directory:",
            "Git repository: false",
            "Available subagents;",
            "worker: PROMPT-WORKER-USAGE",
            "The following configured sandbox rules",
            "Action: DenyRead",
            "Shell path environment",
            "Persistent writable scratch directory",
            "Your durable message and compaction history",
        ] {
            assert_eq!(prompt.matches(marker).count(), 1, "{marker}: {prompt}");
        }
        assert!(prompt.contains(&executable));
        let ordered = [
            "Available optional CLI utilities:",
            "Available CLI utilities:",
            "Platform:",
            "PROMPT-GLOBAL-INSTRUCTION",
            "PROMPT-ANCESTOR-INSTRUCTION",
            "PROMPT-WORKSPACE-INSTRUCTION",
            "Working directory:",
            "Git repository:",
            "Available subagents;",
            "The following configured sandbox rules",
            "Shell path environment",
            "Persistent writable scratch directory",
            "Your durable message and compaction history",
        ];
        assert!(
            ordered
                .windows(2)
                .all(|pair| prompt.find(pair[0]) < prompt.find(pair[1])),
            "{prompt}"
        );
    }
    for (request, profile) in [(root, "build"), (child, "worker"), (grandchild, "worker")] {
        let status = request
            .messages
            .iter()
            .rev()
            .find(|message| {
                message.role == Role::System && message.content().contains("Active profile: ")
            })
            .unwrap()
            .content();
        assert_eq!(
            status
                .matches(&fixture.configuration.runtime.profiles[profile].prompt)
                .count(),
            1
        );
        assert!(!status.contains("@builtin:profile-prompts/"));
    }
    assert!(!root.instructions.contains("PROMPT-EXPLICIT-SCOPE"));
    assert!(child.instructions.contains("Child agent depth: 1"));
    assert!(
        child
            .instructions
            .contains("### Self\nPROMPT-EXPLICIT-SCOPE")
    );
    assert!(grandchild.instructions.contains("Child agent depth: 2"));
    assert!(
        grandchild
            .instructions
            .contains("### 1st Ancestor (prompt-child)\nPROMPT-EXPLICIT-SCOPE")
    );
    drop(requests);
    std::fs::remove_dir_all(&fixture.root).unwrap();
}

#[tokio::test]
async fn production_requests_use_loaded_inline_prompt_overrides() {
    let mut fixture = Fixture::new();
    let inherited = fixture.configuration.system_prompts.clone();
    let inherited_profiles = fixture.configuration.runtime.profiles.clone();
    let path = fixture.root.join("config.yaml");
    std::fs::write(
        &path,
        "model: local/test\nskills:\n  enabled: false\nsystem_prompts:\n  runtime:system-context:01-base: PROMPT-CUSTOM-BASE\nprofiles:\n  build:\n    prompt: PROMPT-CUSTOM-BUILD\n",
    )
    .unwrap();
    let environment = BTreeMap::from([(
        "HOME".into(),
        fixture.root.join("home").to_string_lossy().into_owned(),
    )]);
    let (configuration, templates) =
        PromptTemplateCatalog::load(&path, &fixture.root.join("predefined.yaml"), &environment)
            .unwrap();
    fixture.configuration = configuration;
    fixture.templates = Arc::new(templates);
    assert_eq!(
        fixture.configuration.runtime.profiles["worker"],
        inherited_profiles["worker"]
    );
    let capture = CaptureProvider::default();
    let host = fixture.host_with(Arc::new(capture.clone()));
    let session = bounded(host.service().create_session(
        &wire::CreateSessionRequest {
            model: "local/test".into(),
            mode: "build".into(),
            ..Default::default()
        },
        Arc::new(NeverCancelled),
    ))
    .await
    .unwrap();
    bounded(host.service().send_message(
        &wire::SendMessageRequest {
            user_session_id: session.id,
            text: "Capture the overridden system prompt.".into(),
            delivery: wire::Delivery::DELIVERY_QUEUE.into(),
            ..Default::default()
        },
        Arc::new(NeverCancelled),
    ))
    .await
    .unwrap();
    bounded(async {
        while !capture.requests.lock().unwrap().iter().any(|request| {
            request
                .instructions
                .contains("Child agent name: prompt-grandchild\n")
        }) {
            tokio::task::yield_now().await;
        }
    })
    .await;
    bounded(host.close()).await.unwrap();
    let requests = capture.requests.lock().unwrap();
    for request in requests.iter() {
        let prompt = &request.instructions;
        let status = request
            .messages
            .iter()
            .rev()
            .find(|message| {
                message.role == Role::System && message.content().contains("Active profile: ")
            })
            .unwrap()
            .content();
        assert_eq!(prompt.matches("PROMPT-CUSTOM-BASE").count(), 1);
        if prompt.contains("Child agent session:") {
            assert!(!status.contains("PROMPT-CUSTOM-BUILD"));
            assert_eq!(
                status.matches(&inherited_profiles["worker"].prompt).count(),
                1
            );
        } else {
            assert_eq!(status.matches("PROMPT-CUSTOM-BUILD").count(), 1);
            assert!(!status.contains(&inherited_profiles["build"].prompt));
        }
        assert!(!status.contains("@builtin:profile-prompts/"));
        assert!(!prompt.contains("@builtin:system-prompts/"));
        assert!(!prompt.contains("@builtin:profile-prompts/"));
        for (key, text) in &inherited {
            if key == "runtime:system-context:01-base" {
                assert!(!prompt.contains(text));
            } else {
                assert_eq!(prompt.matches(text).count(), 1, "{key}");
            }
        }
        let ordered = fixture
            .configuration
            .system_prompts
            .values()
            .cloned()
            .collect::<Vec<_>>()
            .join("\n\n");
        assert!(prompt.contains(&ordered));
    }
    drop(requests);
    std::fs::remove_dir_all(&fixture.root).unwrap();
}
