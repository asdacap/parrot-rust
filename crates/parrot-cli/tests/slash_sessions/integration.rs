use std::{
    collections::BTreeMap,
    path::Path,
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use parrot_cli::{
    slash_sessions::*,
    slash_settings::{AuthServices, SettingsServices},
};
use parrot_domain::{
    contracts::async_ops::{BoxFuture, Cancellation, OperationResult},
    protocol::messages::parrot as wire,
};
use parrot_foundation::{
    config::Configuration,
    credentials::{CredentialPresets, CredentialStore, OAuthCredential},
    oauth::{AuthError, DeviceAuthorization, OAuthClient},
};
use parrot_host::web::router::WebClient;
use parrot_storage::diagnostics::FileDiagnosticLog;
use protobuf::Message;
use tonic::{Code, Status};

use crate::{
    control_fixtures::{Record, Session, refresh},
    native_fixture::{Fixture, RecordingDialog, Stop},
};

struct UnusedOAuth;
impl OAuthClient for UnusedOAuth {
    fn now(&self) -> SystemTime {
        UNIX_EPOCH
    }
    fn authorization_url(&self, _: &str, _: &str, _: &str) -> String {
        panic!("OAuth not authorized in registry fixture")
    }
    fn browser_login<'a>(
        &'a self,
        _: &'a dyn Cancellation,
    ) -> BoxFuture<'a, OperationResult<OAuthCredential, AuthError>> {
        Box::pin(async { panic!("unused OAuth") })
    }
    fn start_device_authorization<'a>(
        &'a self,
        _: &'a dyn Cancellation,
    ) -> BoxFuture<'a, OperationResult<DeviceAuthorization, AuthError>> {
        Box::pin(async { panic!("unused OAuth") })
    }
    fn await_device_authorization<'a>(
        &'a self,
        _: &'a DeviceAuthorization,
        _: &'a dyn Cancellation,
    ) -> BoxFuture<'a, OperationResult<OAuthCredential, AuthError>> {
        Box::pin(async { panic!("unused OAuth") })
    }
    fn refresh<'a>(
        &'a self,
        _: &'a OAuthCredential,
        _: &'a dyn Cancellation,
    ) -> BoxFuture<'a, OperationResult<OAuthCredential, AuthError>> {
        Box::pin(async { panic!("unused OAuth") })
    }
}

pub fn configuration(root: &Path) -> Arc<Mutex<Configuration>> {
    let path = root.join("config.yaml");
    std::fs::write(&path, "model: provider/current\n").unwrap();
    Arc::new(Mutex::new(
        Configuration::load(
            &path,
            &root.join("predefined.yaml"),
            &BTreeMap::from([("HOME".into(), root.to_string_lossy().into_owned())]),
            |_| Ok(()),
        )
        .unwrap(),
    ))
}
pub fn auth(root: &Path) -> Arc<AuthServices> {
    Arc::new(AuthServices {
        credentials: Arc::new(CredentialStore::at_selected_path(
            root.join("credentials.json"),
        )),
        presets: Arc::new(CredentialPresets::at_selected_config_directory(root)),
        oauth: Arc::new(UnusedOAuth),
        provider_ids: vec!["provider".into()],
        diagnostics: Arc::new(
            FileDiagnosticLog::open_global(root.join("registry-logs"), "registry-test", |_| {})
                .unwrap(),
        ),
    })
}
pub fn initial() -> wire::UserSession {
    wire::UserSession {
        id: "old".into(),
        model: "provider/current".into(),
        mode: "build".into(),
        loaded: true,
        working_directory: "/synthetic/workspace".into(),
        ..Default::default()
    }
}
fn session(
    client: WebClient,
    configuration: Arc<Mutex<Configuration>>,
    binding: Box<dyn SlashSessionBinding>,
) -> RpcSlashSession {
    RpcSlashSession::new(client, initial(), configuration, true, binding)
}
fn inert_binding() -> Box<dyn SlashSessionBinding> {
    Box::new(CallbackSessionBinding(Arc::new(|_, _| {
        Box::pin(async { Ok(()) })
    })))
}
fn refresh_noop() -> SkillRefresh {
    Arc::new(|_| Box::pin(async { Ok(()) }))
}

