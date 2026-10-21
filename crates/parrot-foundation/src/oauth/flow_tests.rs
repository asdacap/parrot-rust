use super::{
    AuthDiagnostic, AuthDiagnosticLog, AuthError, AuthFlows, AuthOutput, CredentialStorage,
    DeviceAuthorization, OAuthClient, StoredApiKeySource,
};
use crate::credentials::{Credential, CredentialType, OAuthCredential, Secret};
use parrot_domain::contracts::async_ops::{
    BoxFuture, Cancellation, OperationError, OperationResult,
};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime};

struct NeverCancelled;
impl Cancellation for NeverCancelled {
    fn is_cancelled(&self) -> bool {
        false
    }
    fn cancelled(&self) -> BoxFuture<'_, ()> {
        Box::pin(std::future::pending())
    }
}
static NEVER: NeverCancelled = NeverCancelled;

struct AlreadyCancelled;
impl Cancellation for AlreadyCancelled {
    fn is_cancelled(&self) -> bool {
        true
    }
    fn cancelled(&self) -> BoxFuture<'_, ()> {
        Box::pin(async {})
    }
}
static CANCELLED: AlreadyCancelled = AlreadyCancelled;

#[derive(Default)]
struct FakeStore {
    entries: Mutex<HashMap<String, Credential>>,
    gets: Mutex<Vec<String>>,
    sets: Mutex<Vec<(String, Credential)>>,
    failure: Mutex<Option<OperationError<AuthError>>>,
}
impl CredentialStorage for FakeStore {
    fn get<'a>(
        &'a self,
        name: &'a str,
        cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, OperationResult<Option<Credential>, AuthError>> {
        Box::pin(async move {
            if cancellation.is_cancelled() {
                return Err(OperationError::Cancelled);
            }
            self.gets.lock().unwrap().push(name.to_owned());
            Ok(self.entries.lock().unwrap().get(name).cloned())
        })
    }
    fn set<'a>(
        &'a self,
        name: &'a str,
        credential: &'a Credential,
        cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, OperationResult<(), AuthError>> {
        Box::pin(async move {
            if cancellation.is_cancelled() {
                return Err(OperationError::Cancelled);
            }
            self.sets
                .lock()
                .unwrap()
                .push((name.to_owned(), credential.clone()));
            if let Some(error) = self.failure.lock().unwrap().clone() {
                return Err(error);
            }
            self.entries
                .lock()
                .unwrap()
                .insert(name.to_owned(), credential.clone());
            Ok(())
        })
    }
}

#[derive(Default)]
struct CapturedDiagnostics(Mutex<Vec<AuthDiagnostic>>);
impl AuthDiagnosticLog for CapturedDiagnostics {
    fn write(&self, event: AuthDiagnostic) {
        self.0.lock().unwrap().push(event);
    }
}

#[derive(Default)]
struct CapturedOutput(Mutex<Vec<String>>);
impl AuthOutput for CapturedOutput {
    fn write_line<'a>(
        &'a self,
        line: &'a str,
        cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, OperationResult<(), AuthError>> {
        Box::pin(async move {
            if cancellation.is_cancelled() {
                return Err(OperationError::Cancelled);
            }
            self.0.lock().unwrap().push(line.to_owned());
            Ok(())
        })
    }
}

