use parrot_domain::{
    contracts::async_ops::{BoxFuture, Cancellation},
    protocol::messages::parrot as wire,
};
use parrot_host::web::router::WebClient;
use tonic::Code;

use crate::dispatch::dialog::{DialogOption, SlashDialog};

use super::{CommandContext, SlashCommand, SlashFailure, load_command, native_call};

#[derive(Clone)]
pub struct ModeSelection {
    client: WebClient,
}

impl ModeSelection {
    pub fn new(client: WebClient) -> Self {
        Self { client }
    }

    pub async fn select(
        &self,
        dialog: &mut dyn SlashDialog,
        cancellation: &dyn Cancellation,
    ) -> Result<Option<String>, SlashFailure> {
        let mut client = self.client.clone();
        let listed = load_command(
            dialog,
            "Loading modes…",
            native_call(
                client.list_modes(wire::ListModesRequest::new()),
                cancellation,
            ),
            cancellation,
        )
        .await?;
        let options: Vec<_> = listed
            .modes
            .into_iter()
            .map(|mode| DialogOption::new(mode.id.clone(), mode.id, "Session mode"))
            .collect();
        if options.is_empty() {
            dialog
                .show_error("no modes are available", cancellation)
                .await?;
            return Ok(None);
        }
        Ok(dialog
            .select("Select a mode", &options, cancellation)
            .await?
            .map(|option| option.id))
    }
}

pub struct ModeCommand {
    selection: ModeSelection,
}
impl ModeCommand {
    pub fn new(selection: ModeSelection) -> Self {
        Self { selection }
    }
}
impl SlashCommand for ModeCommand {
    fn name(&self) -> &str {
        "/mode"
    }
    fn summary(&self) -> &str {
        "Switch the mode for this session"
    }
    fn run<'a>(
        &'a self,
        _: &'a str,
        context: &'a mut CommandContext<'_>,
        cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<(), SlashFailure>> {
        Box::pin(async move {
            let Some(selected) = self.selection.select(context.dialog, cancellation).await? else {
                return Ok(());
            };
            context.activity.wait_until_idle(cancellation).await?;
            context.session.select_mode(&selected, cancellation).await?;
            (context.refresh_skills)(cancellation).await?;
            context
                .dialog
                .show(&[format!("mode is now {selected}")], cancellation)
                .await?;
            Ok(())
        })
    }
}

pub struct ModesCommand {
    client: WebClient,
}
impl ModesCommand {
    pub fn new(client: WebClient) -> Self {
        Self { client }
    }
}
impl SlashCommand for ModesCommand {
    fn name(&self) -> &str {
        "/modes"
    }
    fn summary(&self) -> &str {
        "List the available modes"
    }
    fn run<'a>(
        &'a self,
        _: &'a str,
        context: &'a mut CommandContext<'_>,
        cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<(), SlashFailure>> {
        Box::pin(async move {
            let mut client = self.client.clone();
            let listed = load_command(
                context.dialog,
                "Loading modes…",
                native_call(
                    client.list_modes(wire::ListModesRequest::new()),
                    cancellation,
                ),
                cancellation,
            )
            .await?;
            let lines: Vec<_> = listed.modes.into_iter().map(|mode| mode.id).collect();
            context.dialog.show(&lines, cancellation).await?;
            Ok(())
        })
    }
}

pub struct SessionsCommand {
    client: WebClient,
}
impl SessionsCommand {
    pub fn new(client: WebClient) -> Self {
        Self { client }
    }
}
impl SlashCommand for SessionsCommand {
    fn name(&self) -> &str {
        "/sessions"
    }
    fn summary(&self) -> &str {
        "List sessions available on this server"
    }
    fn run<'a>(
        &'a self,
        _: &'a str,
        context: &'a mut CommandContext<'_>,
        cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<(), SlashFailure>> {
        Box::pin(async move {
            let mut client = self.client.clone();
            let listed = load_command(
                context.dialog,
                "Loading sessions…",
                native_call(
                    client.list_sessions(wire::ListSessionsRequest::new()),
                    cancellation,
                ),
                cancellation,
            )
            .await;
            let mut listed = match listed {
                Ok(listed) => listed,
                Err(SlashFailure::Rpc(status)) if status.code() == Code::Unimplemented => {
                    context
                        .dialog
                        .show_error(
                            "session listing is unavailable on this server",
                            cancellation,
                        )
                        .await?;
                    return Ok(());
                }
                Err(failure) => return Err(failure),
            };
            listed.sessions.sort_by(|left, right| {
                left.created_at
                    .encode_utf16()
                    .cmp(right.created_at.encode_utf16())
                    .then_with(|| {
                        left.user_session_id
                            .encode_utf16()
                            .cmp(right.user_session_id.encode_utf16())
                    })
            });
            let lines: Vec<_> = listed
                .sessions
                .into_iter()
                .map(|item| {
                    let marker = if item.user_session_id == context.session.id() {
                        "*"
                    } else {
                        " "
                    };
                    let name = if item.root_agent_name.is_empty() {
                        "<unnamed>"
                    } else {
                        &item.root_agent_name
                    };
                    let model = if item.model.is_empty() {
                        "<unknown>"
                    } else {
                        &item.model
                    };
                    let state = match item.state.enum_value() {
                        Ok(wire::SessionState::SESSION_STATE_ACTIVE) => "active",
                        Ok(wire::SessionState::SESSION_STATE_INACTIVE) => "inactive",
                        Ok(wire::SessionState::SESSION_STATE_CORRUPT) => "corrupt",
                        _ => "unknown",
                    };
                    format!(
                        "{marker} {name}  {}  {model}  {state}",
                        item.user_session_id
                    )
                })
                .collect();
            context.dialog.show(&lines, cancellation).await?;
            Ok(())
        })
    }
}

pub struct StatusCommand {
    client: WebClient,
}
impl StatusCommand {
    pub fn new(client: WebClient) -> Self {
        Self { client }
    }
}
impl SlashCommand for StatusCommand {
    fn name(&self) -> &str {
        "/status"
    }
    fn summary(&self) -> &str {
        "Show the agent status prompt and subscription usage"
    }
    fn run<'a>(
        &'a self,
        _: &'a str,
        context: &'a mut CommandContext<'_>,
        cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<(), SlashFailure>> {
        Box::pin(async move {
            let mut client = self.client.clone();
            let status = load_command(
                context.dialog,
                "Loading status…",
                native_call(
                    client.session_status(wire::SessionStatusRequest {
                        user_session_id: context.session.id().into(),
                        ..Default::default()
                    }),
                    cancellation,
                ),
                cancellation,
            )
            .await;
            let status = match status {
                Ok(status) => status,
                Err(SlashFailure::Rpc(status))
                    if matches!(status.code(), Code::Unimplemented | Code::NotFound) =>
                {
                    let message = if status.code() == Code::Unimplemented {
                        "session status is unavailable on this server"
                    } else {
                        "no status is available yet for this session"
                    };
                    context.dialog.show_error(message, cancellation).await?;
                    return Ok(());
                }
                Err(failure) => return Err(failure),
            };
            let mut lines = Vec::new();
            if !status.status.is_empty() {
                lines.push(status.status);
            }
            lines.extend(status.usage_lines);
            if lines.is_empty() {
                lines.push("no status is currently available".into());
            }
            context.dialog.print(&lines, cancellation).await?;
            Ok(())
        })
    }
}