struct NoNavigation;
impl WorkspaceNavigation for NoNavigation {
    fn list<'a>(
        &'a self,
        _: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<wire::ListSessionsResponse, NavigationFailure>> {
        Box::pin(async { panic!("navigation should not run during registry listing") })
    }
    fn load<'a>(
        &'a self,
        _: &'a str,
        _: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<(), NavigationFailure>> {
        Box::pin(async { panic!("navigation should not run during registry listing") })
    }
}

#[tokio::test]
async fn real_factory_registers_source_order_and_terminal_workspace_completion() {
    let mut fixture =
        Fixture::start(|_, _| panic!("registry discovery must not perform RPC")).await;
    let services = SettingsServices {
        client: fixture.client(),
        auth: auth(&fixture.root),
    };
    let diagnostics = services.auth.diagnostics.clone();
    let exits = Arc::new(AtomicUsize::new(0));
    let observed = exits.clone();
    let application: Arc<dyn ApplicationExit> =
        Arc::new(CallbackApplicationExit(Arc::new(move || {
            observed.fetch_add(1, Ordering::SeqCst);
            Box::pin(async { Ok(()) })
        })));
    let registry = SlashCommands::create(
        services.clone(),
        Some(application.clone()),
        "fixture-version",
    );
    let expected = "/auth|/auth-preset-select|/auth-preset-set|/clear|/compact|/effort|/exit|/goal|/standing-instruction|/help|/mode|/model|/model-alias|/model-preset-select|/model-preset-set|/models|/modes|/sessions|/status|/sandbox_enable|/set-context-limit|/skills|/version";
    assert_eq!(
        registry
            .descriptions()
            .iter()
            .map(|command| command.name.as_str())
            .collect::<Vec<_>>()
            .join("|"),
        expected
    );
    assert!(
        registry
            .commands()
            .iter()
            .all(|command| !command.summary().is_empty())
    );
    assert!(registry.find("/workspace-sessions").is_none());
    assert_eq!(
        registry
            .complete("/model-preset-s")
            .iter()
            .map(|command| command.name.as_str())
            .collect::<Vec<_>>(),
        ["/model-preset-select", "/model-preset-set"]
    );
    let terminal = SlashCommands::create_terminal(
        services.clone(),
        Some(application),
        "fixture-version",
        Arc::new(NoNavigation),
    );
    assert_eq!(
        terminal.complete("/workspace-")[0].name,
        "/workspace-sessions"
    );
    let mut completion = SlashCommandCompletion::new();
    completion.refresh("/workspace-", &terminal);
    assert_eq!(
        completion
            .commands()
            .iter()
            .map(|command| command.name.as_str())
            .collect::<Vec<_>>(),
        ["/workspace-sessions"]
    );
    assert!(terminal.find("/workspace-sessions").is_some());
    assert!(
        SlashCommands::create(services, None, "fixture-version")
            .find("/exit")
            .is_none()
    );
    let record = Record::default();
    let mut current = Session::new(record.clone());
    let activity = PollingSlashActivity(Arc::new(|| false));
    let mut dialog = RecordingDialog::default();
    let refresh = refresh(record);
    let mut context = CommandContext {
        session: &mut current,
        activity: &activity,
        dialog: &mut dialog,
        refresh_skills: &refresh,
        commands: &[],
    };
    registry
        .dispatch("/auth sentinel-argument", &mut context, &Stop::new())
        .await
        .unwrap();
    terminal
        .dispatch("/help", &mut context, &Stop::new())
        .await
        .unwrap();
    registry
        .dispatch("/version ignored", &mut context, &Stop::new())
        .await
        .unwrap();
    registry
        .dispatch("/sandbox-enable true", &mut context, &Stop::new())
        .await
        .unwrap();
    registry
        .dispatch("/exit", &mut context, &Stop::new())
        .await
        .unwrap();
    assert_eq!(exits.load(Ordering::SeqCst), 1);
    assert!(
        dialog
            .shown
            .iter()
            .any(|line| line.contains("/workspace-sessions"))
    );
    for name in ["/model-preset-select", "/model-preset-set"] {
        assert!(dialog.shown.iter().any(|line| line.contains(name)));
    }
    assert!(dialog.shown.contains(&"parrot fixture-version".into()));
    assert_eq!(
        dialog.errors,
        ["unknown command /sandbox-enable, try /help"]
    );
    assert!(fixture.calls.lock().unwrap().is_empty());
    let log = std::fs::read_to_string(diagnostics.path()).unwrap();
    assert!(log.contains("interactive_start"));
    assert!(log.contains("interactive_complete"));
    assert!(log.contains("dismissed"));
    assert!(!log.contains("sentinel-argument"));
    fixture.close().await;
}

