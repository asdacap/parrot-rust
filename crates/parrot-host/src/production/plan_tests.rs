use super::*;
use parrot_provider::sessions::{Completion, Role};
use std::collections::BTreeSet;

const PLAN_LABEL: &str = "Write the canonical Markdown plan to this exact file: ";
const TASK_LABEL: &str = "Write the required AgentTask v1 JSON artifact to this exact file: ";
const TASKS: &str = r#"{"schema_version":1,"tasks":[{"name":"implement","description":"Implement the approved change","payload":"Implement the approved change","acceptance_criteria":"The requested behavior works"}]}"#;

fn current_status(request: &Request) -> &str {
    request
        .messages
        .iter()
        .rev()
        .find(|message| {
            message.role == Role::System && message.content().contains("Active profile: ")
        })
        .expect("the production request must include its runtime status")
        .content()
}
fn artifact_path(status: &str, label: &str) -> PathBuf {
    PathBuf::from(
        status
            .split_once(label)
            .expect("prepared workflow prompt")
            .1
            .split_once(". ")
            .expect("artifact path terminator")
            .0,
    )
}

#[derive(Clone)]
struct PlanCaptureProvider {
    requests: Arc<Mutex<Vec<Request>>>,
    spawned: Arc<Mutex<BTreeSet<String>>>,
    root: PathBuf,
    nested: bool,
}
impl PlanCaptureProvider {
    fn new(fixture: &Fixture, nested: bool) -> Self {
        Self {
            requests: Arc::new(Mutex::new(Vec::new())),
            spawned: Arc::new(Mutex::new(BTreeSet::new())),
            root: fixture.root.clone(),
            nested,
        }
    }
    fn stream(&self, request: Request) -> Result<Box<dyn EventStream>, Error> {
        let status = current_status(&request);
        let child = request
            .instructions
            .contains("Child agent name: plan-child\n");
        let grandchild = request
            .instructions
            .contains("Child agent name: plan-grandchild\n");
        let role = if grandchild {
            "grandchild"
        } else if child {
            "child"
        } else {
            "root"
        };
        let artifacts = if status.contains("Active profile: plan\n") {
            let plan = artifact_path(status, PLAN_LABEL);
            let tasks = artifact_path(status, TASK_LABEL);
            for path in [&plan, &tasks] {
                assert!(path.starts_with(&self.root));
                assert!(
                    path.is_file(),
                    "artifact must be prepared before requesting a tool: {}",
                    path.display()
                );
            }
            Some((plan, tasks))
        } else {
            None
        };
        let tool = if self.nested && !grandchild && self.spawned.lock().unwrap().insert(role.into())
        {
            Some((
                "agent_spawn",
                serde_json::json!({
                    "agent":"plan", "name":if child { "plan-grandchild" } else { "plan-child" },
                    "model":"local/test", "prompt":"Write the requested plan artifacts.", "fork":"empty"
                }),
            ))
        } else if let Some((plan, tasks)) = artifacts {
            if std::fs::read_to_string(&plan).unwrap().is_empty() {
                Some((
                    "write",
                    serde_json::json!({"path":plan,"content":"# Approved plan\nImplement the requested change."}),
                ))
            } else if std::fs::read_to_string(&tasks).unwrap().is_empty() {
                Some(("write", serde_json::json!({"path":tasks,"content":TASKS})))
            } else {
                None
            }
        } else {
            None
        };
        self.requests.lock().unwrap().push(request);
        let completion: Completion = reply(tool.as_ref().map(|(name, args)| (*name, args.clone())));
        Ok(Box::new(OneCompletion(Some(completion))))
    }
}
impl Provider for PlanCaptureProvider {
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
impl Session for PlanCaptureProvider {
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

fn agent_directory(fixture: &Fixture, host: &ProductionHost, id: &str) -> PathBuf {
    let sessions = host
        .service()
        .list_sessions(&wire::ListSessionsRequest::new())
        .unwrap();
    let summary = sessions
        .sessions
        .iter()
        .find(|session| session.user_session_id == id)
        .unwrap();
    fixture
        .roots
        .current()
        .state()
        .join("sessions")
        .join(id)
        .join("root-agents")
        .join(&summary.root_agent_name)
}
fn assert_plan_contract(request: &Request, directory: &std::path::Path) -> (PathBuf, PathBuf) {
    let status = current_status(request);
    assert!(status.contains("PLAN-PROFILE-MARKER"));
    assert!(status.contains("schema_version equal to 1 and a nonempty tasks array"));
    assert!(status.contains("Finish only after writing both canonical files."));
    assert!(status.contains(&format!(
        "under this plan directory and reference them from the canonical plan: {}.",
        directory.display()
    )));
    let plan = artifact_path(status, PLAN_LABEL);
    let tasks = artifact_path(status, TASK_LABEL);
    assert_eq!(plan.parent(), Some(directory));
    assert_eq!(tasks.parent(), Some(directory));
    let token = plan
        .file_name()
        .unwrap()
        .to_str()
        .unwrap()
        .strip_prefix("plan-")
        .unwrap()
        .strip_suffix(".md")
        .unwrap();
    assert_eq!(token.len(), 32);
    assert!(token.bytes().all(|byte| byte.is_ascii_hexdigit()));
    assert_eq!(
        tasks.file_name().unwrap().to_str().unwrap(),
        format!("plan-{token}.tasks.json")
    );
    (plan, tasks)
}
async fn send(host: &ProductionHost, id: &str) {
    bounded(host.service().send_message(
        &wire::SendMessageRequest {
            user_session_id: id.into(),
            text: "Inspect the request and finish the selected mode.".into(),
            delivery: wire::Delivery::DELIVERY_QUEUE.into(),
            ..Default::default()
        },
        Arc::new(NeverCancelled),
    ))
    .await
    .unwrap();
}

#[tokio::test]
async fn production_plan_requests_use_prepared_workflow_and_follow_live_mode_switches() {
    let mut fixture = Fixture::new();
    for (id, prompt) in [
        ("build", "BUILD-PROFILE-MARKER"),
        ("plan", "PLAN-PROFILE-MARKER"),
        ("query", "QUERY-PROFILE-MARKER"),
    ] {
        fixture
            .configuration
            .runtime
            .profiles
            .get_mut(id)
            .unwrap()
            .prompt = prompt.into();
    }
    let capture = PlanCaptureProvider::new(&fixture, false);
    let host = fixture.host_with(Arc::new(capture.clone()));
    let session = host
        .service()
        .create_session(
            &wire::CreateSessionRequest {
                model: "local/test".into(),
                mode: "build".into(),
                ..Default::default()
            },
            Arc::new(NeverCancelled),
        )
        .await
        .unwrap();
    let directory = agent_directory(&fixture, &host, &session.id).join("plan");
    let mut stream = host
        .service()
        .listen(
            &wire::ListenRequest {
                user_session_id: session.id.clone(),
                ..Default::default()
            },
            Arc::new(NeverCancelled),
        )
        .unwrap();
    let mut plan_completed = Vec::new();
    for mode in ["build", "plan", "query", "plan", "build"] {
        host.service()
            .update_session(&wire::UpdateSessionRequest {
                user_session_id: session.id.clone(),
                mode: mode.into(),
                ..Default::default()
            })
            .await
            .unwrap();
        let before = capture.requests.lock().unwrap().len();
        send(&host, &session.id).await;
        bounded(async {
            loop {
                let event = stream.next(&NeverCancelled).await.unwrap().unwrap();
                assert!(
                    !event.has_turn_failed(),
                    "production turn failed: {event:?}"
                );
                if event.has_plan_completed() {
                    plan_completed.push(event.plan_completed().clone());
                }
                if event.has_mode_turn_completed() {
                    break;
                }
            }
        })
        .await;
        let requests = capture.requests.lock().unwrap();
        let requests = &requests[before..];
        assert!(!requests.is_empty());
        for request in requests {
            let status = current_status(request);
            assert!(
                status.contains(&format!("Active profile: {mode}\n")),
                "{status}"
            );
            if mode == "plan" {
                assert_plan_contract(request, &directory);
            } else {
                assert!(status.contains(&format!("{}-PROFILE-MARKER", mode.to_ascii_uppercase())));
                assert!(!status.contains(PLAN_LABEL));
                assert!(!status.contains("PLAN-PROFILE-MARKER"));
            }
        }
    }
    assert_eq!(
        plan_completed.len(),
        2,
        "the prompt and completion must share one workflow"
    );
    assert_eq!(
        plan_completed[0].markdown,
        "# Approved plan\nImplement the requested change."
    );
    assert_eq!(plan_completed[0].task_declarations.len(), 1);
    let requests = capture.requests.lock().unwrap();
    let paths: BTreeSet<_> = requests
        .iter()
        .filter(|request| current_status(request).contains(PLAN_LABEL))
        .map(|request| assert_plan_contract(request, &directory))
        .collect();
    assert_eq!(
        paths.len(),
        1,
        "switching back to plan must retain its artifact pair"
    );
    drop(requests);
    drop(stream);
    bounded(host.close()).await.unwrap();
    std::fs::remove_dir_all(&fixture.root).unwrap();
}

#[tokio::test]
async fn production_nested_plan_requests_use_each_agents_own_prepared_scratch() {
    let mut fixture = Fixture::new();
    let plan = fixture
        .configuration
        .runtime
        .profiles
        .get_mut("plan")
        .unwrap();
    plan.prompt = "PLAN-PROFILE-MARKER".into();
    plan.is_agent_selectable = true;
    let capture = PlanCaptureProvider::new(&fixture, true);
    let host = fixture.host_with(Arc::new(capture.clone()));
    let session = host
        .service()
        .create_session(
            &wire::CreateSessionRequest {
                model: "local/test".into(),
                mode: "build".into(),
                ..Default::default()
            },
            Arc::new(NeverCancelled),
        )
        .await
        .unwrap();
    let directory = agent_directory(&fixture, &host, &session.id);
    let mut stream = host
        .service()
        .listen(
            &wire::ListenRequest {
                user_session_id: session.id.clone(),
                ..Default::default()
            },
            Arc::new(NeverCancelled),
        )
        .unwrap();
    send(&host, &session.id).await;
    await_turn(stream.as_mut()).await;
    drop(stream);
    bounded(host.close()).await.unwrap();
    let requests = capture.requests.lock().unwrap();
    for (name, expected) in [
        ("plan-child", directory.join("plan-child/plan")),
        (
            "plan-grandchild",
            directory.join("plan-child/plan-grandchild/plan"),
        ),
    ] {
        let request = requests
            .iter()
            .find(|request| {
                request
                    .instructions
                    .contains(&format!("Child agent name: {name}\n"))
            })
            .expect("real nested agent must call the provider");
        let (plan, tasks) = assert_plan_contract(request, &expected);
        assert_eq!(
            std::fs::read_to_string(plan).unwrap(),
            "# Approved plan\nImplement the requested change."
        );
        assert_eq!(std::fs::read_to_string(tasks).unwrap(), TASKS);
    }
    assert!(
        !directory.join("plan").exists(),
        "ordinary root mode must not prepare plan artifacts"
    );
    drop(requests);
    std::fs::remove_dir_all(&fixture.root).unwrap();
}
