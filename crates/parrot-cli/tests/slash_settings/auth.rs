use std::{
    collections::HashMap,
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::{Duration, SystemTime},
};

use parrot_cli::{
    slash_sessions::SlashFailure,
    slash_settings::auth::{AuthCommand, AuthServices},
};
use parrot_domain::contracts::async_ops::{
    BoxFuture, Cancellation, OperationError, OperationResult,
};
use parrot_foundation::{
    credentials::{Credential, CredentialPresets, CredentialStore, OAuthCredential, Secret},
    oauth::{AuthError, DeviceAuthorization, OAuthClient},
    paths::ApplicationRoots,
};
use parrot_storage::diagnostics::FileDiagnosticLog;
use tonic::Code;

use super::support::{Activity, Dialog, NEVER, Stop, Temp, run};

pub(super) struct Fixture {
    pub services: Arc<AuthServices>,
    pub legacy: PathBuf,
    pub oauth: Arc<FakeOAuth>,
    _temp: Temp,
}
impl Fixture {
    pub fn new(providers: &[&str], failure: Option<AuthError>) -> Self {
        let temp = Temp::new();
        let roots = ApplicationRoots::resolve(&HashMap::from([
            ("HOME".into(), temp.path.to_string_lossy().into_owned()),
            (
                "XDG_CONFIG_HOME".into(),
                temp.path.join("config").to_string_lossy().into_owned(),
            ),
        ]));
        let legacy = roots.legacy().config().to_path_buf();
        std::fs::create_dir_all(legacy.join("credential_presets")).unwrap();
        std::fs::write(
            legacy.join("credentials.json"),
            "legacy-credential-sentinel",
        )
        .unwrap();
        std::fs::write(
            legacy.join("credential_presets/old.json"),
            "legacy-preset-sentinel",
        )
        .unwrap();
        let oauth = Arc::new(FakeOAuth {
            failure,
            calls: AtomicUsize::new(0),
        });
        let services = Arc::new(AuthServices {
            credentials: Arc::new(CredentialStore::at_selected_path(
                roots.current().config().join("credentials.json"),
            )),
            presets: Arc::new(CredentialPresets::at_selected_config_directory(
                roots.current().config(),
            )),
            oauth: oauth.clone(),
            provider_ids: providers.iter().map(|id| (*id).into()).collect(),
            diagnostics: Arc::new(
                FileDiagnosticLog::open_global(temp.path.join("logs"), "auth", |_| {}).unwrap(),
            ),
        });
        Self {
            services,
            legacy,
            oauth,
            _temp: temp,
        }
    }
    pub fn diagnostics(&self) -> String {
        std::fs::read_to_string(self.services.diagnostics.path()).unwrap()
    }
    pub fn assert_legacy_untouched(&self) {
        assert_eq!(
            std::fs::read_to_string(self.legacy.join("credentials.json")).unwrap(),
            "legacy-credential-sentinel"
        );
        assert_eq!(
            std::fs::read_to_string(self.legacy.join("credential_presets/old.json")).unwrap(),
            "legacy-preset-sentinel"
        );
    }
    pub fn command(&self) -> AuthCommand {
        AuthCommand::new(self.services.clone())
    }
}

