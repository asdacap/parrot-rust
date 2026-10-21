//! Synthetic command fixtures and an actual generated native RPC endpoint.
//! Unused service/session operations fail explicitly; they are not product paths.

use std::{
    collections::VecDeque,
    path::PathBuf,
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
};

use parrot_cli::{
    dispatch::dialog::{DialogOption, SlashDialog},
    slash_sessions::{
        CommandContext, ExistingSessionBinding, SkillRefresh, SlashActivity, SlashCommand,
        SlashFailure, SlashSession,
    },
};
use parrot_domain::{
    contracts::async_ops::{BoxFuture, Cancellation},
    protocol::{bindings::parrot::parrot_server::Parrot, messages::parrot as wire},
};
use parrot_host::{
    grpc::{
        address::TransportAddress,
        token::TransportToken,
        transport::{GrpcServer, GrpcTransportClient},
    },
    web::router::WebClient,
};
use parrot_storage::diagnostics::FileDiagnosticLog;
use tokio::sync::watch;
use tonic::{Request, Response, Status};

fn check(cancellation: &dyn Cancellation) -> Result<(), Status> {
    cancellation
        .check()
        .map_err(|_| Status::cancelled("operation cancelled"))
}

pub struct Never;
impl Cancellation for Never {
    fn is_cancelled(&self) -> bool {
        false
    }
    fn cancelled(&self) -> BoxFuture<'_, ()> {
        Box::pin(std::future::pending())
    }
}
pub static NEVER: Never = Never;

pub struct Stop(watch::Sender<bool>);
impl Default for Stop {
    fn default() -> Self {
        Self::new()
    }
}
impl Stop {
    pub fn new() -> Self {
        Self(watch::channel(false).0)
    }
    pub fn cancel(&self) {
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

#[derive(Default)]
pub struct Dialog {
    choices: VecDeque<Option<String>>,
    secret_values: VecDeque<Option<String>>,
    text_values: VecDeque<Option<String>>,
    confirmations: VecDeque<bool>,
    pub pickers: Vec<(String, Vec<DialogOption>)>,
    pub shown: Vec<String>,
    pub errors: Vec<String>,
    pub loads: Vec<String>,
    pub load_failures: Vec<Status>,
    pub confirmed: Vec<Vec<String>>,
    pub secret_prompts: Vec<String>,
    pub text_prompts: Vec<String>,
    pub failure: Option<Status>,
}
impl Dialog {
    pub fn selects<'a>(mut self, values: impl IntoIterator<Item = Option<&'a str>>) -> Self {
        self.choices
            .extend(values.into_iter().map(|value| value.map(str::to_owned)));
        self
    }
    pub fn secrets<'a>(mut self, values: impl IntoIterator<Item = Option<&'a str>>) -> Self {
        self.secret_values
            .extend(values.into_iter().map(|value| value.map(str::to_owned)));
        self
    }
    pub fn confirmations(mut self, values: impl IntoIterator<Item = bool>) -> Self {
        self.confirmations.extend(values);
        self
    }
    fn boundary(&mut self, cancellation: &dyn Cancellation) -> Result<(), Status> {
        check(cancellation)?;
        match self.failure.take() {
            Some(failure) => Err(failure),
            None => Ok(()),
        }
    }
}
impl SlashDialog for Dialog {
    fn select<'a>(
        &'a mut self,
        title: &'a str,
        options: &'a [DialogOption],
        cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<Option<DialogOption>, Status>> {
        Box::pin(async move {
            self.boundary(cancellation)?;
            self.pickers.push((title.to_owned(), options.to_vec()));
            Ok(self.choices.pop_front().flatten().map(|id| {
                options
                    .iter()
                    .find(|option| option.id == id)
                    .cloned()
                    .expect("scripted selection must exist in the actual command options")
            }))
        })
    }
    fn read_text<'a>(
        &'a mut self,
        prompt: &'a str,
        cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<Option<String>, Status>> {
        Box::pin(async move {
            self.boundary(cancellation)?;
            self.text_prompts.push(prompt.to_owned());
            Ok(self.text_values.pop_front().flatten())
        })
    }
    fn read_secret<'a>(
        &'a mut self,
        prompt: &'a str,
        cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<Option<String>, Status>> {
        Box::pin(async move {
            self.boundary(cancellation)?;
            self.secret_prompts.push(prompt.to_owned());
            Ok(self.secret_values.pop_front().flatten())
        })
    }
    fn confirm<'a>(
        &'a mut self,
        lines: &'a [String],
        cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<bool, Status>> {
        Box::pin(async move {
            self.boundary(cancellation)?;
            self.confirmed.push(lines.to_vec());
            self.shown.extend_from_slice(lines);
            Ok(self.confirmations.pop_front().unwrap_or(true))
        })
    }
    fn show<'a>(
        &'a mut self,
        lines: &'a [String],
        cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<(), Status>> {
        Box::pin(async move {
            self.boundary(cancellation)?;
            self.shown.extend_from_slice(lines);
            Ok(())
        })
    }
    fn print<'a>(
        &'a mut self,
        lines: &'a [String],
        cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<(), Status>> {
        self.show(lines, cancellation)
    }
    fn show_error<'a>(
        &'a mut self,
        message: &'a str,
        cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<(), Status>> {
        Box::pin(async move {
            self.boundary(cancellation)?;
            self.errors.push(message.to_owned());
            Ok(())
        })
    }
    fn load<'a>(
        &'a mut self,
        activity: &'a str,
        work: BoxFuture<'a, Result<(), Status>>,
        cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<(), Status>> {
        Box::pin(async move {
            self.boundary(cancellation)?;
            self.loads.push(activity.to_owned());
            let result = work.await;
            if let Err(status) = &result {
                self.load_failures.push(status.clone());
            }
            result
        })
    }
}

