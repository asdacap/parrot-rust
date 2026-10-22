//! T62 public ParrotService contract tests.
//! Pinned source: 5f27a8a92625ee31d8bec094e5c3e09550193781,
//! test/Parrot.Core.Tests/ParrotServiceTests.cs (source names on each test).
//! The local FakeHostedSession proves routing only, not production agent/session
//! composition, provider execution, skill discovery or permission authorization.
#[path = "support.rs"]
pub(super) mod support;
use parrot_domain::{
    contracts::async_ops::{BoxFuture, Cancellation},
    protocol::messages::parrot as wire,
    values::{context::ContextSize, identifiers::UserSessionId},
};
use parrot_host::composition::protocol::{stream::bounded_client_stream, *};
use parrot_runtime::{
    interactions::*,
    prompt_context::{SkillLoadError, SkillMetadata, SkillScope},
};
use parrot_storage::{events::ConversationPart, history_files::SessionCatalog};
use protobuf::Message;
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use support::*;
use tokio::sync::{Semaphore, oneshot};

// Source: A_prompt_is_answered_with_the_admission_it_made.
#[tokio::test]
async fn prompt_returns_actual_durable_admission_and_idempotent_retry() {
    let f = Fixture::new();
    let first = f.create().await;
    let other = f.create().await;
    let request = prompt(&first.id(), "msg-one");
    let reply = bounded(f.service.send_message(&request, active()))
        .await
        .unwrap();
    assert_eq!(reply.message_id, "msg-one");
    assert!(!reply.input_id.is_empty());
    assert!(reply.created);
    let retry = bounded(f.service.send_message(&request, active()))
        .await
        .unwrap();
    assert_eq!(reply.input_id, retry.input_id);
    assert!(!retry.created);
    assert!(other.calls().is_empty());
    let persisted = first.resources.events.replay().await.unwrap();
    assert_eq!(persisted.len(), 1);
    assert_eq!(persisted[0].input_admitted().message_id, "msg-one");
    let generated = bounded(f.service.send_message(&prompt(&first.id(), ""), active()))
        .await
        .unwrap();
    assert!(!generated.message_id.is_empty());
    assert_ne!(generated.input_id, reply.input_id);
    f.close().await;
}

// Source: In_process_goal_calls_set_and_clear_the_goal;
// In_process_standing_instruction_calls_set_and_clear_the_instruction;
// In_process_compact_routes_to_the_selected_live_user_session_and_awaits_it.
#[tokio::test]
async fn selected_session_receives_goal_instruction_interrupt_and_awaited_compaction() {
    let f = Fixture::new();
    let selected = f.create().await;
    let untouched = f.create().await;
    let id = selected.id();
    for state in [
        wire::set_goal_request::State::Goal("finish the task".into()),
        wire::set_goal_request::State::Clear(wire::ClearGoal::new()),
    ] {
        bounded(f.service.set_goal(
            &wire::SetGoalRequest {
                user_session_id: id.clone(),
                state: Some(state),
                ..Default::default()
            },
            active(),
        ))
        .await
        .unwrap();
    }
    for instruction in ["be concise", ""] {
        f.service
            .set_standing_instruction(&wire::SetStandingInstructionRequest {
                user_session_id: id.clone(),
                instruction: instruction.into(),
                ..Default::default()
            })
            .unwrap();
    }
    bounded(f.service.interrupt(
        &wire::InterruptRequest {
            user_session_id: id.clone(),
            ..Default::default()
        },
        active(),
    ))
    .await
    .unwrap();
    let gate = Arc::new(Semaphore::new(0));
    *selected.compact_gate.lock().unwrap() = Some(gate.clone());
    let service = f.service.clone();
    let compact_id = id.clone();
    let compact = tokio::spawn(async move {
        service
            .compact(
                &wire::CompactRequest {
                    user_session_id: compact_id,
                    target_context_size: "2k".into(),
                    ..Default::default()
                },
                active(),
            )
            .await
    });
    until(|| {
        selected
            .calls()
            .iter()
            .any(|c| matches!(c, Call::Compact(_)))
    })
    .await;
    assert!(
        !compact.is_finished(),
        "service must await selected session compaction"
    );
    gate.add_permits(1);
    bounded(compact).await.unwrap().unwrap();
    assert_eq!(
        selected.calls(),
        vec![
            Call::Goal(Some("finish the task".into())),
            Call::Goal(None),
            Call::Instruction("be concise".into()),
            Call::Instruction("".into()),
            Call::Interrupt,
            Call::Compact(Some(ContextSize::parse("2k").unwrap()))
        ]
    );
    assert!(untouched.calls().is_empty());
    f.close().await;
}

// Source: A_prompt_with_no_delivery_is_refused;
// Send_message_refuses_legacy_text_together_with_structured_parts;
// A_session_nobody_opened_can_be_neither_prompted_nor_interrupted.
#[tokio::test]
async fn bad_input_and_unhosted_sessions_are_refused_without_dispatch() {
    let f = Fixture::new();
    let selected = f.create().await;
    let id = selected.id();
    for delivery in [0, 314] {
        let mut request = prompt(&id, "bad");
        request.delivery = protobuf::EnumOrUnknown::from_i32(delivery);
        code(
            f.service.send_message(&request, active()).await,
            RpcCode::InvalidArgument,
        );
    }
    let mut request = prompt(&id, "bad");
    request.parts.push(wire::MessageContentPart {
        content: Some(wire::message_content_part::Content::Text("part".into())),
        ..Default::default()
    });
    code(
        f.service.send_message(&request, active()).await,
        RpcCode::InvalidArgument,
    );
    request.text.clear();
    request.parts = vec![wire::MessageContentPart::new()];
    code(
        f.service.send_message(&request, active()).await,
        RpcCode::InvalidArgument,
    );
    code(
        f.service
            .send_message(&prompt("missing", "bad"), active())
            .await,
        RpcCode::NotFound,
    );
    code(
        f.service
            .interrupt(
                &wire::InterruptRequest {
                    user_session_id: "missing".into(),
                    ..Default::default()
                },
                active(),
            )
            .await,
        RpcCode::NotFound,
    );
    code(
        f.service
            .compact(
                &wire::CompactRequest {
                    user_session_id: id.clone(),
                    target_context_size: "not-size".into(),
                    ..Default::default()
                },
                active(),
            )
            .await,
        RpcCode::InvalidArgument,
    );
    for state in [
        None,
        Some(wire::set_goal_request::State::Goal(String::new())),
    ] {
        code(
            f.service
                .set_goal(
                    &wire::SetGoalRequest {
                        user_session_id: id.clone(),
                        state,
                        ..Default::default()
                    },
                    active(),
                )
                .await,
            RpcCode::InvalidArgument,
        );
    }
    assert!(selected.calls().is_empty());
    assert!(selected.resources.events.replay().await.unwrap().is_empty());
    f.close().await;
}

// Source: Modes_are_listed_created_updated_and_validated;
// List_modes_exposes_only_user_selectable_profiles (boundary routing, fake catalog).
#[tokio::test]
async fn mode_and_model_updates_resolve_before_dispatch_and_keep_other_session_unchanged() {
    let f = Fixture::new();
    let session = f.create().await;
    let other = f.create().await;
    assert_eq!(
        f.service
            .list_modes(&wire::ListModesRequest::new())
            .modes
            .iter()
            .map(|m| m.id.as_str())
            .collect::<Vec<_>>(),
        ["default", "plan"]
    );
    let updated = f
        .service
        .update_session(&wire::UpdateSessionRequest {
            user_session_id: session.id(),
            model: "scripted/new".into(),
            mode: "plan".into(),
            ..Default::default()
        })
        .await
        .unwrap();
    assert_eq!(updated.model, "scripted/new");
    assert_eq!(updated.mode, "plan");
    assert!(!updated.loaded);
    for (model, mode) in [("", "hidden"), ("not-a-provider/model", "")] {
        code(
            f.service
                .update_session(&wire::UpdateSessionRequest {
                    user_session_id: session.id(),
                    model: model.into(),
                    mode: mode.into(),
                    ..Default::default()
                })
                .await,
            RpcCode::InvalidArgument,
        );
    }
    assert_eq!(
        session.calls(),
        vec![Call::Update(
            Some("scripted/new".into()),
            Some("plan".into())
        )]
    );
    assert_eq!(other.snapshot().model, "scripted/model");
    code(
        f.service
            .create_session(
                &wire::CreateSessionRequest {
                    model: "scripted/model".into(),
                    mode: "hidden".into(),
                    ..Default::default()
                },
                active(),
            )
            .await,
        RpcCode::InvalidArgument,
    );
    assert_eq!(f.factory.created.lock().unwrap().len(), 2);
    f.close().await;
}

