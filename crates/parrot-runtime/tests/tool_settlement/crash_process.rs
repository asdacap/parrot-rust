use super::*;

struct PersistentEffectTool {
    metadata: ToolMetadata,
    effect: PathBuf,
}

impl PersistentEffectTool {
    fn new(effect: PathBuf) -> Self {
        Self {
            metadata: ToolMetadata {
                name: "counter".into(),
                aliases: vec![],
            },
            effect,
        }
    }
}

impl Tool for PersistentEffectTool {
    type Definition = ToolMetadata;
    type Invocation = ToolInvocation;
    type Selection = TurnSelection<Security>;
    type Security = SharedSecurity<Security>;
    type Output = ToolOutput;
    type Error = ToolFailure;

    fn definition(&self) -> &ToolMetadata {
        &self.metadata
    }
    fn parallel_safety(&self, _: &ToolInvocation) -> ParallelSafety {
        ParallelSafety::Sequential
    }
    fn enabled_after_interruption(&self) -> bool {
        false
    }
    fn invoke<'a>(
        &'a self,
        _: ToolInvocation,
        _: InvocationContext<'a, SharedSecurity<Security>, TurnSelection<Security>>,
    ) -> BoxFuture<'a, Result<ToolOutput, OperationError<ToolFailure>>> {
        Box::pin(async move {
            let previous: usize =
                std::fs::read_to_string(&self.effect).map_or(0, |text| text.parse().unwrap());
            let pending = self.effect.with_extension("pending");
            std::fs::write(&pending, (previous + 1).to_string()).unwrap();
            std::fs::rename(pending, &self.effect).unwrap();
            std::future::pending().await
        })
    }
}

struct ProbeProcess(std::process::Child);
impl Drop for ProbeProcess {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

#[tokio::test]
async fn process_kill_does_not_replay_an_unfinished_tool_effect() {
    const ROOT: &str = "T47_UNFINISHED_TOOL_ROOT";
    const SEQUENCE: &str = "T47_UNFINISHED_TOOL_SEQUENCE";
    if let Some(root) = std::env::var_os(ROOT) {
        let path = PathBuf::from(root);
        let repository = Fixture::open(&path);
        let history = Fixture::history_for(&path, &repository);
        let fixture = Fixture {
            path,
            repository,
            history,
            fresh_batches: Mutex::new(VecDeque::new()),
        };
        let sequence = std::env::var(SEQUENCE).unwrap().parse().unwrap();
        fixture
            .fresh_batches
            .lock()
            .unwrap()
            .push_back((sequence, vec![call("kill-effect", "counter").into()]));
        let tool = Arc::new(PersistentEffectTool::new(
            fixture.path.join("external-effect"),
        ));
        let mut harness = Harness::new(&fixture, vec![ToolHandle::Ordinary(tool)]).await;
        harness.run(&fixture, &NeverCancelled).await.unwrap();
        panic!("crash probe must remain inside the unfinished tool");
    }
    let fixture = Fixture::new();
    let sequence = fixture.batch(vec![call("kill-effect", "counter")]).await;
    fixture
        .repository
        .events()
        .database()
        .close()
        .await
        .unwrap();
    let effect = fixture.path.join("external-effect");
    let mut child = ProbeProcess(
        std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "crash_process::process_kill_does_not_replay_an_unfinished_tool_effect",
                "--nocapture",
            ])
            .env(ROOT, &fixture.path)
            .env(SEQUENCE, sequence.to_string())
            .spawn()
            .unwrap(),
    );
    bounded(async {
        loop {
            if effect.exists() {
                break;
            }
            if let Some(status) = child.0.try_wait().unwrap() {
                panic!("crash probe exited before its external effect: {status}");
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    })
    .await;
    child.0.kill().unwrap();
    assert!(!child.0.wait().unwrap().success());
    assert_eq!(std::fs::read_to_string(&effect).unwrap(), "1");
    let repository = Fixture::open(&fixture.path);
    let restarted = Fixture {
        path: fixture.path.clone(),
        history: Fixture::history_for(&fixture.path, &repository),
        repository,
        fresh_batches: Mutex::new(VecDeque::new()),
    };
    let tool = Arc::new(PersistentEffectTool::new(effect.clone()));
    let mut harness = Harness::new(&restarted, vec![ToolHandle::Ordinary(tool)]).await;
    bounded(harness.restore(&restarted)).await.unwrap();
    bounded(harness.restore(&restarted)).await.unwrap();
    assert_eq!(std::fs::read_to_string(&effect).unwrap(), "1");
    let terminals = restarted
        .history
        .tool_terminals_of_calls(vec!["kill-effect".into()])
        .await
        .unwrap();
    assert_eq!(terminals.len(), 1);
    assert_eq!(terminals[0].status, ToolExecutionStatus::Error);
    assert!(terminals[0].content().contains(CRASH_MESSAGE));
    let events = restarted.events().await;
    assert_eq!(
        events
            .iter()
            .filter(|event| event.has_tool_started())
            .count(),
        1
    );
    assert_eq!(
        events
            .iter()
            .filter(|event| event.has_tool_execution_started())
            .count(),
        1
    );
    assert_eq!(
        events.iter().filter(|event| event.has_tool_error()).count(),
        1
    );
    assert_eq!(
        events
            .iter()
            .find(|event| event.has_tool_error())
            .unwrap()
            .tool_error()
            .message,
        CRASH_MESSAGE
    );
    assert_eq!(
        harness
            .publisher
            .events
            .lock()
            .unwrap()
            .iter()
            .filter(|event| event.has_tool_error())
            .count(),
        1
    );
}
