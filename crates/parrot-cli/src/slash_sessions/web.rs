use std::sync::{Arc, Mutex};

use parrot_domain::{
    contracts::async_ops::{BoxFuture, Cancellation},
    protocol::messages::{parrot as wire, web},
};
use parrot_foundation::config::Configuration;
use parrot_host::web::{
    dialog::WebSlashDialog,
    slash::{WebSlashContext, WebSlashFactory, WebSlashFailure, WebSlashRegistry},
};
use tonic::Status;

use super::{
    CallbackSessionBinding, CommandContext, PollingSlashActivity, RpcSlashSession, SkillRefresh,
    SlashActivity, SlashCommandRegistry, SlashCommands, SlashFailure, SlashSession,
};
use crate::{
    dispatch::dialog::{DialogOption, SlashDialog},
    slash_settings::{AuthServices, SettingsServices},
};

/// Only an adapter to the delivered web dialog. Prompt/run/answer IDs and load
/// completion balancing stay in WebSlashDialog; no second protocol is created.
pub struct BrowserSlashDialog(pub Arc<WebSlashDialog>);
impl SlashDialog for BrowserSlashDialog {
    fn select<'a>(
        &'a mut self,
        title: &'a str,
        options: &'a [DialogOption],
        c: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<Option<DialogOption>, Status>> {
        Box::pin(async move {
            let options: Vec<_> = options
                .iter()
                .map(|option| web::SlashOption {
                    id: option.id.clone(),
                    label: option.label.clone(),
                    description: option.description.clone(),
                    ..Default::default()
                })
                .collect();
            Ok(self
                .0
                .select(title, &options, c)
                .await?
                .map(|option| DialogOption::new(option.id, option.label, option.description)))
        })
    }
    fn read_text<'a>(
        &'a mut self,
        prompt: &'a str,
        c: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<Option<String>, Status>> {
        Box::pin(async move { self.0.read_text(prompt, c).await })
    }
    fn read_secret<'a>(
        &'a mut self,
        prompt: &'a str,
        c: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<Option<String>, Status>> {
        Box::pin(async move { self.0.read_secret(prompt, c).await })
    }
    fn confirm<'a>(
        &'a mut self,
        lines: &'a [String],
        c: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<bool, Status>> {
        Box::pin(async move { self.0.confirm(lines, c).await })
    }
    fn show<'a>(
        &'a mut self,
        lines: &'a [String],
        c: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<(), Status>> {
        Box::pin(async move { self.0.show(lines, c).await })
    }
    fn print<'a>(
        &'a mut self,
        lines: &'a [String],
        c: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<(), Status>> {
        Box::pin(async move { self.0.print(lines, c).await })
    }
    fn show_error<'a>(
        &'a mut self,
        message: &'a str,
        c: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<(), Status>> {
        Box::pin(async move { self.0.show_error(message, c).await })
    }
    fn load<'a>(
        &'a mut self,
        activity: &'a str,
        work: BoxFuture<'a, Result<(), Status>>,
        c: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<(), Status>> {
        Box::pin(async move { self.0.load(activity, work, c).await })
    }
}

/// Real source registry/session/activity factory consumed by the existing web
/// service. The final application supplies actual configuration/auth services.
pub struct WebCommandFactory {
    configuration: Arc<Mutex<Configuration>>,
    auth: Arc<AuthServices>,
    version: String,
}
impl WebCommandFactory {
    pub fn new(
        configuration: Arc<Mutex<Configuration>>,
        auth: Arc<AuthServices>,
        version: impl Into<String>,
    ) -> Self {
        Self {
            configuration,
            auth,
            version: version.into(),
        }
    }
}
impl WebSlashFactory for WebCommandFactory {
    fn create(&self, context: WebSlashContext) -> Box<dyn WebSlashRegistry> {
        let dialog = context.dialog.clone();
        let binding = CallbackSessionBinding(Arc::new(move |session, c| {
            let dialog = dialog.clone();
            Box::pin(async move {
                dialog
                    .session_replaced(session, c)
                    .await
                    .map_err(Into::into)
            })
        }));
        let session = RpcSlashSession::new(
            context.client.clone(),
            context.initial_session,
            self.configuration.clone(),
            true,
            Box::new(binding),
        );
        let registry = SlashCommands::create(
            SettingsServices {
                client: context.client,
                auth: self.auth.clone(),
            },
            None,
            &self.version,
        );
        let activity = PollingSlashActivity(Arc::new(move || context.activity.is_busy()));
        // The pinned WebSlashService deliberately has no terminal skill menu.
        let refresh: SkillRefresh = Arc::new(|_| Box::pin(async { Ok(()) }));
        Box::new(WebCommandRegistry {
            registry,
            session,
            dialog: BrowserSlashDialog(context.dialog),
            activity: Box::new(activity),
            refresh,
        })
    }
}

struct WebCommandRegistry {
    registry: SlashCommandRegistry,
    session: RpcSlashSession,
    dialog: BrowserSlashDialog,
    activity: Box<dyn SlashActivity>,
    refresh: SkillRefresh,
}
impl WebSlashRegistry for WebCommandRegistry {
    fn commands(&self) -> Vec<web::SlashCommand> {
        self.registry
            .descriptions()
            .into_iter()
            .map(|command| web::SlashCommand {
                name: command.name,
                summary: command.summary,
                ..Default::default()
            })
            .collect()
    }
    fn dispatch<'a>(
        &'a mut self,
        text: &'a str,
        c: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<(), WebSlashFailure>> {
        Box::pin(async move {
            let mut context = CommandContext {
                session: &mut self.session,
                activity: self.activity.as_ref(),
                dialog: &mut self.dialog,
                refresh_skills: &self.refresh,
                commands: &[],
            };
            self.registry
                .dispatch(text, &mut context, c)
                .await
                .map_err(|failure| match failure {
                    SlashFailure::Rpc(status) => WebSlashFailure::Rpc(status),
                    SlashFailure::Command(message) => WebSlashFailure::Command(message),
                })
        })
    }
    fn current_session(&self) -> wire::UserSession {
        self.session.current_session().clone()
    }
}