// Source: Skill_inventory_and_exact_path_configuration_use_the_hosted_session;
// Skill_management_rejects_unknown_sessions_and_paths. Catalog here is test-local.
#[tokio::test]
async fn skill_metadata_and_exact_configuration_route_without_exposing_body() {
    let f = Fixture::new();
    let selected = f.create().await;
    let other = f.create().await;
    let path = f.root.join("demo/SKILL.md").to_string_lossy().into_owned();
    selected.skills.lock().unwrap().skills.push(SkillMetadata {
        name: "demo".into(),
        description: "metadata only".into(),
        path: path.clone(),
        scope: SkillScope::Repo,
        display_name: Some("Demo".into()),
        short_description: Some("Short".into()),
        enabled: true,
        prompt_visible: true,
        discovery_path: "secret body must not be transmitted".into(),
    });
    selected.skills.lock().unwrap().errors.push(SkillLoadError {
        path: "x".repeat(5000),
        message: "y".repeat(5000),
    });
    let request = wire::ListSkillsRequest {
        user_session_id: selected.id(),
        ..Default::default()
    };
    let listed = f
        .service
        .list_skills(&request, active().as_ref())
        .await
        .unwrap();
    assert_eq!(listed.skills[0].path, path);
    assert_eq!(
        listed.skills[0].scope.enum_value().unwrap(),
        wire::ListedSkillScope::SKILL_SCOPE_REPO
    );
    assert_eq!(listed.errors[0].path.len(), 4096);
    assert_eq!(listed.errors[0].message.len(), 4096);
    assert!(!format!("{listed:?}").contains("secret body"));
    let reply = f
        .service
        .configure_skill(
            &wire::ConfigureSkillRequest {
                user_session_id: selected.id(),
                path: path.clone(),
                enabled: false,
                ..Default::default()
            },
            active().as_ref(),
        )
        .await
        .unwrap();
    assert!(!reply.skill.as_ref().unwrap().enabled);
    assert!(
        !f.service
            .list_skills(&request, active().as_ref())
            .await
            .unwrap()
            .skills[0]
            .enabled
    );
    for invalid in ["", "relative/SKILL.md"] {
        code(
            f.service
                .configure_skill(
                    &wire::ConfigureSkillRequest {
                        user_session_id: selected.id(),
                        path: invalid.into(),
                        ..Default::default()
                    },
                    active().as_ref(),
                )
                .await,
            RpcCode::InvalidArgument,
        );
    }
    code(
        f.service
            .configure_skill(
                &wire::ConfigureSkillRequest {
                    user_session_id: selected.id(),
                    path: format!("{path}-missing"),
                    ..Default::default()
                },
                active().as_ref(),
            )
            .await,
        RpcCode::NotFound,
    );
    code(
        f.service
            .list_skills(
                &wire::ListSkillsRequest {
                    user_session_id: "missing".into(),
                    ..Default::default()
                },
                active().as_ref(),
            )
            .await,
        RpcCode::NotFound,
    );
    assert!(other.calls().is_empty());
    f.close().await;
}

// Source: Session_status_reports_the_root_agent_and_provider_usage;
// Session_status_rejects_unhosted_sessions. Usage provider adapter is synthetic;
// separate ModelOperations tests own detailed usage formatting.
#[tokio::test]
async fn status_routes_to_owner_and_preserves_failure_and_cancellation() {
    let f = Fixture::new();
    let session = f.create().await;
    let other = f.create().await;
    let request = wire::SessionStatusRequest {
        user_session_id: session.id(),
        ..Default::default()
    };
    let response = f
        .service
        .session_status(&request, active().as_ref())
        .await
        .unwrap();
    assert_eq!(response.status, format!("status:{}", session.id()));
    assert_eq!(response.usage_lines, ["Provider does not report usage"]);
    code(
        f.service
            .session_status(
                &wire::SessionStatusRequest {
                    user_session_id: "missing".into(),
                    ..Default::default()
                },
                active().as_ref(),
            )
            .await,
        RpcCode::NotFound,
    );
    let failure = RpcError::new(RpcCode::PermissionDenied, "owner refused status");
    *session.failure.lock().unwrap() = Some(failure.clone());
    assert_eq!(
        f.service
            .session_status(&request, active().as_ref())
            .await
            .unwrap_err(),
        failure
    );
    code(
        f.service
            .session_status(&request, Cancel::new(true).as_ref())
            .await,
        RpcCode::Cancelled,
    );
    assert!(other.calls().is_empty());
    f.close().await;
}

// Source: Existing_session_operations_reject_wrong_identity_workspace_and_unhosted_sessions;
// Resume_and_concurrent_attach_preserve_the_exact_session_without_new_activation.
#[tokio::test]
async fn concurrent_attach_keeps_exact_hosted_arc_and_activation_and_rejects_wrong_workspace() {
    let f = Fixture::new();
    let session = f.create().await;
    let activation = f
        .claim
        .is_active(
            &UserSessionId::try_parse(&session.id()).unwrap(),
            &f.workspace,
        )
        .unwrap();
    assert!(activation);
    let directory = f.workspace.to_string_lossy().into_owned();
    let request = wire::AttachSessionRequest {
        user_session_id: session.id(),
        working_directory: directory,
        ..Default::default()
    };
    let (first, second) = tokio::join!(
        async { f.service.attach_session(&request, active().as_ref()) },
        async { f.service.attach_session(&request, active().as_ref()) }
    );
    assert_eq!(first.unwrap(), second.unwrap());
    let hosted: Arc<dyn HostedSession> = session.clone();
    assert!(Arc::ptr_eq(
        &f.service.sessions().find(&session.id()).unwrap(),
        &hosted
    ));
    assert_eq!(f.factory.created.lock().unwrap().len(), 1);
    assert_eq!(session.calls(), vec![Call::Opened, Call::Opened]);
    let wrong = f.root.join("other-workspace");
    std::fs::create_dir(&wrong).unwrap();
    code(
        f.service.attach_session(
            &wire::AttachSessionRequest {
                working_directory: wrong.to_string_lossy().into_owned(),
                ..request.clone()
            },
            active().as_ref(),
        ),
        RpcCode::FailedPrecondition,
    );
    code(
        f.service.attach_session(
            &wire::AttachSessionRequest {
                user_session_id: "../bad".into(),
                ..request.clone()
            },
            active().as_ref(),
        ),
        RpcCode::InvalidArgument,
    );
    code(
        f.service.attach_session(
            &wire::AttachSessionRequest {
                user_session_id: "missing".into(),
                ..request.clone()
            },
            active().as_ref(),
        ),
        RpcCode::NotFound,
    );
    code(
        f.service
            .attach_session(&request, Cancel::new(true).as_ref()),
        RpcCode::Cancelled,
    );
    code(
        f.service
            .resume_session(
                &wire::ResumeSessionRequest {
                    user_session_id: "missing".into(),
                    working_directory: f.workspace.to_string_lossy().into_owned(),
                    ..Default::default()
                },
                active(),
            )
            .await,
        RpcCode::NotFound,
    );
    code(
        f.service
            .resume_session(
                &wire::ResumeSessionRequest {
                    user_session_id: session.id(),
                    working_directory: wrong.to_string_lossy().into_owned(),
                    ..Default::default()
                },
                active(),
            )
            .await,
        RpcCode::FailedPrecondition,
    );
    f.close().await;
}

// Source: Sessions_are_listed_from_the_server_catalog_with_hosted_state;
// Session_listing_rejects_invalid_nonempty_workspace_filters;
// Session_listing_matches_physical_launch_directory_not_repository_or_subdirectories.
#[tokio::test]
async fn listing_uses_real_catalog_physical_workspace_filter_and_hosted_state() {
    let f = Fixture::new();
    let first = f.create().await;
    let second = f.create().await;
    let listed = f
        .service
        .list_sessions(&wire::ListSessionsRequest::new())
        .unwrap();
    assert_eq!(listed.sessions.len(), 2);
    assert!(
        listed
            .sessions
            .iter()
            .all(|s| s.state.enum_value().unwrap() == wire::SessionState::SESSION_STATE_ACTIVE)
    );
    let alias = f.root.join("alias");
    std::os::unix::fs::symlink(&f.workspace, &alias).unwrap();
    assert_eq!(
        f.service
            .list_sessions(&wire::ListSessionsRequest {
                working_directory: alias.to_string_lossy().into_owned(),
                ..Default::default()
            })
            .unwrap(),
        listed
    );
    let child = f.workspace.join("child");
    std::fs::create_dir(&child).unwrap();
    assert!(
        f.service
            .list_sessions(&wire::ListSessionsRequest {
                working_directory: child.to_string_lossy().into_owned(),
                ..Default::default()
            })
            .unwrap()
            .sessions
            .is_empty()
    );
    code(
        f.service.list_sessions(&wire::ListSessionsRequest {
            working_directory: f.root.join("absent").to_string_lossy().into_owned(),
            ..Default::default()
        }),
        RpcCode::InvalidArgument,
    );
    assert_eq!(SessionCatalog::new(&f.state).list().len(), 2);
    f.close().await;
    assert!(first.closed.load(Ordering::SeqCst));
    assert!(second.closed.load(Ordering::SeqCst));
    assert!(
        f.service
            .list_sessions(&wire::ListSessionsRequest::new())
            .unwrap()
            .sessions
            .iter()
            .all(|s| s.state.enum_value().unwrap() == wire::SessionState::SESSION_STATE_INACTIVE)
    );
}

// Source: Sandbox_enable_flips_the_gate_and_reports_the_state;
// Set_context_limit_resolves_session_before_persisting_and_returns_persistence_response.
#[tokio::test]
async fn sandbox_and_context_limit_validate_owner_before_mutating_shared_configuration() {
    let f = Fixture::new();
    let session = f.create().await;
    let before = std::fs::read(f.root.join("config.yaml")).unwrap();
    code(
        f.service.set_context_limit(
            &wire::SetContextLimitRequest {
                user_session_id: "missing".into(),
                context_limit: "2k".into(),
                ..Default::default()
            },
            active().as_ref(),
        ),
        RpcCode::NotFound,
    );
    assert_eq!(std::fs::read(f.root.join("config.yaml")).unwrap(), before);
    code(
        f.service.sandbox_enable(
            &wire::SandboxEnableRequest {
                user_session_id: "missing".into(),
                enabled: false,
                ..Default::default()
            },
            active().as_ref(),
        ),
        RpcCode::NotFound,
    );
    assert!(f.sandbox.enabled());
    let response = f
        .service
        .set_context_limit(
            &wire::SetContextLimitRequest {
                user_session_id: session.id(),
                context_limit: "2k".into(),
                ..Default::default()
            },
            active().as_ref(),
        )
        .unwrap();
    assert_eq!(response.context_limit, "2000");
    assert_ne!(std::fs::read(f.root.join("config.yaml")).unwrap(), before);
    assert_eq!(
        f.coordinator
            .capture_configuration()
            .models
            .context_limit
            .unwrap()
            .to_string(),
        "2000"
    );
    for enabled in [false, true] {
        let response = f
            .service
            .sandbox_enable(
                &wire::SandboxEnableRequest {
                    user_session_id: session.id(),
                    enabled,
                    ..Default::default()
                },
                active().as_ref(),
            )
            .unwrap();
        assert_eq!(response.enabled, enabled);
        assert_eq!(f.sandbox.enabled(), enabled);
    }
    let diagnostics = std::fs::read_to_string(f.diagnostics.path()).unwrap();
    assert!(diagnostics.contains("disabled"));
    f.close().await;
}