#[tokio::test]
async fn registry_dispatches_first_exact_token_preserving_argument_tail() {
    let mut fixture = Fixture::start(|_, _| panic!("goal uses supplied session only")).await;
    let registry = SlashCommands::create(
        SettingsServices {
            client: fixture.client(),
            auth: auth(&fixture.root),
        },
        None,
        "version",
    );
    let record = Record::default();
    let mut session = Session::new(record.clone());
    let activity = PollingSlashActivity(Arc::new(|| false));
    let mut dialog = RecordingDialog::default();
    let refresh = refresh(record);
    {
        let mut context = CommandContext {
            session: &mut session,
            activity: &activity,
            dialog: &mut dialog,
            refresh_skills: &refresh,
            commands: &[],
        };
        for entered in [
            "/goal\t \r\n keep  trailing \n",
            "/goal",
            "/standing-instruction\t every agent  ",
            " /goal",
            "/Goal x",
        ] {
            registry
                .dispatch(entered, &mut context, &Stop::new())
                .await
                .unwrap();
        }
    }
    assert_eq!(session.goals, ["keep  trailing \n"]);
    assert_eq!(session.cleared_goals, 1);
    assert_eq!(session.instructions, ["every agent  "]);
    assert_eq!(
        dialog.errors,
        [
            "unknown command , try /help",
            "unknown command /Goal, try /help"
        ]
    );
    fixture.close().await;
}

#[tokio::test]
async fn clear_uses_same_real_model_wizard_and_mode_selection_before_new_binding() {
    let mut fixture = Fixture::start(|method, bytes| match method {
        "list_models" => Ok(wire::ListModelsResponse {
            models: vec![wire::Model {
                id: "model".into(),
                provider_id: "provider".into(),
                ..Default::default()
            }],
            ..Default::default()
        }
        .write_to_bytes()
        .unwrap()),
        "list_modes" => Ok(wire::ListModesResponse {
            modes: vec![wire::Mode {
                id: "query".into(),
                ..Default::default()
            }],
            ..Default::default()
        }
        .write_to_bytes()
        .unwrap()),
        "create_session" => {
            let request = wire::CreateSessionRequest::parse_from_bytes(bytes).unwrap();
            assert_eq!(request.model, "provider/model");
            assert_eq!(request.mode, "query");
            assert!(request.interactive_permissions);
            Ok(wire::UserSession {
                id: "fresh".into(),
                model: request.model,
                mode: request.mode,
                ..Default::default()
            }
            .write_to_bytes()
            .unwrap())
        }
        _ => panic!("unexpected source clear RPC {method}"),
    })
    .await;
    let configuration = configuration(&fixture.root);
    let original_config = std::fs::read(fixture.root.join("config.yaml")).unwrap();
    let bound = Arc::new(Mutex::new(Vec::new()));
    let observed = bound.clone();
    let mut session = session(
        fixture.client(),
        configuration,
        Box::new(CallbackSessionBinding(Arc::new(move |session, _| {
            observed.lock().unwrap().push(session);
            Box::pin(async { Ok(()) })
        }))),
    );
    let registry = SlashCommands::create(
        SettingsServices {
            client: fixture.client(),
            auth: auth(&fixture.root),
        },
        None,
        "version",
    );
    let mut dialog = RecordingDialog::default();
    dialog.selections.extend([
        Some("provider".into()),
        Some("model".into()),
        Some("query".into()),
    ]);
    let activity = PollingSlashActivity(Arc::new(|| false));
    let refresh = refresh_noop();
    registry
        .dispatch(
            "/clear",
            &mut CommandContext {
                session: &mut session,
                activity: &activity,
                dialog: &mut dialog,
                refresh_skills: &refresh,
                commands: &[],
            },
            &Stop::new(),
        )
        .await
        .unwrap();
    assert_eq!(session.id(), "fresh");
    assert_eq!(session.model(), "provider/model");
    assert_eq!(session.mode(), "query");
    assert_eq!(dialog.shown, ["new session fresh"]);
    assert_eq!(bound.lock().unwrap().len(), 1);
    assert_eq!(
        std::fs::read(fixture.root.join("config.yaml")).unwrap(),
        original_config
    );
    assert_eq!(
        fixture
            .calls
            .lock()
            .unwrap()
            .iter()
            .map(|call| call.method)
            .collect::<Vec<_>>(),
        ["list_models", "list_modes", "create_session"]
    );
    fixture.close().await;
}

