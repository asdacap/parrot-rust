//! Component decision characterization over real claim files and native gRPC.
//! The RPC endpoint/identity inspector are controlled source-like fixtures, not
//! production composition/runtime/provider construction proof. Import as `mod
//! startup;` from tests/dispatch/main.rs after shared registration.
#![cfg(unix)]

use parrot_cli::dispatch::startup::{
    ConfigureFresh, LocalChatStartup, OpenLocalClient, StartupError,
};
use parrot_domain::{
    contracts::{
        async_ops::{BoxFuture, Cancellation},
        paths::{RootOrigin, SelectedSessionRoot},
    },
    protocol::{bindings::parrot::parrot_server::Parrot, messages::parrot as wire},
    values::identifiers::UserSessionId,
};
use parrot_foundation::paths::ApplicationRoots;
use parrot_host::{
    grpc::{
        address::TransportAddress,
        transport::{GrpcServer, GrpcTransportClient},
    },
    web::router::{WebClient, WebSessionRouter, WebStartup},
};
use parrot_storage::{
    diagnostics::FileDiagnosticLog,
    history_files::{SessionIndex, SessionMeta},
    ownership::{
        ProcessIdentity, ProcessIdentityStatus, RuntimeIdentity, SessionActivationLease,
        WorkingDirectoryClaim,
    },
};
use std::{
    collections::HashMap,
    future::Future,
    path::PathBuf,
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};
use tokio::sync::{Notify, watch};
use tonic::{Code, Request, Response, Status};

static NEXT: AtomicUsize = AtomicUsize::new(0);

struct Stop(watch::Sender<bool>);
impl Stop {
    fn new() -> Self {
        Self(watch::channel(false).0)
    }
    fn cancel(&self) {
        self.0.send_replace(true);
    }
}
impl Cancellation for Stop {
    fn is_cancelled(&self) -> bool {
        *self.0.borrow()
    }
    fn cancelled(&self) -> BoxFuture<'_, ()> {
        Box::pin(async move {
            let mut receiver = self.0.subscribe();
            loop {
                if *receiver.borrow_and_update() {
                    return;
                }
                if receiver.changed().await.is_err() {
                    return;
                }
            }
        })
    }
}
async fn bounded<T>(future: impl Future<Output = T>) -> T {
    tokio::time::timeout(Duration::from_secs(12), future)
        .await
        .expect("bounded startup component fixture")
}

