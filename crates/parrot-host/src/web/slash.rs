//! Web protocol orchestration. The CLI owner supplies the real registry/session
//! adapter through these ports; fixture implementations do not close that join.

use super::{
    dialog::{WebSlashDialog, random_id},
    router::{WebClient, WebSessionRouter},
};
use futures_core::Stream;
use parrot_domain::{
    contracts::async_ops::{BoxFuture, Cancellation},
    protocol::{
        bindings::web::parrot_web_server::ParrotWeb,
        messages::{parrot as wire, web},
    },
    values::identifiers::Identifier,
};
use std::{
    collections::HashMap,
    pin::Pin,
    sync::{Arc, Mutex},
    task::{Context, Poll},
};
use tokio::sync::{mpsc, watch};
use tonic::{Request, Response, Status};

/// Binds the source CLI SlashCommands.Create and SlashSession to browser dialogs.
/// Production construction belongs to the CLI owner, not a second web registry.
pub trait WebSlashFactory: Send + Sync {
    fn create(&self, context: WebSlashContext) -> Box<dyn WebSlashRegistry>;
}

pub struct WebSlashContext {
    pub client: WebClient,
    pub initial_session: wire::UserSession,
    pub dialog: Arc<WebSlashDialog>,
    pub activity: WebSlashActivity,
}

pub struct WebSlashActivity(Option<(Arc<WebSessionRouter>, String)>);
impl WebSlashActivity {
    pub fn is_busy(&self) -> bool {
        self.0
            .as_ref()
            .is_some_and(|(router, id)| router.is_busy(id))
    }
}

/// A real CLI registry plus its mutable SlashSession, scoped to one web run.
pub trait WebSlashRegistry: Send {
    fn commands(&self) -> Vec<web::SlashCommand>;
    fn dispatch<'a>(
        &'a mut self,
        text: &'a str,
        cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<(), WebSlashFailure>>;
    fn current_session(&self) -> wire::UserSession;
}

pub enum WebSlashFailure {
    Rpc(Status),
    Command(String),
}
impl From<Status> for WebSlashFailure {
    fn from(status: Status) -> Self {
        Self::Rpc(status)
    }
}

type Runs = Arc<Mutex<HashMap<String, Arc<WebSlashDialog>>>>;

#[derive(Clone)]
pub struct WebSlashService {
    router: Arc<WebSessionRouter>,
    local: WebClient,
    factory: Arc<dyn WebSlashFactory>,
    working_directory: String,
    runs: Runs,
}

impl WebSlashService {
    pub fn new(
        router: Arc<WebSessionRouter>,
        local: WebClient,
        factory: Arc<dyn WebSlashFactory>,
        working_directory: String,
    ) -> Self {
        Self {
            router,
            local,
            factory,
            working_directory,
            runs: Arc::new(Mutex::new(HashMap::new())),
        }
    }
}

#[tonic::async_trait]
impl ParrotWeb for WebSlashService {
    async fn describe_host(
        &self,
        _: Request<web::DescribeHostRequest>,
    ) -> Result<Response<web::DescribeHostResponse>, Status> {
        Ok(Response::new(web::DescribeHostResponse {
            working_directory: self.working_directory.clone(),
            ..Default::default()
        }))
    }

    async fn open_default_session(
        &self,
        _: Request<web::OpenDefaultSessionRequest>,
    ) -> Result<Response<wire::UserSession>, Status> {
        let observation = Observation::new();
        self.router
            .open_default(observation.0.as_ref())
            .await
            .map(Response::new)
    }

    async fn list_slash_commands(
        &self,
        _: Request<web::ListSlashCommandsRequest>,
    ) -> Result<Response<web::ListSlashCommandsResponse>, Status> {
        let (frames, _receiver) = mpsc::unbounded_channel();
        let registry = self.factory.create(WebSlashContext {
            client: self.local.clone(),
            initial_session: wire::UserSession::new(),
            dialog: Arc::new(WebSlashDialog::new(frames)),
            activity: WebSlashActivity(None),
        });
        Ok(Response::new(web::ListSlashCommandsResponse {
            commands: registry.commands(),
            ..Default::default()
        }))
    }

    type RunSlashCommandStream =
        Pin<Box<dyn Stream<Item = Result<web::SlashFrame, Status>> + Send + 'static>>;

    async fn run_slash_command(
        &self,
        request: Request<web::RunSlashCommandRequest>,
    ) -> Result<Response<Self::RunSlashCommandStream>, Status> {
        let request = request.into_inner();
        let run_id = random_id()?;
        let (frames, receiver) = mpsc::unbounded_channel();
        let dialog = Arc::new(WebSlashDialog::new(frames));
        self.runs
            .lock()
            .expect("slash runs lock")
            .insert(run_id.clone(), dialog.clone());
        let observation = Observation::new();
        let cancellation = observation.0.clone();
        let router = self.router.clone();
        let factory = self.factory.clone();
        let working_directory = self.working_directory.clone();
        let dispatch = Box::pin(async move {
            let mut client = router.client_for(&request.user_session_id);
            let initial = client
                .attach_session(wire::AttachSessionRequest {
                    user_session_id: request.user_session_id.clone(),
                    working_directory,
                    ..Default::default()
                })
                .await?
                .into_inner();
            let mut registry = factory.create(WebSlashContext {
                client,
                initial_session: initial.clone(),
                dialog: dialog.clone(),
                activity: WebSlashActivity(Some((router, request.user_session_id))),
            });
            match registry
                .dispatch(&request.text, cancellation.as_ref())
                .await
            {
                Ok(()) => {}
                Err(WebSlashFailure::Command(message)) => {
                    dialog.show_error(&message, cancellation.as_ref()).await?
                }
                Err(WebSlashFailure::Rpc(status)) => return Err(status),
            }
            let current = registry.current_session();
            Ok((current.id == initial.id
                && (current.model != initial.model || current.mode != initial.mode))
                .then(|| wire::UserSession {
                    id: current.id,
                    model: current.model,
                    mode: current.mode,
                    ..Default::default()
                }))
        });
        let started = web::SlashFrame {
            payload: Some(web::slash_frame::Payload::Started(web::SlashStarted {
                run_id: run_id.clone(),
                ..Default::default()
            })),
            ..Default::default()
        };
        Ok(Response::new(Box::pin(RunStream {
            started: Some(started),
            dispatch: Some(dispatch),
            frames: receiver,
            completion: None,
            ended: false,
            _guard: RunGuard {
                id: run_id,
                runs: self.runs.clone(),
                _observation: observation,
            },
        })))
    }