#[tokio::test]
async fn native_session_forwards_all_current_id_operations_and_persists_selected_model() {
    let mut fixture = Fixture::start(|method, bytes| match method {
        "update_session" => {
            let request = wire::UpdateSessionRequest::parse_from_bytes(bytes).unwrap();
            assert_eq!(request.user_session_id, "old");
            Ok(wire::UserSession {
                id: "old".into(),
                model: if request.model.is_empty() {
                    "canonical-model".into()
                } else {
                    "server-model".into()
                },
                mode: if request.mode.is_empty() {
                    "build".into()
                } else {
                    request.mode
                },
                ..Default::default()
            }
            .write_to_bytes()
            .unwrap())
        }
        "set_model_preset" => Ok(wire::SetModelPresetResponse {
            preset: Some(wire::ModelPreset {
                name: "saved".into(),
                ..Default::default()
            })
            .into(),
            ..Default::default()
        }
        .write_to_bytes()
        .unwrap()),
        "select_model_preset" => Ok(wire::SelectModelPresetResponse {
            session: Some(initial()).into(),
            preset: Some(wire::ModelPreset {
                name: "selected".into(),
                ..Default::default()
            })
            .into(),
            ..Default::default()
        }
        .write_to_bytes()
        .unwrap()),
        "list_model_presets" => Ok(wire::ListModelPresetsResponse {
            presets: vec![wire::ModelPreset {
                name: "listed".into(),
                ..Default::default()
            }],
            ..Default::default()
        }
        .write_to_bytes()
        .unwrap()),
        "set_goal" => Ok(wire::SetGoalResponse::new().write_to_bytes().unwrap()),
        "set_standing_instruction" => Ok(wire::SetStandingInstructionResponse::new()
            .write_to_bytes()
            .unwrap()),
        "compact" => Ok(wire::CompactResponse::new().write_to_bytes().unwrap()),
        "set_context_limit" => Ok(wire::SetContextLimitResponse::new()
            .write_to_bytes()
            .unwrap()),
        "sandbox_enable" => Ok(wire::SandboxEnableResponse {
            enabled: false,
            ..Default::default()
        }
        .write_to_bytes()
        .unwrap()),
        "list_skills" => Ok(wire::ListSkillsResponse::new().write_to_bytes().unwrap()),
        "configure_skill" => Ok(wire::ConfigureSkillResponse {
            skill: Some(wire::Skill {
                path: "exact/path".into(),
                enabled: true,
                ..Default::default()
            })
            .into(),
            ..Default::default()
        }
        .write_to_bytes()
        .unwrap()),
        _ => panic!("unexpected session RPC {method}"),
    })
    .await;
    let configuration = configuration(&fixture.root);
    let mut session = session(fixture.client(), configuration.clone(), inert_binding());
    let stop = Stop::new();
    session
        .select_model("requested-model", &stop)
        .await
        .unwrap();
    assert_eq!(session.model(), "server-model");
    assert_eq!(configuration.lock().unwrap().model, "server-model");
    session.select_mode("plan", &stop).await.unwrap();
    assert_eq!(session.mode(), "plan");
    assert_eq!(configuration.lock().unwrap().model, "server-model");
    assert_eq!(
        session.set_model_preset("save", &stop).await.unwrap().name,
        "saved"
    );
    assert_eq!(
        session
            .select_model_preset("select", &stop)
            .await
            .unwrap()
            .name,
        "selected"
    );
    assert_eq!(
        session.list_model_presets(&stop).await.unwrap()[0].name,
        "listed"
    );
    session.set_goal(" exact goal ", &stop).await.unwrap();
    session.clear_goal(&stop).await.unwrap();
    session
        .set_standing_instruction("instruction", &stop)
        .await
        .unwrap();
    session.compact(Some("20%"), &stop).await.unwrap();
    session.set_context_limit("100k", &stop).await.unwrap();
    assert!(!session.sandbox_enable(true, &stop).await.unwrap().enabled);
    session.list_skills(&stop).await.unwrap();
    assert!(
        session
            .configure_skill("exact/path", true, &stop)
            .await
            .unwrap()
            .enabled
    );
    let calls = fixture.calls.lock().unwrap().clone();
    let goals: Vec<_> = calls
        .iter()
        .filter(|call| call.method == "set_goal")
        .map(|call| wire::SetGoalRequest::parse_from_bytes(&call.body).unwrap())
        .collect();
    assert_eq!(goals[0].user_session_id, "old");
    assert_eq!(goals[0].goal(), " exact goal ");
    assert!(goals[1].has_clear());
    let compact = calls.iter().find(|call| call.method == "compact").unwrap();
    let compact = wire::CompactRequest::parse_from_bytes(&compact.body).unwrap();
    assert_eq!(compact.user_session_id, "old");
    assert_eq!(compact.target_context_size, "20%");
    let skill = calls
        .iter()
        .find(|call| call.method == "configure_skill")
        .unwrap();
    let skill = wire::ConfigureSkillRequest::parse_from_bytes(&skill.body).unwrap();
    assert_eq!(skill.user_session_id, "old");
    assert_eq!(skill.path, "exact/path");
    assert!(skill.enabled);
    fixture.close().await;
}

