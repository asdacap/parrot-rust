//! Browser projection of the source Parrot route map, using lossless messages.

use super::router::{HandlerCancellation, WebSessionRouter};
use futures_core::Stream;
use parrot_domain::protocol::{bindings::parrot::parrot_server::Parrot, messages::parrot as wire};
use std::{pin::Pin, sync::Arc};
use tonic::{Code, Request, Response, Status};

pub struct WebParrotProxy<S> {
    service: S,
    router: Arc<WebSessionRouter>,
}

impl<S> WebParrotProxy<S> {
    pub fn new(service: S, router: Arc<WebSessionRouter>) -> Self {
        Self { service, router }
    }
}

type EventStream = Pin<Box<dyn Stream<Item = Result<wire::Event, Status>> + Send + 'static>>;

macro_rules! implement_proxy {
    (local { $( $local:ident : $local_request:ident => $local_response:ident ),* $(,)? }
     routed { $( $routed:ident : $request:ident => $response:ident ),* $(,)? }) => {
        #[tonic::async_trait]
        impl<S: Parrot> Parrot for WebParrotProxy<S> {
            $(async fn $local(&self, request: Request<wire::$local_request>)
                -> Result<Response<wire::$local_response>, Status> {
                self.service.$local(request).await
            })*

            $(async fn $routed(&self, request: Request<wire::$request>)
                -> Result<Response<wire::$response>, Status> {
                let id = request.get_ref().user_session_id.clone();
                let result = self.router.client_for(&id).$routed(request).await;
                if result.as_ref().is_err_and(|error| error.code() == Code::Unavailable) {
                    self.router.forget(&id);
                }
                result
            })*

            async fn resume_session(&self, request: Request<wire::ResumeSessionRequest>)
                -> Result<Response<wire::UserSession>, Status> {
                let id = request.get_ref().user_session_id.clone();
                let mut attach = wire::AttachSessionRequest::new();
                attach.user_session_id = id.clone();
                attach.working_directory = request.get_ref().working_directory.clone();
                let (metadata, extensions, message) = request.into_parts();
                if !self.router.is_remote(&id) {
                    let resume = Request::from_parts(metadata.clone(), extensions.clone(), message);
                    match self.service.resume_session(resume).await {
                        Err(error) if error.code() == Code::AlreadyExists => {},
                        result => return result,
                    }
                }
                self.attach_session(Request::from_parts(metadata, extensions, attach)).await
            }

            async fn attach_session(&self, request: Request<wire::AttachSessionRequest>)
                -> Result<Response<wire::UserSession>, Status> {
                let id = request.get_ref().user_session_id.clone();
                if self.router.is_remote(&id) {
                    let result = self.router.client_for(&id).attach_session(request).await;
                    if result.as_ref().is_err_and(|error| error.code() == Code::Unavailable) {
                        self.router.forget(&id);
                    }
                    return result;
                }
                let (metadata, extensions, message) = request.into_parts();
                match self.service.attach_session(Request::from_parts(metadata, extensions, message.clone())).await {
                    Err(error) if error.code() == Code::NotFound => self.router
                        .attach_remote(message, &HandlerCancellation).await.map(Response::new),
                    result => result,
                }
            }

            async fn upload_attachment(&self, request: Request<tonic::Streaming<wire::AttachmentUploadFrame>>)
                -> Result<Response<wire::AttachmentUploadResponse>, Status> {
                self.service.upload_attachment(request).await
            }

            type ListenStream = EventStream;
            async fn listen(&self, request: Request<wire::ListenRequest>)
                -> Result<Response<Self::ListenStream>, Status> {
                let id = request.get_ref().user_session_id.clone();
                let response = match self.router.client_for(&id).listen(request).await {
                    Ok(response) => response,
                    Err(error) => {
                        if error.code() == Code::Unavailable { self.router.forget(&id); }
                        return Err(error);
                    }
                };
                let (metadata, source, extensions) = response.into_parts();
                let router = self.router.clone();
                let stream = futures_util::stream::unfold((Some(source), router, id),
                    |(mut source, router, id)| async move {
                        let result = source.as_mut()?.message().await;
                        match result {
                            Ok(Some(event)) => {
                                router.observe(&id, &event);
                                Some((Ok(event), (source, router, id)))
                            },
                            Ok(None) => match source.as_mut()?.trailers().await {
                                Ok(Some(metadata)) if !metadata.is_empty() => {
                                    // The generated stream trait has no trailer item. A
                                    // terminal OK status uses Tonic's normal trailer path.
                                    Some((Err(Status::with_metadata(Code::Ok, "", metadata)), (None, router, id)))
                                },
                                Ok(_) => None,
                                Err(error) => {
                                    if error.code() == Code::Unavailable { router.forget(&id); }
                                    Some((Err(error), (None, router, id)))
                                },
                            },
                            Err(error) => {
                                if error.code() == Code::Unavailable { router.forget(&id); }
                                Some((Err(error), (None, router, id)))
                            },
                        }
                    });
                Ok(Response::from_parts(metadata, Box::pin(stream) as EventStream, extensions))
            }
        }
    };
}

implement_proxy! {
    local {
        list_models: ListModelsRequest => ListModelsResponse,
        list_model_aliases: ListModelAliasesRequest => ListModelAliasesResponse,
        configure_model_alias: ConfigureModelAliasRequest => ConfigureModelAliasResponse,
        list_provider_model_alias_defaults: ListProviderModelAliasDefaultsRequest => ListProviderModelAliasDefaultsResponse,
        apply_provider_model_alias_defaults: ApplyProviderModelAliasDefaultsRequest => ApplyProviderModelAliasDefaultsResponse,
        list_model_presets: ListModelPresetsRequest => ListModelPresetsResponse,
        list_modes: ListModesRequest => ListModesResponse,
        create_session: CreateSessionRequest => UserSession,
        list_sessions: ListSessionsRequest => ListSessionsResponse,
    }
    routed {
        set_model_preset: SetModelPresetRequest => SetModelPresetResponse,
        select_model_preset: SelectModelPresetRequest => SelectModelPresetResponse,
        list_skills: ListSkillsRequest => ListSkillsResponse,
        configure_skill: ConfigureSkillRequest => ConfigureSkillResponse,
        update_session: UpdateSessionRequest => UserSession,
        set_goal: SetGoalRequest => SetGoalResponse,
        set_standing_instruction: SetStandingInstructionRequest => SetStandingInstructionResponse,
        send_message: SendMessageRequest => SendMessageResponse,
        interrupt: InterruptRequest => InterruptResponse,
        compact: CompactRequest => CompactResponse,
        set_context_limit: SetContextLimitRequest => SetContextLimitResponse,
        sandbox_enable: SandboxEnableRequest => SandboxEnableResponse,
        session_status: SessionStatusRequest => SessionStatusResponse,
        list_pending_questions: ListPendingQuestionsRequest => ListPendingQuestionsResponse,
        reply_question: ReplyQuestionRequest => ReplyQuestionResponse,
        reject_question: RejectQuestionRequest => RejectQuestionResponse,
        list_pending_permissions: ListPendingPermissionsRequest => ListPendingPermissionsResponse,
        reply_permission: ReplyPermissionRequest => ReplyPermissionResponse,
    }
}