struct Context {
    root: PathBuf,
    roots: ApplicationRoots,
    runtime: RuntimeIdentity,
    diagnostics: Arc<FileDiagnosticLog>,
    progress: Arc<Mutex<Vec<u8>>>,
    opens: Arc<AtomicUsize>,
    configurations: Arc<AtomicUsize>,
}
impl Context {
    fn new() -> Self {
        let root = PathBuf::from(format!(
            "/tmp/t65-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::SeqCst)
        ));
        std::fs::create_dir_all(&root).unwrap();
        let roots = ApplicationRoots::resolve(&HashMap::from([
            ("HOME".into(), root.to_string_lossy().into_owned()),
            (
                "XDG_STATE_HOME".into(),
                root.join("s").to_string_lossy().into_owned(),
            ),
        ]));
        // Controlled incarnation semantics for source-like owner/race cases.
        // This is not native process identity or T36 security evidence.
        let runtime = RuntimeIdentity::new(
            "host".into(),
            "boot".into(),
            41,
            "fixture-start".into(),
            "fixture-runtime".into(),
            Arc::new(|_| ProcessIdentity {
                status: ProcessIdentityStatus::Found,
                process_start_token: Some("fixture-start".into()),
            }),
        )
        .with_proven_comparable_process_start_tokens();
        let diagnostics =
            Arc::new(FileDiagnosticLog::open_global(root.join("logs"), "startup", |_| {}).unwrap());
        Self {
            root,
            roots,
            runtime,
            diagnostics,
            progress: Arc::default(),
            opens: Arc::default(),
            configurations: Arc::default(),
        }
    }
    fn claim(&self) -> WorkingDirectoryClaim {
        WorkingDirectoryClaim::new(self.roots.current().state(), self.runtime.clone())
    }
    fn admit(&self, foreign: bool) -> Arc<SessionActivationLease> {
        let mut runtime = self.runtime.clone();
        if foreign {
            runtime.host_identity = "foreign".into();
        }
        let id = UserSessionId::parse("existing").unwrap();
        let admission = WorkingDirectoryClaim::new(self.roots.current().state(), runtime)
            .create_fresh(&self.root, &id)
            .unwrap();
        let index = SessionIndex::new(
            self.roots.select_new(&id, &self.root).unwrap(),
            id,
            &self.root,
        )
        .unwrap();
        let mut metadata = SessionMeta::new(
            "existing",
            self.root.to_string_lossy(),
            "stored-provider",
            "stored-model",
        );
        metadata.mode = Some("stored-mode".into());
        index.publish(&metadata).unwrap();
        Arc::new(admission.activation_lease.expect("real claim activation"))
    }
    fn admit_legacy(&self) -> Arc<SessionActivationLease> {
        let id = UserSessionId::parse("existing").unwrap();
        let admission =
            WorkingDirectoryClaim::new(self.roots.legacy().state(), self.runtime.clone())
                .create_fresh(&self.root, &id)
                .unwrap();
        let directory = self.roots.legacy().state().join("sessions/existing");
        std::fs::create_dir_all(&directory).unwrap();
        let selected = SelectedSessionRoot::new(
            std::fs::canonicalize(directory).unwrap(),
            RootOrigin::Legacy,
        )
        .unwrap();
        let index = SessionIndex::new(selected, id, &self.root).unwrap();
        let mut metadata = SessionMeta::new(
            "existing",
            self.root.to_string_lossy(),
            "stored-provider",
            "stored-model",
        );
        metadata.mode = Some("stored-mode".into());
        index.publish(&metadata).unwrap();
        Arc::new(admission.activation_lease.expect("real legacy activation"))
    }
    fn owner_socket(&self) -> PathBuf {
        self.roots
            .current()
            .state()
            .join("sessions/existing/parrot.sock")
    }
    fn startup(&self, client: WebClient, take_over: bool) -> LocalChatStartup {
        let opens = self.opens.clone();
        let open: OpenLocalClient = Arc::new(move |cancellation| {
            let client = client.clone();
            let opens = opens.clone();
            Box::pin(async move {
                cancellation
                    .check()
                    .map_err(|_| Status::cancelled("fixture stopped"))?;
                opens.fetch_add(1, Ordering::SeqCst);
                Ok(client)
            })
        });
        let configurations = self.configurations.clone();
        let progress = self.progress.clone();
        let configure: ConfigureFresh = Arc::new(move |_, cancellation| {
            let configurations = configurations.clone();
            let progress = progress.clone();
            Box::pin(async move {
                cancellation
                    .check()
                    .map_err(|_| Status::cancelled("fixture stopped"))?;
                assert!(
                    String::from_utf8(progress.lock().unwrap().clone())
                        .unwrap()
                        .contains("creating a new user session...")
                );
                configurations.fetch_add(1, Ordering::SeqCst);
                Ok(wire::CreateSessionRequest {
                    model: "fresh-model".into(),
                    mode: "fresh-mode".into(),
                    // Startup must override this source request setting.
                    interactive_permissions: false,
                    ..Default::default()
                })
            })
        });
        LocalChatStartup::from_working_directory(
            self.roots.clone(),
            &self.root,
            self.runtime.clone(),
            self.progress.clone(),
            self.diagnostics.clone(),
            open,
            configure,
            take_over,
        )
        .unwrap()
    }
    fn text(&self) -> String {
        String::from_utf8(self.progress.lock().unwrap().clone()).unwrap()
    }
    fn log(&self) -> String {
        std::fs::read_to_string(self.diagnostics.path()).unwrap()
    }
    async fn server(&self, endpoint: Endpoint, owner: bool) -> GrpcServer {
        GrpcServer::start_local(
            endpoint,
            if owner {
                self.owner_socket()
            } else {
                self.root.join("local.sock")
            },
            self.diagnostics.clone(),
        )
        .await
        .unwrap()
    }
    fn client(&self, server: &GrpcServer) -> GrpcTransportClient {
        GrpcTransportClient::connect(server.address(), None, None, self.diagnostics.clone())
            .unwrap()
    }
    fn assert_diagnostics(&self, outcome: &str) {
        let log = self.log();
        let start = log
            .lines()
            .find(|line| line.contains("event=\"local.open.start\""))
            .unwrap();
        let complete = log
            .lines()
            .find(|line| line.contains("event=\"local.open.complete\""))
            .unwrap();
        let correlation = |line: &str| {
            line.split_whitespace()
                .find(|field| field.starts_with("correlation="))
                .unwrap()
                .to_owned()
        };
        assert_eq!(correlation(start), correlation(complete));
        assert!(complete.contains("duration_ms="));
        assert!(complete.contains(&format!("outcome=\"{outcome}\"")));
        for private in [
            self.root.to_str().unwrap(),
            "fresh-model",
            "stored-model",
            "Attachment rejected.",
            "Stored settings are unavailable.",
        ] {
            assert!(!log.contains(private), "diagnostic payload leak: {private}");
        }
    }
}
impl Drop for Context {
    fn drop(&mut self) {
        std::fs::remove_dir_all(&self.root).unwrap();
    }
}