pub(super) struct FakeOAuth {
    failure: Option<AuthError>,
    calls: AtomicUsize,
}
impl FakeOAuth {
    fn reply(
        &self,
        cancellation: &dyn Cancellation,
    ) -> OperationResult<OAuthCredential, AuthError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        if cancellation.is_cancelled() {
            return Err(OperationError::Cancelled);
        }
        if let Some(failure) = &self.failure {
            return Err(OperationError::Failed(failure.clone()));
        }
        Ok(OAuthCredential::create(
            "private-access",
            "private-refresh",
            "1970-01-01T00:00:00+00:00",
            "private-account",
        ))
    }
}
impl OAuthClient for FakeOAuth {
    fn now(&self) -> SystemTime {
        SystemTime::UNIX_EPOCH
    }
    fn authorization_url(&self, _: &str, _: &str, _: &str) -> String {
        panic!("fake authorization never opens a URL")
    }
    fn browser_login<'a>(
        &'a self,
        cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, OperationResult<OAuthCredential, AuthError>> {
        Box::pin(async move { self.reply(cancellation) })
    }
    fn start_device_authorization<'a>(
        &'a self,
        cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, OperationResult<DeviceAuthorization, AuthError>> {
        Box::pin(async move {
            self.calls.fetch_add(1, Ordering::SeqCst);
            if cancellation.is_cancelled() {
                return Err(OperationError::Cancelled);
            }
            Ok(DeviceAuthorization {
                device_auth_id: Secret::new("private-device"),
                user_code: Secret::new("sentinel-code"),
                verification_url: "https://example.invalid/authorize".into(),
                interval: Duration::from_secs(1),
                expires_at: SystemTime::UNIX_EPOCH + Duration::from_secs(600),
            })
        })
    }
    fn await_device_authorization<'a>(
        &'a self,
        _: &'a DeviceAuthorization,
        cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, OperationResult<OAuthCredential, AuthError>> {
        Box::pin(async move { self.reply(cancellation) })
    }
    fn refresh<'a>(
        &'a self,
        _: &'a OAuthCredential,
        _: &'a dyn Cancellation,
    ) -> BoxFuture<'a, OperationResult<OAuthCredential, AuthError>> {
        Box::pin(async { panic!("slash login never refreshes") })
    }
}

#[tokio::test]
async fn s3_empty_keys_do_not_persist_partial_credentials() {
    for (access, secret) in [("", "unused"), ("access", "")] {
        let fixture = Fixture::new(&[], None);
        let mut dialog = Dialog::default()
            .selects([Some("image-upload")])
            .secrets([Some(access), Some(secret)]);
        run(
            &fixture.command(),
            "ignored",
            &mut dialog,
            &Activity::default(),
            &NEVER,
        )
        .await
        .unwrap();
        assert!(
            fixture
                .services
                .credentials
                .list(&NEVER)
                .await
                .unwrap()
                .is_empty()
        );
        assert!(!fixture.services.credentials.path().exists());
        assert_eq!(dialog.errors, ["nothing entered"]);
        assert!(fixture.diagnostics().contains("outcome=\"empty_input\""));
        fixture.assert_legacy_untouched();
    }
}

#[tokio::test]
async fn s3_stores_both_trimmed_redacted_keys_only_in_new_root() {
    let fixture = Fixture::new(&[], None);
    let mut dialog = Dialog::default()
        .selects([Some("image-upload")])
        .secrets([Some(" access-value "), Some(" secret-value ")]);
    run(
        &fixture.command(),
        "",
        &mut dialog,
        &Activity::default(),
        &NEVER,
    )
    .await
    .unwrap();
    let credential = fixture
        .services
        .credentials
        .get("image_upload", &NEVER)
        .await
        .unwrap()
        .unwrap();
    let s3 = credential.s3.as_ref().unwrap();
    assert_eq!(s3.access_key.value(), "access-value");
    assert_eq!(s3.secret_key.value(), "secret-value");
    assert_eq!(dialog.shown, ["stored a credential for image_upload"]);
    let display = format!(
        "{}{}{:?}",
        dialog.shown.join("|"),
        fixture.diagnostics(),
        credential
    );
    assert!(!display.contains("access-value"));
    assert!(!display.contains("secret-value"));
    fixture.assert_legacy_untouched();
}