struct FakeOAuth {
    fail_browser: bool,
    fail_device_wait: bool,
}
impl OAuthClient for FakeOAuth {
    fn now(&self) -> SystemTime {
        SystemTime::UNIX_EPOCH
    }
    fn authorization_url(&self, _: &str, _: &str, _: &str) -> String {
        "https://example.invalid/auth".into()
    }
    fn browser_login<'a>(
        &'a self,
        cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, OperationResult<OAuthCredential, AuthError>> {
        Box::pin(async move {
            if cancellation.is_cancelled() {
                return Err(OperationError::Cancelled);
            }
            if self.fail_browser {
                return Err(OperationError::Failed(AuthError::InvalidOperation(
                    "sentinel-failure-token",
                )));
            }
            Ok(oauth_credential())
        })
    }
    fn start_device_authorization<'a>(
        &'a self,
        cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, OperationResult<DeviceAuthorization, AuthError>> {
        Box::pin(async move {
            if cancellation.is_cancelled() {
                return Err(OperationError::Cancelled);
            }
            Ok(DeviceAuthorization {
                device_auth_id: Secret::new("fake-device-id"),
                user_code: Secret::new("sentinel-code"),
                verification_url: "https://example.invalid/device".into(),
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
        Box::pin(async move {
            if cancellation.is_cancelled() {
                return Err(OperationError::Cancelled);
            }
            if self.fail_device_wait {
                return Err(OperationError::Failed(AuthError::InvalidOperation(
                    "sentinel-failure-token",
                )));
            }
            Ok(oauth_credential())
        })
    }
    fn refresh<'a>(
        &'a self,
        _: &'a OAuthCredential,
        _: &'a dyn Cancellation,
    ) -> BoxFuture<'a, OperationResult<OAuthCredential, AuthError>> {
        Box::pin(async { Err(OperationError::Failed(AuthError::Message("not exercised"))) })
    }
}

fn oauth_credential() -> OAuthCredential {
    OAuthCredential::create(
        "fake-access-token",
        "fake-refresh-token",
        "2026-01-01T01:00:00+00:00",
        "fake-account",
    )
}

fn assert_diagnostic_pair(
    events: &[AuthDiagnostic],
    start: &str,
    terminal: &str,
    severity: super::DiagnosticSeverity,
    code: Option<&str>,
) {
    assert_eq!(events.len(), 2);
    assert_eq!(events[0].category, "auth");
    assert_eq!(events[0].operation, start);
    assert_eq!(events[0].severity, super::DiagnosticSeverity::Information);
    assert_eq!(events[0].duration_ms, None);
    assert_eq!(events[0].error_code, None);
    assert_eq!(events[1].category, "auth");
    assert_eq!(events[1].operation, terminal);
    assert_eq!(events[1].severity, severity);
    assert!(events[1].duration_ms.is_some());
    assert_eq!(events[1].error_code, code);
    assert_eq!(events[0].correlation_id, events[1].correlation_id);
    let id = events[0].correlation_id.as_bytes();
    assert_eq!(id.len(), 32);
    assert!(
        id.iter()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(byte))
    );
}

fn assert_no_canaries(events: &[AuthDiagnostic]) {
    let text = format!("{events:?}");
    for canary in [
        "provider-secret",
        "credential-secret",
        "driving the loop stores no credential",
        "sentinel",
        "fake-access-token",
        "fake-refresh-token",
        "sentinel-code",
        "fake-device-id",
    ] {
        assert!(!text.contains(canary), "diagnostics leaked {canary}");
    }
}

#[tokio::test]
async fn api_key_success_stores_and_logs_without_secret_values() {
    let store = FakeStore::default();
    let diagnostics = CapturedDiagnostics::default();
    AuthFlows::store_api_key(
        &store,
        "provider-secret",
        "credential-secret",
        &diagnostics,
        &NEVER,
    )
    .await
    .unwrap();
    assert_eq!(store.sets.lock().unwrap().len(), 1);
    let (name, value) = &store.sets.lock().unwrap()[0];
    assert_eq!(name, "provider-secret");
    assert_eq!(value.credential_type, CredentialType::ApiKey);
    assert_eq!(
        value.api_key.as_ref().unwrap().key.value(),
        "credential-secret"
    );
    assert_diagnostic_pair(
        &diagnostics.0.lock().unwrap(),
        "api_key_start",
        "api_key_complete",
        super::DiagnosticSeverity::Information,
        None,
    );
    assert_no_canaries(&diagnostics.0.lock().unwrap());
}

#[tokio::test]
async fn api_key_failure_propagates_and_diagnostics_exclude_failure_text() {
    let store = FakeStore::default();
    *store.failure.lock().unwrap() = Some(OperationError::Failed(AuthError::NotSupported(
        "driving the loop stores no credential",
    )));
    let diagnostics = CapturedDiagnostics::default();
    let result = AuthFlows::store_api_key(
        &store,
        "provider-secret",
        "credential-secret",
        &diagnostics,
        &NEVER,
    )
    .await;
    assert_eq!(
        result,
        Err(OperationError::Failed(AuthError::NotSupported(
            "driving the loop stores no credential"
        )))
    );
    assert_diagnostic_pair(
        &diagnostics.0.lock().unwrap(),
        "api_key_start",
        "api_key_failure",
        super::DiagnosticSeverity::Error,
        Some("unexpected"),
    );
    assert_no_canaries(&diagnostics.0.lock().unwrap());
}

#[tokio::test]
async fn detailed_failures_keep_typed_diagnostic_codes_without_private_messages() {
    for (error, expected_code) in [
        (
            AuthError::TransportWithMessage("private-detail transport".into()),
            "http",
        ),
        (AuthError::IoWithMessage("private-detail IO".into()), "io"),
        (
            AuthError::ArgumentOutOfRangeWithParameter {
                message: "private-detail range".into(),
                parameter: Some("private-parameter".into()),
            },
            "argument",
        ),
        (
            AuthError::FormatWithMessage("private-detail format".into()),
            "unexpected",
        ),
    ] {
        let store = FakeStore::default();
        *store.failure.lock().unwrap() = Some(OperationError::Failed(error.clone()));
        let diagnostics = CapturedDiagnostics::default();
        let result = AuthFlows::store_api_key(
            &store,
            "provider-secret",
            "credential-secret",
            &diagnostics,
            &NEVER,
        )
        .await;
        assert_eq!(result, Err(OperationError::Failed(error)));
        let events = diagnostics.0.lock().unwrap();
        assert_diagnostic_pair(
            &events,
            "api_key_start",
            "api_key_failure",
            super::DiagnosticSeverity::Error,
            Some(expected_code),
        );
        assert_no_canaries(&events);
        let text = format!("{events:?}");
        assert!(!text.contains("private-detail"));
        assert!(!text.contains("private-parameter"));
    }
}

#[tokio::test]
async fn oauth_browser_success_logs_and_stores_chatgpt_credential() {
    let store = FakeStore::default();
    let diagnostics = CapturedDiagnostics::default();
    let output = CapturedOutput::default();
    let oauth = FakeOAuth {
        fail_browser: false,
        fail_device_wait: false,
    };
    AuthFlows::oauth_login(&oauth, &store, false, &output, &diagnostics, &NEVER)
        .await
        .unwrap();
    assert_eq!(
        output.0.lock().unwrap().as_slice(),
        ["  opening your browser to authorize..."]
    );
    let entries = store.sets.lock().unwrap();
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].0, "chatgpt");
    assert_eq!(entries[0].1.credential_type, CredentialType::OAuth);
    assert_eq!(
        entries[0].1.oauth.as_ref().unwrap().account_id.as_deref(),
        Some("fake-account")
    );
    assert_diagnostic_pair(
        &diagnostics.0.lock().unwrap(),
        "oauth_start",
        "oauth_complete",
        super::DiagnosticSeverity::Information,
        None,
    );
    assert_no_canaries(&diagnostics.0.lock().unwrap());
}