#[derive(Default)]
struct Calls {
    attached: Vec<wire::AttachSessionRequest>,
    resumed: Vec<wire::ResumeSessionRequest>,
    created: Vec<wire::CreateSessionRequest>,
}
#[derive(Clone)]
struct Endpoint {
    scenario: &'static str,
    claim: WorkingDirectoryClaim,
    directory: PathBuf,
    calls: Arc<Mutex<Calls>>,
    owner: Option<Arc<SessionActivationLease>>,
    attach_started: Arc<Notify>,
    resume_started: Arc<Notify>,
    retry_started: Arc<Notify>,
}
impl Endpoint {
    fn new(context: &Context, scenario: &'static str) -> Self {
        Self {
            scenario,
            claim: context.claim(),
            directory: context.root.clone(),
            calls: Arc::default(),
            owner: None,
            attach_started: Arc::default(),
            resume_started: Arc::default(),
            retry_started: Arc::default(),
        }
    }
    async fn create(
        &self,
        request: wire::CreateSessionRequest,
    ) -> Result<wire::UserSession, Status> {
        self.calls.lock().unwrap().created.push(request.clone());
        Ok(wire::UserSession {
            id: "new-session".into(),
            model: request.model,
            mode: request.mode,
            ..Default::default()
        })
    }
    async fn resume(
        &self,
        request: wire::ResumeSessionRequest,
    ) -> Result<wire::UserSession, Status> {
        self.calls.lock().unwrap().resumed.push(request.clone());
        self.resume_started.notify_one();
        match self.scenario {
            "cancel-recovery" => return std::future::pending().await,
            "fail-recovery" => {
                return Err(Status::failed_precondition(
                    "Stored settings are unavailable.",
                ));
            }
            "rpc-cancel-recovery" => return Err(Status::cancelled("source RPC cancellation")),
            "race" | "recover-race" | "cancel-retry" => {
                return Err(Status::already_exists("Another runtime owns the session."));
            }
            _ => {}
        }
        let admission = self
            .claim
            .resume(
                &self.directory,
                &UserSessionId::parse(&request.user_session_id).unwrap(),
                request.take_over,
            )
            .unwrap();
        let _activation = admission
            .activation_lease
            .ok_or_else(|| Status::already_exists("Another runtime owns the session."))?;
        Ok(stored(&request.user_session_id))
    }
    async fn attach(
        &self,
        request: wire::AttachSessionRequest,
    ) -> Result<wire::UserSession, Status> {
        let count = {
            let mut calls = self.calls.lock().unwrap();
            calls.attached.push(request.clone());
            calls.attached.len()
        };
        self.attach_started.notify_one();
        match self.scenario {
            "recover" => {
                self.owner.as_ref().unwrap().release().unwrap();
                return Err(Status::permission_denied("The owner stopped."));
            }
            "race" | "recover-race" | "cancel-retry" if count == 1 => {
                return Err(Status::permission_denied("Attachment is not admitted yet."));
            }
            "cancel-retry" => {
                self.retry_started.notify_one();
                return std::future::pending().await;
            }
            "cancel" => return std::future::pending().await,
            "rpc-cancel-attach" => return Err(Status::cancelled("Attachment cancelled.")),
            "rejected" | "cancel-recovery" | "fail-recovery" | "rpc-cancel-recovery" => {
                return Err(Status::permission_denied("Attachment rejected."));
            }
            "wrong-id" => return Ok(stored("different")),
            _ => {}
        }
        Ok(stored(&request.user_session_id))
    }
}
fn stored(id: &str) -> wire::UserSession {
    wire::UserSession {
        id: id.into(),
        model: "stored-model".into(),
        mode: "stored-mode".into(),
        loaded: true,
        ..Default::default()
    }
}