#[tokio::test]
async fn api_key_login_list_logout_preserve_menu_messages_and_diagnostic_privacy() {
    let fixture = Fixture::new(&["provider"], None);
    let mut login = Dialog::default()
        .selects([Some("login"), Some("provider")])
        .secrets([Some("  private-key  ")]);
    run(
        &fixture.command(),
        "sentinel-argument",
        &mut login,
        &Activity::default(),
        &NEVER,
    )
    .await
    .unwrap();
    assert_eq!(
        fixture
            .services
            .credentials
            .get("provider", &NEVER)
            .await
            .unwrap()
            .unwrap(),
        Credential::for_api_key("private-key")
    );
    assert_eq!(login.pickers[0].0, "Authentication");
    assert_eq!(
        login.pickers[0]
            .1
            .iter()
            .map(|option| (&*option.id, &*option.label, &*option.description))
            .collect::<Vec<_>>(),
        [
            ("login", "Login", "Store a provider credential"),
            (
                "image-upload",
                "Image upload",
                "Store S3-compatible storage credentials"
            ),
            ("list", "List", "List stored credentials"),
            ("logout", "Logout", "Remove a stored credential"),
        ]
    );
    assert_eq!(login.pickers[1].1[0].description, "Provider credential");
    assert_eq!(login.shown, ["stored a credential for provider"]);
    let mut list = Dialog::default().selects([Some("list")]);
    run(
        &fixture.command(),
        "",
        &mut list,
        &Activity::default(),
        &NEVER,
    )
    .await
    .unwrap();
    assert_eq!(list.shown, ["provider"]);
    assert_eq!(list.loads, ["Loading credentials…"]);
    let mut logout = Dialog::default().selects([Some("logout"), Some("provider")]);
    run(
        &fixture.command(),
        "",
        &mut logout,
        &Activity::default(),
        &NEVER,
    )
    .await
    .unwrap();
    assert_eq!(logout.shown, ["removed the credential for provider"]);
    assert_eq!(logout.pickers[1].0, "Remove a credential");
    assert_eq!(logout.pickers[1].1[0].description, "Stored credential");
    assert!(
        fixture
            .services
            .credentials
            .get("provider", &NEVER)
            .await
            .unwrap()
            .is_none()
    );
    let log = fixture.diagnostics();
    for operation in [
        "login_start",
        "login_complete",
        "list_start",
        "list_complete",
        "logout_start",
        "logout_complete",
    ] {
        assert!(log.contains(operation), "{log}");
    }
    assert!(log.contains("duration_ms="));
    assert!(log.contains("correlation="));
    for secret in ["private-key", "provider", "sentinel"] {
        assert!(!log.contains(secret), "{log}");
    }
    assert_eq!(fixture.oauth.calls.load(Ordering::SeqCst), 0);
    fixture.assert_legacy_untouched();
}

#[tokio::test]
async fn dismissed_and_empty_auth_wizards_record_exact_outcomes() {
    for (stage, outcome) in [
        ("interactive", "dismissed"),
        ("login", "dismissed"),
        ("oauth", "dismissed"),
        ("empty_input", "empty_input"),
        ("no_providers", "no_providers"),
    ] {
        let providers: &[&str] = if stage == "no_providers" {
            &[]
        } else {
            &["sentinel-provider", "chatgpt"]
        };
        let fixture = Fixture::new(providers, None);
        let mut dialog = match stage {
            "interactive" => Dialog::default().selects([None]),
            "login" => Dialog::default().selects([Some("login"), None]),
            "oauth" => Dialog::default().selects([Some("login"), Some("chatgpt"), None]),
            "empty_input" => Dialog::default()
                .selects([Some("login"), Some("sentinel-provider")])
                .secrets([Some(" ")]),
            _ => Dialog::default().selects([Some("login")]),
        };
        run(
            &fixture.command(),
            "sentinel-argument",
            &mut dialog,
            &Activity::default(),
            &NEVER,
        )
        .await
        .unwrap();
        let log = fixture.diagnostics();
        assert!(log.contains("interactive_start") && log.contains("interactive_complete"));
        assert!(log.contains(&format!("outcome=\"{outcome}\"")) && log.contains("duration_ms="));
        assert!(!log.contains("sentinel"));
        if stage == "login" || stage == "oauth" {
            assert!(
                log.contains(&format!("{stage}_start"))
                    && log.contains(&format!("{stage}_complete"))
            );
        }
        assert!(!fixture.services.credentials.path().exists());
        fixture.assert_legacy_untouched();
    }
}

