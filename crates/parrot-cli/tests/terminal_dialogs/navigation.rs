//! Navigation/controller characterization over real native UDS transports.
//! Controlled endpoints are orchestration evidence, never a local security factory.
#![cfg(unix)]

use parrot_cli::{
    dispatch::startup::OpenLocalClient,
    slash_sessions::{
        CallbackSessionBinding, NavigationFailure, RpcSlashSession, SlashSession,
        WorkspaceNavigation,
    },
    terminal_dialogs::navigation::{
        ExplicitRemoteTerminalSessionNavigation, LocalTerminalSessionNavigation,
        SessionReplacement, TerminalSessionController, TerminalSessionNavigation,
        TerminalSessionTarget,
    },
};
use parrot_domain::{
    contracts::async_ops::{BoxFuture, Cancellation},
    protocol::{bindings::parrot::parrot_server::Parrot, messages::parrot as wire},
    values::project_workspace::ProjectWorkspace,
};
use parrot_foundation::{config::Configuration, paths::ApplicationRoots};
use parrot_host::{
    composition::factory::user::ordinary_workspace_resolver,
    grpc::transport::{GrpcServer, GrpcTransportClient},
    web::router::WebClient,
};
use parrot_storage::diagnostics::FileDiagnosticLog;
use std::{
    collections::{BTreeMap, HashMap, VecDeque},
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
            while !*receiver.borrow_and_update() {
                if receiver.changed().await.is_err() {
                    return;
                }
            }
        })
    }
}

