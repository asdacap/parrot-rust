//! Loop plumbing shared by the basic and enhanced chat sessions: the pending
//! interaction queue, the session event binding, and the interrupt and
//! reconciliation rounds both loops drive.

use std::collections::{HashSet, VecDeque};
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, Ordering},
};
use std::time::Duration;
use std::{fmt, io};

use parrot_core::Cancellation;
use parrot_domain::protocol::messages::parrot as wire;
use parrot_host::web::router::WebClient;
use parrot_runtime::agent_turn::lifecycle::Signal;
use tokio::sync::Notify;
use tonic::{Code, Status};

use crate::attachments::uploader::AttachmentError;
use crate::dispatch::basic_render::RenderStreamError;
use crate::dispatch::prepared_stream::PreparedSessionStream;
use crate::slash_sessions::{
    CallbackApplicationExit, NavigationFailure, SlashCommandRegistry, SlashCommands, SlashFailure,
    native_call,
};
use crate::slash_settings::{AuthServices, SettingsServices};
use crate::terminal_dialogs::navigation::TerminalSessionController;

#[derive(Debug)]
pub enum ChatLoopFailure {
    Rpc(Status),
    Slash(SlashFailure),
    Io(io::Error),
    Attachment(AttachmentError),
    Navigation(NavigationFailure),
}
impl fmt::Display for ChatLoopFailure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Rpc(error) => error.fmt(f),
            Self::Slash(error) => error.fmt(f),
            Self::Io(error) => error.fmt(f),
            Self::Attachment(error) => error.fmt(f),
            Self::Navigation(NavigationFailure::Rpc(error)) => error.fmt(f),
            Self::Navigation(
                NavigationFailure::Recoverable(message) | NavigationFailure::Other(message),
            ) => f.write_str(message),
        }
    }
}
impl std::error::Error for ChatLoopFailure {}
impl From<Status> for ChatLoopFailure {
    fn from(error: Status) -> Self {
        Self::Rpc(error)
    }
}
impl From<SlashFailure> for ChatLoopFailure {
    fn from(error: SlashFailure) -> Self {
        Self::Slash(error)
    }
}
impl From<io::Error> for ChatLoopFailure {
    fn from(error: io::Error) -> Self {
        Self::Io(error)
    }
}
impl From<AttachmentError> for ChatLoopFailure {
    fn from(error: AttachmentError) -> Self {
        Self::Attachment(error)
    }
}
impl From<NavigationFailure> for ChatLoopFailure {
    fn from(error: NavigationFailure) -> Self {
        Self::Navigation(error)
    }
}
impl From<RenderStreamError> for ChatLoopFailure {
    fn from(error: RenderStreamError) -> Self {
        match error {
            RenderStreamError::Output(error) => Self::Io(error),
            RenderStreamError::Rpc(error) => Self::Rpc(error),
            RenderStreamError::Cancelled => Self::Rpc(Status::cancelled("operation cancelled")),
        }
    }
}

/// Last observed remote session identity the loop helpers act on.
#[derive(Clone)]
pub(crate) struct SessionSnapshot {
    pub(crate) session: wire::UserSession,
    pub(crate) client: WebClient,
    pub(crate) generation: u64,
}

/// The session event stream a loop replays from and the signal that retires it
/// before the stream itself is closed.
pub(crate) struct Binding {
    pub(crate) stream: PreparedSessionStream,
    pub(crate) stop: Arc<Signal>,
}

/// Cancels the current binding so its next stream read hands the lock back.
pub(crate) fn stop_binding(binding_stop: &Mutex<Arc<Signal>>) {
    binding_stop.lock().expect("binding stop poisoned").cancel();
}

#[derive(Default)]
pub(crate) struct Interactions {
    pub(crate) pending: VecDeque<Interaction>,
    pub(crate) questions: HashSet<String>,
    pub(crate) permissions: HashSet<String>,
}

pub(crate) enum Interaction {
    Question {
        generation: u64,
        session_id: String,
        pending: wire::PendingQuestion,
    },
    Permission {
        generation: u64,
        session_id: String,
        pending: wire::PendingPermission,
    },
    Plan {
        generation: u64,
        completed: wire::PlanCompleted,
    },
}

impl Interaction {
    pub(crate) fn generation(&self) -> u64 {
        match self {
            Self::Question { generation, .. }
            | Self::Permission { generation, .. }
            | Self::Plan { generation, .. } => *generation,
        }
    }
}

impl Interactions {
    /// Offers a listed question once per id; returns whether it was new.
    pub(crate) fn offer_question(
        &mut self,
        generation: u64,
        session_id: &str,
        pending: wire::PendingQuestion,
    ) -> bool {
        if !self.questions.insert(pending.id.clone()) {
            return false;
        }
        self.pending.push_back(Interaction::Question {
            generation,
            session_id: session_id.to_owned(),
            pending,
        });
        true
    }

    /// Offers a pending permission once per id, ahead of or behind the existing
    /// queue; returns whether it was new.
    pub(crate) fn offer_permission(
        &mut self,
        generation: u64,
        session_id: &str,
        pending: &wire::PendingPermission,
        front: bool,
    ) -> bool {
        if !self.permissions.insert(pending.id.clone()) {
            return false;
        }
        let interaction = Interaction::Permission {
            generation,
            session_id: session_id.to_owned(),
            pending: pending.clone(),
        };
        if front {
            self.pending.push_front(interaction);
        } else {
            self.pending.push_back(interaction);
        }
        true
    }
}