const PIXEL: &[u8] =
    include_bytes!("../../../../parrot-storage/tests/artifacts/fixtures/pixel.png");
fn image_upload(id: &str, upload: &str) -> Frames {
    frames(vec![
        header(id, upload),
        chunk(PIXEL[..12].to_vec()),
        chunk(PIXEL[12..].to_vec()),
        description("pixel.png", "image/png", PIXEL.len() as i64),
    ])
}
fn text_part(text: &str) -> wire::MessageContentPart {
    wire::MessageContentPart {
        content: Some(wire::message_content_part::Content::Text(text.into())),
        ..Default::default()
    }
}
fn image_part(id: &str) -> wire::MessageContentPart {
    wire::MessageContentPart {
        content: Some(wire::message_content_part::Content::ArtifactId(id.into())),
        ..Default::default()
    }
}

// Source: Images_are_uploaded_with_canonical_metadata_and_admitted_as_structured_parts.
#[tokio::test]
async fn real_image_upload_persists_canonical_metadata_is_idempotent_and_preserves_prompt_order() {
    let f = Fixture::new();
    let owner = f.create().await;
    let other = f.create().await;
    let response = bounded(f.service.upload_attachment(
        &mut image_upload(&owner.id(), "upload-one"),
        active().as_ref(),
    ))
    .await
    .unwrap();
    let artifact = response.artifact.as_ref().unwrap();
    assert_eq!(
        (
            artifact.media_type.as_str(),
            artifact.display_name.as_str(),
            artifact.origin.as_str()
        ),
        ("image/png", "pixel.png", "upload")
    );
    assert_eq!(artifact.byte_length, PIXEL.len() as i64);
    assert_eq!(
        (
            artifact.width,
            artifact.height,
            artifact.frame_count,
            artifact.aggregate_pixels
        ),
        (1, 1, 1, 1)
    );
    assert_eq!(
        artifact.sha256,
        "4ff6ab670a58c14270e034e2090d9a432caa263a14e0a25785386b0c12f880b5"
    );
    assert_eq!(artifact.artifact_id, artifact.sha256);
    assert_eq!(
        std::fs::read(
            owner
                .resources
                .images
                .store()
                .path_of(&artifact.artifact_id)
                .unwrap()
        )
        .unwrap(),
        PIXEL
    );
    assert_eq!(
        owner
            .resolve_image(&artifact.artifact_id)
            .await
            .unwrap()
            .unwrap()
            .sha256,
        artifact.sha256
    );
    assert!(
        other
            .resolve_image(&artifact.artifact_id)
            .await
            .unwrap()
            .is_none()
    );
    let retry = f
        .service
        .upload_attachment(
            &mut image_upload(&owner.id(), "upload-one"),
            active().as_ref(),
        )
        .await
        .unwrap();
    assert_eq!(response, retry);
    let request = wire::SendMessageRequest {
        user_session_id: owner.id(),
        message_id: "structured".into(),
        delivery: wire::Delivery::DELIVERY_QUEUE.into(),
        parts: vec![
            text_part("before"),
            image_part(&artifact.artifact_id),
            text_part("after"),
        ],
        ..Default::default()
    };
    let admission = f.service.send_message(&request, active()).await.unwrap();
    assert!(admission.created);
    assert_eq!(
        owner.calls(),
        vec![Call::Send(
            vec![
                ConversationPart::text("before"),
                ConversationPart::image(&artifact.artifact_id, "image/png", "pixel.png"),
                ConversationPart::text("after")
            ],
            "structured".into(),
            wire::Delivery::DELIVERY_QUEUE
        )]
    );
    // Inspect the actual persisted input using the fake's stable agent name.
    let agent = owner.root_agent.clone();
    let persisted = owner
        .resources
        .events
        .transaction(move |tx| parrot_storage::events::inputs::existing(tx, &agent, "structured"))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        persisted.parts,
        vec![
            ConversationPart::text("before"),
            ConversationPart::image(&artifact.artifact_id, "image/png", "pixel.png"),
            ConversationPart::text("after")
        ]
    );
    let foreign = wire::SendMessageRequest {
        user_session_id: other.id(),
        ..request.clone()
    };
    code(
        f.service.send_message(&foreign, active()).await,
        RpcCode::InvalidArgument,
    );
    let mut missing = request.clone();
    missing.parts = vec![image_part("missing")];
    code(
        f.service.send_message(&missing, active()).await,
        RpcCode::InvalidArgument,
    );
    let mut too_many = request;
    too_many.parts = (0..=parrot_storage::artifacts::ImageLimits::SOURCE.maximum_images)
        .map(|_| image_part(&artifact.artifact_id))
        .collect();
    code(
        f.service.send_message(&too_many, active()).await,
        RpcCode::InvalidArgument,
    );
    f.close().await;
}

// Additional source upload reader/frame boundaries; these exercise service
// validation before the real repository and repository error mapping afterward.
#[tokio::test]
async fn upload_rejects_bad_frame_order_sizes_descriptions_and_media_without_persisting() {
    let f = Fixture::new();
    let owner = f.create().await;
    let id = owner.id();
    let h = header(&id, "bad-upload");
    let d = description("pixel.png", "image/png", PIXEL.len() as i64);
    let cases = vec![
        vec![],
        vec![chunk(PIXEL.to_vec())],
        vec![wire::AttachmentUploadFrame::new()],
        vec![header("", "x")],
        vec![header(&id, " ")],
        vec![h.clone(), h.clone()],
        vec![h.clone(), wire::AttachmentUploadFrame::new()],
        vec![h.clone(), chunk(Vec::new())],
        vec![h.clone(), chunk(vec![0; 1024 * 1024 + 1])],
        vec![h.clone(), d.clone()],
        vec![h.clone(), chunk(PIXEL.to_vec())],
        vec![h.clone(), chunk(PIXEL.to_vec()), d.clone(), chunk(vec![1])],
        vec![h.clone(), chunk(PIXEL.to_vec()), d.clone(), d.clone()],
        vec![
            h.clone(),
            chunk(PIXEL.to_vec()),
            description("../pixel.png", "image/png", PIXEL.len() as i64),
        ],
        vec![
            h.clone(),
            chunk(PIXEL.to_vec()),
            description("pixel\n.png", "image/png", PIXEL.len() as i64),
        ],
        vec![
            h.clone(),
            chunk(PIXEL.to_vec()),
            description("pixel.png", "", PIXEL.len() as i64),
        ],
        vec![
            h.clone(),
            chunk(PIXEL.to_vec()),
            description("pixel.png", "image/png", 0),
        ],
        vec![
            h.clone(),
            chunk(PIXEL.to_vec()),
            description("pixel.png", "image/jpeg", PIXEL.len() as i64),
        ],
        vec![
            h.clone(),
            chunk(PIXEL.to_vec()),
            description("pixel.png", "image/png", PIXEL.len() as i64 + 1),
        ],
        vec![
            h.clone(),
            chunk(vec![1, 2, 3]),
            description("pixel.png", "image/png", 3),
        ],
    ];
    for (index, case) in cases.into_iter().enumerate() {
        let error = f
            .service
            .upload_attachment(&mut frames(case), active().as_ref())
            .await
            .unwrap_err();
        assert_eq!(
            error.code,
            RpcCode::InvalidArgument,
            "upload case {index}: {error}"
        );
    }
    let mut oversized = vec![h];
    oversized.extend((0..6).map(|_| chunk(vec![0; 1024 * 1024])));
    oversized.push(d);
    code(
        f.service
            .upload_attachment(&mut frames(oversized), active().as_ref())
            .await,
        RpcCode::InvalidArgument,
    );
    code(
        f.service
            .upload_attachment(&mut frames(vec![header("missing", "x")]), active().as_ref())
            .await,
        RpcCode::NotFound,
    );
    let failure = RpcError::new(RpcCode::Unavailable, "reader failed");
    let mut failed = Frames(vec![Ok(header(&id, "failed")), Err(failure.clone())].into());
    assert_eq!(
        f.service
            .upload_attachment(&mut failed, active().as_ref())
            .await
            .unwrap_err(),
        failure
    );
    code(
        f.service
            .upload_attachment(
                &mut image_upload(&id, "cancelled"),
                Cancel::new(true).as_ref(),
            )
            .await,
        RpcCode::Cancelled,
    );
    assert!(owner.resources.events.replay().await.unwrap().is_empty());
    assert!(
        std::fs::read_dir(owner.resources.images.store().directory())
            .map(|entries| entries.count() == 0)
            .unwrap_or(true)
    );
    f.close().await;
}

