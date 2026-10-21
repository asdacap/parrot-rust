use super::{models::ModelOperations, *};
use parrot_domain::{
    contracts::async_ops::{BoxFuture, Cancellation},
    protocol::messages::parrot as wire,
    values::{
        context::ContextSize,
        identifiers::{Identifier, UserSessionId},
    },
};
use parrot_runtime::{
    interactions::{InteractionError, InteractionErrorKind, QuestionReply, QuestionReplyKind},
    prompt_context::{SkillMetadata, SkillScope},
};
use parrot_storage::{
    artifacts::{self, ImageArtifactMetadata},
    diagnostics::{DiagnosticEvent, DiagnosticSeverity, FileDiagnosticLog},
    events::ConversationPart,
    history_files::{SessionCatalog, SessionCatalogState},
    ownership::{
        ClaimDisposition, SessionModelSelection, SessionStore, SessionStoreError,
        WorkingDirectoryClaim,
    },
};
use parrot_tools::process::execution::runner::SandboxGate;
use std::{
    path::Path,
    sync::{Arc, Mutex},
    time::Instant,
};

/// Source ParrotService. All acquired sessions pass through the concrete store,
/// the authoritative factory binding, and the listener registry, in that order.
pub struct ParrotService {
    models: Arc<ModelOperations>,
    store: Arc<SessionStore>,
    catalog: SessionCatalog,
    factory: Arc<dyn HostedSessionFactory>,
    host: Arc<dyn SessionHost>,
    claim: Arc<WorkingDirectoryClaim>,
    sandbox: Arc<SandboxGate>,
    diagnostics: Arc<FileDiagnosticLog>,
    sessions: Arc<UserSessionRegistry>,
}
impl ParrotService {
    /// The construction owner must retain the actual `owner_runtime` Runtime
    /// through `close().await`, including the registry close owner's actual join.
    /// A Handle or an RPC observer alone is not a runtime lifetime guarantee.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        models: Arc<ModelOperations>,
        store: Arc<SessionStore>,
        catalog: SessionCatalog,
        factory: Arc<dyn HostedSessionFactory>,
        host: Arc<dyn SessionHost>,
        claim: Arc<WorkingDirectoryClaim>,
        sandbox: Arc<SandboxGate>,
        diagnostics: Arc<FileDiagnosticLog>,
        owner_runtime: tokio::runtime::Handle,
    ) -> Self {
        Self {
            models,
            store,
            catalog,
            factory,
            host,
            claim,
            sandbox,
            diagnostics,
            sessions: UserSessionRegistry::new(owner_runtime),
        }
    }
    pub fn sessions(&self) -> &Arc<UserSessionRegistry> {
        &self.sessions
    }
    pub fn diagnostics(&self) -> &Arc<FileDiagnosticLog> {
        &self.diagnostics
    }
    pub async fn list_models(
        &self,
        _: &wire::ListModelsRequest,
        cancellation: &dyn Cancellation,
    ) -> Result<wire::ListModelsResponse> {
        let mut trace = Trace::new(self.diagnostics.clone(), "list_models")?;
        let result = self.models.list_models(cancellation).await;
        trace.finish(&result);
        result
    }
    pub fn list_model_aliases(
        &self,
        _: &wire::ListModelAliasesRequest,
    ) -> wire::ListModelAliasesResponse {
        self.models.list_model_aliases()
    }
    pub fn configure_model_alias(
        &self,
        request: &wire::ConfigureModelAliasRequest,
    ) -> Result<wire::ConfigureModelAliasResponse> {
        self.models.configure_model_alias(request)
    }
    pub async fn list_provider_model_alias_defaults(
        &self,
        _: &wire::ListProviderModelAliasDefaultsRequest,
        cancellation: &dyn Cancellation,
    ) -> Result<wire::ListProviderModelAliasDefaultsResponse> {
        self.models
            .list_provider_model_alias_defaults(cancellation)
            .await
    }
    pub async fn apply_provider_model_alias_defaults(
        &self,
        request: &wire::ApplyProviderModelAliasDefaultsRequest,
        cancellation: &dyn Cancellation,
    ) -> Result<wire::ApplyProviderModelAliasDefaultsResponse> {
        self.models
            .apply_provider_model_alias_defaults(request, cancellation)
            .await
    }
    pub fn list_model_presets(
        &self,
        _: &wire::ListModelPresetsRequest,
        cancellation: &dyn Cancellation,
    ) -> Result<wire::ListModelPresetsResponse> {
        self.models.list_model_presets(cancellation)
    }
    pub fn set_model_preset(
        &self,
        request: &wire::SetModelPresetRequest,
        cancellation: &dyn Cancellation,
    ) -> Result<wire::SetModelPresetResponse> {
        check(cancellation)?;
        let session = self.sessions.find(&request.user_session_id)?;
        self.models
            .set_model_preset(request, &session.snapshot().model, cancellation)
    }
    pub fn select_model_preset(
        &self,
        request: &wire::SelectModelPresetRequest,
        cancellation: &dyn Cancellation,
    ) -> Result<wire::SelectModelPresetResponse> {
        check(cancellation)?;
        let session = self.sessions.find(&request.user_session_id)?;
        let (preset, _) = self.models.select_model_preset(
            request,
            &session.snapshot().model,
            |selected| session.update_selection(selected),
            cancellation,
        )?;
        Ok(wire::SelectModelPresetResponse {
            session: Some(map_session(session.as_ref(), false)).into(),
            preset: Some(preset).into(),
            ..Default::default()
        })
    }
    pub fn list_modes(&self, _: &wire::ListModesRequest) -> wire::ListModesResponse {
        wire::ListModesResponse {
            modes: self
                .factory
                .list_modes()
                .into_iter()
                .map(|id| wire::Mode {
                    id,
                    ..Default::default()
                })
                .collect(),
            ..Default::default()
        }
    }
    pub async fn list_skills(
        &self,
        request: &wire::ListSkillsRequest,
        cancellation: &dyn Cancellation,
    ) -> Result<wire::ListSkillsResponse> {
        check(cancellation)?;
        let session = self.sessions.find(&request.user_session_id)?;
        let snapshot = session.refresh_skills(cancellation).await?;
        Ok(wire::ListSkillsResponse {
            skills: snapshot.skills.into_iter().map(map_skill).collect(),
            errors: snapshot
                .errors
                .into_iter()
                .map(|error| wire::SkillLoadError {
                    path: limit(&error.path),
                    message: limit(&error.message),
                    ..Default::default()
                })
                .collect(),
            ..Default::default()
        })
    }
    pub async fn configure_skill(
        &self,
        request: &wire::ConfigureSkillRequest,
        cancellation: &dyn Cancellation,
    ) -> Result<wire::ConfigureSkillResponse> {
        check(cancellation)?;
        if request.path.trim().is_empty() || !Path::new(&request.path).is_absolute() {
            return Err(invalid("skill path must be an absolute catalog path"));
        }
        let session = self.sessions.find(&request.user_session_id)?;
        let skill = session
            .configure_skill(&request.path, request.enabled, cancellation)
            .await?;
        Ok(wire::ConfigureSkillResponse {
            skill: Some(map_skill(skill)).into(),
            ..Default::default()
        })
    }
    pub fn list_sessions(
        &self,
        request: &wire::ListSessionsRequest,
    ) -> Result<wire::ListSessionsResponse> {
        let workspace = if request.working_directory.is_empty() {
            None
        } else {
            Some(
                self.factory
                    .resolve_workspace(Path::new(&request.working_directory))
                    .map_err(|_| invalid("a valid workspace directory is required"))?,
            )
        };
        let mut entries = self.catalog.list();
        if let Some(workspace) = workspace {
            entries.retain(|entry| {
                self.factory
                    .resolve_workspace(Path::new(&entry.working_directory))
                    .is_ok_and(|candidate| candidate == workspace)
            });
        }
        entries.sort_by(|left, right| {
            left.created_at
                .as_deref()
                .unwrap_or("")
                .encode_utf16()
                .cmp(right.created_at.as_deref().unwrap_or("").encode_utf16())
                .then_with(|| {
                    left.id
                        .value()
                        .encode_utf16()
                        .cmp(right.id.value().encode_utf16())
                })
        });
        Ok(wire::ListSessionsResponse {
            sessions: entries
                .into_iter()
                .map(|entry| {
                    let state = if self.sessions.contains(entry.id.value()) {
                        wire::SessionState::SESSION_STATE_ACTIVE
                    } else if entry.state == SessionCatalogState::Corrupt {
                        wire::SessionState::SESSION_STATE_CORRUPT
                    } else if self
                        .claim
                        .is_active(&entry.id, Path::new(&entry.working_directory))
                        .unwrap_or(false)
                    {
                        wire::SessionState::SESSION_STATE_ACTIVE
                    } else {
                        wire::SessionState::SESSION_STATE_INACTIVE
                    };
                    wire::SessionSummary {
                        user_session_id: entry.id.value().into(),
                        model: entry.model,
                        mode: entry.mode.unwrap_or_default(),
                        created_at: entry.created_at.unwrap_or_default(),
                        root_agent_name: entry.root_agent_name.unwrap_or_default(),
                        state: state.into(),
                        ..Default::default()
                    }
                })
                .collect(),
            ..Default::default()
        })
    }
    pub async fn create_session(
        &self,
        request: &wire::CreateSessionRequest,
        cancellation: Arc<dyn Cancellation>,
    ) -> Result<wire::UserSession> {
        let mut trace = Trace::new(self.diagnostics.clone(), "create_session")?;
        let result = async {
            let model = self.models.resolve(&request.model)?;
            self.factory.resolve_mode(&request.mode)?;
            let model = SessionModelSelection {
                provider_id: model.canonical_model.provider.id().into(),
                canonical_model: model.canonical_model.selector(),
                selector: model.requested_selector.value.clone(),
            };
            let store = self.store.clone();
            let factory = self.factory.clone();
            let mode = request.mode.clone();
            let interactive = request.interactive_permissions;
            let opened = Box::pin(async move {
                let opened = store
                    .create_fresh(model, &mode, interactive)
                    .await
                    .map_err(|error| store_error(error, false))?;
                factory.bind_opened(opened).await
            });
            let session = self
                .sessions
                .host(opened, self.traced_host(&trace), cancellation)
                .await?;
            *trace
                .user_session_id
                .lock()
                .expect("protocol trace poisoned") = Some(session.snapshot().id.clone());
            Ok(map_session(session.as_ref(), false))
        }
        .await;
        trace.finish(&result);
        result
    }
    pub async fn resume_session(
        &self,
        request: &wire::ResumeSessionRequest,
        cancellation: Arc<dyn Cancellation>,
    ) -> Result<wire::UserSession> {
        let mut trace = Trace::new(self.diagnostics.clone(), "resume_session")?;
        let result = async {
            check(cancellation.as_ref())?;
            let id = parse_id(&request.user_session_id)?;
            *trace
                .user_session_id
                .lock()
                .expect("protocol trace poisoned") = Some(id.value().into());
            let metadata = self
                .catalog
                .find(&id)
                .ok_or_else(|| RpcError::new(RpcCode::NotFound, format!("no user session {id}")))?;
            if metadata.state == SessionCatalogState::Corrupt {
                return Err(RpcError::new(
                    RpcCode::FailedPrecondition,
                    "session metadata is corrupt",
                ));
            }
            self.validate_workspace(&request.working_directory, &metadata.working_directory)?;
            let store = self.store.clone();
            let factory = self.factory.clone();
            let interactive = request.interactive_permissions;
            let takeover = request.take_over;
            let opened = Box::pin(async move {
                let opened = store
                    .resume(&id, interactive, takeover)
                    .await
                    .map_err(|error| store_error(error, true))?;
                factory.bind_opened(opened).await
            });
            let session = self
                .sessions
                .host(opened, self.traced_host(&trace), cancellation)
                .await?;
            *trace
                .user_session_id
                .lock()
                .expect("protocol trace poisoned") = Some(session.snapshot().id.clone());
            Ok(map_session(session.as_ref(), true))
        }
        .await;
        trace.finish(&result);
        result
    }
    pub fn attach_session(
        &self,
        request: &wire::AttachSessionRequest,
        cancellation: &dyn Cancellation,
    ) -> Result<wire::UserSession> {
        let mut trace = Trace::new(self.diagnostics.clone(), "attach_session")?;
        let result = (|| {
            check(cancellation)?;
            let id = parse_id(&request.user_session_id)?;
            let session = self.sessions.find(id.value())?;
            trace.session = Some(session.clone());
            self.validate_workspace(
                &request.working_directory,
                &session.snapshot().working_directory,
            )?;
            session.record_opened()?;
            Ok(map_session(session.as_ref(), false))
        })();
        trace.finish(&result);
        result
    }
    pub async fn update_session(
        &self,
        request: &wire::UpdateSessionRequest,
    ) -> Result<wire::UserSession> {
        let session = self.sessions.find(&request.user_session_id)?;
        let mode = if request.mode.is_empty() {
            None
        } else {
            Some(session.resolve_mode(&request.mode)?)
        };
        let model = if request.model.is_empty() {
            None
        } else {
            Some(self.models.resolve(&request.model)?)
        };
        session.update(model, mode).await?;
        Ok(map_session(session.as_ref(), false))
    }
    pub async fn set_goal(
        &self,
        request: &wire::SetGoalRequest,
        cancellation: Arc<dyn Cancellation>,
    ) -> Result<wire::SetGoalResponse> {
        let session = self.sessions.find(&request.user_session_id)?;
        let goal = match &request.state {
            Some(wire::set_goal_request::State::Goal(goal)) if !goal.is_empty() => {
                Some(goal.clone())
            }
            Some(wire::set_goal_request::State::Clear(_)) => None,
            Some(wire::set_goal_request::State::Goal(_)) => {
                return Err(invalid("a goal is required"));
            }
            _ => return Err(invalid("a goal or clear state is required")),
        };
        session.set_goal(goal, cancellation).await?;
        Ok(wire::SetGoalResponse::new())
    }
    pub fn set_standing_instruction(
        &self,
        request: &wire::SetStandingInstructionRequest,
    ) -> Result<wire::SetStandingInstructionResponse> {
        self.sessions
            .find(&request.user_session_id)?
            .set_standing_instruction(request.instruction.clone())?;
        Ok(wire::SetStandingInstructionResponse::new())
    }
    pub async fn upload_attachment(
        &self,
        stream: &mut dyn UploadStream,
        cancellation: &dyn Cancellation,
    ) -> Result<wire::AttachmentUploadResponse> {
        let first = stream
            .next(cancellation)
            .await?
            .ok_or_else(|| invalid("the first upload frame must be a header"))?;
        let Some(wire::attachment_upload_frame::Payload::Header(header)) = first.payload else {
            return Err(invalid("the first upload frame must be a header"));
        };
        if header.user_session_id.trim().is_empty() || header.upload_id.trim().is_empty() {
            return Err(invalid(
                "an upload header needs a user session id and upload id",
            ));
        }
        let session = self.sessions.find(&header.user_session_id)?;
        let mut content = Vec::new();
        let mut description = None;
        let mut chunks = 0;
        while let Some(frame) = stream.next(cancellation).await? {
            if description.is_some() {
                return Err(invalid("the upload description must be the terminal frame"));
            }
            match frame.payload {
                Some(wire::attachment_upload_frame::Payload::Chunk(chunk)) => {
                    if chunk.is_empty() || chunk.len() > 1024 * 1024 {
                        return Err(invalid("an upload chunk must contain at most 1 MiB"));
                    }
                    if content.len() > 5 * 1024 * 1024 - chunk.len() {
                        return Err(invalid("an image cannot exceed 5 MiB"));
                    }
                    content.extend_from_slice(&chunk);
                    chunks += 1;
                }
                Some(wire::attachment_upload_frame::Payload::Description(value)) => {
                    if chunks == 0 {
                        return Err(invalid("an upload needs at least one chunk"));
                    }
                    description = Some(value);
                }
                _ => {
                    return Err(invalid(
                        "upload frames must contain chunks followed by one description",
                    ));
                }
            }
        }
        let description =
            description.ok_or_else(|| invalid("an upload needs a terminal description"))?;
        validate_upload_description(&description)?;
        let artifact = session
            .persist_image(
                ImageUpload {
                    content,
                    upload_id: header.upload_id,
                    display_name: description.display_name,
                    media_type: description.media_type,
                    byte_length: description.byte_length,
                },
                cancellation,
            )
            .await?;
        Ok(wire::AttachmentUploadResponse {
            artifact: Some(map_artifact(artifact)).into(),
            ..Default::default()
        })
    }
    pub async fn send_message(
        &self,
        request: &wire::SendMessageRequest,
        cancellation: Arc<dyn Cancellation>,
    ) -> Result<wire::SendMessageResponse> {
        let delivery = match request.delivery.enum_value() {
            Ok(wire::Delivery::DELIVERY_STEER) => wire::Delivery::DELIVERY_STEER,
            Ok(wire::Delivery::DELIVERY_QUEUE) => wire::Delivery::DELIVERY_QUEUE,
            _ => return Err(invalid("a prompt needs a delivery")),
        };
        let session = self.sessions.find(&request.user_session_id)?;
        if !request.text.is_empty() && !request.parts.is_empty() {
            return Err(invalid(
                "legacy text and structured prompt parts are mutually exclusive",
            ));
        }
        let parts = if request.parts.is_empty() {
            vec![ConversationPart::text(&request.text)]
        } else {
            resolve_parts(session.as_ref(), &request.parts).await?
        };
        let message_id = if request.message_id.is_empty() {
            Identifier::message_id().map_err(internal)?
        } else {
            request.message_id.clone()
        };
        let admitted = session
            .send(parts, message_id, delivery, cancellation)
            .await?;
        let created = admitted.created();
        Ok(wire::SendMessageResponse {
            message_id: admitted.input.message_id,
            input_id: admitted.input.id,
            created,
            ..Default::default()
        })
    }
    pub async fn interrupt(
        &self,
        request: &wire::InterruptRequest,
        cancellation: Arc<dyn Cancellation>,
    ) -> Result<wire::InterruptResponse> {
        self.sessions
            .find(&request.user_session_id)?
            .interrupt(cancellation)
            .await?;
        Ok(wire::InterruptResponse::new())
    }
    pub async fn compact(
        &self,
        request: &wire::CompactRequest,
        cancellation: Arc<dyn Cancellation>,
    ) -> Result<wire::CompactResponse> {
        let target = if request.target_context_size.is_empty() {
            None
        } else {
            Some(
                ContextSize::parse(&request.target_context_size)
                    .map_err(|error| invalid(error.to_string()))?,
            )
        };
        self.sessions
            .find(&request.user_session_id)?
            .compact(target, cancellation)
            .await?;
        Ok(wire::CompactResponse::new())
    }
    pub fn set_context_limit(
        &self,
        request: &wire::SetContextLimitRequest,
        cancellation: &dyn Cancellation,
    ) -> Result<wire::SetContextLimitResponse> {
        check(cancellation)?;
        let session = self.sessions.find(&request.user_session_id)?;
        self.models
            .set_context_limit(request, &session.snapshot().model, cancellation)
    }
    pub fn sandbox_enable(
        &self,
        request: &wire::SandboxEnableRequest,
        cancellation: &dyn Cancellation,
    ) -> Result<wire::SandboxEnableResponse> {
        check(cancellation)?;
        let _ = self.sessions.find(&request.user_session_id)?;
        self.sandbox.set_enabled(request.enabled);
        if !self.sandbox.enabled() {
            let mut event =
                DiagnosticEvent::new("sandbox", "disabled", DiagnosticSeverity::Warning);
            event.user_session_id = Some(request.user_session_id.clone());
            self.diagnostics.write(&event);
        }
        Ok(wire::SandboxEnableResponse {
            enabled: self.sandbox.enabled(),
            ..Default::default()
        })
    }
    pub fn list_pending_questions(
        &self,
        request: &wire::ListPendingQuestionsRequest,
    ) -> Result<wire::ListPendingQuestionsResponse> {
        let session = self.sessions.find(&request.user_session_id)?;
        Ok(wire::ListPendingQuestionsResponse {
            questions: session
                .pending_questions()?
                .into_iter()
                .map(map_question)
                .collect(),
            ..Default::default()
        })
    }
    pub fn reply_question(
        &self,
        request: &wire::ReplyQuestionRequest,
    ) -> Result<wire::ReplyQuestionResponse> {
        if request.question_request_id.is_empty() {
            return Err(invalid("a question request id is required"));
        }
        self.sessions
            .find(&request.user_session_id)?
            .reply_question(
                &request.question_request_id,
                QuestionReply {
                    kind: QuestionReplyKind::Answered,
                    answers: request
                        .answers
                        .iter()
                        .map(|answer| answer.text.clone())
                        .collect(),
                },
            )
            .map_err(interaction_error)?;
        Ok(wire::ReplyQuestionResponse::new())
    }
    pub fn reject_question(
        &self,
        request: &wire::RejectQuestionRequest,
    ) -> Result<wire::RejectQuestionResponse> {
        if request.question_request_id.is_empty() {
            return Err(invalid("a question request id is required"));
        }
        self.sessions
            .find(&request.user_session_id)?
            .reject_question(&request.question_request_id)
            .map_err(interaction_error)?;
        Ok(wire::RejectQuestionResponse::new())
    }
    pub async fn list_pending_permissions(
        &self,
        request: &wire::ListPendingPermissionsRequest,
    ) -> Result<wire::ListPendingPermissionsResponse> {
        Ok(wire::ListPendingPermissionsResponse {
            permissions: self
                .sessions
                .find(&request.user_session_id)?
                .pending_permissions()
                .await?,
            ..Default::default()
        })
    }
    pub async fn reply_permission(
        &self,
        request: &wire::ReplyPermissionRequest,
    ) -> Result<wire::ReplyPermissionResponse> {
        if request.permission_request_id.is_empty() {
            return Err(invalid("a permission request id is required"));
        }
        self.sessions
            .find(&request.user_session_id)?
            .reply_permission(
                &request.permission_request_id,
                &request.choice_value,
                &request.reason,
            )
            .await
            .map_err(interaction_error)?;
        Ok(wire::ReplyPermissionResponse::new())
    }
    pub fn listen(
        &self,
        request: &wire::ListenRequest,
        cancellation: Arc<dyn Cancellation>,
    ) -> Result<Box<dyn SessionEventStream>> {
        self.sessions.find(&request.user_session_id)?.listen(
            request
                .replay
                .then(|| request.replay_after_event_id.clone()),
            cancellation,
        )
    }
    pub async fn session_status(
        &self,
        request: &wire::SessionStatusRequest,
        cancellation: &dyn Cancellation,
    ) -> Result<wire::SessionStatusResponse> {
        let mut trace = Trace::new(self.diagnostics.clone(), "session_status")?;
        let result = async {
            check(cancellation)?;
            let session = self.sessions.find(&request.user_session_id)?;
            trace.session = Some(session.clone());
            let resolved = self.models.resolve(&session.snapshot().model)?;
            let usage = self.models.usage_lines(&resolved, cancellation);
            let status = session.observe_status(cancellation);
            // Status failure is terminal immediately; a usage result (including
            // cancellation) is consumed only after status succeeds, as in source.
            let (usage_lines, status) =
                tokio::try_join!(async { Ok::<_, RpcError>(usage.await) }, status,)?;
            Ok(wire::SessionStatusResponse {
                status,
                usage_lines: usage_lines?,
                ..Default::default()
            })
        }
        .await;
        trace.finish(&result);
        result
    }
    pub async fn close(&self) -> Result<()> {
        self.sessions.close().await
    }
    fn validate_workspace(&self, requested: &str, current: &str) -> Result<()> {
        let requested = self.factory.resolve_workspace(Path::new(requested))?;
        let current = self.factory.resolve_workspace(Path::new(current))?;
        if requested != current {
            return Err(RpcError::new(
                RpcCode::FailedPrecondition,
                "the user session belongs to another workspace",
            ));
        }
        Ok(())
    }
    fn traced_host(&self, trace: &Trace) -> Arc<dyn SessionHost> {
        Arc::new(TracedHost {
            host: self.host.clone(),
            diagnostics: self.diagnostics.clone(),
            correlation_id: trace.correlation_id.clone(),
            user_session_id: trace.user_session_id.clone(),
        })
    }
}

