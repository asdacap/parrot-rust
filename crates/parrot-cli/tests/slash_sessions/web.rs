//! Concrete CLI factory + existing generated web service join. The controlled
//! native endpoint and unused startup are not application/startup/HTTP proof.
use std::{collections::HashMap, sync::Arc, time::Duration};

use futures_util::StreamExt;
use parrot_cli::slash_sessions::WebCommandFactory;
use parrot_domain::{
    contracts::async_ops::{BoxFuture, Cancellation},
    protocol::{
        bindings::web::parrot_web_server::ParrotWeb,
        messages::{parrot as wire, web},
    },
    values::ProjectWorkspace,
};
use parrot_foundation::paths::ApplicationRoots;
use parrot_host::web::{
    router::{WebOpenedSession, WebSessionRouter, WebStartup},
    slash::WebSlashService,
};
use parrot_storage::diagnostics::FileDiagnosticLog;
use protobuf::Message;
use tonic::{Code, Request, Status};

use crate::{
    integration::{auth, configuration, initial},
    native_fixture::Fixture,
};

struct UnusedStartup;
impl WebStartup for UnusedStartup {
    fn open<'a>(
        &'a self,
        _: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<WebOpenedSession, Status>> {
        Box::pin(async { panic!("run/list adapter fixtures must not claim startup coverage") })
    }
}
fn service(fixture: &mut Fixture) -> WebSlashService {
    let client = fixture.client();
    let roots = ApplicationRoots::resolve(&HashMap::from([(
        "HOME".into(),
        fixture.root.to_string_lossy().into_owned(),
    )]));
    let workspace =
        ProjectWorkspace::from_resolved(fixture.root.clone(), fixture.root.clone(), None, None);
    let diagnostics = Arc::new(
        FileDiagnosticLog::open_global(fixture.root.join("web-logs"), "web-adapter", |_| {})
            .unwrap(),
    );
    let router = Arc::new(WebSessionRouter::new(
        client.clone(),
        roots,
        workspace,
        Arc::new(UnusedStartup),
        diagnostics,
    ));
    let factory = Arc::new(WebCommandFactory::new(
        configuration(&fixture.root),
        auth(&fixture.root),
        "adapter-version",
    ));
    WebSlashService::new(router, client, factory, "/synthetic/web-workspace".into())
}
fn attach(bytes: &[u8]) -> Vec<u8> {
    let request = wire::AttachSessionRequest::parse_from_bytes(bytes).unwrap();
    assert_eq!(request.user_session_id, "old");
    assert_eq!(request.working_directory, "/synthetic/web-workspace");
    initial().write_to_bytes().unwrap()
}
async fn answer(
    service: &WebSlashService,
    run_id: &str,
    prompt_id: &str,
    answer: web::answer_slash_prompt_request::Answer,
) {
    service
        .answer_slash_prompt(Request::new(web::AnswerSlashPromptRequest {
            run_id: run_id.into(),
            prompt_id: prompt_id.into(),
            answer: Some(answer),
            ..Default::default()
        }))
        .await
        .unwrap();
}
async fn drive(
    service: &WebSlashService,
    text: &str,
    selections: &[&str],
) -> Result<Vec<web::slash_frame::Payload>, Status> {
    let mut run = service
        .run_slash_command(Request::new(web::RunSlashCommandRequest {
            user_session_id: "old".into(),
            text: text.into(),
            ..Default::default()
        }))
        .await?
        .into_inner();
    let mut frames = Vec::new();
    let mut run_id = String::new();
    let mut selections = selections.iter();
    while let Some(frame) = tokio::time::timeout(Duration::from_secs(3), run.next())
        .await
        .expect("adapter stream stalled")
    {
        let payload = frame?.payload.unwrap();
        match &payload {
            web::slash_frame::Payload::Started(started) => {
                assert!(frames.is_empty());
                run_id = started.run_id.clone();
                assert_eq!(run_id.len(), 32);
            }
            web::slash_frame::Payload::Select(select) => {
                let selected = *selections.next().expect("unexpected selection prompt");
                assert!(select.options.iter().any(|option| option.id == selected));
                answer(
                    service,
                    &run_id,
                    &select.prompt_id,
                    web::answer_slash_prompt_request::Answer::OptionId(selected.into()),
                )
                .await;
            }
            web::slash_frame::Payload::Show(show) => {
                answer(
                    service,
                    &run_id,
                    &show.prompt_id,
                    web::answer_slash_prompt_request::Answer::Confirmed(true),
                )
                .await
            }
            _ => {}
        }
        frames.push(payload);
    }
    assert!(selections.next().is_none());
    Ok(frames)
}