// Only the three source startup RPCs have behavior. The unchanged native server
// still transports the generated contract; unused methods fail explicitly.
macro_rules! endpoint {
    ($($method:ident($request:ty) -> $response:ty),* $(,)?) => {
        #[tonic::async_trait]
        impl Parrot for Endpoint {
            $(async fn $method(&self, _: Request<$request>) -> Result<Response<$response>, Status> {
                Err(Status::unimplemented("unused startup fixture operation"))
            })*
            async fn create_session(&self, request: Request<wire::CreateSessionRequest>) -> Result<Response<wire::UserSession>, Status> {
                self.create(request.into_inner()).await.map(Response::new)
            }
            async fn resume_session(&self, request: Request<wire::ResumeSessionRequest>) -> Result<Response<wire::UserSession>, Status> {
                self.resume(request.into_inner()).await.map(Response::new)
            }
            async fn attach_session(&self, request: Request<wire::AttachSessionRequest>) -> Result<Response<wire::UserSession>, Status> {
                self.attach(request.into_inner()).await.map(Response::new)
            }
            type ListenStream = tokio_stream::Empty<Result<wire::Event, Status>>;
            async fn listen(&self, _: Request<wire::ListenRequest>) -> Result<Response<Self::ListenStream>, Status> {
                Err(Status::unimplemented("unused startup fixture operation"))
            }
        }
    };
}
endpoint! {
    list_models(wire::ListModelsRequest) -> wire::ListModelsResponse,
    list_model_aliases(wire::ListModelAliasesRequest) -> wire::ListModelAliasesResponse,
    configure_model_alias(wire::ConfigureModelAliasRequest) -> wire::ConfigureModelAliasResponse,
    list_provider_model_alias_defaults(wire::ListProviderModelAliasDefaultsRequest) -> wire::ListProviderModelAliasDefaultsResponse,
    apply_provider_model_alias_defaults(wire::ApplyProviderModelAliasDefaultsRequest) -> wire::ApplyProviderModelAliasDefaultsResponse,
    set_model_preset(wire::SetModelPresetRequest) -> wire::SetModelPresetResponse,
    select_model_preset(wire::SelectModelPresetRequest) -> wire::SelectModelPresetResponse,
    list_model_presets(wire::ListModelPresetsRequest) -> wire::ListModelPresetsResponse,
    list_modes(wire::ListModesRequest) -> wire::ListModesResponse,
    list_skills(wire::ListSkillsRequest) -> wire::ListSkillsResponse,
    configure_skill(wire::ConfigureSkillRequest) -> wire::ConfigureSkillResponse,
    list_sessions(wire::ListSessionsRequest) -> wire::ListSessionsResponse,
    update_session(wire::UpdateSessionRequest) -> wire::UserSession,
    set_goal(wire::SetGoalRequest) -> wire::SetGoalResponse,
    set_standing_instruction(wire::SetStandingInstructionRequest) -> wire::SetStandingInstructionResponse,
    send_message(wire::SendMessageRequest) -> wire::SendMessageResponse,
    upload_attachment(tonic::Streaming<wire::AttachmentUploadFrame>) -> wire::AttachmentUploadResponse,
    interrupt(wire::InterruptRequest) -> wire::InterruptResponse,
    compact(wire::CompactRequest) -> wire::CompactResponse,
    set_context_limit(wire::SetContextLimitRequest) -> wire::SetContextLimitResponse,
    sandbox_enable(wire::SandboxEnableRequest) -> wire::SandboxEnableResponse,
    session_status(wire::SessionStatusRequest) -> wire::SessionStatusResponse,
    list_pending_questions(wire::ListPendingQuestionsRequest) -> wire::ListPendingQuestionsResponse,
    reply_question(wire::ReplyQuestionRequest) -> wire::ReplyQuestionResponse,
    reject_question(wire::RejectQuestionRequest) -> wire::RejectQuestionResponse,
    list_pending_permissions(wire::ListPendingPermissionsRequest) -> wire::ListPendingPermissionsResponse,
    reply_permission(wire::ReplyPermissionRequest) -> wire::ReplyPermissionResponse,
}

