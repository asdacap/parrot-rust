//! Real file-store to token-source precision consumer with synthetic credentials only.
use chrono::DateTime;
use parrot_domain::contracts::async_ops::{BoxFuture, Cancellation, OperationResult, OwnedWork};
use parrot_foundation::credentials::{CredentialStore, OAuthCredential};
use parrot_foundation::oauth::{AuthError, DeviceAuthorization, OAuthClient, OAuthTokenSource};
use serde_json::{Value, json};
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicUsize, Ordering},
};
use std::time::{Instant, SystemTime};

pub struct PrecisionClock;
impl parrot_domain::contracts::async_ops::Clock for PrecisionClock {
    fn now(&self) -> Instant {
        Instant::now()
    }
    fn utc_now(&self) -> SystemTime {
        DateTime::parse_from_rfc3339(CLOCK).unwrap().into()
    }
    fn sleep_until<'a>(
        &'a self,
        deadline: Instant,
        cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<(), parrot_domain::contracts::async_ops::Cancelled>> {
        Box::pin(async move {
            tokio::select! {
                biased;
                _ = cancellation.cancelled() => Err(parrot_domain::contracts::async_ops::Cancelled),
                _ = tokio::time::sleep_until(deadline.into()) => Ok(()),
            }
        })
    }
}