#[derive(Default)]
pub struct Activity {
    waits: AtomicUsize,
    pub failure: Option<Status>,
}
impl Activity {
    pub fn waits(&self) -> usize {
        self.waits.load(Ordering::SeqCst)
    }
}
impl SlashActivity for Activity {
    fn wait_until_idle<'a>(
        &'a self,
        cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<(), SlashFailure>> {
        Box::pin(async move {
            check(cancellation)?;
            self.waits.fetch_add(1, Ordering::SeqCst);
            match self.failure.as_ref() {
                Some(failure) => Err(failure.clone().into()),
                None => Ok(()),
            }
        })
    }
}

pub struct Session {
    pub current: wire::UserSession,
    pub presets: Vec<wire::ModelPreset>,
    pub selected_models: Vec<String>,
    pub selected_presets: Vec<String>,
    pub saved_presets: Vec<String>,
    pub context_limits: Vec<String>,
    pub alias_override: bool,
    pub failure: Option<Status>,
    pub generic_failure: Option<String>,
}
impl Session {
    pub fn new(model: &str) -> Self {
        Self {
            current: wire::UserSession {
                id: "session-7".into(),
                model: model.into(),
                mode: "build".into(),
                ..Default::default()
            },
            presets: Vec::new(),
            selected_models: Vec::new(),
            selected_presets: Vec::new(),
            saved_presets: Vec::new(),
            context_limits: Vec::new(),
            alias_override: false,
            failure: None,
            generic_failure: None,
        }
    }
    fn boundary(&self, cancellation: &dyn Cancellation) -> Result<(), SlashFailure> {
        check(cancellation)?;
        if let Some(failure) = &self.failure {
            return Err(failure.clone().into());
        }
        if let Some(failure) = &self.generic_failure {
            return Err(SlashFailure::Command(failure.clone()));
        }
        Ok(())
    }
}
impl SlashSession for Session {
    fn current_session(&self) -> &wire::UserSession {
        &self.current
    }
    fn select_model<'a>(
        &'a mut self,
        model: &'a str,
        cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<(), SlashFailure>> {
        Box::pin(async move {
            self.boundary(cancellation)?;
            self.selected_models.push(model.to_owned());
            self.current.model = model.to_owned();
            Ok(())
        })
    }
    fn set_model_preset<'a>(
        &'a mut self,
        name: &'a str,
        cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<wire::ModelPreset, SlashFailure>> {
        Box::pin(async move {
            self.boundary(cancellation)?;
            self.saved_presets.push(name.to_owned());
            let preset = wire::ModelPreset {
                name: name.to_owned(),
                model: self.current.model.clone(),
                ..Default::default()
            };
            if let Some(existing) = self.presets.iter_mut().find(|preset| preset.name == name) {
                *existing = preset.clone();
            } else {
                self.presets.push(preset.clone());
            }
            Ok(preset)
        })
    }
    fn select_model_preset<'a>(
        &'a mut self,
        name: &'a str,
        cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<wire::ModelPreset, SlashFailure>> {
        Box::pin(async move {
            self.boundary(cancellation)?;
            let preset = self
                .presets
                .iter()
                .find(|preset| preset.name == name)
                .cloned()
                .ok_or_else(|| Status::not_found("model preset not found"))?;
            self.selected_presets.push(name.to_owned());
            self.current.model = preset.model.clone();
            Ok(preset)
        })
    }
    fn list_model_presets<'a>(
        &'a mut self,
        cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<Vec<wire::ModelPreset>, SlashFailure>> {
        Box::pin(async move {
            self.boundary(cancellation)?;
            Ok(self.presets.clone())
        })
    }
    fn set_context_limit<'a>(
        &'a mut self,
        limit: &'a str,
        cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<wire::SetContextLimitResponse, SlashFailure>> {
        Box::pin(async move {
            self.boundary(cancellation)?;
            self.context_limits.push(limit.to_owned());
            Ok(wire::SetContextLimitResponse {
                context_limit: limit.to_owned(),
                alias_override: self.alias_override,
                ..Default::default()
            })
        })
    }
    fn select_mode<'a>(
        &'a mut self,
        _: &'a str,
        cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<(), SlashFailure>> {
        unused_session(cancellation)
    }
    fn start_new<'a>(
        &'a mut self,
        _: &'a str,
        _: &'a str,
        cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<(), SlashFailure>> {
        unused_session(cancellation)
    }
    fn load_existing<'a>(
        &'a mut self,
        _: wire::UserSession,
        _: &'a mut dyn ExistingSessionBinding,
        cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<(), SlashFailure>> {
        unused_session(cancellation)
    }
    fn set_goal<'a>(
        &'a mut self,
        _: &'a str,
        cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<(), SlashFailure>> {
        unused_session(cancellation)
    }
    fn clear_goal<'a>(
        &'a mut self,
        cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<(), SlashFailure>> {
        unused_session(cancellation)
    }
    fn set_standing_instruction<'a>(
        &'a mut self,
        _: &'a str,
        cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<(), SlashFailure>> {
        unused_session(cancellation)
    }
    fn compact<'a>(
        &'a mut self,
        _: Option<&'a str>,
        cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<(), SlashFailure>> {
        unused_session(cancellation)
    }
    fn sandbox_enable<'a>(
        &'a mut self,
        _: bool,
        cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<wire::SandboxEnableResponse, SlashFailure>> {
        unused_session(cancellation)
    }
    fn list_skills<'a>(
        &'a mut self,
        cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<wire::ListSkillsResponse, SlashFailure>> {
        unused_session(cancellation)
    }
    fn configure_skill<'a>(
        &'a mut self,
        _: &'a str,
        _: bool,
        cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<wire::Skill, SlashFailure>> {
        unused_session(cancellation)
    }
}
fn unused_session<T: Send>(
    cancellation: &dyn Cancellation,
) -> BoxFuture<'_, Result<T, SlashFailure>> {
    Box::pin(async move {
        check(cancellation)?;
        Err(Status::unimplemented("unused settings fixture session operation").into())
    })
}

