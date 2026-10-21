#[path = "image_tests.rs"]
mod images;
#[path = "plan_tests.rs"]
mod plan;
#[path = "prompt_tests.rs"]
mod prompt_context;
#[path = "skill_tests.rs"]
mod skills;

use super::*;
use parrot_domain::protocol::messages::parrot as wire;
use parrot_provider::{
    catalogs::Model,
    sessions::{Error, EventStream, Provider, Request, Session},
};
use parrot_storage::events::NeverCancelled;
use std::{
    collections::{BTreeMap, HashMap},
    sync::atomic::AtomicU64,
};

mod explore_alias;
#[cfg(unix)]
mod workspace_identity;

static NEXT: AtomicU64 = AtomicU64::new(0);
#[derive(Default)]
struct NoCallProvider(AtomicU64);
impl NoCallProvider {
    fn models() -> Vec<Model> {
        let mut model = Model::new("test", "local");
        model.context_window = 32768;
        model.max_output_tokens = 1024;
        model.capabilities.tools = true;
        vec![model]
    }
}
impl Provider for NoCallProvider {
    fn id(&self) -> &str {
        "local"
    }
    fn call(&self, _: Request) -> Result<Box<dyn EventStream>, Error> {
        self.0.fetch_add(1, Ordering::SeqCst);
        Err(Error::InvalidOperation(
            "constructor test must not call provider".into(),
        ))
    }
    fn has_credential<'a>(&'a self, _: &'a dyn Cancellation) -> BoxFuture<'a, Result<bool, Error>> {
        Box::pin(async { Ok(true) })
    }
    fn seed_models(&self) -> Vec<Model> {
        Self::models()
    }
    fn list_models<'a>(
        &'a self,
        _: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<Vec<Model>, Error>> {
        Box::pin(async { Ok(Self::models()) })
    }
    fn open_session(&self) -> Result<Arc<dyn Session>, Error> {
        self.0.fetch_add(1, Ordering::SeqCst);
        Err(Error::InvalidOperation(
            "constructor test must not open provider session".into(),
        ))
    }
}
struct Fixture {
    root: PathBuf,
    roots: ApplicationRoots,
    configuration: Configuration,
    templates: Arc<PromptTemplateCatalog>,
    diagnostics: Arc<FileDiagnosticLog>,
}
impl Fixture {
    fn new() -> Self {
        let root = PathBuf::from(format!(
            "/tmp/parrot-prod-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(root.join("workspace")).unwrap();
        let values = BTreeMap::from([
            (
                "HOME".into(),
                root.join("home").to_string_lossy().into_owned(),
            ),
            (
                "XDG_STATE_HOME".into(),
                root.join("s").to_string_lossy().into_owned(),
            ),
            (
                "XDG_CONFIG_HOME".into(),
                root.join("c").to_string_lossy().into_owned(),
            ),
            (
                "XDG_DATA_HOME".into(),
                root.join("d").to_string_lossy().into_owned(),
            ),
        ]);
        let (configuration, templates) = PromptTemplateCatalog::from_content(
            "model: local/test\nskills:\n  enabled: false\n",
            &values,
        )
        .unwrap();
        let roots = ApplicationRoots::resolve(&values.into_iter().collect::<HashMap<_, _>>());
        let diagnostics = Arc::new(
            FileDiagnosticLog::open_global(root.join("logs"), "production-constructor", |_| {})
                .unwrap(),
        );
        Self {
            root,
            roots,
            configuration,
            templates: Arc::new(templates),
            diagnostics,
        }
    }
    fn host(&self) -> ProductionHost {
        self.host_with(Arc::new(NoCallProvider::default()))
    }
    fn host_with(&self, provider: Arc<dyn Provider>) -> ProductionHost {
        self.host_at(self.root.join("workspace"), provider)
    }
    fn host_at(
        &self,
        launch_directory: impl AsRef<Path>,
        provider: Arc<dyn Provider>,
    ) -> ProductionHost {
        ProductionHost::new(ProductionHostConfig {
            configuration: self.configuration.clone(),
            templates: self.templates.clone(),
            roots: self.roots.clone(),
            launch_directory: launch_directory.as_ref().to_owned(),
            providers: Arc::new(
                ProviderRegistry::new(
                    vec![provider],
                    BTreeMap::from([("local".into(), NoCallProvider::models())]),
                )
                .unwrap(),
            ),
            diagnostics: self.diagnostics.clone(),
            owner_runtime: tokio::runtime::Handle::current(),
        })
        .unwrap()
    }
}
async fn bounded<T>(future: impl std::future::Future<Output = T>) -> T {
    tokio::time::timeout(Duration::from_secs(15), future)
        .await
        .expect("production lifecycle did not settle")
}

#[tokio::test]
async fn real_production_host_creates_resumes_and_joins_local_transports() {
    let fixture = Fixture::new();
    let host = fixture.host();
    let mut client = bounded(host.open_local_client(&NeverCancelled))
        .await
        .unwrap();
    let session = bounded(client.create_session(wire::CreateSessionRequest {
        model: "local/test".into(),
        mode: "build".into(),
        interactive_permissions: true,
        ..Default::default()
    }))
    .await
    .unwrap()
    .into_inner();
    let socket = fixture
        .roots
        .current()
        .state()
        .join("sessions")
        .join(&session.id)
        .join("parrot.sock");
    assert!(socket.exists(), "real per-user listener was not created");
    let mut attached = GrpcTransportClient::connect(
        &TransportAddress::Unix(socket.clone()),
        None,
        None,
        fixture.diagnostics.clone(),
    )
    .unwrap();
    assert_eq!(
        bounded(
            attached.attach(
                wire::AttachSessionRequest {
                    user_session_id: session.id.clone(),
                    working_directory: fixture
                        .root
                        .join("workspace")
                        .to_string_lossy()
                        .into_owned(),
                    ..Default::default()
                },
                &NeverCancelled
            )
        )
        .await
        .unwrap()
        .id,
        session.id
    );
    attached.close();
    bounded(host.close()).await.unwrap();
    assert!(
        !socket.exists(),
        "per-user listener was not joined and removed"
    );
    assert!(host.open_local_client(&NeverCancelled).await.is_err());
    let resumed = fixture.host();
    let mut client = resumed.open_local_client(&NeverCancelled).await.unwrap();
    let result = bounded(
        client.resume_session(wire::ResumeSessionRequest {
            user_session_id: session.id.clone(),
            working_directory: fixture
                .root
                .join("workspace")
                .to_string_lossy()
                .into_owned(),
            ..Default::default()
        }),
    )
    .await
    .unwrap()
    .into_inner();
    assert_eq!(result.id, session.id);
    bounded(resumed.close()).await.unwrap();
    assert!(!socket.exists());
}

#[tokio::test]
async fn real_production_host_retries_resume_after_session_scratch_directory_is_repaired() {
    let fixture = Fixture::new();
    let provider = Arc::new(NoCallProvider::default());
    let host = fixture.host_with(provider.clone());
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
    bounded(host.close()).await.unwrap();

    let scratch = fixture
        .roots
        .current()
        .state()
        .join("sessions")
        .join(&session.id)
        .join("scratch");
    std::fs::remove_dir_all(&scratch).unwrap();
    std::fs::write(&scratch, "not a directory").unwrap();

    let resumed = fixture.host_with(provider.clone());
    let request = wire::ResumeSessionRequest {
        user_session_id: session.id.clone(),
        working_directory: fixture
            .root
            .join("workspace")
            .to_string_lossy()
            .into_owned(),
        ..Default::default()
    };
    assert!(
        bounded(
            resumed
                .service()
                .resume_session(&request, Arc::new(NeverCancelled))
        )
        .await
        .is_err()
    );
    std::fs::remove_file(&scratch).unwrap();
    std::fs::create_dir_all(&scratch).unwrap();
    let result = bounded(
        resumed
            .service()
            .resume_session(&request, Arc::new(NeverCancelled)),
    )
    .await
    .unwrap();
    assert_eq!(result.id, session.id);
    bounded(resumed.close()).await.unwrap();
    assert_eq!(provider.0.load(Ordering::SeqCst), 0);
    std::fs::remove_dir_all(&fixture.root).unwrap();
}

#[tokio::test]
async fn cancelled_local_open_is_effect_free_and_failed_model_still_closes() {
    let fixture = Fixture::new();
    let host = fixture.host();
    let cancelled = parrot_runtime::agent_turn::lifecycle::Signal::new();
    cancelled.cancel();
    assert_eq!(
        host.open_local_client(&cancelled).await.unwrap_err().code(),
        tonic::Code::Cancelled
    );
    assert!(!host.local_path.exists());
    let mut client = host.open_local_client(&NeverCancelled).await.unwrap();
    assert!(
        bounded(client.create_session(wire::CreateSessionRequest {
            model: "missing/model".into(),
            mode: "build".into(),
            ..Default::default()
        }))
        .await
        .is_err()
    );
    bounded(host.close()).await.unwrap();
    assert!(!host.local_path.exists());
}

#[derive(Clone)]
struct ScriptedProvider(
    Arc<Mutex<std::collections::VecDeque<parrot_provider::sessions::Completion>>>,
);
impl ScriptedProvider {
    fn stream(&self) -> Result<Box<dyn EventStream>, Error> {
        let completion = self
            .0
            .lock()
            .unwrap()
            .pop_front()
            .ok_or_else(|| Error::InvalidOperation("unexpected provider call".into()))?;
        Ok(Box::new(OneCompletion(Some(completion))))
    }
}
impl Provider for ScriptedProvider {
    fn id(&self) -> &str {
        "local"
    }
    fn call(&self, _: Request) -> Result<Box<dyn EventStream>, Error> {
        self.stream()
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
impl Session for ScriptedProvider {
    fn begin_turn(&self) -> BoxFuture<'_, Result<(), Error>> {
        Box::pin(async { Ok(()) })
    }
    fn call(&self, _: Request) -> Result<Box<dyn EventStream>, Error> {
        self.stream()
    }
    fn try_fall_back_to_http(&self) -> BoxFuture<'_, Result<bool, Error>> {
        Box::pin(async { Ok(false) })
    }
    fn close(&self) -> BoxFuture<'_, Result<(), Error>> {
        Box::pin(async { Ok(()) })
    }
}
struct OneCompletion(Option<parrot_provider::sessions::Completion>);
impl EventStream for OneCompletion {
    fn next<'a>(
        &'a mut self,
        cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<Option<parrot_provider::sessions::Event>, Error>> {
        Box::pin(async move {
            cancellation.check().map_err(|_| Error::Cancelled)?;
            Ok(self
                .0
                .take()
                .map(parrot_provider::sessions::Event::Completed))
        })
    }
    fn close(&mut self) -> BoxFuture<'_, Result<(), Error>> {
        self.0 = None;
        Box::pin(async { Ok(()) })
    }
}
fn reply(tool: Option<(&str, serde_json::Value)>) -> parrot_provider::sessions::Completion {
    parrot_provider::sessions::Completion {
        response_id: "production-test-response".into(),
        provider_id: "local".into(),
        finish_reason: if tool.is_some() { "tool_calls" } else { "stop" }.into(),
        assistant_text: if tool.is_some() { "" } else { "finished" }.into(),
        tool_calls: tool
            .into_iter()
            .map(|(name, args)| parrot_provider::sessions::ToolCall {
                id: Identifier::event_id().unwrap(),
                name: name.into(),
                arguments_json: args.to_string(),
            })
            .collect(),
        input_tokens: 30,
        output_tokens: 3,
        ..Default::default()
    }
}
async fn await_turn(stream: &mut dyn protocol::SessionEventStream) {
    bounded(async {
        loop {
            let event = stream.next(&NeverCancelled).await.unwrap().unwrap();
            assert!(
                !event.has_turn_failed(),
                "actual production turn failed: {event:?}"
            );
            if event.has_mode_turn_completed() {
                break;
            }
        }
    })
    .await;
}

