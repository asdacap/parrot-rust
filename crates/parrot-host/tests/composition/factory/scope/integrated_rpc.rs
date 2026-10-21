//! Public RPCs against the real SessionStore -> UserSessionFactory ->
//! AuthoritativeAgentSource -> HostedCore path. Only provider/OS capabilities and
//! the in-process listener boundary are synthetic; no HostedSession is replaced.
use super::*;

#[path = "persisted_selection.rs"]
mod persisted_selection;
use parrot_domain::protocol::messages::parrot as wire;
use parrot_host::composition::protocol::{
    ParrotService, SessionHost, SessionListener, UploadStream, models::ModelOperations,
};
use parrot_provider::routing::configuration::ModelConfigurationCoordinator;
use parrot_runtime::{
    agent_turn::lifecycle::{IncomingActivity, Signal},
    interactions::{
        InteractionErrorKind, PermissionDecision, QuestionDefinition, QuestionReplyKind,
    },
};
use parrot_storage::{history_files::SessionCatalog, ownership::WorkingDirectoryClaim};
use parrot_tools::process::execution::runner::SandboxGate;
use std::{
    collections::VecDeque,
    sync::atomic::{AtomicUsize, Ordering},
};

#[derive(Default)]
struct InProcessHost(Arc<AtomicUsize>);
struct InProcessListener {
    session: Arc<dyn HostedSession>,
    closed: Arc<AtomicUsize>,
}
impl SessionHost for InProcessHost {
    fn host(
        &self,
        session: Arc<dyn HostedSession>,
        cancellation: Arc<dyn Cancellation>,
    ) -> BoxFuture<'_, protocol::Result<Arc<dyn SessionListener>>> {
        let closed = self.0.clone();
        Box::pin(async move {
            cancellation.check().map_err(|_| RpcError::cancelled())?;
            Ok(Arc::new(InProcessListener { session, closed }) as Arc<dyn SessionListener>)
        })
    }
}
impl SessionListener for InProcessListener {
    fn close(&self) -> BoxFuture<'_, protocol::Result<()>> {
        Box::pin(async move {
            // Registry ordering must close the listener before the real core.
            self.session.pending_questions()?;
            self.closed.fetch_add(1, Ordering::SeqCst);
            Ok(())
        })
    }
}