pub async fn run(
    command: &dyn SlashCommand,
    arguments: &str,
    dialog: &mut Dialog,
    activity: &Activity,
    cancellation: &dyn Cancellation,
) -> Result<(), SlashFailure> {
    run_with_session(
        command,
        arguments,
        &mut Session::new("provider/current/low"),
        dialog,
        activity,
        cancellation,
    )
    .await
}
pub async fn run_with_session(
    command: &dyn SlashCommand,
    arguments: &str,
    session: &mut dyn SlashSession,
    dialog: &mut Dialog,
    activity: &Activity,
    cancellation: &dyn Cancellation,
) -> Result<(), SlashFailure> {
    let refresh: SkillRefresh = Arc::new(|cancellation| {
        Box::pin(async move {
            check(cancellation)?;
            Ok(())
        })
    });
    let mut context = CommandContext {
        session,
        activity,
        dialog,
        refresh_skills: &refresh,
        commands: &[],
    };
    command.run(arguments, &mut context, cancellation).await
}

static NEXT: AtomicUsize = AtomicUsize::new(0);
pub struct Temp {
    pub path: PathBuf,
}
impl Default for Temp {
    fn default() -> Self {
        Self::new()
    }
}
impl Temp {
    pub fn new() -> Self {
        let path = PathBuf::from(format!(
            "/tmp/t66-settings-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::SeqCst)
        ));
        std::fs::create_dir_all(&path).unwrap();
        Self { path }
    }
}