#[tokio::test]
async fn web_factory_lists_same_registry_without_exit_or_native_side_effects() {
    let mut fixture =
        Fixture::start(|_, _| panic!("command listing must not call native RPC")).await;
    let service = service(&mut fixture);
    let commands = service
        .list_slash_commands(Request::new(web::ListSlashCommandsRequest::new()))
        .await
        .unwrap()
        .into_inner()
        .commands;
    assert_eq!(commands.len(), 22);
    assert_eq!(commands[0].name, "/auth");
    assert_eq!(commands.last().unwrap().name, "/version");
    assert!(
        !commands
            .iter()
            .any(|command| matches!(command.name.as_str(), "/exit" | "/workspace-sessions"))
    );
    assert!(commands.iter().all(|command| !command.summary.is_empty()));
    assert!(fixture.calls.lock().unwrap().is_empty());
    drop(service);
    fixture.close().await;
}

#[tokio::test]
async fn web_mode_dispatch_answers_same_registry_and_emits_authoritative_session_update() {
    let mut fixture = Fixture::start(|method, bytes| match method {
        "attach_session" => Ok(attach(bytes)),
        "list_modes" => Ok(wire::ListModesResponse {
            modes: vec![wire::Mode {
                id: "plan".into(),
                ..Default::default()
            }],
            ..Default::default()
        }
        .write_to_bytes()
        .unwrap()),
        "update_session" => {
            let request = wire::UpdateSessionRequest::parse_from_bytes(bytes).unwrap();
            assert_eq!(request.user_session_id, "old");
            assert_eq!(request.mode, "plan");
            Ok(wire::UserSession {
                mode: "server-plan".into(),
                ..initial()
            }
            .write_to_bytes()
            .unwrap())
        }
        _ => panic!("unexpected /mode RPC {method}"),
    })
    .await;
    let service = service(&mut fixture);
    let frames = drive(&service, "/mode", &["plan"]).await.unwrap();
    assert!(matches!(
        frames.as_slice(),
        [
            web::slash_frame::Payload::Started(_),
            web::slash_frame::Payload::Loading(_),
            web::slash_frame::Payload::Loaded(_),
            web::slash_frame::Payload::Select(_),
            web::slash_frame::Payload::Show(_),
            web::slash_frame::Payload::SessionUpdated(_)
        ]
    ));
    assert!(
        matches!(frames.last().unwrap(),web::slash_frame::Payload::SessionUpdated(session)if session.id=="old"&&session.model=="provider/current"&&session.mode=="server-plan")
    );
    assert_eq!(
        fixture
            .calls
            .lock()
            .unwrap()
            .iter()
            .map(|call| call.method)
            .collect::<Vec<_>>(),
        ["attach_session", "list_modes", "update_session"]
    );
    drop(service);
    fixture.close().await;
}