struct RpcFixture {
    service: Arc<ParrotService>,
    store: Arc<SessionStore>,
    factory: Arc<UserSessionFactory<Security>>,
    observations: Arc<Mutex<Observations>>,
    models: Arc<ModelOperations>,
    coordinator: Arc<ModelConfigurationCoordinator>,
    host: Arc<InProcessHost>,
    claim: Arc<WorkingDirectoryClaim>,
    diagnostics: Arc<FileDiagnosticLog>,
    sandbox: Arc<SandboxGate>,
    state: PathBuf,
    workspace: String,
    config_path: PathBuf,
    requests: Arc<Mutex<Vec<Request>>>,
    bootstrap: wire::UserSession,
    owner_runtime: tokio::runtime::Handle,
}
impl RpcFixture {
    async fn new() -> Self {
        // The harness owns this executor through service close/join/publication.
        // RPC caller runtimes are observers, not the registry execution owner.
        let owner_runtime = tokio::runtime::Handle::current();
        let fixture = Fixture::new(false, false).await;
        // Obtain roots from an actual acquired lease, never a parallel index or
        // hand-built HostedSession. Its inactive session also tests real resume.
        let hosted = bounded(fixture.open()).await.unwrap();
        let bootstrap = hosted.snapshot();
        let state = fixture
            .user()
            .lease
            .selected_root()
            .path()
            .parent()
            .unwrap()
            .parent()
            .unwrap()
            .to_owned();
        let root = state.parent().unwrap();
        let config_path = root.join("rpc-config.yaml");
        std::fs::write(&config_path, "model: local/model\nmodel_aliases:\n  live:\n    model_string: local/model\n    usage: Local routing\nprovider_model_alias_defaults:\n  local:\n    low_llm: local/model\n    medium_llm: local/model\n    high_llm: local/model-next\n    xhigh_llm: local/model-next\n").unwrap();
        let environment = BTreeMap::from([("HOME".into(), root.to_string_lossy().into_owned())]);
        let configuration = Configuration::load(
            &config_path,
            &root.join("rpc-predefined.yaml"),
            &environment,
            |_| Ok(()),
        )
        .unwrap();
        let coordinator = Arc::new(ModelConfigurationCoordinator::new(
            configuration,
            fixture.routing.clone(),
            fixture.router.clone(),
        ));
        let models = Arc::new(ModelOperations::new(
            coordinator.clone(),
            fixture.registry.clone(),
            fixture.router.clone(),
        ));
        bounded(hosted.close()).await.unwrap();
        let claim = Arc::new(WorkingDirectoryClaim::new(
            &state,
            RuntimeIdentity::new(
                "host".into(),
                "boot".into(),
                123,
                "start".into(),
                "runtime".into(),
                Arc::new(|_| ProcessIdentity {
                    status: ProcessIdentityStatus::Found,
                    process_start_token: Some("start".into()),
                }),
            )
            .with_proven_comparable_process_start_tokens(),
        ));
        let diagnostics = Arc::new(
            FileDiagnosticLog::open_global(root.join("rpc-logs"), "actual-rpc-test", |_| {})
                .unwrap(),
        );
        let host = Arc::new(InProcessHost::default());
        let sandbox = Arc::new(SandboxGate::new(true));
        let store = Arc::new(fixture.store);
        let service = Arc::new(ParrotService::new(
            models.clone(),
            store.clone(),
            SessionCatalog::new(&state),
            fixture.factory.clone(),
            host.clone(),
            claim.clone(),
            sandbox.clone(),
            diagnostics.clone(),
            owner_runtime.clone(),
        ));
        Self {
            service,
            store,
            factory: fixture.factory,
            observations: fixture.observations,
            models,
            coordinator,
            host,
            claim,
            diagnostics,
            sandbox,
            workspace: bootstrap.working_directory.clone(),
            state,
            config_path,
            requests: fixture.requests,
            bootstrap,
            owner_runtime,
        }
    }
    fn new_service(&self) -> Arc<ParrotService> {
        Arc::new(ParrotService::new(
            self.models.clone(),
            self.store.clone(),
            SessionCatalog::new(&self.state),
            self.factory.clone(),
            self.host.clone(),
            self.claim.clone(),
            self.sandbox.clone(),
            self.diagnostics.clone(),
            self.owner_runtime.clone(),
        ))
    }
    async fn create(&self) -> wire::UserSession {
        bounded(self.service.create_session(
            &wire::CreateSessionRequest {
                model: "local/model".into(),
                mode: "build".into(),
                interactive_permissions: true,
                ..Default::default()
            },
            active(),
        ))
        .await
        .unwrap()
    }
    fn user(&self) -> Arc<UserServices<Security>> {
        self.observations.lock().unwrap().user.clone().unwrap()
    }
    fn root(&self) -> Arc<Scope<Security>> {
        self.observations.lock().unwrap().bound.clone().unwrap()
    }
    async fn close(&self) {
        bounded(self.service.close()).await.unwrap();
        assert_database_closed(&self.user());
    }
}
fn active() -> Arc<dyn Cancellation> {
    Arc::new(NeverCancelled)
}
fn code<T>(result: protocol::Result<T>, expected: RpcCode) {
    match result {
        Ok(_) => panic!("expected {expected:?}"),
        Err(error) => assert_eq!(error.code, expected, "{error}"),
    }
}
async fn until(mut ready: impl FnMut() -> bool) {
    bounded(async {
        while !ready() {
            tokio::task::yield_now().await;
        }
    })
    .await;
}
fn prompt(id: &str, message: &str) -> wire::SendMessageRequest {
    wire::SendMessageRequest {
        user_session_id: id.into(),
        text: "actual authoritative RPC input".into(),
        message_id: message.into(),
        delivery: wire::Delivery::DELIVERY_QUEUE.into(),
        ..Default::default()
    }
}
struct Upload(VecDeque<wire::AttachmentUploadFrame>);
impl UploadStream for Upload {
    fn next<'a>(
        &'a mut self,
        cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, protocol::Result<Option<wire::AttachmentUploadFrame>>> {
        Box::pin(async move {
            cancellation.check().map_err(|_| RpcError::cancelled())?;
            Ok(self.0.pop_front())
        })
    }
}