struct ExistingBinding {
    commit: bool,
    fail: bool,
}
impl ExistingSessionBinding for ExistingBinding {
    fn bind<'a>(
        &'a mut self,
        commit: &'a mut (dyn FnMut() + Send),
        _: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<(), SlashFailure>> {
        Box::pin(async move {
            if self.commit {
                commit();
            }
            if self.fail {
                Err(SlashFailure::Command("bind failed".into()))
            } else {
                Ok(())
            }
        })
    }
}
#[tokio::test]
async fn native_session_new_and_existing_binding_failures_restore_complete_previous_session() {
    let mut fixture = Fixture::start(|method, _| {
        assert_eq!(method, "create_session");
        Ok(wire::UserSession {
            id: "replacement".into(),
            ..Default::default()
        }
        .write_to_bytes()
        .unwrap())
    })
    .await;
    let mut session = session(
        fixture.client(),
        configuration(&fixture.root),
        Box::new(CallbackSessionBinding(Arc::new(|_, _| {
            Box::pin(async { Err(SlashFailure::Command("bind failed".into())) })
        }))),
    );
    let previous = session.current_session().clone();
    let stop = Stop::new();
    assert!(session.start_new("model", "mode", &stop).await.is_err());
    assert_eq!(*session.current_session(), previous);
    for commit in [false, true] {
        assert!(
            session
                .load_existing(
                    wire::UserSession {
                        id: "loaded".into(),
                        ..Default::default()
                    },
                    &mut ExistingBinding { commit, fail: true },
                    &stop
                )
                .await
                .is_err()
        );
        assert_eq!(*session.current_session(), previous);
    }
    session
        .load_existing(
            wire::UserSession {
                id: "loaded".into(),
                ..Default::default()
            },
            &mut ExistingBinding {
                commit: false,
                fail: false,
            },
            &stop,
        )
        .await
        .unwrap();
    assert_eq!(*session.current_session(), previous);
    session
        .load_existing(
            wire::UserSession {
                id: "loaded".into(),
                ..Default::default()
            },
            &mut ExistingBinding {
                commit: true,
                fail: false,
            },
            &stop,
        )
        .await
        .unwrap();
    assert_eq!(session.id(), "loaded");
    fixture.close().await;
}