async fn run_case(scenario: &'static str, interactive: bool, expected_opens: usize, fresh: usize) {
    let context = Context::new();
    let mut endpoint = Endpoint::new(&context, scenario);
    let activation = (scenario != "fresh").then(|| context.admit(scenario.starts_with("foreign")));
    if matches!(scenario, "load" | "inactive-connect" | "recover-race") {
        activation.as_ref().unwrap().release().unwrap();
    }
    endpoint.owner = activation;
    let mut local_server = context.server(endpoint.clone(), false).await;
    let mut local_owner = context.client(&local_server);
    let local_client = local_owner.client().unwrap().clone();
    let mut owner_server = if matches!(
        scenario,
        "connect" | "inactive-connect" | "rejected" | "race" | "recover" | "recover-race"
    ) {
        Some(context.server(endpoint.clone(), true).await)
    } else {
        None
    };
    let take_over = scenario == "foreign-take-over";
    let startup = context.startup(local_client, take_over);
    assert_eq!(startup.workspace().launch_directory(), context.root);
    assert_eq!(startup.workspace().physical_identity(), context.root);
    let stop = Stop::new();
    let opened = bounded(startup.open(interactive, &stop)).await.unwrap();
    assert_eq!(context.opens.load(Ordering::SeqCst), expected_opens);
    assert_eq!(context.configurations.load(Ordering::SeqCst), fresh);
    let resumes = usize::from(!matches!(
        scenario,
        "fresh" | "connect" | "inactive-connect"
    ));
    let attachments = match scenario {
        "fresh" | "load" | "missing" | "foreign" | "foreign-take-over" => 0,
        "race" | "recover-race" | "rejected" => 2,
        _ => 1,
    };
    {
        let calls = endpoint.calls.lock().unwrap();
        assert_eq!(calls.created.len(), fresh);
        assert_eq!(calls.resumed.len(), resumes);
        assert_eq!(calls.attached.len(), attachments);
        if fresh != 0 {
            assert_eq!(calls.created[0].interactive_permissions, interactive);
            assert_eq!(calls.created[0].model, "fresh-model");
            assert_eq!(calls.created[0].mode, "fresh-mode");
        }
        for resumed in &calls.resumed {
            assert_eq!(resumed.user_session_id, "existing");
            assert_eq!(resumed.working_directory, context.root.to_str().unwrap());
            assert_eq!(resumed.interactive_permissions, interactive);
            assert_eq!(resumed.take_over, take_over);
        }
        for attached in &calls.attached {
            assert_eq!(attached.user_session_id, "existing");
            assert_eq!(attached.working_directory, context.root.to_str().unwrap());
        }
    }
    assert_eq!(
        opened.session.id,
        if fresh == 1 {
            "new-session"
        } else {
            "existing"
        }
    );
    assert_eq!(
        opened.session.model,
        if fresh == 1 {
            "fresh-model"
        } else {
            "stored-model"
        }
    );
    assert_eq!(
        opened.session.mode,
        if fresh == 1 {
            "fresh-mode"
        } else {
            "stored-mode"
        }
    );
    assert_eq!(
        opened.remote.is_some(),
        fresh == 0 && !matches!(scenario, "load" | "recover" | "foreign-take-over")
    );
    let text = context.text();
    assert!(text.contains("looking for the workspace's latest user session..."));
    assert_eq!(text.contains("creating a new user session..."), fresh == 1);
    assert_eq!(
        text.contains("created user session new-session"),
        fresh == 1
    );
    assert_eq!(
        text.contains("restoring existing user session existing..."),
        resumes != 0
    );
    assert_eq!(
        text.contains("connecting to existing user session"),
        !matches!(scenario, "fresh" | "foreign" | "foreign-take-over")
    );
    if scenario != "fresh" {
        let expected = match scenario {
            "load" | "recover" | "foreign-take-over" => "loaded existing user session existing",
            "foreign" => "run `parrot chat --take-over` to reclaim it; creating a new user session",
            "connect" | "inactive-connect" | "race" | "recover-race" => {
                "connected to existing user session existing"
            }
            _ => "unable to connect to existing user session existing",
        };
        assert!(text.contains(expected), "{scenario}: {text}");
    }
    context.assert_diagnostics("success");
    drop(opened);
    if let Some(server) = owner_server.as_mut() {
        bounded(server.close()).await.unwrap();
    }
    local_owner.close();
    bounded(local_server.close()).await.unwrap();
}
macro_rules! scenario_tests {
    ($($name:ident: $scenario:literal, $interactive:literal, $opens:literal, $fresh:literal);* $(;)?) => {
        $(#[tokio::test]
        async fn $name() {
            run_case($scenario, $interactive, $opens, $fresh).await;
        })*
    };
}
scenario_tests! {
    fresh: "fresh", true, 1, 1;
    load: "load", false, 1, 0;
    connect: "connect", true, 0, 0;
    missing: "missing", false, 1, 1;
    rejected: "rejected", true, 1, 1;
    inactive_connect: "inactive-connect", true, 0, 0;
    race: "race", true, 1, 0;
    recover: "recover", false, 1, 0;
    recover_race: "recover-race", true, 1, 0;
    foreign: "foreign", true, 1, 1;
    foreign_take_over: "foreign-take-over", false, 1, 0;
}

async fn cancellation_case(during_attach: bool) {
    let context = Context::new();
    let _activation = context.admit(false);
    let endpoint = Endpoint::new(&context, "cancel");
    let mut server = context.server(endpoint.clone(), true).await;
    let mut local_owner = context.client(&server);
    let startup = context.startup(local_owner.client().unwrap().clone(), false);
    let stop = Stop::new();
    if !during_attach {
        stop.cancel();
    }
    let (result, ()) = tokio::join!(bounded(startup.open(true, &stop)), async {
        if during_attach {
            bounded(endpoint.attach_started.notified()).await;
            assert!(
                context
                    .text()
                    .contains("connecting to existing user session existing...")
            );
            stop.cancel();
        }
    });
    assert!(matches!(result, Err(StartupError::Rpc(status)) if status.code() == Code::Cancelled));
    assert_eq!(context.opens.load(Ordering::SeqCst), 0);
    assert_eq!(context.configurations.load(Ordering::SeqCst), 0);
    assert!(endpoint.calls.lock().unwrap().created.is_empty());
    assert!(endpoint.calls.lock().unwrap().resumed.is_empty());
    assert_eq!(context.text().is_empty(), !during_attach);
    assert!(!context.text().contains("connected to existing"));
    context.assert_diagnostics("cancelled");
    local_owner.close();
    bounded(server.close()).await.unwrap();
}
#[tokio::test]
async fn cancellation_before_start_keeps_local_providers_lazy() {
    cancellation_case(false).await;
}
#[tokio::test]
async fn cancellation_during_native_attach_has_no_fallback() {
    cancellation_case(true).await;
}