#[tokio::test]
async fn models_modes_create_attach_update_and_inactive_resume_use_real_constructor() {
    let f = RpcFixture::new().await;
    let modes = f.service.list_modes(&wire::ListModesRequest::new());
    assert_eq!(
        modes
            .modes
            .iter()
            .map(|m| m.id.as_str())
            .collect::<Vec<_>>(),
        ["build"]
    );
    let models = f
        .service
        .list_models(&wire::ListModelsRequest::new(), &NeverCancelled)
        .await
        .unwrap();
    // This fixture's local provider deliberately has no credential; model
    // resolution uses its actual seeded catalogue without claiming availability.
    assert!(models.models.is_empty());
    assert!(
        f.service
            .list_model_aliases(&wire::ListModelAliasesRequest::new())
            .aliases
            .iter()
            .any(|a| a.name == "live")
    );
    assert!(
        f.service
            .list_provider_model_alias_defaults(
                &wire::ListProviderModelAliasDefaultsRequest::new(),
                &NeverCancelled
            )
            .await
            .unwrap()
            .providers
            .is_empty()
    );
    code(
        f.service
            .apply_provider_model_alias_defaults(
                &wire::ApplyProviderModelAliasDefaultsRequest {
                    provider_id: "local".into(),
                    ..Default::default()
                },
                &NeverCancelled,
            )
            .await,
        RpcCode::InvalidArgument,
    );
    let session = f.create().await;
    assert!(!session.loaded);
    assert_eq!(session.mode, "build");
    assert_eq!(f.observations.lock().unwrap().constructors, 2);
    assert_eq!(f.user().id, session.id);
    assert!(Arc::ptr_eq(f.root().identity(), &f.user().root_identity));
    let attached = f
        .service
        .attach_session(
            &wire::AttachSessionRequest {
                user_session_id: session.id.clone(),
                working_directory: f.workspace.clone(),
                ..Default::default()
            },
            &NeverCancelled,
        )
        .unwrap();
    assert_eq!(attached.id, session.id);
    code(
        f.service.attach_session(
            &wire::AttachSessionRequest {
                user_session_id: session.id.clone(),
                working_directory: Path::new(&f.workspace)
                    .parent()
                    .unwrap()
                    .to_string_lossy()
                    .into_owned(),
                ..Default::default()
            },
            &NeverCancelled,
        ),
        RpcCode::FailedPrecondition,
    );
    let list = f
        .service
        .list_sessions(&wire::ListSessionsRequest {
            working_directory: f.workspace.clone(),
            ..Default::default()
        })
        .unwrap();
    assert_eq!(list.sessions.len(), 2);
    assert!(list.sessions.iter().any(|s| s.user_session_id == session.id
        && s.state.enum_value().unwrap() == wire::SessionState::SESSION_STATE_ACTIVE));
    let changed = f
        .service
        .configure_model_alias(&wire::ConfigureModelAliasRequest {
            name: "live".into(),
            model_string: "local/model-next".into(),
            ..Default::default()
        })
        .unwrap();
    assert_eq!(changed.alias.unwrap().model_string, "local/model-next");
    let updated = f
        .service
        .update_session(&wire::UpdateSessionRequest {
            user_session_id: session.id.clone(),
            model: "live".into(),
            mode: "build".into(),
            ..Default::default()
        })
        .await
        .unwrap();
    assert_eq!(updated.model, "live");
    let preset = f
        .service
        .set_model_preset(
            &wire::SetModelPresetRequest {
                user_session_id: session.id.clone(),
                name: "rpc-preset".into(),
                ..Default::default()
            },
            &NeverCancelled,
        )
        .unwrap();
    assert_eq!(preset.preset.unwrap().model, "live");
    assert!(
        f.service
            .list_model_presets(&wire::ListModelPresetsRequest::new(), &NeverCancelled)
            .unwrap()
            .presets
            .iter()
            .any(|p| p.name == "rpc-preset")
    );
    let selected = f
        .service
        .select_model_preset(
            &wire::SelectModelPresetRequest {
                user_session_id: session.id.clone(),
                name: "rpc-preset".into(),
                ..Default::default()
            },
            &NeverCancelled,
        )
        .unwrap();
    assert_eq!(selected.session.unwrap().model, "live");
    let limit = f
        .service
        .set_context_limit(
            &wire::SetContextLimitRequest {
                user_session_id: session.id.clone(),
                context_limit: "32k".into(),
                ..Default::default()
            },
            &NeverCancelled,
        )
        .unwrap();
    assert_eq!(limit.context_limit, "32000");
    assert_eq!(
        f.coordinator
            .capture_routing()
            .context_limit
            .as_ref()
            .unwrap()
            .to_string(),
        "32000"
    );
    assert!(
        std::fs::read_to_string(&f.config_path)
            .unwrap()
            .contains("rpc-preset")
    );
    for enabled in [false, true] {
        assert_eq!(
            f.service
                .sandbox_enable(
                    &wire::SandboxEnableRequest {
                        user_session_id: session.id.clone(),
                        enabled,
                        ..Default::default()
                    },
                    &NeverCancelled
                )
                .unwrap()
                .enabled,
            enabled
        );
        assert_eq!(f.sandbox.enabled(), enabled);
    }
    f.close().await;
    assert_eq!(f.host.0.load(Ordering::SeqCst), 1);
    let resumed_service = f.new_service();
    let resumed = bounded(resumed_service.resume_session(
        &wire::ResumeSessionRequest {
            user_session_id: session.id.clone(),
            working_directory: f.workspace.clone(),
            interactive_permissions: true,
            ..Default::default()
        },
        active(),
    ))
    .await
    .unwrap();
    assert_eq!(resumed.id, session.id);
    assert!(resumed.loaded);
    assert_eq!(resumed.model, "live");
    assert_eq!(f.user().id, session.id);
    bounded(resumed_service.close()).await.unwrap();
    assert_database_closed(&f.user());
    assert_eq!(f.host.0.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn status_before_text_input_keeps_real_skills_prompt_usable_and_replay_is_durable() {
    let f = RpcFixture::new().await;
    let session = f.create().await;
    let id = &session.id;
    let status = bounded(f.service.session_status(
        &wire::SessionStatusRequest {
            user_session_id: id.clone(),
            ..Default::default()
        },
        &NeverCancelled,
    ))
    .await
    .unwrap();
    assert!(!status.status.is_empty());
    assert_eq!(status.usage_lines, ["Provider does not report usage"]);
    assert!(
        f.requests.lock().unwrap().is_empty(),
        "observation cannot invoke the provider"
    );
    let skills = f
        .service
        .list_skills(
            &wire::ListSkillsRequest {
                user_session_id: id.clone(),
                ..Default::default()
            },
            &NeverCancelled,
        )
        .await
        .unwrap();
    assert!(skills.skills.is_empty());
    assert!(skills.errors.is_empty());
    code(
        f.service
            .configure_skill(
                &wire::ConfigureSkillRequest {
                    user_session_id: id.clone(),
                    path: "/synthetic/not-discovered/SKILL.md".into(),
                    enabled: false,
                    ..Default::default()
                },
                &NeverCancelled,
            )
            .await,
        RpcCode::NotFound,
    );
    f.service
        .set_standing_instruction(&wire::SetStandingInstructionRequest {
            user_session_id: id.clone(),
            instruction: "RPC standing instruction".into(),
            ..Default::default()
        })
        .unwrap();
    let mut stream = f
        .service
        .listen(
            &wire::ListenRequest {
                user_session_id: id.clone(),
                replay: true,
                ..Default::default()
            },
            active(),
        )
        .unwrap();
    let request = prompt(id, "rpc-text-after-status");
    let admitted = bounded(f.service.send_message(&request, active()))
        .await
        .unwrap();
    assert!(admitted.created);
    let retry = bounded(f.service.send_message(&request, active()))
        .await
        .unwrap();
    assert!(!retry.created);
    assert_eq!(retry.input_id, admitted.input_id);
    let root = f.root();
    let runtime = root.get_service::<RuntimeSession<Security>>().unwrap();
    let drained = tokio::time::timeout(Duration::from_secs(10), async {
        while !runtime.is_idle() || f.requests.lock().unwrap().is_empty() {
            tokio::task::yield_now().await;
        }
    })
    .await;
    let events = f.user().lease.events.replay().await.unwrap();
    assert!(
        drained.is_ok(),
        "state={:?}; provider calls={}; durable events={events:?}",
        runtime.state(),
        f.requests.lock().unwrap().len()
    );
    assert_eq!(
        events
            .iter()
            .filter(|e| e.has_input_admitted()
                && e.input_admitted().message_id == "rpc-text-after-status")
            .count(),
        1
    );
    assert!(events.iter().any(|e| e.has_turn_ended()));
    {
        let requests = f.requests.lock().unwrap();
        assert_eq!(
            requests.len(),
            1,
            "same actual provider drain succeeds after external SkillPrompt build"
        );
        assert!(
            requests[0]
                .instructions
                .contains("RPC standing instruction")
        );
    }
    let replayed = bounded(async {
        loop {
            let event = stream.next(&NeverCancelled).await.unwrap().unwrap();
            if event.has_turn_ended() {
                break event;
            }
        }
    })
    .await;
    assert!(events.iter().any(|event| event.id == replayed.id));
    bounded(f.service.compact(
        &wire::CompactRequest {
            user_session_id: id.clone(),
            target_context_size: "2k".into(),
            ..Default::default()
        },
        active(),
    ))
    .await
    .unwrap();
    assert!(
        f.user()
            .lease
            .events
            .replay()
            .await
            .unwrap()
            .iter()
            .any(|e| e.has_compaction_finished())
    );
    bounded(f.service.set_goal(
        &wire::SetGoalRequest {
            user_session_id: id.clone(),
            state: Some(wire::set_goal_request::State::Goal("RPC goal".into())),
            ..Default::default()
        },
        active(),
    ))
    .await
    .unwrap();
    bounded(f.service.set_goal(
        &wire::SetGoalRequest {
            user_session_id: id.clone(),
            state: Some(wire::set_goal_request::State::Clear(wire::ClearGoal::new())),
            ..Default::default()
        },
        active(),
    ))
    .await
    .unwrap();
    bounded(f.service.interrupt(
        &wire::InterruptRequest {
            user_session_id: id.clone(),
            ..Default::default()
        },
        active(),
    ))
    .await
    .unwrap();
    let cancelled = Signal::new();
    cancelled.cancel();
    let cancelled_read = bounded(async {
        loop {
            match stream.next(&cancelled).await {
                Ok(Some(_)) => continue, // The broker drains already-buffered events first.
                result => break result,
            }
        }
    })
    .await;
    code(cancelled_read, RpcCode::Cancelled);
    f.close().await;
    bounded(async { while stream.next(&NeverCancelled).await.unwrap().is_some() {} }).await;
}

#[tokio::test]
async fn question_and_permission_rpcs_resolve_the_actual_brokers_and_same_durable_receipt() {
    let f = RpcFixture::new().await;
    let session = f.create().await;
    let user = f.user();
    for rejected in [false, true] {
        let questions = user.questions.clone();
        let ask = tokio::spawn(async move {
            questions
                .ask(
                    vec![QuestionDefinition {
                        header: "actual question".into(),
                        prompt: "Choose a response".into(),
                        options: vec![],
                        custom: true,
                        multiple: false,
                    }],
                    &NeverCancelled,
                )
                .await
        });
        until(|| !user.questions.pending().is_empty()).await;
        let pending = f
            .service
            .list_pending_questions(&wire::ListPendingQuestionsRequest {
                user_session_id: session.id.clone(),
                ..Default::default()
            })
            .unwrap();
        assert_eq!(pending.questions.len(), 1);
        let id = pending.questions[0].id.clone();
        if rejected {
            f.service
                .reject_question(&wire::RejectQuestionRequest {
                    user_session_id: session.id.clone(),
                    question_request_id: id,
                    ..Default::default()
                })
                .unwrap();
            assert!(
                matches!(bounded(ask).await.unwrap(), Err(OperationError::Failed(error)) if error.kind == InteractionErrorKind::QuestionRejected)
            );
        } else {
            f.service
                .reply_question(&wire::ReplyQuestionRequest {
                    user_session_id: session.id.clone(),
                    question_request_id: id,
                    answers: vec![wire::QuestionAnswer {
                        text: "actual answer".into(),
                        ..Default::default()
                    }],
                    ..Default::default()
                })
                .unwrap();
            let reply = bounded(ask).await.unwrap().unwrap();
            assert_eq!(reply.kind, QuestionReplyKind::Answered);
            assert_eq!(reply.answers, ["actual answer"]);
        }
    }
    let permission_user = user.clone();
    let permission = tokio::spawn(async move {
        permission_user
            .permissions
            .request(
                permission_request(&permission_user, "actual RPC permission"),
                &NeverCancelled,
            )
            .await
    });
    let pending = bounded(async {
        loop {
            let pending = f
                .service
                .list_pending_permissions(&wire::ListPendingPermissionsRequest {
                    user_session_id: session.id.clone(),
                    ..Default::default()
                })
                .await
                .unwrap();
            if !pending.permissions.is_empty() {
                break pending;
            }
            tokio::task::yield_now().await;
        }
    })
    .await;
    assert_eq!(pending.permissions.len(), 1);
    let request_id = pending.permissions[0].id.clone();
    let stored = user
        .lease
        .events
        .replay()
        .await
        .unwrap()
        .into_iter()
        .find(|e| e.has_permission_pending() && e.permission_pending().id == request_id)
        .unwrap();
    assert_eq!(stored.permission_pending(), &pending.permissions[0]);
    f.service
        .reply_permission(&wire::ReplyPermissionRequest {
            user_session_id: session.id.clone(),
            permission_request_id: request_id.clone(),
            choice_value: "reject".into(),
            reason: String::new(),
            ..Default::default()
        })
        .await
        .unwrap();
    assert_eq!(
        bounded(permission).await.unwrap().unwrap().decision,
        PermissionDecision::Reject
    );
    assert!(user.permissions.pending().await.is_empty());
    code(
        f.service
            .reply_permission(&wire::ReplyPermissionRequest {
                user_session_id: session.id.clone(),
                permission_request_id: request_id,
                choice_value: "reject".into(),
                ..Default::default()
            })
            .await,
        RpcCode::NotFound,
    );
    f.close().await;
}

#[tokio::test]
async fn pre_cancelled_queue_observer_preserves_exactly_once_owned_input_and_close_join() {
    let f = RpcFixture::new().await;
    let session = f.create().await;
    let user = f.user();
    assert_eq!(user.id, session.id);
    let mut stream = f
        .service
        .listen(
            &wire::ListenRequest {
                user_session_id: session.id.clone(),
                replay: true,
                ..Default::default()
            },
            active(),
        )
        .unwrap();
    let cancelled = Arc::new(Signal::new());
    cancelled.cancel();
    let request = prompt(&session.id, "cancelled-rpc");
    // Pin 5f27a8a: UserSession.cs329-366 starts RunQueued before the caller
    // waits; CompleteMode (UserSession.cs460-471) uses the session lifetime.
    // A completed admission may win the observer's cancellation race. Neither
    // legal observer result revokes the already-owned queued work.
    let observed = bounded(f.service.send_message(&request, cancelled)).await;
    match &observed {
        Ok(admission) => assert!(admission.created),
        Err(error) => assert_eq!(error.code, RpcCode::Cancelled, "{error}"),
    }
    assert!(!user.lifetime.is_cancelled());
    bounded(async {
        loop {
            let event = stream.next(&NeverCancelled).await.unwrap().unwrap();
            if event.has_mode_turn_completed() {
                break;
            }
        }
    })
    .await;
    let replay = user.lease.events.replay().await.unwrap();
    let admissions = replay
        .iter()
        .filter(|event| event.has_input_admitted())
        .collect::<Vec<_>>();
    assert_eq!(admissions.len(), 1, "{replay:#?}");
    let event = admissions[0];
    let admitted = event.input_admitted();
    assert_eq!(
        event.agent_session_id,
        user.root_identity.session.session_id
    );
    assert_eq!(admitted.message_id, request.message_id);
    assert_eq!(admitted.content, request.text);
    assert_eq!(admitted.parts.len(), 1);
    assert_eq!(admitted.parts[0].text(), request.text);
    assert_eq!(
        admitted.source.enum_value().unwrap(),
        wire::InputSource::INPUT_SOURCE_UNSPECIFIED
    );
    if let Ok(observer_admission) = observed {
        assert_eq!(observer_admission.input_id, admitted.input_id);
    }
    assert!(replay.iter().any(|event| event.has_turn_ended()));
    assert!(!replay.iter().any(|event| event.has_agent_failed()));
    assert_eq!(f.requests.lock().unwrap().len(), 1);
    assert!(!user.lifetime.is_cancelled());
    let runtime = f.root().get_service::<RuntimeSession<Security>>().unwrap();
    assert!(runtime.is_idle());
    f.close().await;
    assert!(user.lifetime.is_cancelled());
    assert_database_closed(&user);
    bounded(async { while stream.next(&NeverCancelled).await.unwrap().is_some() {} }).await;
}

#[tokio::test]
async fn actual_rpc_boundaries_cancel_or_reject_before_unintended_mutation() {
    let f = RpcFixture::new().await;
    let cancelled = Arc::new(Signal::new());
    cancelled.cancel();
    code(
        f.service
            .create_session(
                &wire::CreateSessionRequest {
                    model: "local/model".into(),
                    mode: "build".into(),
                    ..Default::default()
                },
                cancelled.clone(),
            )
            .await,
        RpcCode::Cancelled,
    );
    let session = f.create().await;
    code(
        f.service
            .create_session(
                &wire::CreateSessionRequest {
                    model: "local/model".into(),
                    mode: "missing".into(),
                    ..Default::default()
                },
                active(),
            )
            .await,
        RpcCode::InvalidArgument,
    );
    code(
        f.service
            .update_session(&wire::UpdateSessionRequest {
                user_session_id: session.id.clone(),
                mode: "missing".into(),
                ..Default::default()
            })
            .await,
        RpcCode::InvalidArgument,
    );
    code(
        f.service
            .resume_session(
                &wire::ResumeSessionRequest {
                    user_session_id: f.bootstrap.id.clone(),
                    working_directory: Path::new(&f.workspace)
                        .parent()
                        .unwrap()
                        .to_string_lossy()
                        .into_owned(),
                    ..Default::default()
                },
                active(),
            )
            .await,
        RpcCode::FailedPrecondition,
    );
    code(
        f.service
            .configure_skill(
                &wire::ConfigureSkillRequest {
                    user_session_id: session.id.clone(),
                    path: "relative/SKILL.md".into(),
                    ..Default::default()
                },
                &NeverCancelled,
            )
            .await,
        RpcCode::InvalidArgument,
    );
    let mut no_delivery = prompt(&session.id, "bad-delivery");
    no_delivery.delivery = protobuf::EnumOrUnknown::from_i32(900);
    code(
        f.service.send_message(&no_delivery, active()).await,
        RpcCode::InvalidArgument,
    );
    let mut mixed = prompt(&session.id, "mixed");
    mixed.parts.push(wire::MessageContentPart {
        content: Some(wire::message_content_part::Content::Text("part".into())),
        ..Default::default()
    });
    code(
        f.service.send_message(&mixed, active()).await,
        RpcCode::InvalidArgument,
    );
    let mut invalid_upload = Upload(VecDeque::from([wire::AttachmentUploadFrame::new()]));
    code(
        f.service
            .upload_attachment(&mut invalid_upload, &NeverCancelled)
            .await,
        RpcCode::InvalidArgument,
    );
    code(
        f.service
            .session_status(
                &wire::SessionStatusRequest {
                    user_session_id: session.id.clone(),
                    ..Default::default()
                },
                cancelled.as_ref(),
            )
            .await,
        RpcCode::Cancelled,
    );
    assert!(f.requests.lock().unwrap().is_empty());
    let observed_user = f.user();
    let replay = observed_user.lease.events.replay().await.unwrap();
    assert!(
        !replay.iter().any(|e| e.has_input_admitted()),
        "unexpected actual admission: requested_user_session_id={}, bootstrap_user_session_id={}, observed_root_session_id={}, observed_selected_root={:?}, replay={replay:#?}",
        session.id,
        f.bootstrap.id,
        observed_user.root_identity.session.session_id,
        observed_user.lease.selected_root().path(),
    );
    f.close().await;
}

async fn upload_image(f: &RpcFixture, id: &str) -> wire::ArtifactReference {
    let upload = image_upload();
    let mut frames = Upload(VecDeque::from([
        wire::AttachmentUploadFrame {
            payload: Some(wire::attachment_upload_frame::Payload::Header(
                wire::AttachmentUploadHeader {
                    user_session_id: id.into(),
                    upload_id: upload.upload_id,
                    ..Default::default()
                },
            )),
            ..Default::default()
        },
        wire::AttachmentUploadFrame {
            payload: Some(wire::attachment_upload_frame::Payload::Chunk(
                upload.content,
            )),
            ..Default::default()
        },
        wire::AttachmentUploadFrame {
            payload: Some(wire::attachment_upload_frame::Payload::Description(
                wire::AttachmentUploadDescription {
                    display_name: upload.display_name,
                    media_type: upload.media_type,
                    byte_length: upload.byte_length,
                    ..Default::default()
                },
            )),
            ..Default::default()
        },
    ]));
    let artifact = bounded(f.service.upload_attachment(&mut frames, &NeverCancelled))
        .await
        .unwrap()
        .artifact
        .unwrap();
    assert_eq!(
        (artifact.width, artifact.height, artifact.byte_length),
        (1, 1, 70)
    );
    assert_eq!(artifact.media_type, "image/png");
    assert_eq!(artifact.display_name, "pixel.png");
    artifact
}

#[tokio::test]
async fn actual_uploaded_image_reaches_provider_through_same_session_resources() {
    let f = RpcFixture::new().await;
    let session = f.create().await;
    let id = &session.id;
    let artifact = upload_image(&f, id).await;
    let request = wire::SendMessageRequest {
        user_session_id: id.clone(),
        message_id: "rpc-with-image".into(),
        delivery: wire::Delivery::DELIVERY_QUEUE.into(),
        parts: vec![
            wire::MessageContentPart {
                content: Some(wire::message_content_part::Content::Text(
                    "actual image prompt".into(),
                )),
                ..Default::default()
            },
            wire::MessageContentPart {
                content: Some(wire::message_content_part::Content::ArtifactId(
                    artifact.artifact_id.clone(),
                )),
                ..Default::default()
            },
        ],
        ..Default::default()
    };
    let admitted = bounded(f.service.send_message(&request, active()))
        .await
        .unwrap();
    assert!(admitted.created);
    let root = f.root();
    let runtime = root.get_service::<RuntimeSession<Security>>().unwrap();
    let drained = tokio::time::timeout(Duration::from_secs(10), async {
        while !runtime.is_idle() || f.requests.lock().unwrap().is_empty() {
            tokio::task::yield_now().await;
        }
    })
    .await;
    let events = f.user().lease.events.replay().await.unwrap();
    assert!(
        drained.is_ok(),
        "state={:?}; provider calls={}; durable events={events:?}",
        runtime.state(),
        f.requests.lock().unwrap().len()
    );
    assert!(
        events
            .iter()
            .any(|e| e.has_input_admitted() && e.input_admitted().message_id == "rpc-with-image")
    );
    assert!(events.iter().any(|e| e.has_turn_ended()));
    assert!(!events.iter().any(|e| e.has_turn_failed()));
    assert_provider_image_bytes(&f, 0);
    let original_user = f.user();
    let foreign = f.create().await;
    let mut foreign_request = request;
    foreign_request.user_session_id = foreign.id;
    foreign_request.message_id = "foreign-image".into();
    code(
        f.service.send_message(&foreign_request, active()).await,
        RpcCode::InvalidArgument,
    );
    f.close().await;
    assert_database_closed(&original_user);
    let reopened = f.new_service();
    let resumed = bounded(reopened.resume_session(
        &wire::ResumeSessionRequest {
            user_session_id: session.id.clone(),
            working_directory: f.workspace.clone(),
            interactive_permissions: true,
            ..Default::default()
        },
        active(),
    ))
    .await
    .unwrap();
    assert!(resumed.loaded);
    assert_eq!(resumed.id, session.id);
    let resumed_root = f.root();
    let resumed_runtime = resumed_root
        .get_service::<RuntimeSession<Security>>()
        .unwrap();
    bounded(reopened.send_message(&prompt(&session.id, "after-image-resume"), active()))
        .await
        .unwrap();
    until(|| resumed_runtime.is_idle() && f.requests.lock().unwrap().len() == 2).await;
    assert_provider_image_bytes(&f, 1);
    bounded(reopened.close()).await.unwrap();
    assert_database_closed(&f.user());
}

fn assert_provider_image_bytes(fixture: &RpcFixture, request_index: usize) {
    let requests = fixture.requests.lock().unwrap();
    let image = requests[request_index]
        .messages
        .iter()
        .flat_map(|message| message.contents())
        .find(|content| matches!(content, Content::Image { .. }))
        .expect("actual provider request must contain the admitted image");
    let Content::Image {
        path,
        media_type,
        width,
        height,
        bytes,
        ..
    } = image
    else {
        unreachable!()
    };
    assert_eq!((media_type.as_str(), *width, *height), ("image/png", 1, 1));
    assert!(Path::new(path).starts_with(fixture.user().lease.selected_root().path()));
    let content = bytes
        .clone()
        .unwrap_or_else(|| std::fs::read(path).unwrap());
    assert_eq!(content, image_upload().content);
}

#[tokio::test]
async fn actual_listener_inventory_follows_durable_replay_and_preserves_monitor_input_source() {
    let f = RpcFixture::new().await;
    let session = f.create().await;
    let mut fresh = f
        .service
        .listen(
            &wire::ListenRequest {
                user_session_id: session.id.clone(),
                ..Default::default()
            },
            active(),
        )
        .unwrap();
    let first = bounded(fresh.next(&NeverCancelled)).await.unwrap().unwrap();
    assert!(first.has_tool_snapshot());
    assert_eq!(first.tool_snapshot().kind, "queue");
    assert_eq!(
        first.tool_snapshot().owner_agent_session_id,
        f.user().root_identity.session.session_id
    );
    drop(fresh);
    let root = f.root();
    let runtime = root.get_service::<RuntimeSession<Security>>().unwrap();
    let sent = bounded(runtime.send(
        vec![ConversationPart::text("actual monitor-origin input")],
        "rpc-monitor-source".into(),
        wire::Delivery::DELIVERY_STEER,
        IncomingActivity {
            name: "synthetic monitor".into(),
            cause: Some("retained source witness".into()),
            source: wire::InputSource::INPUT_SOURCE_MONITOR,
        },
        active(),
    ))
    .await
    .unwrap();
    bounded(sent.execution.join()).await.unwrap();
    let stored = f.user().lease.events.replay().await.unwrap();
    let admission = stored
        .iter()
        .find(|event| {
            event.has_input_admitted() && event.input_admitted().message_id == "rpc-monitor-source"
        })
        .unwrap();
    assert_eq!(
        admission.input_admitted().source.enum_value().unwrap(),
        wire::InputSource::INPUT_SOURCE_MONITOR
    );
    let mut replay = f
        .service
        .listen(
            &wire::ListenRequest {
                user_session_id: session.id.clone(),
                replay: true,
                ..Default::default()
            },
            active(),
        )
        .unwrap();
    for expected in &stored {
        let actual = bounded(replay.next(&NeverCancelled))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            &actual, expected,
            "persisted events precede current inventory without projection loss"
        );
    }
    let inventory = bounded(replay.next(&NeverCancelled))
        .await
        .unwrap()
        .unwrap();
    assert!(inventory.has_tool_snapshot());
    assert_eq!(inventory.tool_snapshot().kind, "queue");
    f.close().await;
    bounded(async { while replay.next(&NeverCancelled).await.unwrap().is_some() {} }).await;
}