#[derive(Default)]
pub struct RpcState {
    pub models: Vec<wire::Model>,
    pub aliases: Vec<wire::ModelAlias>,
    pub provider_defaults: Vec<wire::ProviderModelAliasDefaults>,
    pub applied_aliases: Vec<wire::ModelAlias>,
    pub configured_aliases: Vec<wire::ConfigureModelAliasRequest>,
    pub applied_defaults: Vec<wire::ApplyProviderModelAliasDefaultsRequest>,
    pub updated_sessions: usize,
    pub update_requests: Vec<wire::UpdateSessionRequest>,
    pub alias_list_failure: Option<Status>,
    pub configure_failure: Option<Status>,
    pub apply_failure: Option<Status>,
    pub model_list_failure: Option<Status>,
    pub default_list_failure: Option<Status>,
    pub model_lists: usize,
    pub alias_lists: usize,
    pub default_lists: usize,
    pub presets: Vec<wire::ModelPreset>,
    pub preset_failure: Option<Status>,
    pub preset_sets: Vec<wire::SetModelPresetRequest>,
    pub preset_selects: Vec<wire::SelectModelPresetRequest>,
    pub preset_lists: usize,
    pub selected_session: Option<wire::UserSession>,
    pub context_limits: Vec<wire::SetContextLimitRequest>,
    pub context_limit_failure: Option<Status>,
    pub alias_override: bool,
}

pub struct RpcFixture {
    pub client: WebClient,
    pub state: Arc<Mutex<RpcState>>,
    server: GrpcServer,
    transport: GrpcTransportClient,
}
impl RpcFixture {
    pub async fn new(state: RpcState) -> Self {
        let temp = Temp::new();
        let diagnostics = Arc::new(
            FileDiagnosticLog::open_global(temp.path.join("logs"), "rpc-fixture", |_| {}).unwrap(),
        );
        let state = Arc::new(Mutex::new(state));
        let token = TransportToken::from_hex(&"11".repeat(32)).unwrap();
        let server = GrpcServer::start(
            Endpoint(state.clone()),
            TransportAddress::parse("http://127.0.0.1:0").unwrap(),
            Some(token.clone()),
            None,
            diagnostics.clone(),
        )
        .await
        .unwrap();
        let mut transport =
            GrpcTransportClient::connect(server.address(), Some(&token), None, diagnostics)
                .unwrap();
        let client = transport.client().unwrap().clone();
        Self {
            client,
            state,
            server,
            transport,
        }
    }
    pub async fn close(mut self) {
        drop(self.client);
        self.transport.close();
        self.server.close().await.unwrap();
    }
}