#[tokio::test]
async fn oauth_fake_browser_and_device_success_and_auth_failure_do_not_leak() {
    for device in [false, true] {
        for fail in [false, true] {
            let fixture = Fixture::new(
                &["chatgpt"],
                fail.then_some(AuthError::Message("sentinel-failure")),
            );
            let mut dialog = Dialog::default().selects([
                Some("login"),
                Some("chatgpt"),
                Some(if device { "device" } else { "browser" }),
            ]);
            run(
                &fixture.command(),
                "sentinel-argument",
                &mut dialog,
                &Activity::default(),
                &NEVER,
            )
            .await
            .unwrap();
            let log = fixture.diagnostics();
            assert_eq!(dialog.load_failures.len(), usize::from(fail));
            if fail {
                assert_eq!(dialog.load_failures[0].code(), Code::Internal);
                assert_eq!(dialog.load_failures[0].message(), "sentinel-failure");
            }
            assert!(log.contains("oauth_start") && log.contains("oauth_complete"));
            assert!(log.contains(if fail {
                "outcome=\"auth_failure\""
            } else {
                "outcome=\"completed\""
            }));
            for secret in [
                "sentinel",
                "chatgpt",
                "private-access",
                "private-refresh",
                "private-account",
                "private-device",
            ] {
                assert!(!log.contains(secret), "{log}");
            }
            if fail {
                assert_eq!(dialog.errors, ["sentinel-failure"]);
                assert!(log.contains("error=\"auth\""));
                assert!(!fixture.services.credentials.path().exists());
            } else {
                assert!(dialog.errors.is_empty());
                assert!(
                    fixture
                        .services
                        .credentials
                        .get("chatgpt", &NEVER)
                        .await
                        .unwrap()
                        .is_some()
                );
            }
            if device {
                assert_eq!(
                    dialog.confirmed,
                    [vec![
                        "Visit https://example.invalid/authorize and enter code sentinel-code"
                            .to_string()
                    ]]
                );
                assert_eq!(dialog.loads, ["Waiting for authorization…"]);
            } else {
                assert_eq!(
                    dialog.confirmed,
                    [vec!["Opening your browser to authorize...".to_string()]]
                );
                assert_eq!(dialog.loads, ["Waiting for browser authorization…"]);
            }
            fixture.assert_legacy_untouched();
        }
    }
}

#[tokio::test]
async fn oauth_declined_confirmation_never_stores_or_runs_authorization_wait() {
    for device in [false, true] {
        let fixture = Fixture::new(&["chatgpt"], None);
        let mut dialog = Dialog::default()
            .selects([
                Some("login"),
                Some("chatgpt"),
                Some(if device { "device" } else { "browser" }),
            ])
            .confirmations([false]);
        run(
            &fixture.command(),
            "",
            &mut dialog,
            &Activity::default(),
            &NEVER,
        )
        .await
        .unwrap();
        assert!(!fixture.services.credentials.path().exists());
        assert!(dialog.loads.is_empty());
        assert_eq!(
            fixture.oauth.calls.load(Ordering::SeqCst),
            usize::from(device)
        );
        assert!(fixture.diagnostics().contains("outcome=\"dismissed\""));
    }
}

#[tokio::test]
async fn auth_actual_cancellation_propagates_rpc_cancelled_and_logs_no_payload() {
    let fixture = Fixture::new(&[], None);
    let stopping = Stop::new();
    stopping.cancel();
    let mut dialog = Dialog::default().selects([Some("list")]);
    let failure = run(
        &fixture.command(),
        "sentinel-argument",
        &mut dialog,
        &Activity::default(),
        &stopping,
    )
    .await
    .unwrap_err();
    assert!(matches!(failure, SlashFailure::Rpc(status) if status.code() == Code::Cancelled));
    let log = fixture.diagnostics();
    assert!(log.contains("interactive_failure") && log.contains("error=\"cancelled\""));
    assert!(!log.contains("sentinel"));
    assert!(!fixture.services.credentials.path().exists());
}

#[tokio::test]
async fn generic_oauth_failure_is_not_swallowed_as_auth_failure() {
    let fixture = Fixture::new(
        &["chatgpt"],
        Some(AuthError::NotSupported("sentinel-generic")),
    );
    let mut dialog = Dialog::default().selects([Some("login"), Some("chatgpt"), Some("browser")]);
    let failure = run(
        &fixture.command(),
        "",
        &mut dialog,
        &Activity::default(),
        &NEVER,
    )
    .await
    .unwrap_err();
    assert!(matches!(failure, SlashFailure::Command(message) if message == "sentinel-generic"));
    assert!(dialog.errors.is_empty());
    assert_eq!(dialog.load_failures.len(), 1);
    assert_eq!(dialog.load_failures[0].code(), Code::Internal);
    assert_eq!(dialog.load_failures[0].message(), "sentinel-generic");
    let log = fixture.diagnostics();
    for operation in ["oauth_failure", "login_failure", "interactive_failure"] {
        assert!(log.contains(operation));
    }
    assert!(log.contains("error=\"unexpected\""));
    assert!(!log.contains("sentinel"));
    assert!(!fixture.services.credentials.path().exists());
}