async fn recovery_failure_case(scenario: &'static str, expected: Code) {
    let context = Context::new();
    let _activation = context.admit(false);
    let endpoint = Endpoint::new(&context, scenario);
    let mut local_server = context.server(endpoint.clone(), false).await;
    let mut owner_server = context.server(endpoint.clone(), true).await;
    let mut local_owner = context.client(&local_server);
    let startup = context.startup(local_owner.client().unwrap().clone(), false);
    let stop = Stop::new();
    let (result, ()) = tokio::join!(bounded(startup.open(true, &stop)), async {
        bounded(endpoint.resume_started.notified()).await;
        if scenario == "cancel-retry" {
            bounded(endpoint.retry_started.notified()).await;
        }
        if matches!(scenario, "cancel-recovery" | "cancel-retry") {
            stop.cancel();
        }
    });
    assert!(matches!(result, Err(StartupError::Rpc(status)) if status.code() == expected));
    assert_eq!(context.opens.load(Ordering::SeqCst), 1);
    assert_eq!(context.configurations.load(Ordering::SeqCst), 0);
    {
        let calls = endpoint.calls.lock().unwrap();
        assert!(calls.created.is_empty());
        assert_eq!(calls.resumed.len(), 1);
        assert_eq!(calls.resumed[0].user_session_id, "existing");
        assert_eq!(
            calls.resumed[0].working_directory,
            context.root.to_str().unwrap()
        );
        assert!(calls.resumed[0].interactive_permissions);
        assert_eq!(
            calls.attached.len(),
            if scenario == "cancel-retry" { 2 } else { 1 }
        );
    }
    for absent in [
        "loaded existing",
        "creating a new user session",
        "connected to existing",
    ] {
        assert!(!context.text().contains(absent));
    }
    context.assert_diagnostics(if expected == Code::Cancelled {
        "cancelled"
    } else {
        "failure"
    });
    local_owner.close();
    bounded(owner_server.close()).await.unwrap();
    bounded(local_server.close()).await.unwrap();
}
#[tokio::test]
async fn recovery_failed_precondition_has_no_fresh_fallback() {
    recovery_failure_case("fail-recovery", Code::FailedPrecondition).await;
}
#[tokio::test]
async fn recovery_cancellation_has_no_fresh_fallback() {
    recovery_failure_case("cancel-recovery", Code::Cancelled).await;
}
#[tokio::test]
async fn retry_attach_cancellation_has_no_fresh_fallback() {
    recovery_failure_case("cancel-retry", Code::Cancelled).await;
}
#[tokio::test]
async fn explicit_rpc_cancelled_resume_is_not_swallowed() {
    recovery_failure_case("rpc-cancel-recovery", Code::Cancelled).await;
}

#[tokio::test]
async fn explicit_rpc_cancelled_attach_with_live_caller_retries_then_creates_fresh() {
    let context = Context::new();
    let _activation = context.admit(false);
    let endpoint = Endpoint::new(&context, "rpc-cancel-attach");
    let mut local_server = context.server(endpoint.clone(), false).await;
    let mut owner_server = context.server(endpoint.clone(), true).await;
    let mut local = context.client(&local_server);
    let startup = context.startup(local.client().unwrap().clone(), false);
    let caller = Stop::new();
    let opened = bounded(startup.open(true, &caller)).await.unwrap();
    assert!(!caller.is_cancelled());
    assert_eq!(opened.session.id, "new-session");
    assert!(opened.remote.is_none());
    assert_eq!(context.opens.load(Ordering::SeqCst), 1);
    assert_eq!(context.configurations.load(Ordering::SeqCst), 1);
    {
        let calls = endpoint.calls.lock().unwrap();
        assert_eq!(calls.attached.len(), 2);
        assert_eq!(calls.resumed.len(), 1);
        assert_eq!(calls.created.len(), 1);
        assert_eq!(calls.resumed[0].user_session_id, "existing");
        assert_eq!(
            calls.resumed[0].working_directory,
            context.root.to_str().unwrap()
        );
        assert!(calls.resumed[0].interactive_permissions);
        assert!(!calls.resumed[0].take_over);
        assert_eq!(calls.created[0].model, "fresh-model");
        assert_eq!(calls.created[0].mode, "fresh-mode");
        assert!(calls.created[0].interactive_permissions);
    }
    let progress = context.text();
    assert!(progress.contains("restoring existing user session existing..."));
    assert!(!progress.contains("loaded existing"));
    assert!(!progress.contains("connected to existing"));
    assert!(progress.contains("unable to connect to existing user session existing"));
    assert!(progress.contains("Attachment cancelled."));
    assert!(progress.contains("created user session new-session"));
    context.assert_diagnostics("success");
    assert!(!context.log().contains("Attachment cancelled."));
    drop(opened);
    local.close();
    bounded(owner_server.close()).await.unwrap();
    bounded(local_server.close()).await.unwrap();
}

#[tokio::test]
async fn corrupt_claim_hard_fails_before_lazy_local_open() {
    let context = Context::new();
    let _activation = context.admit(false);
    let canonical = WorkingDirectoryClaim::canonicalize(&context.root).unwrap();
    let association = context
        .roots
        .current()
        .state()
        .join("owners/workspaces")
        .join(WorkingDirectoryClaim::fingerprint(&canonical))
        .join("existing.json");
    std::fs::write(association, "{").unwrap();
    let endpoint = Endpoint::new(&context, "fresh");
    let mut server = context.server(endpoint.clone(), false).await;
    let mut owner = context.client(&server);
    let startup = context.startup(owner.client().unwrap().clone(), false);
    assert!(matches!(
        startup.open(true, &Stop::new()).await,
        Err(StartupError::CorruptState)
    ));
    assert_eq!(context.opens.load(Ordering::SeqCst), 0);
    assert_eq!(context.configurations.load(Ordering::SeqCst), 0);
    assert!(endpoint.calls.lock().unwrap().created.is_empty());
    context.assert_diagnostics("failure");
    owner.close();
    bounded(server.close()).await.unwrap();
}

