use std::sync::{Arc, Mutex};

use parrot_cli::{
    slash_sessions::*,
    slash_settings::{ModelWizard, SettingsServices},
};
use parrot_domain::{
    contracts::async_ops::{BoxFuture, Cancellation},
    protocol::messages::parrot as wire,
};
use protobuf::Message;

use crate::{
    control_fixtures::{Activity, Record, Session, refresh},
    integration::{auth, configuration, initial},
    native_fixture::{Fixture, RecordingDialog, Stop},
};

struct Command {
    name: &'static str,
    arguments: Arc<Mutex<Vec<String>>>,
}
impl SlashCommand for Command {
    fn name(&self) -> &str {
        self.name
    }
    fn summary(&self) -> &str {
        "test command"
    }
    fn run<'a>(
        &'a self,
        args: &'a str,
        _: &'a mut CommandContext<'_>,
        _: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<(), SlashFailure>> {
        Box::pin(async move {
            self.arguments.lock().unwrap().push(args.into());
            Ok(())
        })
    }
}
#[test]
fn direct_registry_completes_first_token_in_ordinal_order() {
    let registry = SlashCommandRegistry::new(
        ["/model", "/mode", "/exit"]
            .into_iter()
            .map(|name| {
                Box::new(Command {
                    name,
                    arguments: Arc::default(),
                }) as Box<dyn SlashCommand>
            })
            .collect(),
    );
    for (entered, expected) in [
        ("/", vec!["/exit", "/mode", "/model"]),
        ("/mo", vec!["/mode", "/model"]),
        ("/mo ignored", vec!["/mode", "/model"]),
        ("normal prompt", vec![]),
        ("/unknown", vec![]),
    ] {
        assert_eq!(
            registry
                .complete(entered)
                .iter()
                .map(|command| command.name.as_str())
                .collect::<Vec<_>>(),
            expected
        );
    }
}
#[tokio::test]
async fn direct_registry_preserves_command_identity_and_exact_normalized_arguments() {
    let arguments = Arc::new(Mutex::new(Vec::new()));
    let command = Box::new(Command {
        name: "/test",
        arguments: arguments.clone(),
    });
    let address = &*command as *const Command as *const ();
    let registry = SlashCommandRegistry::new(vec![command]);
    let record = Record::default();
    let mut session = Session::new(record.clone());
    let activity = Activity::new(record.clone());
    let refresh = refresh(record);
    let mut dialog = RecordingDialog::default();
    let stop = Stop::new();
    {
        let mut context = CommandContext {
            session: &mut session,
            activity: &activity,
            dialog: &mut dialog,
            refresh_skills: &refresh,
            commands: &[],
        };
        for entered in [
            "/test",
            "/test   spaces",
            "/test\t\t tabs",
            "/test multiword remainder",
            "/unknown arguments",
        ] {
            registry
                .dispatch(entered, &mut context, &stop)
                .await
                .unwrap();
        }
    }
    assert_eq!(
        *arguments.lock().unwrap(),
        ["", "spaces", "tabs", "multiword remainder"]
    );
    assert_eq!(
        registry.find("/test").unwrap() as *const dyn SlashCommand as *const (),
        address
    );
    assert!(registry.find("/unknown").is_none());
    assert_eq!(dialog.errors, ["unknown command /unknown, try /help"]);
}
#[tokio::test]
async fn empty_real_model_and_mode_catalogs_report_in_order_without_opening_picker() {
    let mut fixture = Fixture::start(|method, _| match method {
        "list_models" => Ok(wire::ListModelsResponse::new().write_to_bytes().unwrap()),
        "list_modes" => Ok(wire::ListModesResponse::new().write_to_bytes().unwrap()),
        _ => panic!("unexpected empty catalog RPC {method}"),
    })
    .await;
    let mut dialog = RecordingDialog::default();
    let stop = Stop::new();
    assert!(
        ModelWizard::new(fixture.client())
            .select(None, &mut dialog, &stop)
            .await
            .unwrap()
            .is_none()
    );
    assert!(
        ModeSelection::new(fixture.client())
            .select(&mut dialog, &stop)
            .await
            .unwrap()
            .is_none()
    );
    assert_eq!(
        dialog.errors,
        ["no providers are configured", "no modes are available"]
    );
    assert!(dialog.pickers.is_empty());
    assert_eq!(fixture.calls.lock().unwrap().len(), 2);
    fixture.close().await;
}
#[tokio::test]
async fn mode_clear_and_modes_use_shared_activity_and_actual_replacement() {
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
            modes: ["build", "plan", "query"]
                .into_iter()
                .map(|id| wire::Mode {
                    id: id.into(),
                    ..Default::default()
                })
                .collect(),
            ..Default::default()
        }
        .write_to_bytes()
        .unwrap()),
        "update_session" => {
            let request = wire::UpdateSessionRequest::parse_from_bytes(bytes).unwrap();
            assert_eq!(request.mode, "plan");
            Ok(wire::UserSession {
                mode: request.mode,
                ..initial()
            }
            .write_to_bytes()
            .unwrap())
        }
        "create_session" => {
            let request = wire::CreateSessionRequest::parse_from_bytes(bytes).unwrap();
            Ok(wire::UserSession {
                id: "session-1".into(),
                model: request.model,
                mode: request.mode,
                ..Default::default()
            }
            .write_to_bytes()
            .unwrap())
        }
        _ => panic!("unexpected combined command RPC {method}"),
    })
    .await;
    let configuration = configuration(&fixture.root);
    let mut session = RpcSlashSession::new(
        fixture.client(),
        initial(),
        configuration,
        true,
        Box::new(CallbackSessionBinding(Arc::new(|_, _| {
            Box::pin(async { Ok(()) })
        }))),
    );
    let registry = SlashCommands::create(
        SettingsServices {
            client: fixture.client(),
            auth: auth(&fixture.root),
        },
        None,
        "fixture",
    );
    let record = Record::default();
    let activity = Activity::new(record.clone());
    let refresh = refresh(record.clone());
    let stop = Stop::new();
    let mut mode_dialog = RecordingDialog::default();
    mode_dialog.selections.push_back(Some("plan".into()));
    registry
        .dispatch(
            "/mode",
            &mut CommandContext {
                session: &mut session,
                activity: &activity,
                dialog: &mut mode_dialog,
                refresh_skills: &refresh,
                commands: &[],
            },
            &stop,
        )
        .await
        .unwrap();
    let mut clear_dialog = RecordingDialog::default();
    clear_dialog.selections.extend([
        Some("provider".into()),
        Some("model".into()),
        Some("query".into()),
    ]);
    registry
        .dispatch(
            "/clear",
            &mut CommandContext {
                session: &mut session,
                activity: &activity,
                dialog: &mut clear_dialog,
                refresh_skills: &refresh,
                commands: &[],
            },
            &stop,
        )
        .await
        .unwrap();
    registry
        .dispatch(
            "/modes",
            &mut CommandContext {
                session: &mut session,
                activity: &activity,
                dialog: &mut mode_dialog,
                refresh_skills: &refresh,
                commands: &[],
            },
            &stop,
        )
        .await
        .unwrap();
    assert_eq!(session.id(), "session-1");
    assert_eq!(session.model(), "provider/model");
    assert_eq!(session.mode(), "query");
    assert_eq!(
        record
            .events()
            .iter()
            .filter(|event| event.as_str() == "wait")
            .count(),
        2
    );
    record.assert_same_token(&stop);
    for line in ["mode is now plan", "build", "query"] {
        assert!(mode_dialog.shown.contains(&line.into()));
    }
    assert_eq!(clear_dialog.shown, ["new session session-1"]);
    fixture.close().await;
}