// Source: In_process_question_calls_are_routed;
// In_process_question_calls_preserve_options_and_ordered_text_answers.
#[tokio::test]
async fn question_calls_use_real_broker_preserve_options_order_and_map_errors() {
    let f = Fixture::new();
    let owner = f.create().await;
    let other = f.create().await;
    let asking_owner = owner.clone();
    let asking = tokio::spawn(async move {
        asking_owner
            .questions
            .ask(
                vec![
                    QuestionDefinition {
                        header: "Decision".into(),
                        prompt: "Pick colours".into(),
                        options: vec![
                            QuestionOption {
                                label: "One".into(),
                                description: "First".into(),
                            },
                            QuestionOption {
                                label: "Two".into(),
                                description: "Second".into(),
                            },
                        ],
                        multiple: true,
                        custom: false,
                    },
                    QuestionDefinition {
                        header: "Reason".into(),
                        prompt: "Explain".into(),
                        options: vec![],
                        multiple: false,
                        custom: true,
                    },
                ],
                active().as_ref(),
            )
            .await
    });
    until(|| !owner.questions.pending().is_empty()).await;
    let pending = f
        .service
        .list_pending_questions(&wire::ListPendingQuestionsRequest {
            user_session_id: owner.id(),
            ..Default::default()
        })
        .unwrap()
        .questions
        .remove(0);
    assert_eq!(pending.remaining_timeout_ms, None);
    assert_eq!(pending.questions.len(), 2);
    assert_eq!(
        (
            pending.questions[0].header.as_str(),
            pending.questions[0].prompt.as_str(),
            pending.questions[0].multiple,
            pending.questions[0].custom
        ),
        ("Decision", "Pick colours", true, false)
    );
    assert_eq!(
        pending.questions[0]
            .options
            .iter()
            .map(|o| (o.label.as_str(), o.description.as_str()))
            .collect::<Vec<_>>(),
        [("One", "First"), ("Two", "Second")]
    );
    assert!(
        f.service
            .list_pending_questions(&wire::ListPendingQuestionsRequest {
                user_session_id: other.id(),
                ..Default::default()
            })
            .unwrap()
            .questions
            .is_empty()
    );
    let request = wire::ReplyQuestionRequest {
        user_session_id: owner.id(),
        question_request_id: pending.id.clone(),
        answers: vec![
            wire::QuestionAnswer {
                text: "One, Two".into(),
                ..Default::default()
            },
            wire::QuestionAnswer {
                text: "because".into(),
                ..Default::default()
            },
        ],
        ..Default::default()
    };
    let mut invalid = request.clone();
    invalid.answers.pop();
    code(f.service.reply_question(&invalid), RpcCode::InvalidArgument);
    assert_eq!(owner.questions.pending().len(), 1);
    f.service.reply_question(&request).unwrap();
    assert_eq!(
        bounded(asking).await.unwrap().unwrap().answers,
        ["One, Two", "because"]
    );
    code(f.service.reply_question(&request), RpcCode::NotFound);
    code(
        f.service.reject_question(&wire::RejectQuestionRequest {
            user_session_id: owner.id(),
            question_request_id: pending.id,
            ..Default::default()
        }),
        RpcCode::NotFound,
    );
    code(
        f.service.reply_question(&wire::ReplyQuestionRequest {
            user_session_id: owner.id(),
            ..Default::default()
        }),
        RpcCode::InvalidArgument,
    );
    code(
        f.service.reject_question(&wire::RejectQuestionRequest {
            user_session_id: owner.id(),
            ..Default::default()
        }),
        RpcCode::InvalidArgument,
    );
    let rejecting_owner = owner.clone();
    let rejecting = tokio::spawn(async move {
        rejecting_owner
            .questions
            .ask(
                vec![QuestionDefinition {
                    header: "".into(),
                    prompt: "Reject me".into(),
                    options: vec![],
                    multiple: false,
                    custom: true,
                }],
                active().as_ref(),
            )
            .await
    });
    until(|| !owner.questions.pending().is_empty()).await;
    f.service
        .reject_question(&wire::RejectQuestionRequest {
            user_session_id: owner.id(),
            question_request_id: owner.questions.pending()[0].id.clone(),
            ..Default::default()
        })
        .unwrap();
    assert!(matches!(
        bounded(rejecting).await.unwrap(),
        Err(parrot_domain::contracts::async_ops::OperationError::Failed(
            InteractionError {
                kind: InteractionErrorKind::QuestionRejected,
                ..
            }
        ))
    ));
    f.close().await;
}

// Source: SessionStatus awaits ObserveStatus before the concurrently started usage.
#[tokio::test]
async fn failed_status_does_not_wait_for_a_parked_provider_usage_request() {
    struct ParkedUsage(Semaphore);
    impl parrot_provider::sessions::decorators::UsageReporter for ParkedUsage {
        fn usage<'a>(
            &'a self,
            _: &'a dyn Cancellation,
        ) -> BoxFuture<
            'a,
            std::result::Result<
                parrot_provider::sessions::decorators::SubscriptionUsage,
                parrot_provider::sessions::Error,
            >,
        > {
            Box::pin(async move {
                self.0.acquire().await.unwrap().forget();
                Ok(Default::default())
            })
        }
    }
    let f = Fixture::with_usage_reporter(Arc::new(ParkedUsage(Semaphore::new(0))));
    let owner = f.create().await;
    *owner.failure.lock().unwrap() = Some(RpcError::new(RpcCode::Internal, "status failed"));
    let request = wire::SessionStatusRequest {
        user_session_id: owner.id(),
        ..Default::default()
    };
    let cancellation = active();
    let error = bounded(f.service.session_status(&request, cancellation.as_ref()))
        .await
        .unwrap_err();
    assert_eq!(error.code, RpcCode::Internal);
    assert_eq!(error.message, "status failed");
    f.close().await;
}

// Source: In_process_permission_calls_map_pending_replies_and_errors.
// Target approval and append/publication here are explicitly test adapters;
// the actual broker and protocol mapping are used, not filesystem authority.
#[tokio::test]
async fn permissions_use_real_broker_preserve_targets_choices_and_errors() {
    let f = Fixture::new();
    let owner = f.create_interactive(true).await;
    let other = f.create().await;
    let approval = Arc::new(Approval(AtomicBool::new(false)));
    let approving = approval.clone();
    let requesting_owner = owner.clone();
    let targets = vec![
        PermissionTargetDescription {
            path: "/work/directory".into(),
            kind: PermissionTargetDescriptionKind::Directory,
        },
        PermissionTargetDescription {
            path: "/work/directory/file.txt".into(),
            kind: PermissionTargetDescriptionKind::File,
        },
    ];
    let requesting = tokio::spawn(async move {
        requesting_owner
            .permissions
            .request(
                PermissionRequest {
                    agent_session_id: "agent-one".into(),
                    reason: "update generated files".into(),
                    targets,
                    approval: approving,
                },
                active().as_ref(),
            )
            .await
    });
    bounded(async {
        while owner.permissions.pending().await.is_empty() {
            tokio::task::yield_now().await;
        }
    })
    .await;
    let pending = f
        .service
        .list_pending_permissions(&wire::ListPendingPermissionsRequest {
            user_session_id: owner.id(),
            ..Default::default()
        })
        .await
        .unwrap()
        .permissions
        .remove(0);
    assert_eq!(pending.agent_session_id, "agent-one");
    assert_eq!(pending.reason, "update generated files");
    assert_eq!(
        pending
            .targets
            .iter()
            .map(|t| t.path.as_str())
            .collect::<Vec<_>>(),
        ["/work/directory", "/work/directory/file.txt"]
    );
    assert_eq!(
        pending.targets[0].kind.enum_value().unwrap(),
        wire::PermissionTargetKind::PERMISSION_TARGET_KIND_DIRECTORY
    );
    assert_eq!(
        pending.targets[1].kind.enum_value().unwrap(),
        wire::PermissionTargetKind::PERMISSION_TARGET_KIND_FILE
    );
    assert!(pending.targets.iter().all(|t| t.scope.enum_value().unwrap()
        == wire::PermissionTargetScope::PERMISSION_TARGET_SCOPE_WRITE));
    assert_eq!(
        pending
            .choices
            .iter()
            .map(|c| (c.value.as_str(), c.requires_reason))
            .collect::<Vec<_>>(),
        [
            ("grant", false),
            ("reject", false),
            ("reject with reason", true)
        ]
    );
    assert!(
        f.service
            .list_pending_permissions(&wire::ListPendingPermissionsRequest {
                user_session_id: other.id(),
                ..Default::default()
            })
            .await
            .unwrap()
            .permissions
            .is_empty()
    );
    for (choice, reason) in [("missing", ""), ("reject with reason", "")] {
        code(
            f.service
                .reply_permission(&wire::ReplyPermissionRequest {
                    user_session_id: owner.id(),
                    permission_request_id: pending.id.clone(),
                    choice_value: choice.into(),
                    reason: reason.into(),
                    ..Default::default()
                })
                .await,
            RpcCode::InvalidArgument,
        );
    }
    let request = wire::ReplyPermissionRequest {
        user_session_id: owner.id(),
        permission_request_id: pending.id.clone(),
        choice_value: "grant".into(),
        ..Default::default()
    };
    f.service.reply_permission(&request).await.unwrap();
    assert_eq!(
        bounded(requesting).await.unwrap().unwrap().decision,
        PermissionDecision::Grant
    );
    assert!(approval.0.load(Ordering::SeqCst));
    code(
        f.service.reply_permission(&request).await,
        RpcCode::NotFound,
    );
    code(
        f.service
            .reply_permission(&wire::ReplyPermissionRequest {
                user_session_id: owner.id(),
                ..Default::default()
            })
            .await,
        RpcCode::InvalidArgument,
    );
    assert_eq!(
        *owner.permission_log.0.lock().unwrap(),
        [
            format!("append:{}", pending.id),
            format!("publish:{}", pending.id)
        ]
    );
    f.close().await;
}

