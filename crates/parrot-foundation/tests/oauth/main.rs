mod producer_precision;

use parrot_domain::contracts::async_ops::{
    BoxFuture, Cancellation, OperationError, OperationResult, OwnedWork,
};
use parrot_foundation::credentials::{Credential, CredentialStore, OAuthCredential, Secret};
use parrot_foundation::oauth::{
    AuthDiagnostic, AuthDiagnosticLog, AuthError, AuthFlows, AuthOutput, DeviceAuthorization,
    OAuthClient, OAuthTokenSource,
};
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicUsize, Ordering},
};
use std::time::{Duration, SystemTime};

struct Never;
impl Cancellation for Never {
    fn is_cancelled(&self) -> bool {
        false
    }
    fn cancelled(&self) -> BoxFuture<'_, ()> {
        Box::pin(std::future::pending())
    }
}
static NEVER: Never = Never;

struct FixtureClient {
    refreshes: AtomicUsize,
}
impl OAuthClient for FixtureClient {
    fn now(&self) -> SystemTime {
        SystemTime::UNIX_EPOCH + Duration::from_secs(1_767_225_600)
    }
    fn authorization_url(&self, _: &str, _: &str, _: &str) -> String {
        "fixture-only".into()
    }
    fn browser_login<'a>(
        &'a self,
        _: &'a dyn Cancellation,
    ) -> BoxFuture<'a, OperationResult<OAuthCredential, AuthError>> {
        Box::pin(async {
            Err(OperationError::Failed(AuthError::Message(
                "browser path not exercised by this fixture",
            )))
        })
    }
    fn start_device_authorization<'a>(
        &'a self,
        _: &'a dyn Cancellation,
    ) -> BoxFuture<'a, OperationResult<DeviceAuthorization, AuthError>> {
        Box::pin(async move {
            Ok(DeviceAuthorization {
                device_auth_id: Secret::new("device-FAKE-SECRET-CANARY"),
                user_code: Secret::new("code-FAKE-SECRET-CANARY"),
                verification_url: "https://issuer.invalid/codex/device".into(),
                interval: Duration::from_secs(5),
                expires_at: self.now() + Duration::from_secs(300),
            })
        })
    }
    fn await_device_authorization<'a>(
        &'a self,
        _: &'a DeviceAuthorization,
        _: &'a dyn Cancellation,
    ) -> BoxFuture<'a, OperationResult<OAuthCredential, AuthError>> {
        Box::pin(async { Ok(rotated()) })
    }
    fn refresh<'a>(
        &'a self,
        current: &'a OAuthCredential,
        _: &'a dyn Cancellation,
    ) -> BoxFuture<'a, OperationResult<OAuthCredential, AuthError>> {
        Box::pin(async move {
            self.refreshes.fetch_add(1, Ordering::SeqCst);
            let mut value = rotated();
            value.account_id = current.account_id.clone();
            Ok(value)
        })
    }
}
fn rotated() -> OAuthCredential {
    OAuthCredential::create(
        "access-FAKE-SECRET-CANARY",
        "refresh-FAKE-SECRET-CANARY",
        "2026-01-01T01:00:00+00:00",
        "acct",
    )
}
#[derive(Default)]
struct Log(Mutex<Vec<AuthDiagnostic>>);
impl AuthDiagnosticLog for Log {
    fn write(&self, event: AuthDiagnostic) {
        self.0.lock().unwrap().push(event);
    }
}
#[derive(Default)]
struct Output(Mutex<Vec<String>>);
impl AuthOutput for Output {
    fn write_line<'a>(
        &'a self,
        line: &'a str,
        cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, OperationResult<(), AuthError>> {
        Box::pin(async move {
            cancellation
                .check()
                .map_err(|_| OperationError::Cancelled)?;
            self.0.lock().unwrap().push(line.into());
            Ok(())
        })
    }
}
fn root() -> std::path::PathBuf {
    static NEXT: AtomicUsize = AtomicUsize::new(0);
    std::env::temp_dir().join(format!(
        "parrot-t14-integration-{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ))
}

#[tokio::test]
async fn file_backed_refresh_reopens_rotated_credentials() {
    let root = root();
    let path = root.join("new-config/credentials.json");
    let store = Arc::new(CredentialStore::at_selected_path(&path));
    store
        .set(
            "openai",
            &Credential::for_oauth(OAuthCredential::create(
                "stale",
                "refresh",
                "2026-01-01T00:01:00+00:00",
                "acct",
            )),
            &NEVER,
        )
        .await
        .unwrap();
    let client = Arc::new(FixtureClient {
        refreshes: AtomicUsize::new(0),
    });
    let source = OAuthTokenSource::new(store.clone(), client.clone(), "openai");
    let calls = (0..12)
        .map(|_| source.token(Arc::new(Never)))
        .collect::<Vec<_>>();
    for call in calls {
        assert_eq!(
            call.await.unwrap().access_token.value(),
            "access-FAKE-SECRET-CANARY"
        );
    }
    source.settle().await.unwrap();
    assert_eq!(client.refreshes.load(Ordering::SeqCst), 1);
    let reopened = CredentialStore::at_selected_path(&path)
        .get("openai", &NEVER)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(reopened.oauth.unwrap(), rotated());
    std::fs::remove_dir_all(root).unwrap();
}

#[tokio::test]
async fn api_key_flow_persists_without_diagnostic_secrets() {
    let root = root();
    let path = root.join("new-config/credentials.json");
    let store = CredentialStore::at_selected_path(&path);
    let log = Log::default();
    AuthFlows::store_api_key(
        &store,
        "provider-FAKE-SECRET-CANARY",
        "key-FAKE-SECRET-CANARY",
        &log,
        &NEVER,
    )
    .await
    .unwrap();
    let reopened = CredentialStore::at_selected_path(&path);
    assert_eq!(
        reopened
            .get("provider-FAKE-SECRET-CANARY", &NEVER)
            .await
            .unwrap()
            .unwrap()
            .api_key
            .unwrap()
            .key
            .value(),
        "key-FAKE-SECRET-CANARY"
    );
    let events = log.0.lock().unwrap();
    assert_eq!(
        events
            .iter()
            .map(|event| event.operation)
            .collect::<Vec<_>>(),
        ["api_key_start", "api_key_complete"]
    );
    assert!(!format!("{events:?}").contains("FAKE-SECRET-CANARY"));
    std::fs::remove_dir_all(root).unwrap();
}

#[tokio::test]
async fn device_flow_prints_user_code_without_diagnostic_secrets() {
    let root = root();
    let path = root.join("new-config/credentials.json");
    let store = CredentialStore::at_selected_path(&path);
    let log = Log::default();
    let output = Output::default();
    let client = FixtureClient {
        refreshes: AtomicUsize::new(0),
    };
    AuthFlows::oauth_login(&client, &store, true, &output, &log, &NEVER)
        .await
        .unwrap();
    assert_eq!(
        *output.0.lock().unwrap(),
        ["  visit https://issuer.invalid/codex/device and enter code code-FAKE-SECRET-CANARY"]
    );
    assert_eq!(
        CredentialStore::at_selected_path(&path)
            .get("chatgpt", &NEVER)
            .await
            .unwrap()
            .unwrap()
            .oauth
            .unwrap(),
        rotated()
    );
    let events = log.0.lock().unwrap();
    assert_eq!(
        events
            .iter()
            .map(|event| event.operation)
            .collect::<Vec<_>>(),
        ["oauth_start", "oauth_complete"]
    );
    assert!(!format!("{events:?}").contains("FAKE-SECRET-CANARY"));
    std::fs::remove_dir_all(root).unwrap();
}

#[tokio::test]
async fn raw_file_nullable_account_survives_actual_http_refresh() {
    use parrot_foundation::oauth::{OpenAiOAuthClient, OpenAiOAuthOptions};
    use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
    for (is_fresh, account) in [(true, None), (false, None), (false, Some(""))] {
        let directory = root();
        std::fs::create_dir_all(&directory).unwrap();
        let path = directory.join("credentials.json");
        let expiry = if is_fresh {
            "2030-01-01T01:00:00+00:00"
        } else {
            "2030-01-01T00:00:00+00:00"
        };
        let account_field =
            account.map_or_else(String::new, |value| format!(",\"account_id\":\"{value}\""));
        std::fs::write(&path, format!("{{\"version\":1,\"credentials\":{{\"chatgpt\":{{\"version\":1,\"type\":\"oauth\",\"oauth\":{{\"access_token\":\"accessold\",\"refresh_token\":\"refreshold\",\"expires_at\":\"{expiry}\"{account_field}}}}}}}}}")).unwrap();
        let store = Arc::new(CredentialStore::at_selected_path(&path));
        assert_eq!(
            store
                .get("chatgpt", &NEVER)
                .await
                .unwrap()
                .unwrap()
                .oauth
                .unwrap()
                .account_id
                .as_deref(),
            account
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let issuer = format!("http://{}", listener.local_addr().unwrap());
        let server = if is_fresh {
            drop(listener);
            None
        } else {
            Some(tokio::spawn(async move {
                let (stream, _) = listener.accept().await.unwrap();
                let mut reader = BufReader::new(stream);
                let mut line = String::new();
                reader.read_line(&mut line).await.unwrap();
                assert_eq!(line, "POST /oauth/token HTTP/1.1\r\n");
                let mut length = 0;
                loop {
                    line.clear();
                    reader.read_line(&mut line).await.unwrap();
                    if line == "\r\n" {
                        break;
                    }
                    if let Some((key, value)) = line.split_once(':') {
                        if key.eq_ignore_ascii_case("content-length") {
                            length = value.trim().parse().unwrap();
                        }
                    }
                }
                let mut body = vec![0; length];
                reader.read_exact(&mut body).await.unwrap();
                assert_eq!(
                    String::from_utf8(body).unwrap(),
                    "grant_type=refresh_token&refresh_token=refreshold&client_id=app_EMoamEEZ73f0CkXaXp7hrann"
                );
                let response = r#"{"access_token":"accessnullable","refresh_token":"refreshnullable","expires_in":120}"#;
                reader.get_mut().write_all(format!("HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{response}",response.len()).as_bytes()).await.unwrap();
            }))
        };
        let client = Arc::new(OpenAiOAuthClient::new(
            reqwest::Client::builder().no_proxy().build().unwrap(),
            Arc::new(PrecisionUnusedBrowser),
            OpenAiOAuthOptions {
                issuer,
                clock: Arc::new(producer_precision::PrecisionClock),
                ..Default::default()
            },
        ));
        let source = OAuthTokenSource::new(store, client, "chatgpt");
        let access = source.token(Arc::new(Never)).await.unwrap();
        source.settle().await.unwrap();
        if let Some(server) = server {
            server.await.unwrap();
        }
        assert_eq!(access.account_id.as_deref(), account);
        assert_eq!(
            access.access_token.value(),
            if is_fresh {
                "accessold"
            } else {
                "accessnullable"
            }
        );
        let persisted = CredentialStore::at_selected_path(&path)
            .get("chatgpt", &NEVER)
            .await
            .unwrap()
            .unwrap();
        let oauth = persisted.oauth.as_ref().unwrap();
        assert_eq!(oauth.account_id.as_deref(), account);
        assert_eq!(
            oauth.expires_at,
            if is_fresh {
                "2030-01-01T01:00:00+00:00"
            } else {
                "2029-12-31T23:57:00+00:00"
            }
        );
        assert_eq!(
            serde_json::to_value(&persisted).unwrap()["oauth"]
                .get("account_id")
                .is_some(),
            account.is_some()
        );
        std::fs::remove_dir_all(directory).unwrap();
    }
}
struct PrecisionUnusedBrowser;
impl parrot_foundation::oauth::BrowserOpener for PrecisionUnusedBrowser {
    fn open<'a>(
        &'a self,
        _: &'a str,
        _: &'a dyn Cancellation,
    ) -> BoxFuture<'a, OperationResult<(), AuthError>> {
        Box::pin(async { panic!("nullable-account fixture must not open a browser") })
    }
}

#[tokio::test]
async fn file_store_typed_ticks_control_the_strict_refresh_margin() {
    // Source v9/v10 execute this same raw-file -> typed Get -> Token boundary.
    let observations = producer_precision::observations().await;
    let typed_expiries = [
        "2029-12-31T23:59:59.9999999+00:00",
        "2030-01-01T00:00:00+00:00",
        "2030-01-01T00:00:00+00:00",
        "2030-01-01T00:00:00.0000001+00:00",
        "2030-01-01T00:00:00+00:00",
        "2030-01-01T00:00:00.0000001+00:00",
    ];
    for ((case, expiry), refreshes) in observations
        .iter()
        .zip(typed_expiries)
        .zip([1, 1, 1, 0, 1, 0])
    {
        let observed = &case["observations"];
        let typed_json = serde_json::to_string(expiry).unwrap();
        assert_eq!(observed["typed_expiry_json"], typed_json, "{}", case["id"]);
        assert_eq!(observed["refreshes"], refreshes, "{}", case["id"]);
        assert_eq!(
            observed["outcome"],
            serde_json::json!({"outcome":"completed"})
        );
        assert_eq!(observed["returned"]["account_id"], "acct");
        assert_eq!(observed["persisted"]["account_id"], "acct");
        if refreshes == 1 {
            assert_eq!(observed["refresh_input"]["expires_at_json"], typed_json);
            assert_eq!(observed["refresh_input"]["access_token"], "accessold");
            assert_eq!(observed["refresh_input"]["refresh_token"], "refreshold");
            assert_eq!(observed["returned"]["access_token"], "rotatedaccess");
            assert_eq!(observed["persisted"]["access_token"], "rotatedaccess");
            assert_eq!(observed["persisted"]["refresh_token"], "rotatedrefresh");
            assert_eq!(
                observed["persisted"]["expires_at_json"],
                "\"2030-01-01T01:00:00+00:00\""
            );
        } else {
            assert!(observed["refresh_input"].is_null());
            assert_eq!(observed["returned"]["access_token"], "accessold");
            assert_eq!(observed["persisted"]["access_token"], "accessold");
            assert_eq!(observed["persisted"]["refresh_token"], "refreshold");
            assert_eq!(observed["persisted"]["expires_at_json"], typed_json);
        }
    }
}
