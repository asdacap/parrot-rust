use std::sync::Arc;

use chrono::{DateTime, Local, NaiveDateTime, Utc};
use parrot_domain::{
    contracts::async_ops::{BoxFuture, Cancellation},
    protocol::messages::parrot as wire,
};
use tonic::Code;

use super::{CommandContext, SlashCommand, SlashFailure, load_command};
use crate::dispatch::dialog::DialogOption;

/// Consumer seam for the original ITerminalSessionController. The terminal
/// owner implements navigation/binding; this command does not recreate it.
pub trait WorkspaceNavigation: Send + Sync {
    fn list<'a>(
        &'a self,
        cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<wire::ListSessionsResponse, NavigationFailure>>;
    fn load<'a>(
        &'a self,
        session_id: &'a str,
        cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<(), NavigationFailure>>;
}

#[derive(Debug)]
pub enum NavigationFailure {
    Rpc(tonic::Status),
    /// Source InvalidOperationException, IOException, or TimeoutException.
    Recoverable(String),
    Other(String),
}
impl From<tonic::Status> for NavigationFailure {
    fn from(status: tonic::Status) -> Self {
        Self::Rpc(status)
    }
}
impl From<SlashFailure> for NavigationFailure {
    fn from(failure: SlashFailure) -> Self {
        match failure {
            SlashFailure::Rpc(status) => Self::Rpc(status),
            SlashFailure::Command(message) => Self::Other(message),
        }
    }
}
impl From<NavigationFailure> for SlashFailure {
    fn from(failure: NavigationFailure) -> Self {
        match failure {
            NavigationFailure::Rpc(status) => Self::Rpc(status),
            NavigationFailure::Recoverable(message) | NavigationFailure::Other(message) => {
                Self::Command(message)
            }
        }
    }
}

pub struct WorkspaceSessionsCommand {
    navigation: Arc<dyn WorkspaceNavigation>,
}
impl WorkspaceSessionsCommand {
    pub fn new(navigation: Arc<dyn WorkspaceNavigation>) -> Self {
        Self { navigation }
    }

    async fn run_inner(
        &self,
        context: &mut CommandContext<'_>,
        cancellation: &dyn Cancellation,
    ) -> Result<(), NavigationFailure> {
        let listed = navigation_load(
            context.dialog,
            "Loading workspace sessions…",
            self.navigation.list(cancellation),
            cancellation,
        )
        .await?;
        if listed.sessions.is_empty() {
            context
                .dialog
                .show(
                    &["No sessions in the current workspace.".into()],
                    cancellation,
                )
                .await?;
            return Ok(());
        }
        let options = workspace_options(&listed.sessions, context.session.id());
        let selected = context
            .dialog
            .select("Workspace sessions", &options, cancellation)
            .await?;
        let Some(selected) = selected else {
            return Ok(());
        };
        if selected.id == context.session.id() {
            return Ok(());
        }
        let mut matching = listed
            .sessions
            .iter()
            .filter(|session| session.user_session_id == selected.id);
        let item = matching.next().ok_or_else(|| {
            NavigationFailure::Recoverable("Sequence contains no matching element".into())
        })?;
        if matching.next().is_some() {
            return Err(NavigationFailure::Recoverable(
                "Sequence contains more than one matching element".into(),
            ));
        }
        if item.state.enum_value().ok() == Some(wire::SessionState::SESSION_STATE_CORRUPT) {
            context
                .dialog
                .show_error("session metadata is corrupt", cancellation)
                .await?;
            return Ok(());
        }
        context.activity.wait_until_idle(cancellation).await?;
        navigation_load(
            context.dialog,
            "Loading selected session…",
            self.navigation.load(&selected.id, cancellation),
            cancellation,
        )
        .await
    }
}
impl SlashCommand for WorkspaceSessionsCommand {
    fn name(&self) -> &str {
        "/workspace-sessions"
    }
    fn summary(&self) -> &str {
        "Select a session from the current workspace"
    }
    fn run<'a>(
        &'a self,
        _: &'a str,
        context: &'a mut CommandContext<'_>,
        cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<(), SlashFailure>> {
        Box::pin(async move {
            match self.run_inner(context, cancellation).await {
                Ok(()) => Ok(()),
                Err(NavigationFailure::Rpc(status))
                    if !cancellation.is_cancelled() && status.code() != Code::Cancelled =>
                {
                    let message = if status.code() == Code::Unimplemented {
                        "workspace session selection is unavailable on this server"
                    } else {
                        status.message()
                    };
                    context.dialog.show_error(message, cancellation).await?;
                    Ok(())
                }
                Err(NavigationFailure::Recoverable(message)) if !cancellation.is_cancelled() => {
                    context.dialog.show_error(&message, cancellation).await?;
                    Ok(())
                }
                Err(failure) => Err(failure.into()),
            }
        })
    }
}