#[tokio::test]
async fn web_clear_uses_real_model_and_mode_wizards_then_emits_replacement_not_update() {
    let mut fixture = Fixture::start(|method, bytes| match method {
        "attach_session" => Ok(attach(bytes)),
        "list_models" => Ok(wire::ListModelsResponse {
            models: vec![wire::Model {
                id: "new".into(),
                provider_id: "provider".into(),
                ..Default::default()
            }],
            ..Default::default()
        }
        .write_to_bytes()
        .unwrap()),
        "list_modes" => Ok(wire::ListModesResponse {
            modes: vec![wire::Mode {
                id: "plan".into(),
                ..Default::default()
            }],
            ..Default::default()
        }
        .write_to_bytes()
        .unwrap()),
        "create_session" => {
            let request = wire::CreateSessionRequest::parse_from_bytes(bytes).unwrap();
            assert_eq!(request.model, "provider/new");
            assert_eq!(request.mode, "plan");
            assert!(request.interactive_permissions);
            Ok(wire::UserSession {
                id: "replacement".into(),
                model: request.model,
                mode: request.mode,
                ..Default::default()
            }
            .write_to_bytes()
            .unwrap())
        }
        _ => panic!("unexpected /clear RPC {method}"),
    })
    .await;
    let service = service(&mut fixture);
    let config = std::fs::read(fixture.root.join("config.yaml")).unwrap();
    let frames = drive(&service, "/clear", &["provider", "new", "plan"])
        .await
        .unwrap();
    let replacements: Vec<_> = frames
        .iter()
        .filter_map(|frame| {
            if let web::slash_frame::Payload::SessionReplaced(session) = frame {
                Some(session)
            } else {
                None
            }
        })
        .collect();
    assert_eq!(replacements.len(), 1);
    assert_eq!(replacements[0].id, "replacement");
    assert_eq!(replacements[0].model, "provider/new");
    assert_eq!(replacements[0].mode, "plan");
    assert!(
        !frames
            .iter()
            .any(|frame| matches!(frame, web::slash_frame::Payload::SessionUpdated(_)))
    );
    assert!(
        matches!(frames.last().unwrap(),web::slash_frame::Payload::Show(show)if show.lines==["new session replacement"])
    );
    assert_eq!(
        std::fs::read(fixture.root.join("config.yaml")).unwrap(),
        config
    );
    drop(service);
    fixture.close().await;
}

#[tokio::test]
async fn web_registry_errors_use_error_frames_but_native_cancellation_is_status() {
    for text in ["/exit", "/mode"] {
        let mut fixture = Fixture::start(|method, bytes| match method {
            "attach_session" => Ok(attach(bytes)),
            "list_modes" => Err(Status::cancelled("native cancellation")),
            _ => panic!("unexpected error RPC {method}"),
        })
        .await;
        let service = service(&mut fixture);
        let result = drive(&service, text, &[]).await;
        if text == "/exit" {
            let frames = result.unwrap();
            assert!(
                matches!(frames.as_slice(),[web::slash_frame::Payload::Started(_),web::slash_frame::Payload::Error(error)]if error.message=="unknown command /exit, try /help")
            );
        } else {
            let failure = result.unwrap_err();
            assert_eq!(failure.code(), Code::Cancelled);
            assert_eq!(failure.message(), "native cancellation");
        }
        drop(service);
        fixture.close().await;
    }
}

#[tokio::test]
async fn dropping_web_prompt_run_unregisters_pending_dialog_without_updating_session() {
    let mut fixture = Fixture::start(|method, bytes| match method {
        "attach_session" => Ok(attach(bytes)),
        "list_modes" => Ok(wire::ListModesResponse {
            modes: vec![wire::Mode {
                id: "plan".into(),
                ..Default::default()
            }],
            ..Default::default()
        }
        .write_to_bytes()
        .unwrap()),
        _ => panic!("dropped prompt must not mutate native session: {method}"),
    })
    .await;
    let service = service(&mut fixture);
    let mut run = service
        .run_slash_command(Request::new(web::RunSlashCommandRequest {
            user_session_id: "old".into(),
            text: "/mode".into(),
            ..Default::default()
        }))
        .await
        .unwrap()
        .into_inner();
    let Some(web::slash_frame::Payload::Started(started)) =
        run.next().await.unwrap().unwrap().payload
    else {
        panic!("missing started")
    };
    let prompt_id = loop {
        let frame = tokio::time::timeout(Duration::from_secs(3), run.next())
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        if let Some(web::slash_frame::Payload::Select(select)) = frame.payload {
            break select.prompt_id;
        }
    };
    drop(run);
    let failure = service
        .answer_slash_prompt(Request::new(web::AnswerSlashPromptRequest {
            run_id: started.run_id,
            prompt_id,
            answer: Some(web::answer_slash_prompt_request::Answer::OptionId(
                "plan".into(),
            )),
            ..Default::default()
        }))
        .await
        .unwrap_err();
    assert_eq!(failure.code(), Code::NotFound);
    assert_eq!(
        fixture
            .calls
            .lock()
            .unwrap()
            .iter()
            .map(|call| call.method)
            .collect::<Vec<_>>(),
        ["attach_session", "list_modes"]
    );
    drop(service);
    fixture.close().await;
}