pub const INPUTS: [(&str, &str); 6] = [
    ("below-margin", "2029-12-31T23:59:59.9999999+00:00"),
    ("exact-margin", "2030-01-01T00:00:00+00:00"),
    ("ten-nanoseconds", "2030-01-01T00:00:00.00000001+00:00"),
    ("one-tick", "2030-01-01T00:00:00.0000001+00:00"),
    ("subtick-tail", "2030-01-01T00:00:00.0000000999999999+00:00"),
    (
        "one-tick-tail",
        "2030-01-01T00:00:00.0000001999999999+00:00",
    ),
];
const CLOCK: &str = "2029-12-31T23:55:00+00:00";
struct Never;
impl Cancellation for Never {
    fn is_cancelled(&self) -> bool {
        false
    }
    fn cancelled(&self) -> BoxFuture<'_, ()> {
        Box::pin(std::future::pending())
    }
}
#[derive(Default)]
pub struct Client {
    pub refreshes: AtomicUsize,
    input: Mutex<Option<OAuthCredential>>,
}
impl OAuthClient for Client {
    fn now(&self) -> SystemTime {
        DateTime::parse_from_rfc3339(CLOCK).unwrap().into()
    }
    fn authorization_url(&self, _: &str, _: &str, _: &str) -> String {
        panic!("unused precision fixture API")
    }
    fn browser_login<'a>(
        &'a self,
        _: &'a dyn Cancellation,
    ) -> BoxFuture<'a, OperationResult<OAuthCredential, AuthError>> {
        Box::pin(async { panic!("unused precision fixture API") })
    }
    fn start_device_authorization<'a>(
        &'a self,
        _: &'a dyn Cancellation,
    ) -> BoxFuture<'a, OperationResult<DeviceAuthorization, AuthError>> {
        Box::pin(async { panic!("unused precision fixture API") })
    }
    fn await_device_authorization<'a>(
        &'a self,
        _: &'a DeviceAuthorization,
        _: &'a dyn Cancellation,
    ) -> BoxFuture<'a, OperationResult<OAuthCredential, AuthError>> {
        Box::pin(async { panic!("unused precision fixture API") })
    }
    fn refresh<'a>(
        &'a self,
        current: &'a OAuthCredential,
        _: &'a dyn Cancellation,
    ) -> BoxFuture<'a, OperationResult<OAuthCredential, AuthError>> {
        Box::pin(async move {
            self.refreshes.fetch_add(1, Ordering::Relaxed);
            *self.input.lock().unwrap() = Some(current.clone());
            Ok(OAuthCredential::create(
                "rotatedaccess",
                "rotatedrefresh",
                "2030-01-01T01:00:00+00:00",
                "acct",
            ))
        })
    }
}
fn credential_value(c: &OAuthCredential) -> Value {
    // The string is the actual typed Rust credential field, not normalized by this producer.
    let date = DateTime::parse_from_rfc3339(&c.expires_at).unwrap();
    json!({"access_token":c.access_token.value(),"refresh_token":c.refresh_token.value(),"expires_at_json":serde_json::to_string(&c.expires_at).unwrap(),"expires_at_unix_seconds":date.timestamp(),"account_id":c.account_id})
}
pub async fn observations() -> Vec<Value> {
    static NEXT: AtomicUsize = AtomicUsize::new(0);
    let root = std::env::temp_dir().join(format!(
        "parrot-t14-precision-{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ));
    std::fs::create_dir_all(&root).unwrap();
    let mut values = Vec::new();
    for (id, expiry) in INPUTS {
        let path = root.join(format!("{id}.json"));
        let fixture = format!(
            "{{\"version\":1,\"credentials\":{{\"chatgpt\":{{\"version\":1,\"type\":\"oauth\",\"oauth\":{{\"access_token\":\"accessold\",\"refresh_token\":\"refreshold\",\"expires_at\":\"{expiry}\",\"account_id\":\"acct\"}}}}}}}}"
        );
        std::fs::write(&path, &fixture).unwrap();
        let store = Arc::new(CredentialStore::at_selected_path(&path));
        let typed = store
            .get("chatgpt", &Never)
            .await
            .unwrap()
            .unwrap()
            .oauth
            .unwrap();
        let client = Arc::new(Client {
            refreshes: AtomicUsize::new(0),
            input: Mutex::new(None),
        });
        let source = OAuthTokenSource::new(store, client.clone(), "chatgpt");
        let result = source.token(Arc::new(Never)).await;
        source.settle().await.unwrap();
        let persisted = CredentialStore::at_selected_path(&path)
            .get("chatgpt", &Never)
            .await
            .unwrap()
            .unwrap()
            .oauth
            .unwrap();
        let outcome = match &result {
            Ok(_) => json!({"outcome":"completed"}),
            Err(parrot_domain::contracts::async_ops::OperationError::Cancelled) => {
                json!({"outcome":"cancelled"})
            }
            Err(parrot_domain::contracts::async_ops::OperationError::Failed(error)) => {
                let error_type = match error {
                    AuthError::InvalidOperation(_) => "InvalidOperationException",
                    AuthError::NotSupported(_) => "NotSupportedException",
                    AuthError::Format | AuthError::FormatWithMessage(_) => "FormatException",
                    AuthError::ArgumentOutOfRange(_)
                    | AuthError::ArgumentOutOfRangeWithParameter { .. } => {
                        "ArgumentOutOfRangeException"
                    }
                    AuthError::Transport | AuthError::TransportWithMessage(_) => {
                        "HttpRequestException"
                    }
                    AuthError::Io | AuthError::IoWithMessage(_) => "IOException",
                    _ => "AuthException",
                };
                json!({"outcome":"failed","error_type":error_type,"message":error.to_string()})
            }
        };
        let returned = result
            .ok()
            .map(|a| json!({"access_token":a.access_token.value(),"account_id":a.account_id}));
        let input = client.input.lock().unwrap().as_ref().map(credential_value);
        values.push(json!({"id":format!("oauth.token-source.typed-precision.{id}"),"observations":{"input_expiry":expiry,"clock_json":serde_json::to_string(CLOCK).unwrap(),"fixture_json":fixture,"typed_expiry_json":serde_json::to_string(&typed.expires_at).unwrap(),"refreshes":client.refreshes.load(Ordering::Relaxed),"outcome":outcome,"returned":returned,"refresh_input":input,"persisted":credential_value(&persisted)}}));
    }
    std::fs::remove_dir_all(root).unwrap();
    values
}
