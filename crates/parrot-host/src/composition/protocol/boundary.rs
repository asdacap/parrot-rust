use super::Result;
use parrot_domain::{
    contracts::async_ops::{BoxFuture, Cancellation},
    protocol::messages::parrot as wire,
    values::{context::ContextSize, project_workspace::ProjectWorkspace},
};
use parrot_provider::routing::aliases::ResolvedModelSelection;
use parrot_runtime::{
    interactions::{InteractionError, PendingQuestion, QuestionReply},
    prompt_context::{SkillMetadata, SkillSnapshot},
};
use parrot_storage::{
    artifacts::ImageArtifactMetadata,
    diagnostics::DiagnosticEvent,
    events::{Admission, ConversationPart},
    ownership::OpenedSession,
};
use std::{path::Path, sync::Arc};

/// Validated upload data moved into the actual session admission owner.
pub struct ImageUpload {
    pub content: Vec<u8>,
    pub upload_id: String,
    pub display_name: String,
    pub media_type: String,
    pub byte_length: i64,
}

/// The source IUserSession surface required by the protocol. Implementations
/// own real runtime scopes; this interface supplies no default operation.
pub trait HostedSession: Send + Sync {
    fn snapshot(&self) -> wire::UserSession;
    fn resolve_mode(&self, mode: &str) -> Result<String>;
    fn update(
        &self,
        model: Option<ResolvedModelSelection>,
        mode: Option<String>,
    ) -> BoxFuture<'_, Result<()>>;
    fn update_selection(
        &self,
        selection: ResolvedModelSelection,
    ) -> parrot_provider::routing::configuration::Result<()>;
    fn persist_image<'a>(
        &'a self,
        upload: ImageUpload,
        cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<ImageArtifactMetadata>>;
    fn resolve_image<'a>(
        &'a self,
        id: &'a str,
    ) -> BoxFuture<'a, Result<Option<ImageArtifactMetadata>>>;
    fn pending_questions(&self) -> Result<Vec<PendingQuestion>>;
    fn reply_question(
        &self,
        id: &str,
        reply: QuestionReply,
    ) -> std::result::Result<(), InteractionError>;
    fn reject_question(&self, id: &str) -> std::result::Result<(), InteractionError>;
    fn pending_permissions(&self) -> BoxFuture<'_, Result<Vec<wire::PendingPermission>>>;
    fn reply_permission<'a>(
        &'a self,
        id: &'a str,
        choice: &'a str,
        reason: &'a str,
    ) -> BoxFuture<'a, std::result::Result<(), InteractionError>>;
    fn refresh_skills<'a>(
        &'a self,
        cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<SkillSnapshot>>;
    fn configure_skill<'a>(
        &'a self,
        path: &'a str,
        enabled: bool,
        cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<SkillMetadata>>;
    fn send(
        &self,
        parts: Vec<ConversationPart>,
        message_id: String,
        delivery: wire::Delivery,
        cancellation: Arc<dyn Cancellation>,
    ) -> BoxFuture<'_, Result<Admission>>;
    fn interrupt(&self, cancellation: Arc<dyn Cancellation>) -> BoxFuture<'_, Result<()>>;
    fn compact(
        &self,
        target: Option<ContextSize>,
        cancellation: Arc<dyn Cancellation>,
    ) -> BoxFuture<'_, Result<()>>;
    fn set_goal(
        &self,
        goal: Option<String>,
        cancellation: Arc<dyn Cancellation>,
    ) -> BoxFuture<'_, Result<()>>;
    fn set_standing_instruction(&self, instruction: String) -> Result<()>;
    fn observe_status<'a>(
        &'a self,
        cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<String>>;
    fn listen(
        &self,
        replay_after_event_id: Option<String>,
        cancellation: Arc<dyn Cancellation>,
    ) -> Result<Box<dyn SessionEventStream>>;
    fn record_opened(&self) -> Result<()>;
    fn write_diagnostic(&self, event: &DiagnosticEvent);
    fn close(&self) -> BoxFuture<'_, Result<()>>;
}

/// IUserSessionFactory's host-side binding of the concrete storage acquisition.
/// The factory consumes exactly the capability it created for these resources;
/// implementations verify both ID and Arc identity and retain OpenedSession.
pub trait HostedSessionFactory: Send + Sync {
    fn bind_opened(&self, opened: OpenedSession) -> BoxFuture<'_, Result<Arc<dyn HostedSession>>>;
    fn list_modes(&self) -> Vec<String>;
    fn resolve_mode(&self, mode: &str) -> Result<()>;
    fn resolve_workspace(&self, directory: &Path) -> Result<ProjectWorkspace>;
}

pub trait SessionListener: Send + Sync {
    fn close(&self) -> BoxFuture<'_, Result<()>>;
}
/// Source IUserSessionHost. A listener is established before registry admission.
pub trait SessionHost: Send + Sync {
    fn host(
        &self,
        session: Arc<dyn HostedSession>,
        cancellation: Arc<dyn Cancellation>,
    ) -> BoxFuture<'_, Result<Arc<dyn SessionListener>>>;
}

/// A transport-independent owned subscription. Drop must release upstream work;
/// terminal turn events are ordinary events, never stream completion.
pub trait SessionEventStream: Send {
    fn next<'a>(
        &'a mut self,
        cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<Option<wire::Event>>>;
}

/// Streaming uploads retain the source bounded reader's backpressure boundary.
pub trait UploadStream: Send {
    fn next<'a>(
        &'a mut self,
        cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<Option<wire::AttachmentUploadFrame>>>;
}