// Added admission-port regression: no empty pending snapshot is fabricated
// when the real host read owner refuses admission. Actual lifetime proof is host-owned.
#[tokio::test]
async fn pending_interaction_reads_preserve_host_admission_failure() {
    let f = Fixture::new();
    let owner = f.create().await;
    *owner.failure.lock().unwrap() = Some(RpcError::new(RpcCode::Unavailable, "session closing"));
    code(
        f.service
            .list_pending_permissions(&wire::ListPendingPermissionsRequest {
                user_session_id: owner.id(),
                ..Default::default()
            })
            .await,
        RpcCode::Unavailable,
    );
    code(
        f.service
            .list_pending_questions(&wire::ListPendingQuestionsRequest {
                user_session_id: owner.id(),
                ..Default::default()
            }),
        RpcCode::Unavailable,
    );
    f.close().await;
}

// Added async-port regression: these gates prove protocol awaiting only;
// the authoritative host owns admitted producer lifetime and shutdown joins.
#[tokio::test]
async fn permission_rpcs_await_the_selected_host_without_blocking_question_validation() {
    let f = Fixture::new();
    let owner = f.create().await;
    let gate = Arc::new(Semaphore::new(0));
    *owner.permission_gate.lock().unwrap() = Some(gate.clone());
    let list_request = wire::ListPendingPermissionsRequest {
        user_session_id: owner.id(),
        ..Default::default()
    };
    {
        let listing = f.service.list_pending_permissions(&list_request);
        tokio::pin!(listing);
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(20), listing.as_mut())
                .await
                .is_err()
        );
        gate.add_permits(1);
        assert!(listing.await.unwrap().permissions.is_empty());
    }
    code(
        f.service
            .reply_permission(&wire::ReplyPermissionRequest {
                user_session_id: owner.id(),
                ..Default::default()
            })
            .await,
        RpcCode::InvalidArgument,
    );
    // The independent, unchanged question API is not behind a permission await.
    assert!(
        f.service
            .list_pending_questions(&wire::ListPendingQuestionsRequest {
                user_session_id: owner.id(),
                ..Default::default()
            })
            .unwrap()
            .questions
            .is_empty()
    );
    let reply_request = wire::ReplyPermissionRequest {
        user_session_id: owner.id(),
        permission_request_id: "unknown-permission".into(),
        choice_value: "grant".into(),
        ..Default::default()
    };
    {
        let replying = f.service.reply_permission(&reply_request);
        tokio::pin!(replying);
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(20), replying.as_mut())
                .await
                .is_err()
        );
        gate.add_permits(1);
        code(replying.await, RpcCode::NotFound);
    }
    f.close().await;
}

// Source: Create_session_defaults_to_noninteractive_permissions.
#[tokio::test]
async fn fresh_sessions_are_distinct_and_permissions_default_to_noninteractive() {
    let f = Fixture::new();
    let first = f.create().await;
    let second = f.create().await;
    assert_ne!(first.id(), second.id());
    let approval = Arc::new(Approval(AtomicBool::new(false)));
    let reply = bounded(first.permissions.request(
        PermissionRequest {
            agent_session_id: "agent".into(),
            reason: "write".into(),
            targets: vec![PermissionTargetDescription {
                path: "/work/file".into(),
                kind: PermissionTargetDescriptionKind::File,
            }],
            approval: approval.clone(),
        },
        active().as_ref(),
    ))
    .await
    .unwrap();
    assert_eq!(reply.decision, PermissionDecision::Reject);
    assert!(!approval.0.load(Ordering::SeqCst));
    assert!(first.permissions.pending().await.is_empty());
    f.close().await;
}

// Source in-process stream capacity 4 / Wait full mode. Poll the fifth write
// directly rather than using elapsed-time assertions as a backpressure oracle.
#[tokio::test]
async fn bounded_channel_waits_at_four_drains_on_stop_and_wakes_blocked_writer() {
    let (writer, mut reader) = bounded_client_stream();
    let cancellation = active();
    for value in 0..4 {
        writer.write(value, cancellation.as_ref()).await.unwrap();
    }
    let fifth = writer.write(4, cancellation.as_ref());
    tokio::pin!(fifth);
    assert!(
        std::future::poll_fn(|cx| std::task::Poll::Ready(fifth.as_mut().poll(cx).is_pending()))
            .await
    );
    assert_eq!(reader.next(cancellation.as_ref()).await.unwrap(), Some(0));
    bounded(fifth).await.unwrap();
    let sixth = writer.write(5, cancellation.as_ref());
    tokio::pin!(sixth);
    assert!(
        std::future::poll_fn(|cx| std::task::Poll::Ready(sixth.as_mut().poll(cx).is_pending()))
            .await
    );
    writer.stop();
    writer.stop();
    code(bounded(sixth).await, RpcCode::Internal);
    for expected in 1..5 {
        assert_eq!(
            bounded(reader.next(cancellation.as_ref())).await.unwrap(),
            Some(expected)
        );
    }
    assert_eq!(
        bounded(reader.next(cancellation.as_ref())).await.unwrap(),
        None
    );
    code(
        writer.write(6, cancellation.as_ref()).await,
        RpcCode::Internal,
    );
}

#[tokio::test]
async fn blocked_stream_reads_and_writes_cancel_and_reader_drop_releases_writer() {
    let (writer, mut reader) = bounded_client_stream();
    let cancellation = Cancel::new(false);
    let pending = reader.next(cancellation.as_ref());
    tokio::pin!(pending);
    assert!(
        std::future::poll_fn(|cx| std::task::Poll::Ready(pending.as_mut().poll(cx).is_pending()))
            .await
    );
    cancellation.cancel();
    code(bounded(pending).await, RpcCode::Cancelled);
    let (writer2, reader2) = bounded_client_stream();
    for value in 0..4 {
        writer2.write(value, active().as_ref()).await.unwrap();
    }
    let cancellation = Cancel::new(false);
    let pending = writer2.write(4, cancellation.as_ref());
    tokio::pin!(pending);
    assert!(
        std::future::poll_fn(|cx| std::task::Poll::Ready(pending.as_mut().poll(cx).is_pending()))
            .await
    );
    cancellation.cancel();
    code(bounded(pending).await, RpcCode::Cancelled);
    drop(reader2);
    code(writer2.write(5, active().as_ref()).await, RpcCode::Internal);
    code(
        writer.write(1, Cancel::new(true).as_ref()).await,
        RpcCode::Cancelled,
    );
}

// Source: Listener_replays_persisted_events_before_the_current_inventory;
// Shutdown_completes_an_existing_listener. The subscription is test-local,
// not a claim about the parent's production queue inventory adapter.
#[tokio::test]
async fn listen_forwards_replay_cursor_unknown_fields_and_turn_end_without_ending_subscription() {
    let f = Fixture::new();
    let owner = f.create().await;
    let mut before = wire::Event {
        id: "before".into(),
        agent_session_id: "root".into(),
        ..Default::default()
    };
    before.set_text_chunk(wire::TextChunk {
        fragment: "before".into(),
        ..Default::default()
    });
    owner.resources.events.append(before, None).await.unwrap();
    let mut terminal = wire::Event {
        id: "terminal".into(),
        agent_session_id: "root".into(),
        ..Default::default()
    };
    terminal.set_turn_ended(wire::TurnEnded::new());
    owner
        .resources
        .events
        .append(terminal.clone(), None)
        .await
        .unwrap();
    let mut unknown = wire::Event {
        id: "unknown".into(),
        agent_session_id: "root".into(),
        ..Default::default()
    }
    .write_to_bytes()
    .unwrap();
    unknown.extend_from_slice(&[0xa2, 0x06, 0x03, 7, 8, 9]);
    owner
        .resources
        .events
        .append_raw(unknown.clone(), None)
        .await
        .unwrap();
    let mut stream = f
        .service
        .listen(
            &wire::ListenRequest {
                user_session_id: owner.id(),
                replay: true,
                replay_after_event_id: "before".into(),
                ..Default::default()
            },
            active(),
        )
        .unwrap();
    assert_eq!(
        bounded(stream.next(active().as_ref())).await.unwrap(),
        Some(terminal)
    );
    assert_eq!(
        bounded(stream.next(active().as_ref()))
            .await
            .unwrap()
            .unwrap()
            .write_to_bytes()
            .unwrap(),
        unknown
    );
    let live = wire::Event {
        id: "live-after-turn".into(),
        ..Default::default()
    };
    let writer = owner.writers.lock().unwrap()[0].clone();
    writer.write(live.clone(), active().as_ref()).await.unwrap();
    assert_eq!(
        bounded(stream.next(active().as_ref())).await.unwrap(),
        Some(live)
    );
    assert!(owner.calls().contains(&Call::Listen(Some("before".into()))));
    f.close().await;
    assert_eq!(bounded(stream.next(active().as_ref())).await.unwrap(), None);
}