#[tokio::test]
async fn empty_list_logout_and_dismissed_logout_do_not_delete_credentials() {
    let fixture = Fixture::new(&[], None);
    for action in ["list", "logout"] {
        let mut dialog = Dialog::default().selects([Some(action)]);
        run(
            &fixture.command(),
            "",
            &mut dialog,
            &Activity::default(),
            &NEVER,
        )
        .await
        .unwrap();
        assert_eq!(dialog.shown, ["no credentials are stored"]);
    }
    fixture
        .services
        .credentials
        .set("provider", &Credential::for_api_key("retained"), &NEVER)
        .await
        .unwrap();
    let mut dismissed = Dialog::default().selects([Some("logout"), None]);
    run(
        &fixture.command(),
        "",
        &mut dismissed,
        &Activity::default(),
        &NEVER,
    )
    .await
    .unwrap();
    assert!(
        fixture
            .services
            .credentials
            .get("provider", &NEVER)
            .await
            .unwrap()
            .is_some()
    );
    assert!(dismissed.shown.is_empty());
}

#[tokio::test]
async fn auth_correlation_is_unique_32_hex_and_paired_per_operation() {
    let fixture = Fixture::new(&[], None);
    for _ in 0..2 {
        let mut dialog = Dialog::default().selects([None]);
        run(
            &fixture.command(),
            "",
            &mut dialog,
            &Activity::default(),
            &NEVER,
        )
        .await
        .unwrap();
    }
    let log = fixture.diagnostics();
    let correlations = log
        .lines()
        .map(|line| {
            line.split("correlation=\"")
                .nth(1)
                .unwrap()
                .split('"')
                .next()
                .unwrap()
        })
        .collect::<Vec<_>>();
    assert_eq!(correlations.len(), 4);
    assert!(correlations.iter().all(|id| {
        id.len() == 32
            && id
                .bytes()
                .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
    }));
    assert_eq!(correlations[0], correlations[1]);
    assert_eq!(correlations[2], correlations[3]);
    assert_ne!(correlations[0], correlations[2]);
}

#[tokio::test]
async fn malformed_credentials_list_and_logout_fail_shared_load_work_not_success() {
    for action in ["list", "logout"] {
        let fixture = Fixture::new(&[], None);
        std::fs::create_dir_all(fixture.services.credentials.path().parent().unwrap()).unwrap();
        std::fs::write(
            fixture.services.credentials.path(),
            "malformed-new-sentinel",
        )
        .unwrap();
        let mut dialog = Dialog::default().selects([Some(action)]);
        let failure = run(
            &fixture.command(),
            "",
            &mut dialog,
            &Activity::default(),
            &NEVER,
        )
        .await
        .unwrap_err();
        assert!(
            matches!(failure, SlashFailure::Command(message) if message == "auth: malformed credential store")
        );
        assert_eq!(dialog.loads, ["Loading credentials…"]);
        assert_eq!(dialog.load_failures.len(), 1);
        assert_eq!(dialog.load_failures[0].code(), Code::Internal);
        assert_eq!(
            dialog.load_failures[0].message(),
            "auth: malformed credential store"
        );
        assert!(dialog.errors.is_empty() && dialog.shown.is_empty());
        let log = fixture.diagnostics();
        assert!(log.contains(&format!("{action}_failure")) && log.contains("interactive_failure"));
        assert!(log.contains("error=\"auth\""));
        assert!(!log.contains("malformed") && !log.contains("sentinel"));
        assert_eq!(
            std::fs::read_to_string(fixture.services.credentials.path()).unwrap(),
            "malformed-new-sentinel"
        );
        fixture.assert_legacy_untouched();
    }
}