#[tokio::test]
async fn oauth_device_success_prints_code_and_stores_chatgpt_credential() {
    let store = FakeStore::default();
    let diagnostics = CapturedDiagnostics::default();
    let output = CapturedOutput::default();
    let oauth = FakeOAuth {
        fail_browser: false,
        fail_device_wait: false,
    };
    AuthFlows::oauth_login(&oauth, &store, true, &output, &diagnostics, &NEVER)
        .await
        .unwrap();
    assert_eq!(
        output.0.lock().unwrap().as_slice(),
        ["  visit https://example.invalid/device and enter code sentinel-code"]
    );
    assert_eq!(store.sets.lock().unwrap()[0].0, "chatgpt");
    assert_diagnostic_pair(
        &diagnostics.0.lock().unwrap(),
        "oauth_start",
        "oauth_complete",
        super::DiagnosticSeverity::Information,
        None,
    );
    assert_no_canaries(&diagnostics.0.lock().unwrap());
}

#[tokio::test]
async fn oauth_browser_failure_propagates_and_redacts_diagnostics() {
    let store = FakeStore::default();
    let diagnostics = CapturedDiagnostics::default();
    let output = CapturedOutput::default();
    let oauth = FakeOAuth {
        fail_browser: true,
        fail_device_wait: false,
    };
    assert_eq!(
        AuthFlows::oauth_login(&oauth, &store, false, &output, &diagnostics, &NEVER).await,
        Err(OperationError::Failed(AuthError::InvalidOperation(
            "sentinel-failure-token"
        )))
    );
    assert!(store.sets.lock().unwrap().is_empty());
    assert_eq!(
        output.0.lock().unwrap().as_slice(),
        ["  opening your browser to authorize..."]
    );
    assert_diagnostic_pair(
        &diagnostics.0.lock().unwrap(),
        "oauth_start",
        "oauth_failure",
        super::DiagnosticSeverity::Error,
        Some("invalid_operation"),
    );
    assert_no_canaries(&diagnostics.0.lock().unwrap());
}

