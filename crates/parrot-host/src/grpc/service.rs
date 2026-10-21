//! Native gRPC projection of the transport-independent Parrot service.

use crate::composition::protocol::{
    ParrotService, RpcCode, RpcError, SessionEventStream, UploadStream,
};
use futures_core::Stream;
use parrot_domain::{
    contracts::async_ops::{BoxFuture, Cancellation},
    protocol::{
        bindings::parrot::parrot_server::{Parrot, ParrotServer},
        messages::parrot as wire,
    },
};
use parrot_storage::diagnostics::{
    DiagnosticEvent, DiagnosticFailure, DiagnosticSeverity, FailureKind, FileDiagnosticLog,
};
use std::{pin::Pin, sync::Arc};
use tokio::sync::watch;
use tonic::{Request, Response, Status};

/// Borrows the composition owner's service. Transport disposal never closes it.
#[derive(Clone)]
pub struct GrpcService {
    service: Arc<ParrotService>,
}

impl GrpcService {
    pub fn new(service: Arc<ParrotService>) -> Self {
        Self { service }
    }

    fn failure(&self, method: &str, error: RpcError) -> Status {
        handler_status(self.service.diagnostics(), method, error)
    }
}

/// Source UnhandledFailureInterceptor, bounded to the known typed unexpected IO
/// origin. Ordinary RpcErrors (including Internal) and cancellation are excluded;
/// this is not a universal future failure or panic interception framework.
fn handler_status(diagnostics: &FileDiagnosticLog, method: &str, error: RpcError) -> Status {
    if error.is_unhandled_io() {
        let mut event =
            DiagnosticEvent::new("transport", "handler.failure", DiagnosticSeverity::Error);
        event.request_id = Some(format!(
            "/{}/{method}",
            <ParrotServer<GrpcService> as tonic::server::NamedService>::NAME
        ));
        event.error_code = Some(
            DiagnosticEvent::classify_failure(&DiagnosticFailure {
                kind: FailureKind::Io,
                message: "",
                path: None,
            })
            .into(),
        );
        event.outcome = Some("failure".into());
        diagnostics.write(&event);
    }
    status(error)
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

/// The RPC owns cancellation observation, not work already admitted by a session.
/// Tonic drops a handler future/response stream on disconnect or deadline expiry.
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

fn status(error: RpcError) -> Status {
    let code = match error.code {
        RpcCode::Cancelled => tonic::Code::Cancelled,
        RpcCode::Unknown => tonic::Code::Unknown,
        RpcCode::InvalidArgument => tonic::Code::InvalidArgument,
        RpcCode::NotFound => tonic::Code::NotFound,
        RpcCode::AlreadyExists => tonic::Code::AlreadyExists,
        RpcCode::PermissionDenied => tonic::Code::PermissionDenied,
        RpcCode::FailedPrecondition => tonic::Code::FailedPrecondition,
        RpcCode::Internal => tonic::Code::Internal,
        RpcCode::Unavailable => tonic::Code::Unavailable,
    };
    Status::new(code, error.message)
}

struct IncomingUpload {
    stream: tonic::Streaming<wire::AttachmentUploadFrame>,
    failure: Option<Status>,
}
impl UploadStream for IncomingUpload {
    fn next<'a>(
        &'a mut self,
        cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, crate::composition::protocol::Result<Option<wire::AttachmentUploadFrame>>>
    {
        Box::pin(async move {
            tokio::select! {
                biased;
                _ = cancellation.cancelled() => Err(RpcError::cancelled()),
                result = self.stream.message() => result.map_err(|failure| {
                    let error = RpcError::new(RpcCode::Unknown, failure.message());
                    // UploadStream has fewer codes than Tonic. Retain the original
                    // transport failure for the outward response, including details.
                    self.failure = Some(failure);
                    error
                }),
            }
        })
    }
}

type EventStream = Pin<Box<dyn Stream<Item = Result<wire::Event, Status>> + Send + 'static>>;

fn event_stream(source: Box<dyn SessionEventStream>, observation: Observation) -> EventStream {
    Box::pin(futures_util::stream::unfold(
        (Some(source), observation),
        |(mut source, observation)| async move {
            let result = source.as_mut()?.next(observation.0.as_ref()).await;
            match result {
                Ok(Some(event)) => Some((Ok(event), (source, observation))),
                Ok(None) => None,
                Err(error) => {
                    // Emit a failure once, then finish. Terminal turn events above
                    // remain ordinary events and do not finish the subscription.
                    source.take();
                    Some((Err(status(error)), (source, observation)))
                }
            }
        },
    ))
}

#[tonic::async_trait]
impl Parrot for GrpcService {
    async fn list_models(
        &self,
        request: Request<wire::ListModelsRequest>,
    ) -> Result<Response<wire::ListModelsResponse>, Status> {
        let observation = Observation::new();
        self.service
            .list_models(&request.into_inner(), observation.0.as_ref())
            .await
            .map(Response::new)
            .map_err(|error| self.failure("ListModels", error))
    }

    async fn list_model_aliases(
        &self,
        request: Request<wire::ListModelAliasesRequest>,
    ) -> Result<Response<wire::ListModelAliasesResponse>, Status> {
        Ok(Response::new(
            self.service.list_model_aliases(&request.into_inner()),
        ))
    }

    async fn configure_model_alias(
        &self,
        request: Request<wire::ConfigureModelAliasRequest>,
    ) -> Result<Response<wire::ConfigureModelAliasResponse>, Status> {
        self.service
            .configure_model_alias(&request.into_inner())
            .map(Response::new)
            .map_err(|error| self.failure("ConfigureModelAlias", error))
    }

    async fn list_provider_model_alias_defaults(
        &self,
        request: Request<wire::ListProviderModelAliasDefaultsRequest>,
    ) -> Result<Response<wire::ListProviderModelAliasDefaultsResponse>, Status> {
        let observation = Observation::new();
        self.service
            .list_provider_model_alias_defaults(&request.into_inner(), observation.0.as_ref())
            .await
            .map(Response::new)
            .map_err(|error| self.failure("ListProviderModelAliasDefaults", error))
    }

    async fn apply_provider_model_alias_defaults(
        &self,
        request: Request<wire::ApplyProviderModelAliasDefaultsRequest>,
    ) -> Result<Response<wire::ApplyProviderModelAliasDefaultsResponse>, Status> {
        let observation = Observation::new();
        self.service
            .apply_provider_model_alias_defaults(&request.into_inner(), observation.0.as_ref())
            .await
            .map(Response::new)
            .map_err(|error| self.failure("ApplyProviderModelAliasDefaults", error))
    }

    async fn set_model_preset(
        &self,
        request: Request<wire::SetModelPresetRequest>,
    ) -> Result<Response<wire::SetModelPresetResponse>, Status> {
        let observation = Observation::new();
        self.service
            .set_model_preset(&request.into_inner(), observation.0.as_ref())
            .map(Response::new)
            .map_err(|error| self.failure("SetModelPreset", error))
    }

    async fn select_model_preset(
        &self,
        request: Request<wire::SelectModelPresetRequest>,
    ) -> Result<Response<wire::SelectModelPresetResponse>, Status> {
        let observation = Observation::new();
        self.service
            .select_model_preset(&request.into_inner(), observation.0.as_ref())
            .map(Response::new)
            .map_err(|error| self.failure("SelectModelPreset", error))
    }

    async fn list_model_presets(
        &self,
        request: Request<wire::ListModelPresetsRequest>,
    ) -> Result<Response<wire::ListModelPresetsResponse>, Status> {
        let observation = Observation::new();
        self.service
            .list_model_presets(&request.into_inner(), observation.0.as_ref())
            .map(Response::new)
            .map_err(|error| self.failure("ListModelPresets", error))
    }

    async fn list_modes(
        &self,
        request: Request<wire::ListModesRequest>,
    ) -> Result<Response<wire::ListModesResponse>, Status> {
        Ok(Response::new(
            self.service.list_modes(&request.into_inner()),
        ))
    }

    async fn list_skills(
        &self,
        request: Request<wire::ListSkillsRequest>,
    ) -> Result<Response<wire::ListSkillsResponse>, Status> {
        let observation = Observation::new();
        self.service
            .list_skills(&request.into_inner(), observation.0.as_ref())
            .await
            .map(Response::new)
            .map_err(|error| self.failure("ListSkills", error))
    }

    async fn configure_skill(
        &self,
        request: Request<wire::ConfigureSkillRequest>,
    ) -> Result<Response<wire::ConfigureSkillResponse>, Status> {
        let observation = Observation::new();
        self.service
            .configure_skill(&request.into_inner(), observation.0.as_ref())
            .await
            .map(Response::new)
            .map_err(|error| self.failure("ConfigureSkill", error))
    }

    async fn list_sessions(
        &self,
        request: Request<wire::ListSessionsRequest>,
    ) -> Result<Response<wire::ListSessionsResponse>, Status> {
        self.service
            .list_sessions(&request.into_inner())
            .map(Response::new)
            .map_err(|error| self.failure("ListSessions", error))
    }

    async fn create_session(
        &self,
        request: Request<wire::CreateSessionRequest>,
    ) -> Result<Response<wire::UserSession>, Status> {
        let observation = Observation::new();
        self.service
            .create_session(&request.into_inner(), observation.0.clone())
            .await
            .map(Response::new)
            .map_err(|error| self.failure("CreateSession", error))
    }

    async fn resume_session(
        &self,
        request: Request<wire::ResumeSessionRequest>,
    ) -> Result<Response<wire::UserSession>, Status> {
        let observation = Observation::new();
        self.service
            .resume_session(&request.into_inner(), observation.0.clone())
            .await
            .map(Response::new)
            .map_err(|error| self.failure("ResumeSession", error))
    }

    async fn attach_session(
        &self,
        request: Request<wire::AttachSessionRequest>,
    ) -> Result<Response<wire::UserSession>, Status> {
        let observation = Observation::new();
        self.service
            .attach_session(&request.into_inner(), observation.0.as_ref())
            .map(Response::new)
            .map_err(|error| self.failure("AttachSession", error))
    }

    async fn update_session(
        &self,
        request: Request<wire::UpdateSessionRequest>,
    ) -> Result<Response<wire::UserSession>, Status> {
        self.service
            .update_session(&request.into_inner())
            .await
            .map(Response::new)
            .map_err(|error| self.failure("UpdateSession", error))
    }

    async fn set_goal(
        &self,
        request: Request<wire::SetGoalRequest>,
    ) -> Result<Response<wire::SetGoalResponse>, Status> {
        let observation = Observation::new();
        self.service
            .set_goal(&request.into_inner(), observation.0.clone())
            .await
            .map(Response::new)
            .map_err(|error| self.failure("SetGoal", error))
    }

    async fn set_standing_instruction(
        &self,
        request: Request<wire::SetStandingInstructionRequest>,
    ) -> Result<Response<wire::SetStandingInstructionResponse>, Status> {
        self.service
            .set_standing_instruction(&request.into_inner())
            .map(Response::new)
            .map_err(|error| self.failure("SetStandingInstruction", error))
    }

    async fn send_message(
        &self,
        request: Request<wire::SendMessageRequest>,
    ) -> Result<Response<wire::SendMessageResponse>, Status> {
        let observation = Observation::new();
        self.service
            .send_message(&request.into_inner(), observation.0.clone())
            .await
            .map(Response::new)
            .map_err(|error| self.failure("SendMessage", error))
    }

    async fn upload_attachment(
        &self,
        request: Request<tonic::Streaming<wire::AttachmentUploadFrame>>,
    ) -> Result<Response<wire::AttachmentUploadResponse>, Status> {
        let observation = Observation::new();
        let mut stream = IncomingUpload {
            stream: request.into_inner(),
            failure: None,
        };
        self.service
            .upload_attachment(&mut stream, observation.0.as_ref())
            .await
            .map(Response::new)
            .map_err(|error| {
                stream
                    .failure
                    .unwrap_or_else(|| self.failure("UploadAttachment", error))
            })
    }

    async fn interrupt(
        &self,
        request: Request<wire::InterruptRequest>,
    ) -> Result<Response<wire::InterruptResponse>, Status> {
        let observation = Observation::new();
        self.service
            .interrupt(&request.into_inner(), observation.0.clone())
            .await
            .map(Response::new)
            .map_err(|error| self.failure("Interrupt", error))
    }

    async fn compact(
        &self,
        request: Request<wire::CompactRequest>,
    ) -> Result<Response<wire::CompactResponse>, Status> {
        let observation = Observation::new();
        self.service
            .compact(&request.into_inner(), observation.0.clone())
            .await
            .map(Response::new)
            .map_err(|error| self.failure("Compact", error))
    }

    async fn set_context_limit(
        &self,
        request: Request<wire::SetContextLimitRequest>,
    ) -> Result<Response<wire::SetContextLimitResponse>, Status> {
        let observation = Observation::new();
        self.service
            .set_context_limit(&request.into_inner(), observation.0.as_ref())
            .map(Response::new)
            .map_err(|error| self.failure("SetContextLimit", error))
    }

    async fn sandbox_enable(
        &self,
        request: Request<wire::SandboxEnableRequest>,
    ) -> Result<Response<wire::SandboxEnableResponse>, Status> {
        let observation = Observation::new();
        self.service
            .sandbox_enable(&request.into_inner(), observation.0.as_ref())
            .map(Response::new)
            .map_err(|error| self.failure("SandboxEnable", error))
    }

    async fn session_status(
        &self,
        request: Request<wire::SessionStatusRequest>,
    ) -> Result<Response<wire::SessionStatusResponse>, Status> {
        let observation = Observation::new();
        self.service
            .session_status(&request.into_inner(), observation.0.as_ref())
            .await
            .map(Response::new)
            .map_err(|error| self.failure("SessionStatus", error))
    }

    async fn list_pending_questions(
        &self,
        request: Request<wire::ListPendingQuestionsRequest>,
    ) -> Result<Response<wire::ListPendingQuestionsResponse>, Status> {
        self.service
            .list_pending_questions(&request.into_inner())
            .map(Response::new)
            .map_err(|error| self.failure("ListPendingQuestions", error))
    }

    async fn reply_question(
        &self,
        request: Request<wire::ReplyQuestionRequest>,
    ) -> Result<Response<wire::ReplyQuestionResponse>, Status> {
        self.service
            .reply_question(&request.into_inner())
            .map(Response::new)
            .map_err(|error| self.failure("ReplyQuestion", error))
    }

    async fn reject_question(
        &self,
        request: Request<wire::RejectQuestionRequest>,
    ) -> Result<Response<wire::RejectQuestionResponse>, Status> {
        self.service
            .reject_question(&request.into_inner())
            .map(Response::new)
            .map_err(|error| self.failure("RejectQuestion", error))
    }

    async fn list_pending_permissions(
        &self,
        request: Request<wire::ListPendingPermissionsRequest>,
    ) -> Result<Response<wire::ListPendingPermissionsResponse>, Status> {
        self.service
            .list_pending_permissions(&request.into_inner())
            .await
            .map(Response::new)
            .map_err(|error| self.failure("ListPendingPermissions", error))
    }

    async fn reply_permission(
        &self,
        request: Request<wire::ReplyPermissionRequest>,
    ) -> Result<Response<wire::ReplyPermissionResponse>, Status> {
        self.service
            .reply_permission(&request.into_inner())
            .await
            .map(Response::new)
            .map_err(|error| self.failure("ReplyPermission", error))
    }

    type ListenStream = EventStream;

    async fn listen(
        &self,
        request: Request<wire::ListenRequest>,
    ) -> Result<Response<Self::ListenStream>, Status> {
        let observation = Observation::new();
        let source = self
            .service
            .listen(&request.into_inner(), observation.0.clone())
            .map_err(|error| self.failure("Listen", error))?;
        Ok(Response::new(event_stream(source, observation)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures_util::StreamExt;
    use std::sync::atomic::{AtomicBool, Ordering};

    #[tokio::test]
    async fn observation_drop_cancels_retained_observer_without_lost_wakeup() {
        let observation = Observation::new();
        let cancellation = observation.0.clone();
        let notified = cancellation.cancelled();
        drop(observation);
        assert!(cancellation.is_cancelled());
        tokio::time::timeout(std::time::Duration::from_secs(1), notified)
            .await
            .unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(1), cancellation.cancelled())
            .await
            .unwrap();
    }

    struct Events {
        values:
            std::collections::VecDeque<crate::composition::protocol::Result<Option<wire::Event>>>,
        dropped: Arc<AtomicBool>,
    }
    impl Drop for Events {
        fn drop(&mut self) {
            self.dropped.store(true, Ordering::SeqCst);
        }
    }
    impl SessionEventStream for Events {
        fn next<'a>(
            &'a mut self,
            _: &'a dyn Cancellation,
        ) -> BoxFuture<'a, crate::composition::protocol::Result<Option<wire::Event>>> {
            Box::pin(async move {
                match self.values.pop_front() {
                    Some(value) => value,
                    None => std::future::pending().await,
                }
            })
        }
    }

    #[tokio::test]
    async fn event_failure_is_emitted_once_then_subscription_is_dropped() {
        let dropped = Arc::new(AtomicBool::new(false));
        let source = Events {
            values: [Err(RpcError::new(
                RpcCode::Unavailable,
                "subscription failed",
            ))]
            .into(),
            dropped: dropped.clone(),
        };
        let mut stream = event_stream(Box::new(source), Observation::new());
        let error = stream.next().await.unwrap().unwrap_err();
        assert_eq!(error.code(), tonic::Code::Unavailable);
        assert_eq!(error.message(), "subscription failed");
        assert!(dropped.load(Ordering::SeqCst));
        assert!(stream.next().await.is_none());
    }

    #[tokio::test]
    async fn terminal_events_and_unknown_fields_pass_through_until_actual_eof() {
        use protobuf::Message;
        let dropped = Arc::new(AtomicBool::new(false));
        let mut terminal = wire::Event {
            id: "terminal".into(),
            ..Default::default()
        };
        terminal.set_turn_ended(wire::TurnEnded::new());
        terminal
            .special_fields
            .mut_unknown_fields()
            .add_varint(200, 777);
        let expected = terminal.write_to_bytes().unwrap();
        let source = Events {
            values: [
                Ok(Some(terminal)),
                Ok(Some(wire::Event {
                    id: "after-terminal".into(),
                    ..Default::default()
                })),
                Ok(None),
            ]
            .into(),
            dropped: dropped.clone(),
        };
        let observation = Observation::new();
        let cancellation = observation.0.clone();
        let mut stream = event_stream(Box::new(source), observation);
        assert_eq!(
            stream
                .next()
                .await
                .unwrap()
                .unwrap()
                .write_to_bytes()
                .unwrap(),
            expected
        );
        assert_eq!(stream.next().await.unwrap().unwrap().id, "after-terminal");
        assert!(!cancellation.is_cancelled());
        assert!(stream.next().await.is_none());
        assert!(dropped.load(Ordering::SeqCst));
        assert!(cancellation.is_cancelled());
    }

    #[tokio::test]
    async fn dropping_unpolled_or_pending_stream_releases_subscription_and_cancels_observer() {
        for pending_read in [false, true] {
            let dropped = Arc::new(AtomicBool::new(false));
            let source = Events {
                values: Default::default(),
                dropped: dropped.clone(),
            };
            let observation = Observation::new();
            let cancellation = observation.0.clone();
            let mut stream = event_stream(Box::new(source), observation);
            if pending_read {
                std::future::poll_fn(|cx| {
                    assert!(stream.as_mut().poll_next(cx).is_pending());
                    std::task::Poll::Ready(())
                })
                .await;
            }
            assert!(!dropped.load(Ordering::SeqCst));
            assert!(!cancellation.is_cancelled());
            drop(stream);
            assert!(dropped.load(Ordering::SeqCst));
            assert!(cancellation.is_cancelled());
        }
    }

    #[tokio::test]
    async fn incoming_upload_preserves_messages_unknown_fields_eof_and_cancellation() {
        use bytes::Bytes;
        use http_body_util::Full;
        use parrot_domain::protocol::codec::RustProtobufCodec;
        use protobuf::Message;
        use tonic::codec::Codec;
        let mut frame = wire::AttachmentUploadFrame::new();
        frame
            .special_fields
            .mut_unknown_fields()
            .add_varint(200, 777);
        let expected = frame.write_to_bytes().unwrap();
        let mut body = vec![0];
        body.extend_from_slice(&(expected.len() as u32).to_be_bytes());
        body.extend_from_slice(&expected);
        let mut codec =
            RustProtobufCodec::<wire::AttachmentUploadFrame, wire::AttachmentUploadFrame>::default(
            );
        let mut upload = IncomingUpload {
            stream: tonic::Streaming::new_request(
                codec.decoder(),
                Full::new(Bytes::from(body)),
                None,
                None,
            ),
            failure: None,
        };
        let observation = Observation::new();
        assert_eq!(
            upload
                .next(observation.0.as_ref())
                .await
                .unwrap()
                .unwrap()
                .write_to_bytes()
                .unwrap(),
            expected
        );
        assert!(upload.next(observation.0.as_ref()).await.unwrap().is_none());
        let cancellation = observation.0.clone();
        drop(observation);
        assert_eq!(
            upload.next(cancellation.as_ref()).await.unwrap_err().code,
            RpcCode::Cancelled
        );
        assert!(upload.failure.is_none());
    }

    #[tokio::test]
    async fn upload_retains_transport_status_details_outside_narrow_domain_error_codes() {
        use bytes::Bytes;
        use http_body_util::Full;
        struct FailedDecoder;
        impl tonic::codec::Decoder for FailedDecoder {
            type Item = wire::AttachmentUploadFrame;
            type Error = Status;
            fn decode(
                &mut self,
                _: &mut tonic::codec::DecodeBuf<'_>,
            ) -> Result<Option<Self::Item>, Status> {
                Err(Status::with_details(
                    tonic::Code::DataLoss,
                    "wire failure",
                    Bytes::from_static(b"cause"),
                ))
            }
        }
        let mut upload = IncomingUpload {
            stream: tonic::Streaming::new_request(
                FailedDecoder,
                Full::new(Bytes::from_static(&[0, 0, 0, 0, 1, 1])),
                None,
                None,
            ),
            failure: None,
        };
        let observation = Observation::new();
        let error = upload.next(observation.0.as_ref()).await.unwrap_err();
        let outward = upload.failure.unwrap_or_else(|| status(error));
        assert_eq!(outward.code(), tonic::Code::DataLoss);
        assert_eq!(outward.message(), "wire failure");
        assert_eq!(outward.details(), b"cause");
    }

    #[test]
    fn every_domain_rpc_code_preserves_code_and_message() {
        for code in [
            RpcCode::Cancelled,
            RpcCode::Unknown,
            RpcCode::InvalidArgument,
            RpcCode::NotFound,
            RpcCode::AlreadyExists,
            RpcCode::PermissionDenied,
            RpcCode::FailedPrecondition,
            RpcCode::Internal,
            RpcCode::Unavailable,
        ] {
            let result = status(RpcError::new(code, "exact failure"));
            assert_eq!(result.code() as i32, code as i32);
            assert_eq!(result.message(), "exact failure");
        }
    }
}