#[tokio::test]
async fn dropping_pending_new_binding_restores_session_without_discarding_previous_state() {
    let mut fixture = Fixture::start(|_, _| {
        Ok(wire::UserSession {
            id: "replacement".into(),
            ..Default::default()
        }
        .write_to_bytes()
        .unwrap())
    })
    .await;
    let mut session = session(
        fixture.client(),
        configuration(&fixture.root),
        Box::new(CallbackSessionBinding(Arc::new(|_, _| {
            Box::pin(std::future::pending())
        }))),
    );
    let previous = session.current_session().clone();
    assert!(
        tokio::time::timeout(
            Duration::from_millis(30),
            session.start_new("model", "mode", &Stop::new())
        )
        .await
        .is_err()
    );
    assert_eq!(*session.current_session(), previous);
    assert_eq!(fixture.calls.lock().unwrap().len(), 1);
    fixture.close().await;
}

#[tokio::test]
async fn actual_skill_catalog_rpc_uses_selected_session_and_clears_on_failure_and_cancellation() {
    let mut fixture = Fixture::start(|method, bytes| {
        assert_eq!(method, "list_skills");
        let request = wire::ListSkillsRequest::parse_from_bytes(bytes).unwrap();
        if request.user_session_id == "fail" {
            return Err(Status::unavailable("skills offline"));
        }
        Ok(wire::ListSkillsResponse {
            skills: vec![wire::Skill {
                name: request.user_session_id,
                enabled: true,
                ..Default::default()
            }],
            ..Default::default()
        }
        .write_to_bytes()
        .unwrap())
    })
    .await;
    let mut completion = SkillCompletion::new(fixture.client());
    let stop = Stop::new();
    for id in ["one", "two"] {
        completion.refresh_catalog(id, &stop).await.unwrap();
        completion.refresh(&parrot_cli::terminal_input::PromptState {
            prefix: String::new(),
            text: "$".into(),
            cursor: 1,
        });
        assert_eq!(completion.skills()[0].name, id);
    }
    completion.refresh_catalog("fail", &stop).await.unwrap();
    completion.refresh(&parrot_cli::terminal_input::PromptState {
        prefix: String::new(),
        text: "$".into(),
        cursor: 1,
    });
    assert!(completion.skills().is_empty());
    let stopped = Stop::new();
    stopped.cancel();
    completion
        .refresh_catalog("cancelled", &stopped)
        .await
        .unwrap();
    assert!(completion.skills().is_empty());
    {
        let calls = fixture.calls.lock().unwrap();
        assert_eq!(
            calls
                .iter()
                .map(|call| wire::ListSkillsRequest::parse_from_bytes(&call.body)
                    .unwrap()
                    .user_session_id)
                .collect::<Vec<_>>(),
            ["one", "two", "fail"]
        );
    }
    fixture.close().await;
}

#[tokio::test]
async fn cancelled_exit_never_requests_shutdown_or_mutates_session() {
    let exits = Arc::new(AtomicUsize::new(0));
    let observed = exits.clone();
    let application = CallbackApplicationExit(Arc::new(move || {
        observed.fetch_add(1, Ordering::SeqCst);
        Box::pin(async { Ok(()) })
    }));
    let stop = Stop::new();
    stop.cancel();
    assert!(
        matches!(application.exit(&stop).await,Err(SlashFailure::Rpc(status))if status.code()==Code::Cancelled)
    );
    assert_eq!(exits.load(Ordering::SeqCst), 0);
}