// Source: Shutdown_is_idempotent_and_refuses_new_session_operations.
#[tokio::test]
async fn close_is_idempotent_completes_listener_and_closes_all_session_rpc_admission() {
    let f = Fixture::new();
    let owner = f.create().await;
    let mut stream = f
        .service
        .listen(
            &wire::ListenRequest {
                user_session_id: owner.id(),
                ..Default::default()
            },
            active(),
        )
        .unwrap();
    let (a, b) = bounded(async { tokio::join!(f.service.close(), f.service.close()) }).await;
    a.unwrap();
    b.unwrap();
    assert!(owner.closed.load(Ordering::SeqCst));
    assert_eq!(bounded(stream.next(active().as_ref())).await.unwrap(), None);
    code(
        f.service
            .create_session(
                &wire::CreateSessionRequest {
                    model: "scripted/model".into(),
                    ..Default::default()
                },
                active(),
            )
            .await,
        RpcCode::Unavailable,
    );
    code(
        f.service.attach_session(
            &wire::AttachSessionRequest {
                user_session_id: owner.id(),
                working_directory: f.workspace.to_string_lossy().into_owned(),
                ..Default::default()
            },
            active().as_ref(),
        ),
        RpcCode::Unavailable,
    );
    code(
        f.service
            .send_message(&prompt(&owner.id(), "closed"), active())
            .await,
        RpcCode::Unavailable,
    );
    code(
        f.service
            .interrupt(
                &wire::InterruptRequest {
                    user_session_id: owner.id(),
                    ..Default::default()
                },
                active(),
            )
            .await,
        RpcCode::Unavailable,
    );
    code(
        f.service.listen(
            &wire::ListenRequest {
                user_session_id: owner.id(),
                ..Default::default()
            },
            active(),
        ),
        RpcCode::Unavailable,
    );
    assert_eq!(owner.lifecycle.lock().unwrap().len(), 1);
    assert!(
        !f.claim
            .is_active(
                &UserSessionId::try_parse(&owner.id()).unwrap(),
                &f.workspace
            )
            .unwrap()
    );
}

// Source: Registry_releases_failed_hosts_and_disposes_listener_before_session.
struct OrderedHost {
    session: Arc<FakeHostedSession>,
    fail: bool,
}
struct OrderedListener {
    session: Arc<FakeHostedSession>,
    fail: bool,
}
impl SessionListener for OrderedListener {
    fn close(&self) -> BoxFuture<'_, Result<()>> {
        Box::pin(async move {
            assert!(!self.session.closed.load(Ordering::SeqCst));
            self.session
                .lifecycle
                .lock()
                .unwrap()
                .push(format!("listener:{}", self.session.id()));
            if self.fail {
                Err(RpcError::new(RpcCode::Internal, "listener failed"))
            } else {
                Ok(())
            }
        })
    }
}
impl SessionHost for OrderedHost {
    fn host(
        &self,
        session: Arc<dyn HostedSession>,
        _: Arc<dyn Cancellation>,
    ) -> BoxFuture<'_, Result<Arc<dyn SessionListener>>> {
        Box::pin(async move {
            assert_eq!(session.snapshot().id, self.session.id());
            Ok(Arc::new(OrderedListener {
                session: self.session.clone(),
                fail: self.fail,
            }) as Arc<dyn SessionListener>)
        })
    }
}
fn opening(session: Arc<FakeHostedSession>) -> BoxFuture<'static, Result<Arc<dyn HostedSession>>> {
    Box::pin(async move { Ok(session as Arc<dyn HostedSession>) })
}

#[tokio::test]
async fn registry_failed_host_releases_actual_lease_and_close_attempts_listener_then_every_session()
{
    let f = Fixture::new();
    let failed = f.unhosted().await;
    f.host.fail.store(true, Ordering::SeqCst);
    code(
        bounded(
            f.service
                .sessions()
                .host(opening(failed.clone()), f.host.clone(), active()),
        )
        .await,
        RpcCode::Internal,
    );
    assert!(failed.closed.load(Ordering::SeqCst));
    assert!(!f.service.sessions().contains(&failed.id()));
    assert!(
        !f.claim
            .is_active(
                &UserSessionId::try_parse(&failed.id()).unwrap(),
                &f.workspace
            )
            .unwrap()
    );
    let one = f.unhosted().await;
    let two = f.unhosted().await;
    one.close_error.store(true, Ordering::SeqCst);
    for session in [&one, &two] {
        f.service
            .sessions()
            .host(
                opening(session.clone()),
                Arc::new(OrderedHost {
                    session: session.clone(),
                    fail: true,
                }),
                active(),
            )
            .await
            .unwrap();
    }
    code(bounded(f.service.close()).await, RpcCode::Internal);
    for session in [&one, &two] {
        assert!(session.closed.load(Ordering::SeqCst));
        assert_eq!(
            *session.lifecycle.lock().unwrap(),
            [
                format!("listener:{}", session.id()),
                format!("session:{}", session.id())
            ]
        );
        assert!(
            !f.claim
                .is_active(
                    &UserSessionId::try_parse(&session.id()).unwrap(),
                    &f.workspace
                )
                .unwrap()
        );
    }
    code(f.service.close().await, RpcCode::Internal);
}

// Source: Shutdown_closes_admission_without_waiting_to_acquire_a_slow_open_gate.
#[tokio::test]
async fn shutdown_during_slow_open_refuses_find_and_new_open_then_disposes_late_scope() {
    let f = Fixture::new();
    let late = f.unhosted().await;
    let (entered_send, entered) = oneshot::channel();
    let (release, released) = oneshot::channel();
    let opening_scope = late.clone();
    let registry = f.service.sessions().clone();
    let host = Arc::new(OrderedHost {
        session: late.clone(),
        fail: false,
    });
    let open = tokio::spawn(async move {
        registry
            .host(
                Box::pin(async move {
                    entered_send.send(()).unwrap();
                    released.await.unwrap();
                    Ok(opening_scope as Arc<dyn HostedSession>)
                }),
                host,
                active(),
            )
            .await
    });
    bounded(entered).await.unwrap();
    let service = f.service.clone();
    let shutdown = tokio::spawn(async move { service.close().await });
    until(|| {
        matches!(
            f.service.sessions().find("missing"),
            Err(RpcError {
                code: RpcCode::Unavailable,
                ..
            })
        )
    })
    .await;
    assert!(!shutdown.is_finished());
    let called = Arc::new(AtomicBool::new(false));
    let opening_called = called.clone();
    code(
        f.service
            .sessions()
            .host(
                Box::pin(async move {
                    opening_called.store(true, Ordering::SeqCst);
                    panic!("refused opening future must never be polled")
                }),
                f.host.clone(),
                active(),
            )
            .await,
        RpcCode::Unavailable,
    );
    assert!(!called.load(Ordering::SeqCst));
    release.send(()).unwrap();
    code(bounded(open).await.unwrap(), RpcCode::Unavailable);
    bounded(shutdown).await.unwrap().unwrap();
    assert!(late.closed.load(Ordering::SeqCst));
    assert_eq!(
        *late.lifecycle.lock().unwrap(),
        [
            format!("listener:{}", late.id()),
            format!("session:{}", late.id())
        ]
    );
}

// Source Common AgentSessionServicesTests.Registry_returns_exact_instances_rejects_missing_and_duplicate_keys_and_isolates_registries:
// only the analogous user-session registry contract is claimed here, not the
// source typed agent-service registry (owned by the parent composition tests).
#[tokio::test]
async fn concurrent_duplicate_host_keeps_exact_winner_and_disposes_loser_and_isolates_registries() {
    let f = Fixture::new();
    let first = f.unhosted().await;
    let second = f.unhosted().await;
    let duplicate = first.id();
    second.value.lock().unwrap().id = duplicate.clone();
    let registry = f.service.sessions();
    let (a, b) = bounded(async {
        tokio::join!(
            registry.host(
                opening(first.clone()),
                Arc::new(OrderedHost {
                    session: first.clone(),
                    fail: false
                }),
                active()
            ),
            registry.host(
                opening(second.clone()),
                Arc::new(OrderedHost {
                    session: second.clone(),
                    fail: false
                }),
                active()
            )
        )
    })
    .await;
    let winner = match (a, b) {
        (Ok(winner), Err(error)) | (Err(error), Ok(winner)) => {
            assert_eq!(error.code, RpcCode::AlreadyExists);
            winner
        }
        _ => panic!("exactly one duplicate may be admitted"),
    };
    assert!(Arc::ptr_eq(&registry.find(&duplicate).unwrap(), &winner));
    assert_eq!(
        first.closed.load(Ordering::SeqCst) as u8 + second.closed.load(Ordering::SeqCst) as u8,
        1
    );
    let isolated = UserSessionRegistry::new(tokio::runtime::Handle::current());
    code(isolated.find(&duplicate), RpcCode::NotFound);
    isolated.close().await.unwrap();
    f.close().await;
    assert!(first.closed.load(Ordering::SeqCst));
    assert!(second.closed.load(Ordering::SeqCst));
}

#[tokio::test]
async fn dropped_open_and_shutdown_observers_do_not_abandon_owned_registry_work() {
    let f = Fixture::new();
    let session = f.unhosted().await;
    let (entered_send, entered) = oneshot::channel();
    let (release, released) = oneshot::channel();
    let registry = f.service.sessions().clone();
    let scope = session.clone();
    let host = f.host.clone();
    let mut observing = Box::pin(registry.host(
        Box::pin(async move {
            entered_send.send(()).unwrap();
            released.await.unwrap();
            Ok(scope as Arc<dyn HostedSession>)
        }),
        host,
        active(),
    ));
    assert!(
        std::future::poll_fn(|cx| std::task::Poll::Ready(observing.as_mut().poll(cx).is_pending()))
            .await
    );
    bounded(entered).await.unwrap();
    drop(observing);
    let mut closing = Box::pin(registry.close());
    assert!(
        std::future::poll_fn(|cx| std::task::Poll::Ready(closing.as_mut().poll(cx).is_pending()))
            .await
    );
    drop(closing);
    release.send(()).unwrap();
    bounded(registry.close()).await.unwrap();
    assert!(session.closed.load(Ordering::SeqCst));
    code(registry.find(&session.id()), RpcCode::Unavailable);
}