#[tokio::test]
async fn native_identity_mismatch_is_not_accepted_as_attached() {
    let context = Context::new();
    let activation = context.admit(false);
    activation.release().unwrap();
    let endpoint = Endpoint::new(&context, "wrong-id");
    let mut server = context.server(endpoint.clone(), true).await;
    let mut local = context.client(&server);
    let startup = context.startup(local.client().unwrap().clone(), false);
    let opened = bounded(startup.open(false, &Stop::new())).await.unwrap();
    assert_eq!(opened.session.id, "existing");
    assert!(opened.remote.is_none());
    assert_eq!(endpoint.calls.lock().unwrap().attached.len(), 1);
    assert_eq!(endpoint.calls.lock().unwrap().resumed.len(), 1);
    assert_eq!(context.configurations.load(Ordering::SeqCst), 0);
    drop(opened);
    local.close();
    bounded(server.close()).await.unwrap();
}

#[tokio::test]
async fn web_port_uses_same_startup_and_forces_interactive_permissions() {
    let context = Context::new();
    let endpoint = Endpoint::new(&context, "fresh");
    let mut server = context.server(endpoint.clone(), false).await;
    let mut local = context.client(&server);
    let startup = context.startup(local.client().unwrap().clone(), false);
    let opened = bounded(WebStartup::open(&startup, &Stop::new()))
        .await
        .unwrap();
    assert_eq!(opened.session.id, "new-session");
    assert!(opened.remote.is_none());
    assert!(endpoint.calls.lock().unwrap().created[0].interactive_permissions);
    assert_eq!(context.opens.load(Ordering::SeqCst), 1);
    assert_eq!(context.configurations.load(Ordering::SeqCst), 1);
    context.assert_diagnostics("success");
    local.close();
    bounded(server.close()).await.unwrap();
}

// Ordinary root-selection adaptation beyond the single-StatePaths .NET source:
// these are not source parity or physical-grant/security predicates.
#[tokio::test]
async fn legacy_only_candidate_attaches_to_legacy_socket_in_place() {
    let context = Context::new();
    let _activation = context.admit_legacy();
    let endpoint = Endpoint::new(&context, "connect");
    let selected = context
        .roots
        .discover_session(
            &UserSessionId::parse("existing").unwrap(),
            true,
            &context.root,
        )
        .unwrap()
        .unwrap();
    assert_eq!(selected.origin(), RootOrigin::Legacy);
    let mut server = GrpcServer::start_local(
        endpoint.clone(),
        selected.resolve_internal("parrot.sock").unwrap().path(),
        context.diagnostics.clone(),
    )
    .await
    .unwrap();
    let mut local = context.client(&server);
    let startup = context.startup(local.client().unwrap().clone(), false);
    let opened = bounded(startup.open(false, &Stop::new())).await.unwrap();
    assert_eq!(opened.session.id, "existing");
    assert!(opened.remote.is_some());
    assert_eq!(endpoint.calls.lock().unwrap().attached.len(), 1);
    assert!(endpoint.calls.lock().unwrap().resumed.is_empty());
    assert_eq!(context.opens.load(Ordering::SeqCst), 0);
    assert_eq!(context.configurations.load(Ordering::SeqCst), 0);
    assert!(
        !context
            .roots
            .current()
            .state()
            .join("sessions/existing")
            .exists()
    );
    context.assert_diagnostics("success");
    drop(opened);
    local.close();
    bounded(server.close()).await.unwrap();
}

#[tokio::test]
async fn disabled_fallback_does_not_discover_or_attach_legacy_candidate() {
    let context = Context::new();
    let _activation = context.admit_legacy();
    let legacy = Endpoint::new(&context, "connect");
    let mut legacy_server = GrpcServer::start_local(
        legacy.clone(),
        context
            .roots
            .legacy()
            .state()
            .join("sessions/existing/parrot.sock"),
        context.diagnostics.clone(),
    )
    .await
    .unwrap();
    let fresh = Endpoint::new(&context, "fresh");
    let mut local_server = context.server(fresh.clone(), false).await;
    let mut local = context.client(&local_server);
    let startup = context
        .startup(local.client().unwrap().clone(), false)
        .with_legacy_path_read_fallback(false);
    let opened = bounded(startup.open(false, &Stop::new())).await.unwrap();
    assert_eq!(opened.session.id, "new-session");
    assert!(opened.remote.is_none());
    assert!(legacy.calls.lock().unwrap().attached.is_empty());
    assert!(fresh.calls.lock().unwrap().resumed.is_empty());
    assert_eq!(fresh.calls.lock().unwrap().created.len(), 1);
    assert_eq!(context.opens.load(Ordering::SeqCst), 1);
    assert_eq!(context.configurations.load(Ordering::SeqCst), 1);
    drop(opened);
    local.close();
    bounded(legacy_server.close()).await.unwrap();
    bounded(local_server.close()).await.unwrap();
}

