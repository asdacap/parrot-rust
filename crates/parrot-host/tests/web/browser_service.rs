use futures_core::Stream;
use parrot_domain::protocol::{
    bindings::web::parrot_web_server::ParrotWeb,
    messages::{parrot as wire, web},
};
use std::{
    pin::Pin,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    task::{Context, Poll},
};
use tonic::{Response, Status};

#[derive(Default)]
pub(super) struct NoWeb {
    pub(super) released: Arc<AtomicBool>,
}

pub(super) struct BrowserFrames {
    released: Arc<AtomicBool>,
    sent: bool,
    failure: Option<String>,
}
impl Stream for BrowserFrames {
    type Item = Result<web::SlashFrame, Status>;
    fn poll_next(mut self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        if !self.sent {
            self.sent = true;
            let mut frame = web::SlashFrame::new();
            frame.set_started(web::SlashStarted {
                run_id: "browser-run".into(),
                ..Default::default()
            });
            return Poll::Ready(Some(Ok(frame)));
        }
        if let Some(message) = self.failure.take() {
            let mut metadata = tonic::metadata::MetadataMap::new();
            metadata.insert("x-web-proof", "trailer".parse().unwrap());
            return Poll::Ready(Some(Err(Status::with_metadata(
                tonic::Code::InvalidArgument,
                message,
                metadata,
            ))));
        }
        Poll::Pending
    }
}
impl Drop for BrowserFrames {
    fn drop(&mut self) {
        self.released.store(true, Ordering::SeqCst);
    }
}
#[tonic::async_trait]
impl ParrotWeb for NoWeb {
    async fn describe_host(
        &self,
        _: tonic::Request<web::DescribeHostRequest>,
    ) -> Result<Response<web::DescribeHostResponse>, Status> {
        Ok(Response::new(web::DescribeHostResponse {
            working_directory: "/synthetic-browser-fixture".into(),
            ..Default::default()
        }))
    }
    async fn open_default_session(
        &self,
        _: tonic::Request<web::OpenDefaultSessionRequest>,
    ) -> Result<Response<wire::UserSession>, Status> {
        Err(Status::unimplemented("web fixture is not used"))
    }
    async fn list_slash_commands(
        &self,
        _: tonic::Request<web::ListSlashCommandsRequest>,
    ) -> Result<Response<web::ListSlashCommandsResponse>, Status> {
        Err(Status::unimplemented("web fixture is not used"))
    }
    type RunSlashCommandStream = BrowserFrames;
    async fn run_slash_command(
        &self,
        request: tonic::Request<web::RunSlashCommandRequest>,
    ) -> Result<Response<Self::RunSlashCommandStream>, Status> {
        self.released.store(false, Ordering::SeqCst);
        Ok(Response::new(BrowserFrames {
            released: self.released.clone(),
            sent: false,
            failure: request
                .get_ref()
                .text
                .strip_prefix("failure:")
                .map(str::to_owned)
                .or_else(|| {
                    (request.get_ref().text == "failure").then(|| "prompt failure % café".into())
                }),
        }))
    }
    async fn answer_slash_prompt(
        &self,
        _: tonic::Request<web::AnswerSlashPromptRequest>,
    ) -> Result<Response<web::AnswerSlashPromptResponse>, Status> {
        Err(Status::unimplemented("web fixture is not used"))
    }
    async fn upload_attachment(
        &self,
        _: tonic::Request<web::UploadAttachmentRequest>,
    ) -> Result<Response<wire::AttachmentUploadResponse>, Status> {
        Err(Status::unimplemented("web fixture is not used"))
    }
}