// Source: Create_session_creates_fresh_instead_of_resuming_an_inactive_session;
// Resume_and_concurrent_attach_preserve_the_exact_session_without_new_activation.
#[tokio::test]
async fn closed_storage_session_resumes_exact_identity_and_new_create_is_always_fresh() {
    let f = Fixture::new();
    let original = f.create().await;
    let id = original.id();
    let sent = f
        .service
        .send_message(&prompt(&id, "before-resume"), active())
        .await
        .unwrap();
    f.close().await;
    let service = f.new_service();
    let resumed = bounded(service.resume_session(
        &wire::ResumeSessionRequest {
            user_session_id: id.clone(),
            working_directory: f.workspace.to_string_lossy().into_owned(),
            ..Default::default()
        },
        active(),
    ))
    .await
    .unwrap();
    assert_eq!(resumed.id, id);
    assert!(resumed.loaded);
    let current = f.factory.created.lock().unwrap().get(&id).unwrap().clone();
    let retry = service
        .send_message(&prompt(&id, "before-resume"), active())
        .await
        .unwrap();
    assert_eq!(retry.input_id, sent.input_id);
    assert!(!retry.created);
    let fresh = service
        .create_session(
            &wire::CreateSessionRequest {
                model: "scripted/model".into(),
                ..Default::default()
            },
            active(),
        )
        .await
        .unwrap();
    assert_ne!(fresh.id, id);
    assert!(!fresh.loaded);
    assert!(!Arc::ptr_eq(&current, &original));
    bounded(service.close()).await.unwrap();
    assert_eq!(SessionCatalog::new(&f.state).list().len(), 2);
}

#[tokio::test]
async fn legacy_only_resume_uses_eligible_metadata_and_preserves_validation_and_history() {
    let f = Fixture::new();
    f.admissions.set_legacy_path_read_fallback(true);
    let original = f.create().await;
    let id = original.id();
    let sent = f
        .service
        .send_message(&prompt(&id, "before-legacy-resume"), active())
        .await
        .unwrap();
    f.close().await;
    let legacy = f.root.join("legacy/sessions").join(&id);
    std::fs::create_dir_all(legacy.parent().unwrap()).unwrap();
    std::fs::rename(f.state.join("sessions").join(&id), &legacy).unwrap();
    std::fs::rename(f.state.join("owners"), f.root.join("legacy/owners")).unwrap();
    let discovered = f.store.discover_latest().unwrap();
    assert_eq!(discovered.selected_root.unwrap().path(), legacy);
    let service = f.new_service();
    let request = wire::ResumeSessionRequest {
        user_session_id: id.clone(),
        working_directory: f.workspace.to_string_lossy().into_owned(),
        ..Default::default()
    };
    let metadata = std::fs::read(legacy.join("meta.json")).unwrap();
    std::fs::write(legacy.join("meta.json"), "not json").unwrap();
    let error = bounded(service.resume_session(&request, active()))
        .await
        .unwrap_err();
    assert_eq!(error.code, RpcCode::FailedPrecondition);
    assert_eq!(error.message, "session metadata is corrupt");
    std::fs::write(legacy.join("meta.json"), metadata).unwrap();
    let other = f.root.join("other-workspace");
    std::fs::create_dir(&other).unwrap();
    code(
        bounded(service.resume_session(
            &wire::ResumeSessionRequest {
                working_directory: other.to_string_lossy().into_owned(),
                ..request.clone()
            },
            active(),
        ))
        .await,
        RpcCode::FailedPrecondition,
    );
    let resumed = bounded(service.resume_session(&request, active()))
        .await
        .unwrap();
    assert_eq!(resumed.id, id);
    assert!(resumed.loaded);
    let retry = service
        .send_message(&prompt(&id, "before-legacy-resume"), active())
        .await
        .unwrap();
    assert_eq!(retry.input_id, sent.input_id);
    assert!(!retry.created);
    let hosted = f.factory.created.lock().unwrap().get(&id).unwrap().clone();
    assert_eq!(hosted.resources.selected_root().path(), legacy);
    assert!(!f.state.join("sessions").join(&id).exists());
    bounded(service.close()).await.unwrap();
}

#[tokio::test]
async fn legacy_only_resume_is_not_found_when_fallback_is_disabled() {
    let f = Fixture::new();
    let id = f.create().await.id();
    f.close().await;
    let legacy = f.root.join("legacy/sessions").join(&id);
    std::fs::create_dir_all(legacy.parent().unwrap()).unwrap();
    std::fs::rename(f.state.join("sessions").join(&id), &legacy).unwrap();
    std::fs::rename(f.state.join("owners"), f.root.join("legacy/owners")).unwrap();
    let service = f.new_service();
    code(
        bounded(service.resume_session(
            &wire::ResumeSessionRequest {
                user_session_id: id,
                working_directory: f.workspace.to_string_lossy().into_owned(),
                ..Default::default()
            },
            active(),
        ))
        .await,
        RpcCode::NotFound,
    );
    bounded(service.close()).await.unwrap();
}

#[tokio::test]
async fn resume_metadata_prefers_current_even_when_current_metadata_is_corrupt() {
    for corrupt_current in [false, true] {
        let f = Fixture::new();
        f.admissions.set_legacy_path_read_fallback(true);
        let id = f.create().await.id();
        f.close().await;
        let current = f.state.join("sessions").join(&id);
        let legacy = f.root.join("legacy/sessions").join(&id);
        std::fs::create_dir_all(&legacy).unwrap();
        std::fs::write(
            legacy.join("meta.json"),
            std::fs::read(current.join("meta.json")).unwrap(),
        )
        .unwrap();
        let corrupt = if corrupt_current { &current } else { &legacy };
        std::fs::write(corrupt.join("meta.json"), "not json").unwrap();
        let service = f.new_service();
        let result = bounded(service.resume_session(
            &wire::ResumeSessionRequest {
                user_session_id: id.clone(),
                working_directory: f.workspace.to_string_lossy().into_owned(),
                ..Default::default()
            },
            active(),
        ))
        .await;
        if corrupt_current {
            let error = result.unwrap_err();
            assert_eq!(error.code, RpcCode::FailedPrecondition);
            assert_eq!(error.message, "session metadata is corrupt");
        } else {
            assert_eq!(result.unwrap().id, id);
            let hosted = f.factory.created.lock().unwrap().get(&id).unwrap().clone();
            assert_eq!(hosted.resources.selected_root().path(), current);
        }
        bounded(service.close()).await.unwrap();
    }
}

// Source model cases are thoroughly owned by models.rs; this only checks that
// all model RPC entry points of actual ParrotService delegate to those concrete
// operations and that session-scoped presets use the selected live owner.
#[tokio::test]
async fn model_rpc_entrypoints_delegate_to_real_operations_and_session_preset_owner() {
    let f = Fixture::new();
    let owner = f.create().await;
    let other = f.create().await;
    let models = f
        .service
        .list_models(&wire::ListModelsRequest::new(), active().as_ref())
        .await
        .unwrap();
    assert_eq!(models.models[0].provider_id, "scripted");
    f.service
        .configure_model_alias(&wire::ConfigureModelAliasRequest {
            name: "test-alias".into(),
            model_string: "scripted/model".into(),
            ..Default::default()
        })
        .unwrap();
    assert!(
        f.service
            .list_model_aliases(&wire::ListModelAliasesRequest::new())
            .aliases
            .iter()
            .any(|a| a.name == "test-alias")
    );
    assert_eq!(
        f.service
            .list_provider_model_alias_defaults(
                &wire::ListProviderModelAliasDefaultsRequest::new(),
                active().as_ref()
            )
            .await
            .unwrap()
            .providers[0]
            .provider_id,
        "scripted"
    );
    assert_eq!(
        f.service
            .apply_provider_model_alias_defaults(
                &wire::ApplyProviderModelAliasDefaultsRequest {
                    provider_id: "scripted".into(),
                    ..Default::default()
                },
                active().as_ref()
            )
            .await
            .unwrap()
            .aliases
            .len(),
        4
    );
    let saved = f
        .service
        .set_model_preset(
            &wire::SetModelPresetRequest {
                user_session_id: owner.id(),
                name: "saved".into(),
                ..Default::default()
            },
            active().as_ref(),
        )
        .unwrap();
    assert_eq!(saved.preset.as_ref().unwrap().model, "scripted/model");
    assert_eq!(
        f.service
            .list_model_presets(&wire::ListModelPresetsRequest::new(), active().as_ref())
            .unwrap()
            .presets
            .len(),
        1
    );
    f.service
        .update_session(&wire::UpdateSessionRequest {
            user_session_id: owner.id(),
            model: "scripted/changed".into(),
            ..Default::default()
        })
        .await
        .unwrap();
    let selected = f
        .service
        .select_model_preset(
            &wire::SelectModelPresetRequest {
                user_session_id: owner.id(),
                name: "saved".into(),
                ..Default::default()
            },
            active().as_ref(),
        )
        .unwrap();
    assert_eq!(selected.session.as_ref().unwrap().id, owner.id());
    assert_eq!(owner.snapshot().model, "scripted/model");
    assert!(other.calls().is_empty());
    code(
        f.service.set_model_preset(
            &wire::SetModelPresetRequest {
                user_session_id: "missing".into(),
                name: "bad".into(),
                ..Default::default()
            },
            active().as_ref(),
        ),
        RpcCode::NotFound,
    );
    code(
        f.service.select_model_preset(
            &wire::SelectModelPresetRequest {
                user_session_id: "missing".into(),
                name: "saved".into(),
                ..Default::default()
            },
            active().as_ref(),
        ),
        RpcCode::NotFound,
    );
    f.close().await;
}

