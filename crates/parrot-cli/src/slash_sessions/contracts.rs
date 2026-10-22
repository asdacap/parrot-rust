use std::{fmt, sync::Arc};

use parrot_domain::{
    contracts::async_ops::{BoxFuture, Cancellation},
    protocol::messages::parrot::{
        ListSkillsResponse, ModelPreset, SandboxEnableResponse, SetContextLimitResponse, Skill,
        UserSession,
    },
};
use tonic::Status;

use crate::dispatch::dialog::SlashDialog;

/// RPC (including cancellation) and ordinary command failures remain distinct at
/// the real web boundary. No status is flattened into a generic error frame.
#[derive(Debug)]
pub enum SlashFailure {
    Rpc(Status),
    Command(String),
}

impl SlashFailure {
    pub fn message(&self) -> &str {
        match self {
            Self::Rpc(status) => status.message(),
            Self::Command(message) => message,
        }
    }
}

impl From<Status> for SlashFailure {
    fn from(status: Status) -> Self {
        Self::Rpc(status)
    }
}

impl fmt::Display for SlashFailure {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.message())
    }
}

impl std::error::Error for SlashFailure {}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CommandDescription {
    pub name: String,
    pub summary: String,
}

/// Source ISlashCommand, with its shared mutable dependencies borrowed for one
/// dispatch rather than captured in separate session/dialog mutexes.
pub trait SlashCommand: Send + Sync {
    fn name(&self) -> &str;
    fn summary(&self) -> &str;
    fn run<'a>(
        &'a self,
        arguments: &'a str,
        context: &'a mut CommandContext<'_>,
        cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<(), SlashFailure>>;
}

pub type SkillRefresh = Arc<
    dyn for<'a> Fn(&'a dyn Cancellation) -> BoxFuture<'a, Result<(), SlashFailure>> + Send + Sync,
>;

pub struct CommandContext<'a> {
    pub session: &'a mut dyn SlashSession,
    pub activity: &'a dyn SlashActivity,
    pub dialog: &'a mut dyn SlashDialog,
    pub refresh_skills: &'a SkillRefresh,
    pub commands: &'a [CommandDescription],
}

pub trait SlashActivity: Send + Sync {
    fn wait_until_idle<'a>(
        &'a self,
        cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<(), SlashFailure>>;
}

pub trait ApplicationExit: Send + Sync {
    fn exit<'a>(
        &'a self,
        cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<(), SlashFailure>>;
}

pub trait SlashSessionBinding: Send {
    fn replace<'a>(
        &'a mut self,
        session: UserSession,
        cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<(), SlashFailure>>;
}

/// The terminal owner calls `commit` at the same point it commits its binding.
/// Failure restores the previous slash session even if commit was already called.
pub trait ExistingSessionBinding: Send {
    fn bind<'a>(
        &'a mut self,
        commit: &'a mut (dyn FnMut() + Send),
        cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<(), SlashFailure>>;
}

pub trait SlashSession: Send {
    fn current_session(&self) -> &UserSession;
    fn id(&self) -> &str {
        &self.current_session().id
    }
    fn model(&self) -> &str {
        &self.current_session().model
    }
    fn mode(&self) -> &str {
        &self.current_session().mode
    }
    fn select_model<'a>(
        &'a mut self,
        model: &'a str,
        cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<(), SlashFailure>>;
    fn set_model_preset<'a>(
        &'a mut self,
        name: &'a str,
        cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<ModelPreset, SlashFailure>>;
    fn select_model_preset<'a>(
        &'a mut self,
        name: &'a str,
        cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<ModelPreset, SlashFailure>>;
    fn list_model_presets<'a>(
        &'a mut self,
        cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<Vec<ModelPreset>, SlashFailure>>;
    fn select_mode<'a>(
        &'a mut self,
        mode: &'a str,
        cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<(), SlashFailure>>;
    /// Start a fresh session using the configured default model and mode.
    fn start_new<'a>(
        &'a mut self,
        cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<(), SlashFailure>>;
    fn load_existing<'a>(
        &'a mut self,
        replacement: UserSession,
        binding: &'a mut dyn ExistingSessionBinding,
        cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<(), SlashFailure>>;
    fn set_goal<'a>(
        &'a mut self,
        goal: &'a str,
        cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<(), SlashFailure>>;
    fn clear_goal<'a>(
        &'a mut self,
        cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<(), SlashFailure>>;
    fn set_standing_instruction<'a>(
        &'a mut self,
        instruction: &'a str,
        cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<(), SlashFailure>>;
    fn compact<'a>(
        &'a mut self,
        target: Option<&'a str>,
        cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<(), SlashFailure>>;
    fn set_context_limit<'a>(
        &'a mut self,
        limit: &'a str,
        cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<SetContextLimitResponse, SlashFailure>>;
    fn sandbox_enable<'a>(
        &'a mut self,
        enabled: bool,
        cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<SandboxEnableResponse, SlashFailure>>;
    fn list_skills<'a>(
        &'a mut self,
        cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<ListSkillsResponse, SlashFailure>>;
    fn configure_skill<'a>(
        &'a mut self,
        path: &'a str,
        enabled: bool,
        cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<Skill, SlashFailure>>;
}