#[tokio::test]
async fn actual_permission_broker_grant_refreshes_future_write_and_rejection_does_not() {
    for choice in ["grant", "reject"] {
        let mut fixture = Fixture::new();
        // The packaged profile explicitly grants /tmp. This fixture removes
        // those optional configured grants so its outside-workspace /tmp file
        // requires an actual approval rather than already being authorized.
        fixture.configuration.runtime.sandbox_rules.clear();
        let target = fixture.root.join("outside-workspace.txt");
        std::fs::write(&target, "before").unwrap();
        let scripts = Arc::new(Mutex::new(std::collections::VecDeque::from([
            reply(Some((
                "request_write_permission",
                serde_json::json!({"paths":[target],"reason":"production approval fixture"}),
            ))),
            reply(None),
            reply(Some((
                "write",
                serde_json::json!({"path":target,"content":"after"}),
            ))),
            reply(None),
        ])));
        let host = fixture.host_with(Arc::new(ScriptedProvider(scripts.clone())));
        let session = bounded(host.service().create_session(
            &wire::CreateSessionRequest {
                model: "local/test".into(),
                mode: "build".into(),
                interactive_permissions: true,
                ..Default::default()
            },
            Arc::new(NeverCancelled),
        ))
        .await
        .unwrap();
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
        bounded(host.service().send_message(
            &wire::SendMessageRequest {
                user_session_id: session.id.clone(),
                text: "request an outside write".into(),
                delivery: wire::Delivery::DELIVERY_QUEUE.into(),
                ..Default::default()
            },
            Arc::new(NeverCancelled),
        ))
        .await
        .unwrap();
        let request = bounded(async {
            loop {
                let event = stream.next(&NeverCancelled).await.unwrap().unwrap();
                assert!(
                    !event.has_mode_turn_completed(),
                    "completed before requesting permission"
                );
                assert!(
                    !event.has_turn_failed(),
                    "permission turn failed: {event:?}"
                );
                if event.has_permission_pending() {
                    break event.permission_pending().clone();
                }
            }
        })
        .await;
        let pending = host
            .service()
            .list_pending_permissions(&wire::ListPendingPermissionsRequest {
                user_session_id: session.id.clone(),
                ..Default::default()
            })
            .await
            .unwrap();
        assert_eq!(pending.permissions.len(), 1);
        assert_eq!(pending.permissions[0].id, request.id);
        host.service()
            .reply_permission(&wire::ReplyPermissionRequest {
                user_session_id: session.id.clone(),
                permission_request_id: request.id,
                choice_value: choice.into(),
                ..Default::default()
            })
            .await
            .unwrap();
        await_turn(stream.as_mut()).await;
        assert!(
            host.service()
                .list_pending_permissions(&wire::ListPendingPermissionsRequest {
                    user_session_id: session.id.clone(),
                    ..Default::default()
                })
                .await
                .unwrap()
                .permissions
                .is_empty()
        );
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "before");
        bounded(host.service().send_message(
            &wire::SendMessageRequest {
                user_session_id: session.id.clone(),
                text: "perform a new captured write".into(),
                delivery: wire::Delivery::DELIVERY_QUEUE.into(),
                ..Default::default()
            },
            Arc::new(NeverCancelled),
        ))
        .await
        .unwrap();
        await_turn(stream.as_mut()).await;
        assert_eq!(
            std::fs::read_to_string(&target).unwrap(),
            if choice == "grant" { "after" } else { "before" }
        );
        assert!(scripts.lock().unwrap().is_empty());
        drop(stream);
        bounded(host.close()).await.unwrap();
    }
}