#[derive(Clone)]
struct Endpoint(Arc<Mutex<RpcState>>);
macro_rules! endpoint {
    ($($method:ident($request:ty) -> $response:ty),* $(,)?) => {
        #[tonic::async_trait]
        impl Parrot for Endpoint {
            $(async fn $method(&self, _: Request<$request>) -> Result<Response<$response>, Status> {
                Err(Status::unimplemented("unused settings fixture RPC"))
            })*
            async fn list_models(&self, _: Request<wire::ListModelsRequest>) -> Result<Response<wire::ListModelsResponse>, Status> {
                let mut state = self.0.lock().unwrap();
                state.model_lists += 1;
                if let Some(failure) = &state.model_list_failure { return Err(failure.clone()); }
                Ok(Response::new(wire::ListModelsResponse { models: state.models.clone(), ..Default::default() }))
            }
            async fn list_model_aliases(&self, _: Request<wire::ListModelAliasesRequest>) -> Result<Response<wire::ListModelAliasesResponse>, Status> {
                let mut state = self.0.lock().unwrap();
                state.alias_lists += 1;
                if let Some(failure) = &state.alias_list_failure { return Err(failure.clone()); }
                Ok(Response::new(wire::ListModelAliasesResponse { aliases: state.aliases.clone(), ..Default::default() }))
            }
            async fn list_provider_model_alias_defaults(&self, _: Request<wire::ListProviderModelAliasDefaultsRequest>) -> Result<Response<wire::ListProviderModelAliasDefaultsResponse>, Status> {
                let mut state = self.0.lock().unwrap();
                state.default_lists += 1;
                if let Some(failure) = &state.default_list_failure { return Err(failure.clone()); }
                Ok(Response::new(wire::ListProviderModelAliasDefaultsResponse { providers: state.provider_defaults.clone(), ..Default::default() }))
            }
            async fn configure_model_alias(&self, request: Request<wire::ConfigureModelAliasRequest>) -> Result<Response<wire::ConfigureModelAliasResponse>, Status> {
                let mut state = self.0.lock().unwrap();
                if let Some(failure) = &state.configure_failure { return Err(failure.clone()); }
                let request = request.into_inner();
                let mut alias = state.aliases.iter().find(|alias| alias.name == request.name).cloned().unwrap_or_default();
                alias.name = request.name.clone(); alias.model_string = request.model_string.clone();
                if let Some(existing) = state.aliases.iter_mut().find(|alias| alias.name == request.name) { *existing = alias.clone(); }
                else { state.aliases.push(alias.clone()); }
                state.configured_aliases.push(request);
                Ok(Response::new(wire::ConfigureModelAliasResponse { alias: Some(alias).into(), ..Default::default() }))
            }
            async fn apply_provider_model_alias_defaults(&self, request: Request<wire::ApplyProviderModelAliasDefaultsRequest>) -> Result<Response<wire::ApplyProviderModelAliasDefaultsResponse>, Status> {
                let mut state = self.0.lock().unwrap();
                if let Some(failure) = &state.apply_failure { return Err(failure.clone()); }
                state.applied_defaults.push(request.into_inner());
                let aliases = state.applied_aliases.clone();
                state.aliases = aliases.clone();
                Ok(Response::new(wire::ApplyProviderModelAliasDefaultsResponse { aliases, ..Default::default() }))
            }
            async fn update_session(&self, request: Request<wire::UpdateSessionRequest>) -> Result<Response<wire::UserSession>, Status> {
                let mut state = self.0.lock().unwrap();
                let request = request.into_inner();
                state.updated_sessions += 1;
                state.update_requests.push(request.clone());
                let session = state.selected_session.clone().unwrap_or_else(|| wire::UserSession { id: request.user_session_id, model: request.model, mode: request.mode, ..Default::default() });
                Ok(Response::new(session))
            }
            async fn set_model_preset(&self, request: Request<wire::SetModelPresetRequest>) -> Result<Response<wire::SetModelPresetResponse>, Status> {
                let mut state = self.0.lock().unwrap();
                if let Some(failure) = &state.preset_failure { return Err(failure.clone()); }
                let request = request.into_inner();
                let preset = state.presets.iter().find(|preset| preset.name == request.name).cloned().ok_or_else(|| Status::not_found("model preset not found"))?;
                state.preset_sets.push(request);
                Ok(Response::new(wire::SetModelPresetResponse { preset: Some(preset).into(), ..Default::default() }))
            }
            async fn select_model_preset(&self, request: Request<wire::SelectModelPresetRequest>) -> Result<Response<wire::SelectModelPresetResponse>, Status> {
                let mut state = self.0.lock().unwrap();
                if let Some(failure) = &state.preset_failure { return Err(failure.clone()); }
                let request = request.into_inner();
                let preset = state.presets.iter().find(|preset| preset.name == request.name).cloned().ok_or_else(|| Status::not_found("model preset not found"))?;
                let session = state.selected_session.clone().unwrap_or_else(|| wire::UserSession { id: request.user_session_id.clone(), model: preset.model.clone(), mode: "build".into(), ..Default::default() });
                state.preset_selects.push(request);
                Ok(Response::new(wire::SelectModelPresetResponse { session: Some(session).into(), preset: Some(preset).into(), ..Default::default() }))
            }
            async fn list_model_presets(&self, _: Request<wire::ListModelPresetsRequest>) -> Result<Response<wire::ListModelPresetsResponse>, Status> {
                let mut state = self.0.lock().unwrap();
                state.preset_lists += 1;
                if let Some(failure) = &state.preset_failure { return Err(failure.clone()); }
                Ok(Response::new(wire::ListModelPresetsResponse { presets: state.presets.clone(), ..Default::default() }))
            }
            async fn set_context_limit(&self, request: Request<wire::SetContextLimitRequest>) -> Result<Response<wire::SetContextLimitResponse>, Status> {
                let mut state = self.0.lock().unwrap();
                if let Some(failure) = &state.context_limit_failure { return Err(failure.clone()); }
                let request = request.into_inner();
                let context_limit = request.context_limit.clone();
                state.context_limits.push(request);
                Ok(Response::new(wire::SetContextLimitResponse { context_limit, alias_override: state.alias_override, ..Default::default() }))
            }
            type ListenStream = tokio_stream::Empty<Result<wire::Event, Status>>;
            async fn listen(&self, _: Request<wire::ListenRequest>) -> Result<Response<Self::ListenStream>, Status> {
                Err(Status::unimplemented("unused settings fixture RPC"))
            }
        }
    };
}
endpoint! {
    list_modes(wire::ListModesRequest) -> wire::ListModesResponse,
    list_skills(wire::ListSkillsRequest) -> wire::ListSkillsResponse,
    configure_skill(wire::ConfigureSkillRequest) -> wire::ConfigureSkillResponse,
    list_sessions(wire::ListSessionsRequest) -> wire::ListSessionsResponse,
    create_session(wire::CreateSessionRequest) -> wire::UserSession,
    resume_session(wire::ResumeSessionRequest) -> wire::UserSession,
    attach_session(wire::AttachSessionRequest) -> wire::UserSession,
    set_goal(wire::SetGoalRequest) -> wire::SetGoalResponse,
    set_standing_instruction(wire::SetStandingInstructionRequest) -> wire::SetStandingInstructionResponse,
    send_message(wire::SendMessageRequest) -> wire::SendMessageResponse,
    upload_attachment(tonic::Streaming<wire::AttachmentUploadFrame>) -> wire::AttachmentUploadResponse,
    interrupt(wire::InterruptRequest) -> wire::InterruptResponse,
    compact(wire::CompactRequest) -> wire::CompactResponse,
    sandbox_enable(wire::SandboxEnableRequest) -> wire::SandboxEnableResponse,
    session_status(wire::SessionStatusRequest) -> wire::SessionStatusResponse,
    list_pending_questions(wire::ListPendingQuestionsRequest) -> wire::ListPendingQuestionsResponse,
    reply_question(wire::ReplyQuestionRequest) -> wire::ReplyQuestionResponse,
    reject_question(wire::RejectQuestionRequest) -> wire::RejectQuestionResponse,
    list_pending_permissions(wire::ListPendingPermissionsRequest) -> wire::ListPendingPermissionsResponse,
    reply_permission(wire::ReplyPermissionRequest) -> wire::ReplyPermissionResponse,
}