pub fn map_session(session: &dyn HostedSession, loaded: bool) -> wire::UserSession {
    let mut value = session.snapshot();
    value.loaded = loaded;
    value
}
pub fn map_artifact(metadata: ImageArtifactMetadata) -> wire::ArtifactReference {
    wire::ArtifactReference {
        artifact_id: metadata.artifact_id,
        sha256: metadata.sha256,
        media_type: metadata.media_type,
        byte_length: metadata.byte_length,
        width: metadata.width as i32,
        height: metadata.height as i32,
        frame_count: metadata.frame_count as i32,
        aggregate_pixels: metadata.aggregate_pixels,
        display_name: metadata.display_name,
        origin: metadata.origin,
        ..Default::default()
    }
}
pub fn map_skill(skill: SkillMetadata) -> wire::Skill {
    wire::Skill {
        name: skill.name,
        description: skill.description,
        path: skill.path,
        scope: match skill.scope {
            SkillScope::Repo => wire::ListedSkillScope::SKILL_SCOPE_REPO,
            SkillScope::User => wire::ListedSkillScope::SKILL_SCOPE_USER,
            SkillScope::System => wire::ListedSkillScope::SKILL_SCOPE_SYSTEM,
        }
        .into(),
        enabled: skill.enabled,
        display_name: skill.display_name,
        short_description: skill.short_description,
        ..Default::default()
    }
}
pub fn map_question(
    pending: parrot_runtime::interactions::PendingQuestion,
) -> wire::PendingQuestion {
    wire::PendingQuestion {
        id: pending.id,
        remaining_timeout_ms: pending
            .remaining_milliseconds
            .map(|value| value.min(i64::MAX as u128) as i64),
        questions: pending
            .questions
            .into_iter()
            .map(|question| wire::QuestionDefinition {
                header: question.header,
                prompt: question.prompt,
                multiple: question.multiple,
                custom: question.custom,
                options: question
                    .options
                    .into_iter()
                    .map(|option| wire::QuestionOption {
                        label: option.label,
                        description: option.description,
                        ..Default::default()
                    })
                    .collect(),
                ..Default::default()
            })
            .collect(),
        ..Default::default()
    }
}
pub fn validate_upload_description(description: &wire::AttachmentUploadDescription) -> Result<()> {
    if description.display_name.trim().is_empty()
        || Path::new(&description.display_name)
            .file_name()
            .and_then(|name| name.to_str())
            != Some(&description.display_name)
        || description.display_name.chars().any(char::is_control)
    {
        return Err(invalid("an upload display name must be a basename"));
    }
    if description.media_type.trim().is_empty() || description.byte_length <= 0 {
        return Err(invalid(
            "an upload description needs a media type and positive byte length",
        ));
    }
    Ok(())
}
pub async fn resolve_parts(
    session: &dyn HostedSession,
    parts: &[wire::MessageContentPart],
) -> Result<Vec<ConversationPart>> {
    let mut resolved = Vec::new();
    let mut images = 0;
    let mut bytes = 0i64;
    for part in parts {
        match &part.content {
            Some(wire::message_content_part::Content::Text(text)) => {
                resolved.push(ConversationPart::text(text))
            }
            Some(wire::message_content_part::Content::ArtifactId(id)) => {
                let artifact = session.resolve_image(id).await?.ok_or_else(|| {
                    invalid(format!(
                        "image artifact {id} does not belong to this session"
                    ))
                })?;
                images += 1;
                bytes = bytes.saturating_add(artifact.byte_length);
                if images > artifacts::ImageLimits::SOURCE.maximum_images
                    || bytes > artifacts::ImageLimits::SOURCE.maximum_batch_bytes as i64
                {
                    return Err(invalid(
                        "the prompt image batch exceeds the supported limits",
                    ));
                }
                resolved.push(ConversationPart::image(
                    artifact.artifact_id,
                    artifact.media_type,
                    artifact.display_name,
                ));
            }
            _ => return Err(invalid("every prompt part needs text or an artifact id")),
        }
    }
    if resolved.is_empty() {
        return Err(invalid("a structured prompt needs at least one part"));
    }
    Ok(resolved)
}
fn parse_id(value: &str) -> Result<UserSessionId> {
    UserSessionId::try_parse(value).ok_or_else(|| invalid("a valid user session id is required"))
}
fn limit(value: &str) -> String {
    String::from_utf16_lossy(&value.encode_utf16().take(4096).collect::<Vec<_>>())
}
fn invalid(message: impl Into<String>) -> RpcError {
    RpcError::new(RpcCode::InvalidArgument, message)
}
fn internal(error: impl std::fmt::Display) -> RpcError {
    RpcError::new(RpcCode::Internal, error.to_string())
}
fn interaction_error(error: InteractionError) -> RpcError {
    let code = match error.kind {
        InteractionErrorKind::QuestionNotFound | InteractionErrorKind::PermissionNotFound => {
            RpcCode::NotFound
        }
        InteractionErrorKind::Question
        | InteractionErrorKind::Permission
        | InteractionErrorKind::Argument => RpcCode::InvalidArgument,
        _ => RpcCode::Internal,
    };
    RpcError::new(code, error.message)
}
fn store_error(error: SessionStoreError, resumed: bool) -> RpcError {
    let code = match &error {
        SessionStoreError::Cancelled => RpcCode::Cancelled,
        SessionStoreError::Admission(ClaimDisposition::Live | ClaimDisposition::Contended)
            if resumed =>
        {
            RpcCode::AlreadyExists
        }
        SessionStoreError::Admission(_) | SessionStoreError::InvalidMetadata => {
            RpcCode::FailedPrecondition
        }
        SessionStoreError::Failure(failure)
            if failure
                .downcast_ref::<parrot_provider::sessions::Error>()
                .is_some()
                || failure
                    .downcast_ref::<RpcError>()
                    .is_some_and(|error| error.code == RpcCode::InvalidArgument) =>
        {
            if resumed {
                RpcCode::FailedPrecondition
            } else {
                RpcCode::InvalidArgument
            }
        }
        _ => RpcCode::Internal,
    };
    RpcError::new(code, error.to_string())
}

