//! Native gRPC projection of the transport-independent Parrot service.

use crate::composition::{
    agents::CallerSignal,
    protocol::{ParrotService, RpcCode, RpcError, SessionEventStream, UploadStream},
};
use futures_core::Stream;
use parrot_domain::{
    contracts::async_ops::{BoxFuture, Cancellation},
    protocol::{
        bindings::parrot::parrot_server::{Parrot, ParrotServer},
        messages::parrot as wire,
    },
};
use parrot_runtime::agent_turn::lifecycle::Signal;
use parrot_storage::diagnostics::{
    DiagnosticEvent, DiagnosticFailure, DiagnosticSeverity, FailureKind, FileDiagnosticLog,
};
use std::{pin::Pin, sync::Arc};
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

fn event_stream(source: Box<dyn SessionEventStream>, observation: CallerSignal) -> EventStream {
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

macro_rules! forward_parrot {
    (infallible { $( $method:ident : $request:ident => $response:ident ),* $(,)? }
     plain { $( $plain:ident : $plain_label:ident : $plain_request:ident => $plain_response:ident ),* $(,)? }
     plain_async { $( $async_plain:ident : $async_plain_label:ident : $async_plain_request:ident => $async_plain_response:ident ),* $(,)? }
     signal_ref { $( $signal:ident : $signal_label:ident : $signal_request:ident => $signal_response:ident ),* $(,)? }
     signal_ref_async { $( $signal_async:ident : $signal_async_label:ident : $signal_async_request:ident => $signal_async_response:ident ),* $(,)? }
     signal_owned { $( $owned:ident : $owned_label:ident : $owned_request:ident => $owned_response:ident ),* $(,)? }) => {
        #[tonic::async_trait]
        impl Parrot for GrpcService {
            $(async fn $method(
                &self,
                request: Request<wire::$request>,
            ) -> Result<Response<wire::$response>, Status> {
                Ok(Response::new(self.service.$method(&request.into_inner())))
            })*

            $(async fn $plain(
                &self,
                request: Request<wire::$plain_request>,
            ) -> Result<Response<wire::$plain_response>, Status> {
                self.service
                    .$plain(&request.into_inner())
                    .map(Response::new)
                    .map_err(|error| self.failure(stringify!($plain_label), error))
            })*

            $(async fn $async_plain(
                &self,
                request: Request<wire::$async_plain_request>,
            ) -> Result<Response<wire::$async_plain_response>, Status> {
                self.service
                    .$async_plain(&request.into_inner())
                    .await
                    .map(Response::new)
                    .map_err(|error| self.failure(stringify!($async_plain_label), error))
            })*

            $(async fn $signal(
                &self,
                request: Request<wire::$signal_request>,
            ) -> Result<Response<wire::$signal_response>, Status> {
                let observation = CallerSignal(Arc::new(Signal::new()));
                self.service
                    .$signal(&request.into_inner(), observation.0.as_ref())
                    .map(Response::new)
                    .map_err(|error| self.failure(stringify!($signal_label), error))
            })*

            $(async fn $signal_async(
                &self,
                request: Request<wire::$signal_async_request>,
            ) -> Result<Response<wire::$signal_async_response>, Status> {
                let observation = CallerSignal(Arc::new(Signal::new()));
                self.service
                    .$signal_async(&request.into_inner(), observation.0.as_ref())
                    .await
                    .map(Response::new)
                    .map_err(|error| self.failure(stringify!($signal_async_label), error))
            })*

            $(async fn $owned(
                &self,
                request: Request<wire::$owned_request>,
            ) -> Result<Response<wire::$owned_response>, Status> {
                let observation = CallerSignal(Arc::new(Signal::new()));
                self.service
                    .$owned(&request.into_inner(), observation.0.clone())
                    .await
                    .map(Response::new)
                    .map_err(|error| self.failure(stringify!($owned_label), error))
            })*

            async fn upload_attachment(
                &self,
                request: Request<tonic::Streaming<wire::AttachmentUploadFrame>>,
            ) -> Result<Response<wire::AttachmentUploadResponse>, Status> {
                let observation = CallerSignal(Arc::new(Signal::new()));
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

            type ListenStream = EventStream;

            async fn listen(
                &self,
                request: Request<wire::ListenRequest>,
            ) -> Result<Response<Self::ListenStream>, Status> {
                let observation = CallerSignal(Arc::new(Signal::new()));
                let source = self
                    .service
                    .listen(&request.into_inner(), observation.0.clone())
                    .map_err(|error| self.failure("Listen", error))?;
                Ok(Response::new(event_stream(source, observation)))
            }
        }
    };
}

forward_parrot! {
    infallible {
        list_model_aliases: ListModelAliasesRequest => ListModelAliasesResponse,
        list_modes: ListModesRequest => ListModesResponse,
    }
    plain {
        configure_model_alias: ConfigureModelAlias : ConfigureModelAliasRequest => ConfigureModelAliasResponse,
        list_sessions: ListSessions : ListSessionsRequest => ListSessionsResponse,
        set_standing_instruction: SetStandingInstruction : SetStandingInstructionRequest => SetStandingInstructionResponse,
        list_pending_questions: ListPendingQuestions : ListPendingQuestionsRequest => ListPendingQuestionsResponse,
        reply_question: ReplyQuestion : ReplyQuestionRequest => ReplyQuestionResponse,
        reject_question: RejectQuestion : RejectQuestionRequest => RejectQuestionResponse,
    }
    plain_async {
        update_session: UpdateSession : UpdateSessionRequest => UserSession,
        list_pending_permissions: ListPendingPermissions : ListPendingPermissionsRequest => ListPendingPermissionsResponse,
        reply_permission: ReplyPermission : ReplyPermissionRequest => ReplyPermissionResponse,
    }
    signal_ref {
        set_model_preset: SetModelPreset : SetModelPresetRequest => SetModelPresetResponse,
        select_model_preset: SelectModelPreset : SelectModelPresetRequest => SelectModelPresetResponse,
        list_model_presets: ListModelPresets : ListModelPresetsRequest => ListModelPresetsResponse,
        attach_session: AttachSession : AttachSessionRequest => UserSession,
        set_context_limit: SetContextLimit : SetContextLimitRequest => SetContextLimitResponse,
        sandbox_enable: SandboxEnable : SandboxEnableRequest => SandboxEnableResponse,
    }
    signal_ref_async {
        list_models: ListModels : ListModelsRequest => ListModelsResponse,
        list_provider_model_alias_defaults: ListProviderModelAliasDefaults : ListProviderModelAliasDefaultsRequest => ListProviderModelAliasDefaultsResponse,
        apply_provider_model_alias_defaults: ApplyProviderModelAliasDefaults : ApplyProviderModelAliasDefaultsRequest => ApplyProviderModelAliasDefaultsResponse,
        list_skills: ListSkills : ListSkillsRequest => ListSkillsResponse,
        configure_skill: ConfigureSkill : ConfigureSkillRequest => ConfigureSkillResponse,
        session_status: SessionStatus : SessionStatusRequest => SessionStatusResponse,
    }
    signal_owned {
        create_session: CreateSession : CreateSessionRequest => UserSession,
        resume_session: ResumeSession : ResumeSessionRequest => UserSession,
        set_goal: SetGoal : SetGoalRequest => SetGoalResponse,
        send_message: SendMessage : SendMessageRequest => SendMessageResponse,
        interrupt: Interrupt : InterruptRequest => InterruptResponse,
        compact: Compact : CompactRequest => CompactResponse,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures_util::StreamExt;
    use std::sync::atomic::{AtomicBool, Ordering};

    #[tokio::test]
    async fn observation_drop_cancels_retained_observer_without_lost_wakeup() {
        let observation = CallerSignal(Arc::new(Signal::new()));
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
        let mut stream = event_stream(Box::new(source), CallerSignal(Arc::new(Signal::new())));
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
        let observation = CallerSignal(Arc::new(Signal::new()));
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
            let observation = CallerSignal(Arc::new(Signal::new()));
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
        let observation = CallerSignal(Arc::new(Signal::new()));
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
        let observation = CallerSignal(Arc::new(Signal::new()));
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