/// Lists pending questions and permissions for the snapshot and merges them
/// into the shared queue, letting a generation change discard the listing.
pub(crate) async fn reconcile_interactions(
    snapshot: SessionSnapshot,
    stale: impl Fn(u64) -> bool,
    application: &Arc<Signal>,
    interactions: &Mutex<Interactions>,
    changed: &Notify,
) -> Result<(), SlashFailure> {
    let session_id = snapshot.session.id.clone();
    let questions = native_call(
        snapshot
            .client
            .clone()
            .list_pending_questions(wire::ListPendingQuestionsRequest {
                user_session_id: session_id.clone(),
                ..Default::default()
            }),
        application.as_ref(),
    )
    .await;
    let permissions = native_call(
        snapshot
            .client
            .clone()
            .list_pending_permissions(wire::ListPendingPermissionsRequest {
                user_session_id: session_id.clone(),
                ..Default::default()
            }),
        application.as_ref(),
    )
    .await;
    if stale(snapshot.generation) {
        return Ok(());
    }
    let mut interactions = interactions.lock().expect("interactions poisoned");
    match questions {
        Ok(listed) => {
            interactions
                .questions
                .retain(|id| listed.questions.iter().any(|pending| &pending.id == id));
            for pending in listed.questions {
                interactions.offer_question(snapshot.generation, &session_id, pending);
            }
        }
        Err(error) if application.is_cancelled() => {
            let _ = error;
            return Ok(());
        }
        Err(SlashFailure::Rpc(_)) => {}
        Err(error) => return Err(error),
    }
    match permissions {
        Ok(listed) => {
            interactions
                .permissions
                .retain(|id| listed.permissions.iter().any(|pending| &pending.id == id));
            for pending in listed.permissions {
                interactions.offer_permission(snapshot.generation, &session_id, &pending, false);
            }
        }
        Err(SlashFailure::Rpc(status))
            if matches!(status.code(), Code::Unimplemented | Code::NotFound) => {}
        Err(error) if application.is_cancelled() => {
            let _ = error;
            return Ok(());
        }
        Err(error) => return Err(error),
    }
    if !interactions.pending.is_empty() {
        changed.notify_one();
    }
    Ok(())
}

/// Sends one interrupt request for the session, treating a cancellation racing
/// the request as success.
pub(crate) async fn request_interrupt(
    client: &WebClient,
    session_id: String,
    application: &Arc<Signal>,
) -> Result<(), SlashFailure> {
    if let Err(error) = native_call(
        client.clone().interrupt(wire::InterruptRequest {
            user_session_id: session_id,
            ..Default::default()
        }),
        application.as_ref(),
    )
    .await
    {
        if application.is_cancelled() {
            return Ok(());
        }
        return Err(error);
    }
    Ok(())
}

/// Turns observed interrupts into request rounds until the application stops.
pub(crate) async fn interrupt_loop(
    snapshot: impl Fn() -> SessionSnapshot,
    application: &Arc<Signal>,
    interrupt_pending: &AtomicBool,
    interrupted: &Notify,
) -> Result<(), SlashFailure> {
    loop {
        let notified = interrupted.notified();
        tokio::pin!(notified);
        notified.as_mut().enable();
        if interrupt_pending.swap(false, Ordering::AcqRel) {
            let snapshot = snapshot();
            request_interrupt(&snapshot.client, snapshot.session.id, application).await?;
        }
        tokio::select! {
            biased;
            () = application.cancelled() => return Ok(()),
            () = notified => {}
        }
    }
}

/// Reconciles pending interactions on a fixed interval until the application stops.
pub(crate) async fn reconcile_loop(
    snapshot: impl Fn() -> SessionSnapshot,
    application: &Arc<Signal>,
    interactions: &Mutex<Interactions>,
    changed: &Notify,
) -> Result<(), ChatLoopFailure> {
    let mut interval = tokio::time::interval(Duration::from_millis(250));
    loop {
        tokio::select! { biased; () = application.cancelled() => return Ok(()), _ = interval.tick() => () }
        reconcile_interactions(
            snapshot(),
            |generation| generation != snapshot().generation,
            application,
            interactions,
            changed,
        )
        .await?;
    }
}

/// Builds the slash-command exit callback that cancels the loop lifetime.
pub(crate) fn application_exit(lifetime: Arc<Signal>) -> Arc<CallbackApplicationExit> {
    Arc::new(CallbackApplicationExit(Arc::new(move || {
        let lifetime = lifetime.clone();
        Box::pin(async move {
            lifetime.cancel();
            Ok(())
        })
    })))
}

/// Builds the terminal slash-command registry the input loops rebuild per
/// generation.
pub(crate) fn terminal_commands(
    client: WebClient,
    auth: Arc<AuthServices>,
    exit: Arc<CallbackApplicationExit>,
    version: String,
    controller: Arc<TerminalSessionController>,
) -> SlashCommandRegistry {
    SlashCommands::create_terminal(
        SettingsServices { client, auth },
        Some(exit),
        version,
        controller,
    )
}