#[tokio::test]
async fn cancellation_and_session_failures_are_forwarded_without_fabricating_success() {
    let f = Fixture::new();
    let owner = f.create().await;
    let id = owner.id();
    let cancelled = Cancel::new(true);
    code(
        f.service
            .send_message(&prompt(&id, "cancelled"), cancelled.clone())
            .await,
        RpcCode::Cancelled,
    );
    code(
        f.service
            .interrupt(
                &wire::InterruptRequest {
                    user_session_id: id.clone(),
                    ..Default::default()
                },
                cancelled.clone(),
            )
            .await,
        RpcCode::Cancelled,
    );
    code(
        f.service
            .compact(
                &wire::CompactRequest {
                    user_session_id: id.clone(),
                    ..Default::default()
                },
                cancelled.clone(),
            )
            .await,
        RpcCode::Cancelled,
    );
    code(
        f.service
            .set_goal(
                &wire::SetGoalRequest {
                    user_session_id: id.clone(),
                    state: Some(wire::set_goal_request::State::Clear(wire::ClearGoal::new())),
                    ..Default::default()
                },
                cancelled.clone(),
            )
            .await,
        RpcCode::Cancelled,
    );
    code(
        f.service
            .list_skills(
                &wire::ListSkillsRequest {
                    user_session_id: id.clone(),
                    ..Default::default()
                },
                cancelled.as_ref(),
            )
            .await,
        RpcCode::Cancelled,
    );
    code(
        f.service.set_context_limit(
            &wire::SetContextLimitRequest {
                user_session_id: id.clone(),
                context_limit: "2k".into(),
                ..Default::default()
            },
            cancelled.as_ref(),
        ),
        RpcCode::Cancelled,
    );
    code(
        f.service.sandbox_enable(
            &wire::SandboxEnableRequest {
                user_session_id: id.clone(),
                enabled: false,
                ..Default::default()
            },
            cancelled.as_ref(),
        ),
        RpcCode::Cancelled,
    );
    code(
        f.service.listen(
            &wire::ListenRequest {
                user_session_id: id.clone(),
                ..Default::default()
            },
            cancelled,
        ),
        RpcCode::Cancelled,
    );
    assert!(owner.calls().is_empty());
    assert!(f.sandbox.enabled());
    let failure = RpcError::new(RpcCode::PermissionDenied, "session operation refused");
    *owner.failure.lock().unwrap() = Some(failure.clone());
    assert_eq!(
        f.service
            .send_message(&prompt(&id, "failed"), active())
            .await
            .unwrap_err(),
        failure
    );
    assert_eq!(
        f.service
            .interrupt(
                &wire::InterruptRequest {
                    user_session_id: id.clone(),
                    ..Default::default()
                },
                active()
            )
            .await
            .unwrap_err(),
        failure
    );
    assert_eq!(
        f.service
            .set_standing_instruction(&wire::SetStandingInstructionRequest {
                user_session_id: id.clone(),
                instruction: "x".into(),
                ..Default::default()
            })
            .unwrap_err(),
        failure
    );
    assert_eq!(
        f.service
            .update_session(&wire::UpdateSessionRequest {
                user_session_id: id.clone(),
                ..Default::default()
            })
            .await
            .unwrap_err(),
        failure
    );
    assert_eq!(
        f.service
            .list_skills(
                &wire::ListSkillsRequest {
                    user_session_id: id.clone(),
                    ..Default::default()
                },
                active().as_ref()
            )
            .await
            .unwrap_err(),
        failure
    );
    assert!(owner.resources.events.replay().await.unwrap().is_empty());
    f.close().await;
}

struct ControlledClock(std::sync::Mutex<std::time::Instant>);
impl parrot_domain::contracts::async_ops::Clock for ControlledClock {
    fn now(&self) -> std::time::Instant {
        *self.0.lock().unwrap()
    }
    fn utc_now(&self) -> std::time::SystemTime {
        std::time::SystemTime::UNIX_EPOCH
    }
    fn sleep_until<'a>(
        &'a self,
        _: std::time::Instant,
        c: &'a dyn Cancellation,
    ) -> BoxFuture<'a, std::result::Result<(), parrot_domain::contracts::async_ops::Cancelled>>
    {
        Box::pin(async move {
            c.cancelled().await;
            Err(parrot_domain::contracts::async_ops::Cancelled)
        })
    }
}

// Source: In_process_questions_preserve_remaining_timeout_presence_and_value
// exact source rows (1000,250,750), (1000,1000,0), (-1,1000,null).
#[tokio::test]
async fn question_timeout_presence_preserves_source_rows_without_wall_clock_timing() {
    for (timeout, elapsed, expected) in [
        (Some(1000), 250, Some(750)),
        (Some(1000), 1000, Some(0)),
        (None, 1000, None),
    ] {
        let f = Fixture::new();
        let clock = Arc::new(ControlledClock(std::sync::Mutex::new(
            std::time::Instant::now(),
        )));
        *f.factory.question_timing.lock().unwrap() =
            Some((timeout.map(std::time::Duration::from_millis), clock.clone()));
        let owner = f.create().await;
        let asking_owner = owner.clone();
        let asking = tokio::spawn(async move {
            asking_owner
                .questions
                .ask(
                    vec![QuestionDefinition {
                        header: "Decision".into(),
                        prompt: "Pick a colour".into(),
                        options: vec![QuestionOption {
                            label: "Blue".into(),
                            description: "".into(),
                        }],
                        multiple: false,
                        custom: false,
                    }],
                    active().as_ref(),
                )
                .await
        });
        until(|| !owner.questions.pending().is_empty()).await;
        *clock.0.lock().unwrap() += std::time::Duration::from_millis(elapsed);
        let pending = f
            .service
            .list_pending_questions(&wire::ListPendingQuestionsRequest {
                user_session_id: owner.id(),
                ..Default::default()
            })
            .unwrap()
            .questions
            .remove(0);
        assert_eq!(pending.remaining_timeout_ms, expected);
        f.service
            .reply_question(&wire::ReplyQuestionRequest {
                user_session_id: owner.id(),
                question_request_id: pending.id,
                answers: vec![wire::QuestionAnswer {
                    text: "Blue".into(),
                    ..Default::default()
                }],
                ..Default::default()
            })
            .unwrap();
        assert_eq!(
            bounded(asking).await.unwrap().unwrap().kind,
            QuestionReplyKind::Answered
        );
        f.close().await;
    }
}

// Source: Startup_completion_and_host_stages_remain_in_global_log.
#[tokio::test]
async fn startup_and_host_failure_diagnostics_keep_global_correlation_and_owned_session_identity() {
    let f = Fixture::new();
    let owner = f.create().await;
    let global = std::fs::read_to_string(f.diagnostics.path()).unwrap();
    for operation in [
        "create_session_start",
        "host_start",
        "host_complete",
        "create_session_complete",
    ] {
        assert!(global.contains(operation), "missing {operation}: {global}");
    }
    let completion = global
        .lines()
        .find(|line| line.contains("create_session_complete"))
        .unwrap();
    assert!(completion.contains(&owner.id()));
    let correlation = completion
        .split_whitespace()
        .find_map(|word| word.strip_prefix("correlation="))
        .unwrap();
    let correlation = correlation.trim_matches('"');
    assert_eq!(correlation.len(), 32);
    assert!(correlation.bytes().all(|b| b.is_ascii_hexdigit()));
    for operation in ["host_start", "host_complete"] {
        assert!(
            global
                .lines()
                .find(|line| line.contains(operation))
                .unwrap()
                .contains(correlation)
        );
    }
    f.host.fail.store(true, Ordering::SeqCst);
    code(
        f.service
            .create_session(
                &wire::CreateSessionRequest {
                    model: "scripted/model".into(),
                    ..Default::default()
                },
                active(),
            )
            .await,
        RpcCode::Internal,
    );
    let failed = f
        .factory
        .created
        .lock()
        .unwrap()
        .values()
        .find(|session| session.closed.load(Ordering::SeqCst))
        .unwrap()
        .clone();
    let global = std::fs::read_to_string(f.diagnostics.path()).unwrap();
    for operation in ["host_failure", "create_session_failure"] {
        assert!(
            global
                .lines()
                .find(|line| line.contains(operation))
                .unwrap()
                .contains(&failed.id()),
            "missing session {} on {operation}: {global}",
            failed.id()
        );
    }
    assert!(!global.contains("session operation refused"));
    f.close().await;
}

#[tokio::test]
async fn cancellation_during_pending_compaction_and_upload_releases_waiters() {
    let f = Fixture::new();
    let owner = f.create().await;
    *owner.compact_gate.lock().unwrap() = Some(Arc::new(Semaphore::new(0)));
    let cancellation = active();
    let service = f.service.clone();
    let id = owner.id();
    let operation_cancellation = cancellation.clone();
    let compact = tokio::spawn(async move {
        service
            .compact(
                &wire::CompactRequest {
                    user_session_id: id,
                    ..Default::default()
                },
                operation_cancellation,
            )
            .await
    });
    until(|| {
        owner
            .calls()
            .iter()
            .any(|call| matches!(call, Call::Compact(None)))
    })
    .await;
    assert!(!compact.is_finished());
    cancellation.cancel();
    code(bounded(compact).await.unwrap(), RpcCode::Cancelled);

    let cancellation = active();
    let (writer, mut reader) = bounded_client_stream();
    writer
        .write(header(&owner.id(), "cancel-upload"), active().as_ref())
        .await
        .unwrap();
    let upload = f
        .service
        .upload_attachment(&mut reader, cancellation.as_ref());
    tokio::pin!(upload);
    assert!(
        std::future::poll_fn(|cx| std::task::Poll::Ready(upload.as_mut().poll(cx).is_pending()))
            .await
    );
    cancellation.cancel();
    code(bounded(upload).await, RpcCode::Cancelled);
    assert!(owner.resources.events.replay().await.unwrap().is_empty());
    f.close().await;
}