#[tokio::test]
async fn oauth_device_failure_propagates_after_printing_code_and_redacts_diagnostics() {
    let store = FakeStore::default();
    let diagnostics = CapturedDiagnostics::default();
    let output = CapturedOutput::default();
    let oauth = FakeOAuth {
        fail_browser: false,
        fail_device_wait: true,
    };
    assert_eq!(
        AuthFlows::oauth_login(&oauth, &store, true, &output, &diagnostics, &NEVER).await,
        Err(OperationError::Failed(AuthError::InvalidOperation(
            "sentinel-failure-token"
        )))
    );
    assert!(store.sets.lock().unwrap().is_empty());
    assert_eq!(
        output.0.lock().unwrap().as_slice(),
        ["  visit https://example.invalid/device and enter code sentinel-code"]
    );
    assert_diagnostic_pair(
        &diagnostics.0.lock().unwrap(),
        "oauth_start",
        "oauth_failure",
        super::DiagnosticSeverity::Error,
        Some("invalid_operation"),
    );
    assert_no_canaries(&diagnostics.0.lock().unwrap());
}

#[tokio::test]
async fn oauth_cancellation_has_information_severity_and_cancelled_code() {
    let store = FakeStore::default();
    let diagnostics = CapturedDiagnostics::default();
    let output = CapturedOutput::default();
    let oauth = FakeOAuth {
        fail_browser: false,
        fail_device_wait: false,
    };
    assert_eq!(
        AuthFlows::oauth_login(&oauth, &store, false, &output, &diagnostics, &CANCELLED).await,
        Err(OperationError::Cancelled)
    );
    assert_diagnostic_pair(
        &diagnostics.0.lock().unwrap(),
        "oauth_start",
        "oauth_failure",
        super::DiagnosticSeverity::Information,
        Some("cancelled"),
    );
    assert!(store.sets.lock().unwrap().is_empty());
    assert_no_canaries(&diagnostics.0.lock().unwrap());
}

fn stored_key_source(
    store: Arc<FakeStore>,
    api_key_env: &str,
    env_value: Option<&str>,
    calls: Arc<Mutex<Vec<String>>>,
) -> StoredApiKeySource {
    let lookup_calls = calls.clone();
    let env_value = env_value.map(str::to_owned);
    StoredApiKeySource::new(
        "provider",
        api_key_env,
        store,
        Arc::new(move |name| {
            lookup_calls.lock().unwrap().push(name.to_owned());
            env_value.clone()
        }),
    )
}

