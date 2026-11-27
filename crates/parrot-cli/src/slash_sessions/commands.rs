use std::sync::Arc;

use crate::slash_settings::{
    AuthCommand, AuthPresetSelectCommand, AuthPresetSetCommand, DecisionModelCommand,
    EffortCommand, FastCommand, ModelAliasCommand, ModelCommand, ModelPresetSelectCommand,
    ModelPresetSetCommand, ModelWizard, ModelsCommand, SafetyClassifierEnabledCommand,
    SetContextLimitCommand, SettingsServices,
};
use parrot_domain::contracts::async_ops::{BoxFuture, Cancellation};

use super::{
    CommandEnvironment, CompactCommand, GoalCommand, ModeCommand, ModeSelection, ModesCommand,
    SandboxEnableCommand, SessionsCommand, SkillsCommand, SlashCommand, SlashCommandRegistry,
    SlashFailure, StandingInstructionCommand, StatusCommand, TaskLimitCommand, WorkspaceNavigation,
    WorkspaceSessionsCommand,
};

pub struct ClearCommand;
impl SlashCommand for ClearCommand {
    fn name(&self) -> &str {
        "/clear"
    }
    fn summary(&self) -> &str {
        "Start a fresh session, keeping the old one"
    }
    fn run<'a>(
        &'a self,
        _: &'a str,
        environment: &'a mut CommandEnvironment<'_>,
        c: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<(), SlashFailure>> {
        Box::pin(async move {
            environment.activity.wait_until_idle(c).await?;
            environment.session.start_new(c).await?;
            environment
                .dialog
                .show(&[format!("new session {}", environment.session.id())], c)
                .await?;
            Ok(())
        })
    }
}

pub struct HelpCommand;
impl SlashCommand for HelpCommand {
    fn name(&self) -> &str {
        "/help"
    }
    fn summary(&self) -> &str {
        "List the commands"
    }
    fn run<'a>(
        &'a self,
        _: &'a str,
        environment: &'a mut CommandEnvironment<'_>,
        c: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<(), SlashFailure>> {
        Box::pin(async move {
            let mut commands: Vec<_> = environment.commands.iter().collect();
            commands.sort_by(|left, right| left.name.encode_utf16().cmp(right.name.encode_utf16()));
            let lines: Vec<_> = commands
                .into_iter()
                .map(|command| format!("{:<12} {}", command.name, command.summary))
                .collect();
            environment.dialog.show(&lines, c).await?;
            Ok(())
        })
    }
}

pub struct VersionCommand {
    version: String,
}
impl VersionCommand {
    pub fn new(version: impl Into<String>) -> Self {
        Self {
            version: version.into(),
        }
    }
}
impl SlashCommand for VersionCommand {
    fn name(&self) -> &str {
        "/version"
    }
    fn summary(&self) -> &str {
        "Print the build version"
    }
    fn run<'a>(
        &'a self,
        _: &'a str,
        environment: &'a mut CommandEnvironment<'_>,
        c: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<(), SlashFailure>> {
        Box::pin(async move {
            environment
                .dialog
                .show(&[format!("parrot {}", self.version)], c)
                .await
                .map_err(Into::into)
        })
    }
}

pub struct ExitCommand {
    application: ExitApplication,
}
impl ExitCommand {
    pub fn new(application: ExitApplication) -> Self {
        Self { application }
    }
}
impl SlashCommand for ExitCommand {
    fn name(&self) -> &str {
        "/exit"
    }
    fn summary(&self) -> &str {
        "Leave the session"
    }
    fn run<'a>(
        &'a self,
        _: &'a str,
        _: &'a mut CommandEnvironment<'_>,
        c: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<(), SlashFailure>> {
        (self.application)(c)
    }
}

pub type ExitApplication = Arc<
    dyn for<'a> Fn(&'a dyn Cancellation) -> BoxFuture<'a, Result<(), SlashFailure>> + Send + Sync,
>;

/// The ONE real SlashCommands.Create registry. Both CLI and browser consumers
/// construct these same T66 settings handlers and T67 session-control handlers.
pub struct SlashCommands;
impl SlashCommands {
    pub fn create(
        services: SettingsServices,
        application: Option<ExitApplication>,
        version: impl Into<String>,
    ) -> SlashCommandRegistry {
        Self::create_core(services, application, version.into(), None)
    }
    pub fn create_terminal(
        services: SettingsServices,
        application: Option<ExitApplication>,
        version: impl Into<String>,
        navigation: Arc<dyn WorkspaceNavigation>,
    ) -> SlashCommandRegistry {
        Self::create_core(services, application, version.into(), Some(navigation))
    }
    fn create_core(
        services: SettingsServices,
        application: Option<ExitApplication>,
        version: String,
        navigation: Option<Arc<dyn WorkspaceNavigation>>,
    ) -> SlashCommandRegistry {
        let client = services.client;
        let models = ModelWizard::new(client.clone());
        let modes = ModeSelection::new(client.clone());
        let mut commands: Vec<Box<dyn SlashCommand>> = vec![
            Box::new(AuthCommand::new(services.auth.clone())),
            Box::new(AuthPresetSelectCommand::new(services.auth.clone())),
            Box::new(AuthPresetSetCommand::new(services.auth)),
            Box::new(ClearCommand),
            Box::new(CompactCommand),
            Box::new(EffortCommand::new(client.clone())),
            Box::new(FastCommand),
            Box::new(SafetyClassifierEnabledCommand),
            Box::new(DecisionModelCommand),
        ];
        if let Some(application) = application {
            commands.push(Box::new(ExitCommand::new(application)));
        }
        commands.extend([
            Box::new(GoalCommand) as Box<dyn SlashCommand>,
            Box::new(StandingInstructionCommand),
            Box::new(HelpCommand),
            Box::new(ModeCommand::new(modes)),
            Box::new(ModelCommand::new(models.clone())),
            Box::new(ModelAliasCommand::new(client.clone(), models)),
            Box::new(ModelPresetSelectCommand),
            Box::new(ModelPresetSetCommand),
            Box::new(ModelsCommand::new(client.clone())),
            Box::new(ModesCommand::new(client.clone())),
            Box::new(SessionsCommand::new(client.clone())),
            Box::new(StatusCommand::new(client)),
            Box::new(SandboxEnableCommand),
            Box::new(SetContextLimitCommand),
            Box::new(SkillsCommand),
            Box::new(TaskLimitCommand),
            Box::new(VersionCommand::new(version)),
        ]);
        if let Some(navigation) = navigation {
            commands.push(Box::new(WorkspaceSessionsCommand::new(navigation)));
        }
        SlashCommandRegistry::new(commands)
    }
}