async fn navigation_load<T: Send>(
    dialog: &mut dyn crate::dispatch::dialog::SlashDialog,
    activity: &str,
    work: BoxFuture<'_, Result<T, NavigationFailure>>,
    cancellation: &dyn Cancellation,
) -> Result<T, NavigationFailure> {
    let mut original = None;
    let result = load_command(
        dialog,
        activity,
        async {
            match work.await {
                Ok(value) => Ok(value),
                Err(failure) => {
                    let projected = match &failure {
                        NavigationFailure::Rpc(status) => SlashFailure::Rpc(status.clone()),
                        NavigationFailure::Recoverable(message)
                        | NavigationFailure::Other(message) => {
                            SlashFailure::Command(message.clone())
                        }
                    };
                    original = Some(failure);
                    Err(projected)
                }
            }
        },
        cancellation,
    )
    .await;
    result.map_err(|failure| original.unwrap_or_else(|| failure.into()))
}

/// The actual storage producer uses yyyy-MM-ddTHH:mm:ss.fffffff+00:00;
/// historical offset-bearing roundtrip values are preserved by storage. ROOT
/// explicitly leaves broader non-ISO invariant DateTimeOffset forms OPEN.
pub fn parse_creation_instant(value: &str) -> Option<DateTime<Utc>> {
    let value = value.trim();
    DateTime::parse_from_rfc3339(value)
        .ok()
        .map(|value| value.with_timezone(&Utc))
        .or_else(|| {
            NaiveDateTime::parse_from_str(value, "%Y-%m-%dT%H:%M:%S%.f")
                .ok()
                .map(|value| value.and_utc())
        })
}

pub fn workspace_options(sessions: &[wire::SessionSummary], current_id: &str) -> Vec<DialogOption> {
    let mut items: Vec<_> = sessions
        .iter()
        .map(|session| (session, parse_creation_instant(&session.created_at)))
        .collect();
    items.sort_by(|(left, left_created), (right, right_created)| {
        right_created.cmp(left_created).then_with(|| {
            left.user_session_id
                .encode_utf16()
                .cmp(right.user_session_id.encode_utf16())
        })
    });
    items
        .into_iter()
        .map(|(session, instant)| {
            let marker = if session.user_session_id == current_id {
                "*"
            } else {
                " "
            };
            let name = if session.root_agent_name.is_empty() {
                "<unnamed>"
            } else {
                &session.root_agent_name
            };
            let created = instant
                .map(|instant| {
                    instant
                        .with_timezone(&Local)
                        .format("%Y-%m-%d %H:%M:%S %:z")
                        .to_string()
                })
                .unwrap_or_else(|| "unknown creation time".into());
            DialogOption::new(
                &session.user_session_id,
                format!("{marker} {name}  {}", session.user_session_id),
                format!("{}  {created}", session_state(session.state)),
            )
        })
        .collect()
}

pub(crate) fn session_state(state: protobuf::EnumOrUnknown<wire::SessionState>) -> &'static str {
    match state.enum_value().ok() {
        Some(wire::SessionState::SESSION_STATE_ACTIVE) => "active",
        Some(wire::SessionState::SESSION_STATE_INACTIVE) => "inactive",
        Some(wire::SessionState::SESSION_STATE_CORRUPT) => "corrupt",
        _ => "unknown",
    }
}