#[tokio::test]
async fn nonempty_environment_api_key_wins_over_store_and_ignores_cancellation() {
    let store = Arc::new(FakeStore::default());
    let calls = Arc::new(Mutex::new(Vec::new()));
    let source = stored_key_source(
        store.clone(),
        "T14_SYNTHETIC_API_KEY",
        Some("environment-secret"),
        calls.clone(),
    );
    assert!(source.has_credential(&CANCELLED).await.unwrap());
    assert_eq!(
        source.api_key(&CANCELLED).await.unwrap().value(),
        "environment-secret"
    );
    assert!(store.gets.lock().unwrap().is_empty());
    assert_eq!(
        calls.lock().unwrap().as_slice(),
        ["T14_SYNTHETIC_API_KEY", "T14_SYNTHETIC_API_KEY"]
    );
}

#[tokio::test]
async fn missing_or_empty_environment_key_falls_back_to_stored_key() {
    for environment_value in [None, Some("")] {
        let store = Arc::new(FakeStore::default());
        store
            .entries
            .lock()
            .unwrap()
            .insert("provider".into(), Credential::for_api_key("stored-secret"));
        let calls = Arc::new(Mutex::new(Vec::new()));
        let source = stored_key_source(
            store.clone(),
            "T14_SYNTHETIC_API_KEY",
            environment_value,
            calls,
        );
        assert!(source.has_credential(&NEVER).await.unwrap());
        assert_eq!(
            source.api_key(&NEVER).await.unwrap().value(),
            "stored-secret"
        );
        assert_eq!(
            store.gets.lock().unwrap().as_slice(),
            ["provider", "provider"]
        );
    }
}

#[tokio::test]
async fn empty_environment_name_skips_lookup_and_uses_store() {
    let store = Arc::new(FakeStore::default());
    store
        .entries
        .lock()
        .unwrap()
        .insert("provider".into(), Credential::for_api_key("stored-secret"));
    let calls = Arc::new(Mutex::new(Vec::new()));
    let source = stored_key_source(
        store.clone(),
        "",
        Some("ignored-environment-secret"),
        calls.clone(),
    );
    assert!(source.has_credential(&NEVER).await.unwrap());
    assert_eq!(
        source.api_key(&NEVER).await.unwrap().value(),
        "stored-secret"
    );
    assert!(calls.lock().unwrap().is_empty());
}

#[tokio::test]
async fn empty_environment_fallback_propagates_store_cancellation() {
    let store = Arc::new(FakeStore::default());
    let source = stored_key_source(
        store,
        "T14_SYNTHETIC_API_KEY",
        Some(""),
        Arc::new(Mutex::new(Vec::new())),
    );
    assert_eq!(
        source.has_credential(&CANCELLED).await,
        Err(OperationError::Cancelled)
    );
    assert_eq!(
        source.api_key(&CANCELLED).await,
        Err(OperationError::Cancelled)
    );
}

#[tokio::test]
async fn whitespace_environment_value_is_nonempty_and_wins() {
    let store = Arc::new(FakeStore::default());
    let calls = Arc::new(Mutex::new(Vec::new()));
    let source = stored_key_source(store.clone(), "T14_SYNTHETIC_API_KEY", Some(" "), calls);
    assert!(source.has_credential(&NEVER).await.unwrap());
    assert_eq!(source.api_key(&NEVER).await.unwrap().value(), " ");
    assert!(store.gets.lock().unwrap().is_empty());
}

#[tokio::test]
async fn wrong_type_or_version_has_source_compatible_api_key_results() {
    let store = Arc::new(FakeStore::default());
    store
        .entries
        .lock()
        .unwrap()
        .insert("provider".into(), Credential::for_oauth(oauth_credential()));
    let source = stored_key_source(store.clone(), "", None, Arc::new(Mutex::new(Vec::new())));
    assert!(!source.has_credential(&NEVER).await.unwrap());
    assert_eq!(source.api_key(&NEVER).await.unwrap().value(), "");

    let mut old_version = Credential::for_api_key("stored-secret");
    old_version.version = 999;
    store
        .entries
        .lock()
        .unwrap()
        .insert("provider".into(), old_version);
    assert!(!source.has_credential(&NEVER).await.unwrap());
    assert_eq!(
        source.api_key(&NEVER).await.unwrap().value(),
        "stored-secret"
    );
}