struct Trace {
    global: Arc<FileDiagnosticLog>,
    session: Option<Arc<dyn HostedSession>>,
    operation: &'static str,
    correlation_id: String,
    user_session_id: Arc<Mutex<Option<String>>>,
    started: Instant,
    finished: bool,
}
impl Trace {
    fn new(global: Arc<FileDiagnosticLog>, operation: &'static str) -> Result<Self> {
        let correlation_id = Identifier::event_id().map_err(internal)?.replace('-', "");
        let trace = Self {
            global,
            session: None,
            operation,
            correlation_id,
            user_session_id: Arc::new(Mutex::new(None)),
            started: Instant::now(),
            finished: false,
        };
        trace.write("start", DiagnosticSeverity::Information, None, None);
        Ok(trace)
    }
    fn write(
        &self,
        suffix: &str,
        severity: DiagnosticSeverity,
        outcome: Option<&str>,
        error: Option<&RpcError>,
    ) {
        let mut event =
            DiagnosticEvent::new("protocol", format!("{}_{suffix}", self.operation), severity);
        event.correlation_id = Some(self.correlation_id.clone());
        event.user_session_id = self
            .user_session_id
            .lock()
            .expect("protocol trace poisoned")
            .clone();
        event.outcome = outcome.map(str::to_owned);
        event.error_code = error.map(|error| error.code.name().into());
        if suffix == "complete" {
            event.duration_milliseconds =
                Some(self.started.elapsed().as_millis().min(i64::MAX as u128) as i64);
        }
        if let Some(session) = &self.session {
            session.write_diagnostic(&event);
        } else {
            self.global.write(&event);
        }
    }
    fn finish<T>(&mut self, result: &Result<T>) {
        let outcome = match result {
            Ok(_) => "succeeded",
            Err(error) if error.code == RpcCode::Cancelled => "cancelled",
            Err(_) => "failed",
        };
        if let Err(error) = result {
            self.write(
                "failure",
                if error.code == RpcCode::Cancelled {
                    DiagnosticSeverity::Information
                } else {
                    DiagnosticSeverity::Error
                },
                Some(outcome),
                Some(error),
            );
        }
        self.write(
            "complete",
            DiagnosticSeverity::Information,
            Some(outcome),
            None,
        );
        self.finished = true;
    }
}
impl Drop for Trace {
    fn drop(&mut self) {
        if !self.finished {
            self.write(
                "failure",
                DiagnosticSeverity::Information,
                Some("cancelled"),
                Some(&RpcError::cancelled()),
            );
            self.write(
                "complete",
                DiagnosticSeverity::Information,
                Some("cancelled"),
                None,
            );
        }
    }
}
struct TracedHost {
    user_session_id: Arc<Mutex<Option<String>>>,
    host: Arc<dyn SessionHost>,
    diagnostics: Arc<FileDiagnosticLog>,
    correlation_id: String,
}
impl SessionHost for TracedHost {
    fn host(
        &self,
        session: Arc<dyn HostedSession>,
        cancellation: Arc<dyn Cancellation>,
    ) -> BoxFuture<'_, Result<Arc<dyn SessionListener>>> {
        Box::pin(async move {
            let started = Instant::now();
            let mut event =
                DiagnosticEvent::new("session", "host_start", DiagnosticSeverity::Information);
            event.user_session_id = Some(session.snapshot().id);
            *self
                .user_session_id
                .lock()
                .expect("protocol trace poisoned") = event.user_session_id.clone();
            event.correlation_id = Some(self.correlation_id.clone());
            self.diagnostics.write(&event);
            let result = self.host.host(session, cancellation).await;
            event.operation = if result.is_ok() {
                "host_complete"
            } else {
                "host_failure"
            }
            .into();
            event.outcome = Some(
                match &result {
                    Ok(_) => "success",
                    Err(error) if error.code == RpcCode::Cancelled => "cancelled",
                    Err(_) => "failed",
                }
                .into(),
            );
            event.duration_milliseconds =
                Some(started.elapsed().as_millis().min(i64::MAX as u128) as i64);
            if let Err(error) = &result {
                event.error_code = Some(error.code.name().into());
                event.severity = if error.code == RpcCode::Cancelled {
                    DiagnosticSeverity::Information
                } else {
                    DiagnosticSeverity::Error
                };
            }
            self.diagnostics.write(&event);
            result
        })
    }
}