struct Fixture {
    root: PathBuf,
    roots: ApplicationRoots,
    workspace: ProjectWorkspace,
    diagnostics: Arc<FileDiagnosticLog>,
}
impl Fixture {
    fn new() -> Self {
        let root = PathBuf::from(format!(
            "/tmp/t72n-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(root.join("w")).unwrap();
        let roots = ApplicationRoots::resolve(&HashMap::from([
            ("HOME".into(), root.to_string_lossy().into_owned()),
            (
                "XDG_STATE_HOME".into(),
                root.join("s").to_string_lossy().into_owned(),
            ),
        ]));
        let workspace = ordinary_workspace_resolver()(&root.join("w")).unwrap();
        let diagnostics = Arc::new(
            FileDiagnosticLog::open_global(root.join("logs"), "navigation", |_| {}).unwrap(),
        );
        Self {
            root,
            roots,
            workspace,
            diagnostics,
        }
    }
    async fn server(&self, endpoint: Endpoint, name: &str) -> GrpcServer {
        GrpcServer::start_local(endpoint, self.root.join(name), self.diagnostics.clone())
            .await
            .unwrap()
    }
    fn owner(&self, server: &GrpcServer) -> GrpcTransportClient {
        GrpcTransportClient::connect(server.address(), None, None, self.diagnostics.clone())
            .unwrap()
    }
    fn disconnects(&self) -> usize {
        std::fs::read_to_string(self.diagnostics.path())
            .unwrap()
            .lines()
            .filter(|line| line.contains("event=\"disconnect.complete\""))
            .count()
    }
    fn local(&self, client: WebClient, opens: Arc<AtomicUsize>) -> LocalTerminalSessionNavigation {
        let open: OpenLocalClient = Arc::new(move |c| {
            let client = client.clone();
            let opens = opens.clone();
            Box::pin(async move {
                c.check()
                    .map_err(|_| Status::cancelled("fixture stopped"))?;
                opens.fetch_add(1, Ordering::SeqCst);
                Ok(client)
            })
        });
        LocalTerminalSessionNavigation::new(
            self.roots.clone(),
            self.workspace.clone(),
            self.diagnostics.clone(),
            open,
        )
    }
    fn current(&self) -> wire::UserSession {
        wire::UserSession {
            id: "old".into(),
            model: "old-model".into(),
            mode: "build".into(),
            working_directory: self
                .workspace
                .launch_directory()
                .to_string_lossy()
                .into_owned(),
            ..Default::default()
        }
    }
    fn session_socket(&self, legacy: bool) -> PathBuf {
        let paths = if legacy {
            self.roots.legacy()
        } else {
            self.roots.current()
        };
        let dir = paths.state().join("sessions/selected");
        std::fs::create_dir_all(&dir).unwrap();
        dir.join("parrot.sock")
    }
    fn slash(&self, controller: &TerminalSessionController) -> RpcSlashSession {
        let path = self.root.join("config.yaml");
        std::fs::write(&path, "model: provider/current\n").unwrap();
        let config = Configuration::load(
            &path,
            &self.root.join("predefined.yaml"),
            &BTreeMap::from([("HOME".into(), self.root.to_string_lossy().into_owned())]),
            |_| Ok(()),
        )
        .unwrap();
        let snapshot = controller.snapshot();
        RpcSlashSession::new(
            snapshot.client,
            snapshot.session,
            Arc::new(Mutex::new(config)),
            true,
            Box::new(CallbackSessionBinding(Arc::new(|_, _| {
                Box::pin(async { Ok(()) })
            }))),
        )
    }
}

#[derive(Default)]
struct Calls {
    lists: Vec<wire::ListSessionsRequest>,
    attached: Vec<wire::AttachSessionRequest>,
    resumed: Vec<wire::ResumeSessionRequest>,
    compacted: Vec<wire::CompactRequest>,
}
#[derive(Clone, Default)]
struct Endpoint {
    calls: Arc<Mutex<Calls>>,
    attach_failure: Option<Code>,
    resume_failure: Option<Code>,
    first_attach_deadline: bool,
    returned_id: Option<String>,
    returned_workspace: Option<String>,
}
impl Endpoint {
    fn selected(&self, id: String, workspace: String) -> wire::UserSession {
        wire::UserSession {
            id: self.returned_id.clone().unwrap_or(id),
            working_directory: self.returned_workspace.clone().unwrap_or(workspace),
            model: "selected-model".into(),
            mode: "plan".into(),
            ..Default::default()
        }
    }
}
macro_rules! endpoint {
    ($($method:ident($request:ty) -> $response:ty),* $(,)?) => {
        #[tonic::async_trait]
        impl Parrot for Endpoint {
            $(async fn $method(&self, _: Request<$request>) -> Result<Response<$response>, Status> { Err(Status::unimplemented("unused navigation fixture RPC")) })*
            async fn list_sessions(&self, request: Request<wire::ListSessionsRequest>) -> Result<Response<wire::ListSessionsResponse>, Status> {
                self.calls.lock().unwrap().lists.push(request.into_inner()); Ok(Response::new(wire::ListSessionsResponse::new()))
            }
            async fn attach_session(&self, request: Request<wire::AttachSessionRequest>) -> Result<Response<wire::UserSession>, Status> {
                let request = request.into_inner();
                let count = { let mut calls = self.calls.lock().unwrap(); calls.attached.push(request.clone()); calls.attached.len() };
                if self.first_attach_deadline && count == 1 { return Err(Status::deadline_exceeded("ownership changed")); }
                if let Some(code) = self.attach_failure { return Err(Status::new(code, "attachment failed")); }
                Ok(Response::new(self.selected(request.user_session_id, request.working_directory)))
            }
            async fn resume_session(&self, request: Request<wire::ResumeSessionRequest>) -> Result<Response<wire::UserSession>, Status> {
                let request = request.into_inner(); self.calls.lock().unwrap().resumed.push(request.clone());
                if let Some(code) = self.resume_failure { return Err(Status::new(code, "resume failed")); }
                Ok(Response::new(self.selected(request.user_session_id, request.working_directory)))
            }
            async fn compact(&self, request: Request<wire::CompactRequest>) -> Result<Response<wire::CompactResponse>, Status> {
                self.calls.lock().unwrap().compacted.push(request.into_inner()); Ok(Response::new(wire::CompactResponse::new()))
            }
            type ListenStream = tokio_stream::Empty<Result<wire::Event, Status>>;
            async fn listen(&self, _: Request<wire::ListenRequest>) -> Result<Response<Self::ListenStream>, Status> { Err(Status::unimplemented("stream cases belong to enhanced binding")) }
        }
    };
}
endpoint! {
    create_session(wire::CreateSessionRequest) -> wire::UserSession,
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
    update_session(wire::UpdateSessionRequest) -> wire::UserSession,
    set_goal(wire::SetGoalRequest) -> wire::SetGoalResponse,
    set_standing_instruction(wire::SetStandingInstructionRequest) -> wire::SetStandingInstructionResponse,
    send_message(wire::SendMessageRequest) -> wire::SendMessageResponse,
    upload_attachment(tonic::Streaming<wire::AttachmentUploadFrame>) -> wire::AttachmentUploadResponse,
    interrupt(wire::InterruptRequest) -> wire::InterruptResponse,
    set_context_limit(wire::SetContextLimitRequest) -> wire::SetContextLimitResponse,
    sandbox_enable(wire::SandboxEnableRequest) -> wire::SandboxEnableResponse,
    session_status(wire::SessionStatusRequest) -> wire::SessionStatusResponse,
    list_pending_questions(wire::ListPendingQuestionsRequest) -> wire::ListPendingQuestionsResponse,
    reply_question(wire::ReplyQuestionRequest) -> wire::ReplyQuestionResponse,
    reject_question(wire::RejectQuestionRequest) -> wire::RejectQuestionResponse,
    list_pending_permissions(wire::ListPendingPermissionsRequest) -> wire::ListPendingPermissionsResponse,
    reply_permission(wire::ReplyPermissionRequest) -> wire::ReplyPermissionResponse,
}

struct QueueNavigation {
    targets: Mutex<VecDeque<TerminalSessionTarget>>,
    opened: Mutex<Vec<String>>,
    listed: Mutex<Vec<String>>,
    listed_workspaces: Mutex<Vec<String>>,
}
impl QueueNavigation {
    fn new(targets: Vec<TerminalSessionTarget>) -> Self {
        Self {
            targets: Mutex::new(targets.into()),
            opened: Mutex::default(),
            listed: Mutex::default(),
            listed_workspaces: Mutex::default(),
        }
    }
}
impl TerminalSessionNavigation for QueueNavigation {
    fn list<'a>(
        &'a self,
        current: &'a wire::UserSession,
        c: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<wire::ListSessionsResponse, NavigationFailure>> {
        Box::pin(async move {
            c.check()
                .map_err(|_| NavigationFailure::Rpc(Status::cancelled("stopped")))?;
            self.listed.lock().unwrap().push(current.id.clone());
            self.listed_workspaces
                .lock()
                .unwrap()
                .push(current.working_directory.clone());
            Ok(wire::ListSessionsResponse::new())
        })
    }
    fn open<'a>(
        &'a self,
        _: &'a wire::UserSession,
        id: &'a str,
        c: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<TerminalSessionTarget, NavigationFailure>> {
        Box::pin(async move {
            c.check()
                .map_err(|_| NavigationFailure::Rpc(Status::cancelled("stopped")))?;
            self.opened.lock().unwrap().push(id.into());
            self.targets
                .lock()
                .unwrap()
                .pop_front()
                .ok_or_else(|| NavigationFailure::Rpc(Status::not_found("missing")))
        })
    }
}

struct Replacement {
    behavior: &'static str,
    entered: Arc<Notify>,
}
impl Replacement {
    fn new(behavior: &'static str) -> Self {
        Self {
            behavior,
            entered: Arc::default(),
        }
    }
}
impl SessionReplacement for Replacement {
    fn replace<'a>(
        &'a self,
        _: wire::UserSession,
        _: WebClient,
        commit: Box<dyn FnOnce() + Send + 'a>,
        c: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<(), NavigationFailure>> {
        Box::pin(async move {
            if self.behavior != "fail-before" {
                commit();
            }
            self.entered.notify_one();
            match self.behavior {
                "fail-before" | "fail-after" => {
                    Err(NavigationFailure::Recoverable("binding failed".into()))
                }
                "wait" => {
                    c.cancelled().await;
                    Err(NavigationFailure::Rpc(Status::cancelled("stopped")))
                }
                _ => Ok(()),
            }
        })
    }
}
fn selected(id: &str) -> wire::UserSession {
    wire::UserSession {
        id: id.into(),
        model: "selected-model".into(),
        mode: "plan".into(),
        ..Default::default()
    }
}

#[tokio::test]
async fn successful_loading_commits_route_model_mode_and_retains_until_close() {
    let f = Fixture::new();
    let first = Endpoint::default();
    let second = Endpoint::default();
    let mut server1 = f.server(first.clone(), "a.sock").await;
    let mut server2 = f.server(second.clone(), "b.sock").await;
    let mut initial = f.owner(&server1);
    let initial_client = initial.client().unwrap().clone();
    let mut target_owner = f.owner(&server2);
    let target_client = target_owner.client().unwrap().clone();
    let nav = Arc::new(QueueNavigation::new(vec![TerminalSessionTarget::new(
        target_client,
        selected("selected"),
        Some(target_owner),
    )]));
    let controller = TerminalSessionController::new(
        nav,
        initial_client,
        f.current(),
        Arc::new(Replacement::new("ok")),
    );
    let stop = Stop::new();
    controller.load("selected", &stop).await.unwrap();
    let mut slash = f.slash(&controller);
    slash.compact(None, &stop).await.unwrap();
    assert_eq!(slash.id(), "selected");
    assert_eq!(slash.model(), "selected-model");
    assert_eq!(slash.mode(), "plan");
    assert_eq!(controller.revision(), 1);
    assert!(first.calls.lock().unwrap().compacted.is_empty());
    assert_eq!(
        second.calls.lock().unwrap().compacted[0].user_session_id,
        "selected"
    );
    assert_eq!(f.disconnects(), 0);
    controller.close().await.unwrap();
    controller.close().await.unwrap();
    assert_eq!(f.disconnects(), 1);
    drop(slash);
    drop(controller);
    initial.close();
    server2.close().await.unwrap();
    server1.close().await.unwrap();
}

#[tokio::test]
async fn current_load_is_noop_empty_and_precancel_do_not_open() {
    let f = Fixture::new();
    let mut server = f.server(Endpoint::default(), "a.sock").await;
    let mut owner = f.owner(&server);
    let nav = Arc::new(QueueNavigation::new(vec![]));
    let controller = TerminalSessionController::new(
        nav.clone(),
        owner.client().unwrap().clone(),
        f.current(),
        Arc::new(Replacement::new("fail-before")),
    );
    let stop = Stop::new();
    controller.load("old", &stop).await.unwrap();
    assert!(controller.load("", &stop).await.is_err());
    stop.cancel();
    controller.load("old", &stop).await.unwrap();
    assert!(
        matches!(controller.load("new", &stop).await, Err(NavigationFailure::Rpc(s)) if s.code()==Code::Cancelled)
    );
    assert!(nav.opened.lock().unwrap().is_empty());
    assert_eq!(controller.revision(), 0);
    controller.close().await.unwrap();
    owner.close();
    server.close().await.unwrap();
}

#[tokio::test]
async fn binding_failure_before_and_after_commit_restores_route_and_closes_candidate() {
    for behavior in ["fail-before", "fail-after"] {
        let f = Fixture::new();
        let old = Endpoint::default();
        let target = Endpoint::default();
        let mut server1 = f.server(old.clone(), "a.sock").await;
        let mut server2 = f.server(target.clone(), "b.sock").await;
        let mut initial = f.owner(&server1);
        let mut candidate = f.owner(&server2);
        let client = candidate.client().unwrap().clone();
        let nav = Arc::new(QueueNavigation::new(vec![TerminalSessionTarget::new(
            client,
            selected("selected"),
            Some(candidate),
        )]));
        let controller = TerminalSessionController::new(
            nav,
            initial.client().unwrap().clone(),
            f.current(),
            Arc::new(Replacement::new(behavior)),
        );
        let stop = Stop::new();
        assert!(controller.load("selected", &stop).await.is_err());
        let mut slash = f.slash(&controller);
        slash.compact(None, &stop).await.unwrap();
        assert_eq!(slash.id(), "old");
        assert_eq!(slash.model(), "old-model");
        assert_eq!(slash.mode(), "build");
        assert_eq!(controller.revision(), 0);
        assert_eq!(
            old.calls.lock().unwrap().compacted[0].user_session_id,
            "old"
        );
        assert!(target.calls.lock().unwrap().compacted.is_empty());
        assert_eq!(f.disconnects(), 1);
        controller.close().await.unwrap();
        assert_eq!(f.disconnects(), 1);
        drop(slash);
        drop(controller);
        initial.close();
        server2.close().await.unwrap();
        server1.close().await.unwrap();
    }
}

#[tokio::test]
async fn cancellation_and_dropped_observer_rollback_and_retain_owner_for_close() {
    for drop_observer in [false, true] {
        let f = Fixture::new();
        let mut server = f.server(Endpoint::default(), "a.sock").await;
        let mut initial = f.owner(&server);
        let mut candidate = f.owner(&server);
        let target = TerminalSessionTarget::new(
            candidate.client().unwrap().clone(),
            selected("selected"),
            Some(candidate),
        );
        let replacement = Arc::new(Replacement::new("wait"));
        let controller = TerminalSessionController::new(
            Arc::new(QueueNavigation::new(vec![target])),
            initial.client().unwrap().clone(),
            f.current(),
            replacement.clone(),
        );
        let stop = Stop::new();
        {
            let mut loading = controller.load("selected", &stop);
            tokio::select! { _ = replacement.entered.notified() => {}, result = &mut loading => panic!("unexpected completion: {result:?}") }
            assert_eq!(controller.current().id, "selected");
            if !drop_observer {
                stop.cancel();
                assert!(
                    matches!(loading.await, Err(NavigationFailure::Rpc(s)) if s.code()==Code::Cancelled)
                );
            }
        }
        assert_eq!(controller.current().id, "old");
        assert_eq!(controller.revision(), 0);
        assert_eq!(f.disconnects(), usize::from(!drop_observer));
        controller.close().await.unwrap();
        controller.close().await.unwrap();
        assert_eq!(f.disconnects(), 1);
        initial.close();
        server.close().await.unwrap();
    }
}

#[tokio::test]
async fn repeated_switches_return_to_original_host_and_list_committed_current() {
    let f = Fixture::new();
    let first = Endpoint::default();
    let second = Endpoint::default();
    let mut server1 = f.server(first.clone(), "a.sock").await;
    let mut server2 = f.server(second.clone(), "b.sock").await;
    let mut initial = f.owner(&server1);
    let mut a = f.owner(&server2);
    let mut b = f.owner(&server1);
    let nav = Arc::new(QueueNavigation::new(vec![
        TerminalSessionTarget::new(a.client().unwrap().clone(), selected("selected"), Some(a)),
        TerminalSessionTarget::new(b.client().unwrap().clone(), f.current(), Some(b)),
    ]));
    let controller = TerminalSessionController::new(
        nav.clone(),
        initial.client().unwrap().clone(),
        f.current(),
        Arc::new(Replacement::new("ok")),
    );
    let stop = Stop::new();
    controller.load("selected", &stop).await.unwrap();
    f.slash(&controller).compact(None, &stop).await.unwrap();
    controller.load("old", &stop).await.unwrap();
    f.slash(&controller).compact(None, &stop).await.unwrap();
    controller.list(&stop).await.unwrap();
    assert_eq!(*nav.opened.lock().unwrap(), ["selected", "old"]);
    assert_eq!(*nav.listed.lock().unwrap(), ["old"]);
    assert_eq!(controller.revision(), 2);
    assert_eq!(
        second.calls.lock().unwrap().compacted[0].user_session_id,
        "selected"
    );
    assert_eq!(
        first.calls.lock().unwrap().compacted[0].user_session_id,
        "old"
    );
    assert_eq!(f.disconnects(), 0);
    controller.close().await.unwrap();
    assert_eq!(f.disconnects(), 2);
    initial.close();
    server2.close().await.unwrap();
    server1.close().await.unwrap();
}

#[tokio::test]
async fn synchronized_fresh_session_becomes_current_and_previous_id_can_load_again() {
    let f = Fixture::new();
    let endpoint = Endpoint::default();
    let mut server = f.server(endpoint.clone(), "a.sock").await;
    let mut owner = f.owner(&server);
    let client = owner.client().unwrap().clone();
    let nav = Arc::new(QueueNavigation::new(vec![TerminalSessionTarget::new(
        client.clone(),
        f.current(),
        None,
    )]));
    let controller = TerminalSessionController::new(
        nav.clone(),
        client.clone(),
        f.current(),
        Arc::new(Replacement::new("ok")),
    );
    let stop = Stop::new();
    let mut fresh = selected("fresh");
    fresh.working_directory = "/fresh/workspace".into();
    controller.synchronize_current(client, fresh);
    assert_eq!(controller.current().id, "fresh");
    assert_eq!(controller.current().model, "selected-model");
    assert_eq!(controller.current().mode, "plan");
    assert_eq!(controller.revision(), 1);
    controller.load("fresh", &stop).await.unwrap();
    assert!(nav.opened.lock().unwrap().is_empty());
    controller.list(&stop).await.unwrap();
    assert_eq!(*nav.listed.lock().unwrap(), ["fresh"]);
    assert_eq!(*nav.listed_workspaces.lock().unwrap(), ["/fresh/workspace"]);
    f.slash(&controller).compact(None, &stop).await.unwrap();
    controller.load("old", &stop).await.unwrap();
    assert_eq!(*nav.opened.lock().unwrap(), ["old"]);
    assert_eq!(controller.current().id, "old");
    assert_eq!(controller.revision(), 2);
    f.slash(&controller).compact(None, &stop).await.unwrap();
    assert_eq!(
        endpoint
            .calls
            .lock()
            .unwrap()
            .compacted
            .iter()
            .map(|request| request.user_session_id.as_str())
            .collect::<Vec<_>>(),
        ["fresh", "old"]
    );
    controller.close().await.unwrap();
    owner.close();
    server.close().await.unwrap();
}

#[tokio::test]
async fn synchronized_settings_preserve_revision_and_closed_snapshot() {
    let f = Fixture::new();
    let mut server = f.server(Endpoint::default(), "a.sock").await;
    let mut owner = f.owner(&server);
    let client = owner.client().unwrap().clone();
    let controller = TerminalSessionController::new(
        Arc::new(QueueNavigation::new(vec![])),
        client.clone(),
        f.current(),
        Arc::new(Replacement::new("ok")),
    );
    let mut updated = f.current();
    updated.model = "updated-model".into();
    updated.mode = "plan".into();
    controller.synchronize_current(client.clone(), updated.clone());
    assert_eq!(controller.current(), updated);
    assert_eq!(controller.revision(), 0);
    controller.close().await.unwrap();
    controller.synchronize_current(client, selected("ignored-after-close"));
    assert_eq!(controller.current(), updated);
    assert_eq!(controller.revision(), 0);
    owner.close();
    server.close().await.unwrap();
}

#[tokio::test]
async fn remote_selection_uses_server_workspace_and_resumes_only_not_found() {
    for attach_failure in [
        None,
        Some(Code::NotFound),
        Some(Code::FailedPrecondition),
        Some(Code::PermissionDenied),
        Some(Code::Unavailable),
    ] {
        let f = Fixture::new();
        let endpoint = Endpoint {
            attach_failure,
            ..Default::default()
        };
        let mut server = f.server(endpoint.clone(), "a.sock").await;
        let mut owner = f.owner(&server);
        let nav = ExplicitRemoteTerminalSessionNavigation::new(owner.client().unwrap().clone());
        let stop = Stop::new();
        let current = wire::UserSession {
            working_directory: "/server/workspace".into(),
            ..Default::default()
        };
        nav.list(&current, &stop).await.unwrap();
        let result = nav.open(&current, "selected", &stop).await;
        {
            let calls = endpoint.calls.lock().unwrap();
            assert_eq!(calls.lists[0].working_directory, "/server/workspace");
            assert_eq!(calls.attached[0].working_directory, "/server/workspace");
            if attach_failure.is_none() || attach_failure == Some(Code::NotFound) {
                assert_eq!(result.unwrap().session.id, "selected");
                if attach_failure.is_some() {
                    assert_eq!(calls.resumed[0].user_session_id, "selected");
                    assert_eq!(calls.resumed[0].working_directory, "/server/workspace");
                    assert!(calls.resumed[0].interactive_permissions);
                    assert!(!calls.resumed[0].take_over);
                } else {
                    assert!(calls.resumed.is_empty());
                }
            } else {
                assert!(
                    matches!(result, Err(NavigationFailure::Rpc(s)) if Some(s.code())==attach_failure)
                );
                assert!(calls.resumed.is_empty());
            }
        }
        owner.close();
        server.close().await.unwrap();
    }
}

#[tokio::test]
async fn remote_missing_workspace_and_different_session_are_rejected() {
    let f = Fixture::new();
    let endpoint = Endpoint {
        returned_id: Some("different".into()),
        ..Default::default()
    };
    let mut server = f.server(endpoint.clone(), "a.sock").await;
    let mut owner = f.owner(&server);
    let nav = ExplicitRemoteTerminalSessionNavigation::new(owner.client().unwrap().clone());
    let stop = Stop::new();
    for workspace in ["", " \t"] {
        let current = wire::UserSession {
            working_directory: workspace.into(),
            ..Default::default()
        };
        assert!(matches!(
            nav.list(&current, &stop).await,
            Err(NavigationFailure::Recoverable(_))
        ));
        assert!(nav.open(&current, "selected", &stop).await.is_err());
    }
    assert!(endpoint.calls.lock().unwrap().attached.is_empty());
    assert!(endpoint.calls.lock().unwrap().lists.is_empty());
    assert!(
        matches!(nav.open(&f.current(), "selected", &stop).await, Err(NavigationFailure::Recoverable(s)) if s=="the server returned a different user session")
    );
    owner.close();
    server.close().await.unwrap();
}

#[tokio::test]
async fn local_active_attachment_and_ownership_race_preserve_exact_target() {
    for race in [false, true] {
        let f = Fixture::new();
        let owner_endpoint = Endpoint {
            first_attach_deadline: race,
            ..Default::default()
        };
        let local_endpoint = Endpoint {
            resume_failure: Some(Code::AlreadyExists),
            ..Default::default()
        };
        let mut socket_server = GrpcServer::start_local(
            owner_endpoint.clone(),
            f.session_socket(false),
            f.diagnostics.clone(),
        )
        .await
        .unwrap();
        let mut local_server = f.server(local_endpoint.clone(), "l.sock").await;
        let mut local_owner = f.owner(&local_server);
        let opens = Arc::new(AtomicUsize::new(0));
        let nav = f.local(local_owner.client().unwrap().clone(), opens.clone());
        let stop = Stop::new();
        let mut target = nav.open(&f.current(), "selected", &stop).await.unwrap();
        assert_eq!(target.session.id, "selected");
        assert_eq!(
            owner_endpoint.calls.lock().unwrap().attached.len(),
            if race { 2 } else { 1 }
        );
        assert_eq!(opens.load(Ordering::SeqCst), usize::from(race));
        assert_eq!(
            owner_endpoint.calls.lock().unwrap().attached[0].working_directory,
            f.workspace.launch_directory().to_string_lossy()
        );
        if race {
            let calls = local_endpoint.calls.lock().unwrap();
            assert_eq!(calls.resumed[0].user_session_id, "selected");
            assert!(calls.resumed[0].take_over);
        }
        let before = f.disconnects();
        target.close().await;
        target.close().await;
        assert_eq!(f.disconnects(), before + 1);
        local_owner.close();
        socket_server.close().await.unwrap();
        local_server.close().await.unwrap();
    }
}

#[tokio::test]
async fn local_listing_and_missing_socket_resume_exact_session_with_takeover() {
    let f = Fixture::new();
    let endpoint = Endpoint::default();
    let mut server = f.server(endpoint.clone(), "l.sock").await;
    let mut owner = f.owner(&server);
    let opens = Arc::new(AtomicUsize::new(0));
    let nav = f.local(owner.client().unwrap().clone(), opens.clone());
    let stop = Stop::new();
    let current = wire::UserSession {
        working_directory: "/foreign/host".into(),
        ..Default::default()
    };
    nav.list(&current, &stop).await.unwrap();
    let target = tokio::time::timeout(
        Duration::from_secs(8),
        nav.open(&current, "selected", &stop),
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(target.session.id, "selected");
    {
        let calls = endpoint.calls.lock().unwrap();
        assert_eq!(
            calls.lists[0].working_directory,
            f.workspace.launch_directory().to_string_lossy()
        );
        assert_eq!(
            calls.resumed[0].working_directory,
            f.workspace.launch_directory().to_string_lossy()
        );
        assert_eq!(calls.resumed[0].user_session_id, "selected");
        assert!(calls.resumed[0].interactive_permissions);
        assert!(calls.resumed[0].take_over);
        assert_eq!(opens.load(Ordering::SeqCst), 2);
    }
    owner.close();
    server.close().await.unwrap();
}

#[tokio::test]
async fn local_semantic_failure_or_wrong_workspace_does_not_resume() {
    for wrong_workspace in [false, true] {
        let f = Fixture::new();
        std::fs::create_dir_all(f.root.join("elsewhere")).unwrap();
        let endpoint = Endpoint {
            attach_failure: (!wrong_workspace).then_some(Code::PermissionDenied),
            returned_workspace: wrong_workspace
                .then(|| f.root.join("elsewhere").to_string_lossy().into_owned()),
            ..Default::default()
        };
        let mut server = GrpcServer::start_local(
            endpoint.clone(),
            f.session_socket(false),
            f.diagnostics.clone(),
        )
        .await
        .unwrap();
        let mut owner = f.owner(&server);
        let opens = Arc::new(AtomicUsize::new(0));
        let nav = f.local(owner.client().unwrap().clone(), opens.clone());
        let stop = Stop::new();
        let result = nav.open(&f.current(), "selected", &stop).await;
        if wrong_workspace {
            assert!(
                matches!(result,Err(NavigationFailure::Recoverable(s)) if s=="the user session belongs to another workspace")
            );
        } else {
            assert!(
                matches!(result,Err(NavigationFailure::Rpc(s)) if s.code()==Code::PermissionDenied)
            );
        }
        assert_eq!(opens.load(Ordering::SeqCst), 0);
        assert!(endpoint.calls.lock().unwrap().resumed.is_empty());
        owner.close();
        server.close().await.unwrap();
    }
}

#[tokio::test]
async fn legacy_session_socket_is_selected_in_place_and_current_directory_wins() {
    let f = Fixture::new();
    let legacy = Endpoint::default();
    let current = Endpoint::default();
    let mut legacy_server = GrpcServer::start_local(
        legacy.clone(),
        f.session_socket(true),
        f.diagnostics.clone(),
    )
    .await
    .unwrap();
    let mut seed = f.owner(&legacy_server);
    let nav = f.local(seed.client().unwrap().clone(), Arc::default());
    let stop = Stop::new();
    let mut target = nav.open(&f.current(), "selected", &stop).await.unwrap();
    assert_eq!(legacy.calls.lock().unwrap().attached.len(), 1);
    target.close().await;
    let mut current_server = GrpcServer::start_local(
        current.clone(),
        f.session_socket(false),
        f.diagnostics.clone(),
    )
    .await
    .unwrap();
    let mut target = nav.open(&f.current(), "selected", &stop).await.unwrap();
    assert_eq!(current.calls.lock().unwrap().attached.len(), 1);
    assert_eq!(legacy.calls.lock().unwrap().attached.len(), 1);
    target.close().await;
    seed.close();
    current_server.close().await.unwrap();
    legacy_server.close().await.unwrap();
}

#[tokio::test]
async fn precancelled_local_navigation_does_not_open_any_client() {
    let f = Fixture::new();
    let mut server = f.server(Endpoint::default(), "l.sock").await;
    let mut owner = f.owner(&server);
    let opens = Arc::new(AtomicUsize::new(0));
    let nav = f.local(owner.client().unwrap().clone(), opens.clone());
    let stop = Stop::new();
    stop.cancel();
    assert!(
        matches!(nav.open(&f.current(),"selected",&stop).await,Err(NavigationFailure::Rpc(s)) if s.code()==Code::Cancelled)
    );
    assert!(nav.list(&f.current(), &stop).await.is_err());
    assert_eq!(opens.load(Ordering::SeqCst), 0);
    owner.close();
    server.close().await.unwrap();
}
