use super::support::{Activity, Dialog, NEVER, RpcFixture, RpcState, Session, Temp};
use parrot_cli::{
    slash_sessions::{CommandContext, SkillRefresh, SlashCommandRegistry},
    slash_settings::{AuthServices, SettingsServices, commands},
};
use parrot_domain::contracts::async_ops::{
    BoxFuture, Cancellation, OperationError, OperationResult,
};
use parrot_foundation::{
    credentials::{CredentialPresets, CredentialStore, OAuthCredential},
    oauth::{AuthError, DeviceAuthorization, OAuthClient},
};
use parrot_storage::diagnostics::FileDiagnosticLog;
use std::{sync::Arc, time::SystemTime};

struct NoOAuth;
impl OAuthClient for NoOAuth {
    fn now(&self) -> SystemTime {
        SystemTime::UNIX_EPOCH
    }
    fn authorization_url(&self, _: &str, _: &str, _: &str) -> String {
        panic!("registration fixture must not authorize")
    }
    fn browser_login<'a>(
        &'a self,
        _: &'a dyn Cancellation,
    ) -> BoxFuture<'a, OperationResult<OAuthCredential, AuthError>> {
        Box::pin(async {
            Err(OperationError::Failed(AuthError::NotSupported(
                "unused registration OAuth",
            )))
        })
    }
    fn start_device_authorization<'a>(
        &'a self,
        _: &'a dyn Cancellation,
    ) -> BoxFuture<'a, OperationResult<DeviceAuthorization, AuthError>> {
        Box::pin(async {
            Err(OperationError::Failed(AuthError::NotSupported(
                "unused registration OAuth",
            )))
        })
    }
    fn await_device_authorization<'a>(
        &'a self,
        _: &'a DeviceAuthorization,
        _: &'a dyn Cancellation,
    ) -> BoxFuture<'a, OperationResult<OAuthCredential, AuthError>> {
        Box::pin(async {
            Err(OperationError::Failed(AuthError::NotSupported(
                "unused registration OAuth",
            )))
        })
    }
    fn refresh<'a>(
        &'a self,
        _: &'a OAuthCredential,
        _: &'a dyn Cancellation,
    ) -> BoxFuture<'a, OperationResult<OAuthCredential, AuthError>> {
        Box::pin(async {
            Err(OperationError::Failed(AuthError::NotSupported(
                "unused registration OAuth",
            )))
        })
    }
}

#[tokio::test]
async fn real_settings_handlers_register_exact_source_spellings_and_dispatch_through_same_registry()
{
    let fixture = RpcFixture::new(RpcState::default()).await;
    let temp = Temp::new();
    let auth = Arc::new(AuthServices {
        credentials: Arc::new(CredentialStore::at_selected_path(
            temp.path.join("parrotrust/credentials.json"),
        )),
        presets: Arc::new(CredentialPresets::at_selected_config_directory(
            temp.path.join("parrotrust"),
        )),
        oauth: Arc::new(NoOAuth),
        provider_ids: vec![],
        diagnostics: Arc::new(
            FileDiagnosticLog::open_global(temp.path.join("logs"), "registration", |_| {}).unwrap(),
        ),
    });
    let registry = SlashCommandRegistry::new(commands(SettingsServices {
        client: fixture.client.clone(),
        auth,
    }));
    let expected = [
        ("/auth", "Manage credentials"),
        (
            "/auth-preset-select",
            "Replace all stored credentials with a saved preset",
        ),
        (
            "/auth-preset-set",
            "Save all stored credentials as a named preset",
        ),
        ("/effort", "Switch the model effort for this session"),
        ("/model", "Switch the model for this session"),
        ("/model-alias", "Configure a model alias"),
        (
            "/model-preset-select",
            "Select a saved model selection preset",
        ),
        (
            "/model-preset-set",
            "Save the current model selection and aliases as a preset",
        ),
        ("/models", "List the models the provider serves"),
        (
            "/set-context-limit",
            "Set the automatic context compaction trigger",
        ),
    ];
    let descriptions = registry.descriptions();
    assert_eq!(
        descriptions
            .iter()
            .map(|row| (row.name.as_str(), row.summary.as_str()))
            .collect::<Vec<_>>(),
        expected
    );
    for (name, _) in expected {
        assert!(registry.find(name).is_some());
    }
    assert!(registry.find("/MODEL").is_none());
    assert_eq!(
        registry
            .complete("/model")
            .iter()
            .map(|row| row.name.as_str())
            .collect::<Vec<_>>(),
        [
            "/model",
            "/model-alias",
            "/model-preset-select",
            "/model-preset-set",
            "/models"
        ]
    );
    let mut session = Session::new("high_llm");
    let activity = Activity::default();
    let mut dialog = Dialog::default();
    let refresh: SkillRefresh = Arc::new(|_| Box::pin(async { Ok(()) }));
    let mut context = CommandContext {
        session: &mut session,
        activity: &activity,
        dialog: &mut dialog,
        refresh_skills: &refresh,
        commands: &[],
    };
    registry
        .dispatch("/set-context-limit\t100k", &mut context, &NEVER)
        .await
        .unwrap();
    registry
        .dispatch("/model-preset-set Work", &mut context, &NEVER)
        .await
        .unwrap();
    registry
        .dispatch("/models", &mut context, &NEVER)
        .await
        .unwrap();
    assert_eq!(session.context_limits, ["100k"]);
    assert_eq!(session.saved_presets, ["Work"]);
    assert_eq!(
        dialog.shown,
        [
            "Context compaction limit set to 100k",
            "Model preset saved: Work = high_llm",
            "no providers are configured"
        ]
    );
    fixture.close().await;
}
