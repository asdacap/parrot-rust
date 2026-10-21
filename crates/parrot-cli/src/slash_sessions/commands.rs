use std::sync::Arc;

use crate::slash_settings::{
    AuthCommand, AuthPresetSelectCommand, AuthPresetSetCommand, EffortCommand, ModelAliasCommand,
    ModelCommand, ModelPresetSelectCommand, ModelPresetSetCommand, ModelWizard, ModelsCommand,
    SetContextLimitCommand, SettingsServices,
};
use parrot_domain::contracts::async_ops::{BoxFuture, Cancellation};

use super::{
    ApplicationExit, CommandContext, CompactCommand, GoalCommand, ModeCommand, ModeSelection,
    ModesCommand, SandboxEnableCommand, SessionsCommand, SkillsCommand, SlashCommand,
    SlashCommandRegistry, SlashFailure, StandingInstructionCommand, StatusCommand,
    WorkspaceNavigation, WorkspaceSessionsCommand,
};

pub struct ClearCommand {
    models: ModelWizard,
    modes: ModeSelection,
}
impl ClearCommand {
    pub fn new(models: ModelWizard, modes: ModeSelection) -> Self {
        Self { models, modes }
    }
}
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
        context: &'a mut CommandContext<'_>,
        c: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<(), SlashFailure>> {
        Box::pin(async move {
            let Some(model) = self.models.select(None, context.dialog, c).await? else {
                return Ok(());
            };
            let Some(mode) = self.modes.select(context.dialog, c).await? else {
                return Ok(());
            };
            context.activity.wait_until_idle(c).await?;
            context.session.start_new(&model, &mode, c).await?;
            context
                .dialog
                .show(&[format!("new session {}", context.session.id())], c)
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
        context: &'a mut CommandContext<'_>,
        c: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<(), SlashFailure>> {
        Box::pin(async move {
            let mut commands: Vec<_> = context.commands.iter().collect();
            commands.sort_by(|left, right| left.name.encode_utf16().cmp(right.name.encode_utf16()));
            let lines: Vec<_> = commands
                .into_iter()
                .map(|command| format!("{:<12} {}", command.name, command.summary))
                .collect();
            context.dialog.show(&lines, c).await?;
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
        context: &'a mut CommandContext<'_>,
        c: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<(), SlashFailure>> {
        Box::pin(async move {
            context
                .dialog
                .show(&[format!("parrot {}", self.version)], c)
                .await
                .map_err(Into::into)
        })
    }
}

pub struct ExitCommand {
    application: Arc<dyn ApplicationExit>,
}
impl ExitCommand {
    pub fn new(application: Arc<dyn ApplicationExit>) -> Self {
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
        _: &'a mut CommandContext<'_>,
        c: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<(), SlashFailure>> {
        self.application.exit(c)
    }
}

pub type ExitApplication =
    Arc<dyn Fn() -> BoxFuture<'static, Result<(), SlashFailure>> + Send + Sync>;
pub struct CallbackApplicationExit(pub ExitApplication);
impl ApplicationExit for CallbackApplicationExit {
    fn exit<'a>(&'a self, c: &'a dyn Cancellation) -> BoxFuture<'a, Result<(), SlashFailure>> {
        Box::pin(async move {
            c.check()
                .map_err(|_| tonic::Status::cancelled("operation cancelled"))?;
            (self.0)().await
        })
    }
}

/// The ONE real SlashCommands.Create registry. Both CLI and browser consumers
/// construct these same T66 settings handlers and T67 session-control handlers.
pub struct SlashCommands;
impl SlashCommands {
    pub fn create(
        services: SettingsServices,
        application: Option<Arc<dyn ApplicationExit>>,
        version: impl Into<String>,
    ) -> SlashCommandRegistry {
        Self::create_core(services, application, version.into(), None)
    }
    pub fn create_terminal(
        services: SettingsServices,
        application: Option<Arc<dyn ApplicationExit>>,
        version: impl Into<String>,
        navigation: Arc<dyn WorkspaceNavigation>,
    ) -> SlashCommandRegistry {
        Self::create_core(services, application, version.into(), Some(navigation))
    }
    fn create_core(
        services: SettingsServices,
        application: Option<Arc<dyn ApplicationExit>>,
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
            Box::new(ClearCommand::new(models.clone(), modes.clone())),
            Box::new(CompactCommand),
            Box::new(EffortCommand::new(client.clone())),
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
            Box::new(VersionCommand::new(version)),
        ]);
        if let Some(navigation) = navigation {
            commands.push(Box::new(WorkspaceSessionsCommand::new(navigation)));
        }
        SlashCommandRegistry::new(commands)
    }
}