    async fn answer_slash_prompt(
        &self,
        request: Request<web::AnswerSlashPromptRequest>,
    ) -> Result<Response<web::AnswerSlashPromptResponse>, Status> {
        let request = request.into_inner();
        let dialog = self
            .runs
            .lock()
            .expect("slash runs lock")
            .get(&request.run_id)
            .cloned();
        if dialog.is_some_and(|dialog| dialog.answer(request)) {
            Ok(Response::new(web::AnswerSlashPromptResponse::new()))
        } else {
            Err(Status::not_found(
                "the slash prompt is not waiting for an answer",
            ))
        }
    }

    async fn upload_attachment(
        &self,
        request: Request<web::UploadAttachmentRequest>,
    ) -> Result<Response<wire::AttachmentUploadResponse>, Status> {
        let request = request.into_inner();
        let mut client = self.router.client_for(&request.user_session_id);
        let header = wire::AttachmentUploadHeader {
            user_session_id: request.user_session_id,
            upload_id: Identifier::event_id()
                .map_err(|error| Status::internal(error.to_string()))?,
            ..Default::default()
        };
        let description = wire::AttachmentUploadDescription {
            display_name: request.display_name,
            media_type: request.media_type,
            byte_length: request.content.len() as i64,
            ..Default::default()
        };
        // Pinned PromptAttachmentUploader.Send emits header, 1MiB chunks, description.
        let mut header = Some(header);
        let content = request.content;
        let mut offset = 0;
        let mut description = Some(description);
        let frames = std::iter::from_fn(move || {
            let payload = if let Some(header) = header.take() {
                wire::attachment_upload_frame::Payload::Header(header)
            } else if offset < content.len() {
                let end = (offset + 1024 * 1024).min(content.len());
                let chunk = content[offset..end].to_vec();
                offset = end;
                wire::attachment_upload_frame::Payload::Chunk(chunk)
            } else {
                wire::attachment_upload_frame::Payload::Description(description.take()?)
            };
            Some(wire::AttachmentUploadFrame {
                payload: Some(payload),
                ..Default::default()
            })
        });
        let response = client
            .upload_attachment(futures_util::stream::iter(frames))
            .await?;
        Ok(Response::new(wire::AttachmentUploadResponse {
            artifact: response.into_inner().artifact,
            ..Default::default()
        }))
    }
}

struct RpcCancellation(watch::Sender<bool>);
impl Cancellation for RpcCancellation {
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
struct Observation(Arc<RpcCancellation>);
impl Observation {
    fn new() -> Self {
        Self(Arc::new(RpcCancellation(watch::channel(false).0)))
    }
}
impl Drop for Observation {
    fn drop(&mut self) {
        self.0.0.send_replace(true);
    }
}

struct RunGuard {
    id: String,
    runs: Runs,
    _observation: Observation,
}
impl Drop for RunGuard {
    fn drop(&mut self) {
        self.runs.lock().expect("slash runs lock").remove(&self.id);
    }
}

type Dispatch =
    Pin<Box<dyn std::future::Future<Output = Result<Option<wire::UserSession>, Status>> + Send>>;
struct RunStream {
    started: Option<web::SlashFrame>,
    dispatch: Option<Dispatch>,
    frames: mpsc::UnboundedReceiver<web::SlashFrame>,
    completion: Option<Result<Option<wire::UserSession>, Status>>,
    ended: bool,
    _guard: RunGuard,
}
impl Stream for RunStream {
    type Item = Result<web::SlashFrame, Status>;
    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let this = self.get_mut();
        if let Some(started) = this.started.take() {
            return Poll::Ready(Some(Ok(started)));
        }
        if this.ended {
            return Poll::Ready(None);
        }
        if let Some(dispatch) = this.dispatch.as_mut() {
            if let Poll::Ready(result) = dispatch.as_mut().poll(cx) {
                this.completion = Some(result);
                this.dispatch.take();
            }
        }
        if let Poll::Ready(Some(frame)) = this.frames.poll_recv(cx) {
            return Poll::Ready(Some(Ok(frame)));
        }
        if let Some(completion) = this.completion.take() {
            this.ended = true;
            // Remove the run at completion, not only when the response body drops.
            this._guard
                .runs
                .lock()
                .expect("slash runs lock")
                .remove(&this._guard.id);
            return Poll::Ready(match completion {
                Ok(Some(session)) => Some(Ok(web::SlashFrame {
                    payload: Some(web::slash_frame::Payload::SessionUpdated(session)),
                    ..Default::default()
                })),
                Ok(None) => None,
                Err(status) => Some(Err(status)),
            });
        }
        Poll::Pending
    }
}