#[tokio::test]
async fn duplicate_session_candidate_prefers_current_root_and_socket() {
    let context = Context::new();
    let _legacy_activation = context.admit_legacy();
    let _current_activation = context.admit(false);
    let legacy = Endpoint::new(&context, "connect");
    let current = Endpoint::new(&context, "connect");
    let mut legacy_server = GrpcServer::start_local(
        legacy.clone(),
        context
            .roots
            .legacy()
            .state()
            .join("sessions/existing/parrot.sock"),
        context.diagnostics.clone(),
    )
    .await
    .unwrap();
    let mut current_server = context.server(current.clone(), true).await;
    let mut local = context.client(&current_server);
    let startup = context.startup(local.client().unwrap().clone(), false);
    let opened = bounded(startup.open(true, &Stop::new())).await.unwrap();
    assert_eq!(opened.session.id, "existing");
    assert!(opened.remote.is_some());
    assert_eq!(current.calls.lock().unwrap().attached.len(), 1);
    assert!(legacy.calls.lock().unwrap().attached.is_empty());
    assert_eq!(context.opens.load(Ordering::SeqCst), 0);
    assert_eq!(context.configurations.load(Ordering::SeqCst), 0);
    drop(opened);
    local.close();
    bounded(current_server.close()).await.unwrap();
    bounded(legacy_server.close()).await.unwrap();
}

#[tokio::test]
async fn corrupt_current_duplicate_does_not_fall_back_to_valid_legacy() {
    let context = Context::new();
    let _legacy_activation = context.admit_legacy();
    let _current_activation = context.admit(false);
    std::fs::write(
        context
            .roots
            .current()
            .state()
            .join("sessions/existing/meta.json"),
        "{",
    )
    .unwrap();
    let legacy = Endpoint::new(&context, "connect");
    let mut legacy_server = GrpcServer::start_local(
        legacy.clone(),
        context
            .roots
            .legacy()
            .state()
            .join("sessions/existing/parrot.sock"),
        context.diagnostics.clone(),
    )
    .await
    .unwrap();
    let mut local = context.client(&legacy_server);
    let startup = context.startup(local.client().unwrap().clone(), false);
    assert!(matches!(
        bounded(startup.open(true, &Stop::new())).await,
        Err(StartupError::CorruptState)
    ));
    assert!(legacy.calls.lock().unwrap().attached.is_empty());
    assert!(legacy.calls.lock().unwrap().resumed.is_empty());
    assert_eq!(context.opens.load(Ordering::SeqCst), 0);
    assert_eq!(context.configurations.load(Ordering::SeqCst), 0);
    context.assert_diagnostics("failure");
    local.close();
    bounded(legacy_server.close()).await.unwrap();
}

#[tokio::test]
async fn web_router_adopts_actual_native_owner_from_same_startup() {
    let context = Context::new();
    let _activation = context.admit(false);
    let endpoint = Endpoint::new(&context, "connect");
    let mut owner_server = context.server(endpoint.clone(), true).await;
    // A distinct concrete local client is deliberately never opened by startup.
    let mut unused_local = GrpcTransportClient::connect(
        &TransportAddress::Unix(context.root.join("unused.sock")),
        None,
        None,
        context.diagnostics.clone(),
    )
    .unwrap();
    let local = unused_local.client().unwrap().clone();
    let startup = Arc::new(context.startup(local.clone(), false));
    let router = WebSessionRouter::new(
        local,
        context.roots.clone(),
        startup.workspace().clone(),
        startup,
        context.diagnostics.clone(),
    );
    let session = bounded(router.open_default(&Stop::new())).await.unwrap();
    assert_eq!(session.id, "existing");
    assert!(router.is_remote("existing"));
    assert_eq!(context.opens.load(Ordering::SeqCst), 0);
    assert_eq!(context.configurations.load(Ordering::SeqCst), 0);
    let response = bounded(router.client_for("existing").attach_session(
        wire::AttachSessionRequest {
            user_session_id: "existing".into(),
            working_directory: context.root.to_string_lossy().into_owned(),
            ..Default::default()
        },
    ))
    .await
    .unwrap()
    .into_inner();
    assert_eq!(response.id, "existing");
    assert_eq!(endpoint.calls.lock().unwrap().attached.len(), 2);
    router.forget("existing");
    assert!(!router.is_remote("existing"));
    assert!(context.log().contains("event=\"disconnect.complete\""));
    unused_local.close();
    bounded(owner_server.close()).await.unwrap();
}
