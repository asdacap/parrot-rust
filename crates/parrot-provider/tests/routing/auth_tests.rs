use parrot_domain::contracts::async_ops::{
    BoxFuture, Cancellation, OperationError, OperationResult, OwnedWork,
};
use parrot_foundation::{
    credentials::{Credential, OAuthCredential},
    oauth::{AuthError, CredentialStorage, DeviceAuthorization, OAuthClient},
};
use parrot_provider::{
    routing::auth::{CredentialSources, FoundationCredentials},
    sessions::{Error, provider::OAuthAccess},
};
use std::{
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
    time::{Duration, SystemTime},
};
use tokio::sync::Notify;

struct Token(bool);
impl Cancellation for Token {
    fn is_cancelled(&self) -> bool {
        self.0
    }
    fn cancelled(&self) -> BoxFuture<'_, ()> {
        Box::pin(async move {
            if !self.0 {
                std::future::pending().await
            }
        })
    }
}
const ACTIVE: Token = Token(false);
const CANCELLED: Token = Token(true);
struct Store {
    value: Mutex<Result<Option<Credential>, AuthError>>,
    calls: AtomicUsize,
    names: Mutex<Vec<String>>,
    honor_cancellation: bool,
    pending: bool,
    entered: Notify,
    exits: AtomicUsize,
}
impl Store {
    fn new(value: Option<Credential>) -> Self {
        Self {
            value: Mutex::new(Ok(value)),
            calls: AtomicUsize::new(0),
            names: Mutex::new(Vec::new()),
            honor_cancellation: true,
            pending: false,
            entered: Notify::new(),
            exits: AtomicUsize::new(0),
        }
    }
}
impl CredentialStorage for Store {
    fn get<'a>(
        &'a self,
        name: &'a str,
        cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, OperationResult<Option<Credential>, AuthError>> {
        Box::pin(async move {
            self.calls.fetch_add(1, Ordering::SeqCst);
            self.names.lock().unwrap().push(name.into());
            self.entered.notify_one();
            if self.pending {
                cancellation.cancelled().await;
                self.exits.fetch_add(1, Ordering::SeqCst);
                return Err(OperationError::Cancelled);
            }
            if self.honor_cancellation && cancellation.is_cancelled() {
                return Err(OperationError::Cancelled);
            }
            self.value
                .lock()
                .unwrap()
                .clone()
                .map_err(OperationError::Failed)
        })
    }
    fn set<'a>(
        &'a self,
        _: &'a str,
        value: &'a Credential,
        cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, OperationResult<(), AuthError>> {
        Box::pin(async move {
            if cancellation.is_cancelled() {
                return Err(OperationError::Cancelled);
            }
            *self.value.lock().unwrap() = Ok(Some(value.clone()));
            Ok(())
        })
    }
}
struct Client {
    calls: AtomicUsize,
    result: Result<OAuthCredential, AuthError>,
}
impl Client {
    fn new() -> Self {
        Self {
            calls: AtomicUsize::new(0),
            result: Ok(oauth("refreshed")),
        }
    }
}
impl OAuthClient for Client {
    fn now(&self) -> SystemTime {
        SystemTime::UNIX_EPOCH + Duration::from_secs(1_735_689_600)
    }
    fn authorization_url(&self, _: &str, _: &str, _: &str) -> String {
        panic!("no login during routing")
    }
    fn browser_login<'a>(
        &'a self,
        _: &'a dyn Cancellation,
    ) -> BoxFuture<'a, OperationResult<OAuthCredential, AuthError>> {
        Box::pin(async { panic!("no login during routing") })
    }
    fn start_device_authorization<'a>(
        &'a self,
        _: &'a dyn Cancellation,
    ) -> BoxFuture<'a, OperationResult<DeviceAuthorization, AuthError>> {
        Box::pin(async { panic!("no login during routing") })
    }
    fn await_device_authorization<'a>(
        &'a self,
        _: &'a DeviceAuthorization,
        _: &'a dyn Cancellation,
    ) -> BoxFuture<'a, OperationResult<OAuthCredential, AuthError>> {
        Box::pin(async { panic!("no login during routing") })
    }
    fn refresh<'a>(
        &'a self,
        _: &'a OAuthCredential,
        _: &'a dyn Cancellation,
    ) -> BoxFuture<'a, OperationResult<OAuthCredential, AuthError>> {
        Box::pin(async move {
            self.calls.fetch_add(1, Ordering::SeqCst);
            self.result.clone().map_err(OperationError::Failed)
        })
    }
}
fn oauth(token: &str) -> OAuthCredential {
    OAuthCredential::create(
        token,
        "refresh",
        "2099-01-01T00:00:00Z",
        "synthetic-account",
    )
}
fn credentials(store: Arc<Store>, client: Arc<Client>, env: &str) -> FoundationCredentials {
    let env = env.to_owned();
    FoundationCredentials::new(
        store,
        Arc::new(move |name| {
            if name == "SYNTHETIC_KEY" {
                Some(env.clone())
            } else {
                None
            }
        }),
        client,
    )
}
#[tokio::test]
async fn environment_precedes_store_and_ignores_cancellation_exactly_like_foundation_source() {
    let store = Arc::new(Store::new(Some(Credential::for_api_key("stored"))));
    let c = credentials(store.clone(), Arc::new(Client::new()), "environment");
    let key = c.api_key("kimi-api", "SYNTHETIC_KEY");
    assert!(key.has_credential(&CANCELLED).await.unwrap());
    assert_eq!(key.api_key(&CANCELLED).await.unwrap(), "environment");
    assert_eq!(store.calls.load(Ordering::SeqCst), 0);
    let stored = c.api_key("kimi-api", "");
    assert_eq!(stored.api_key(&ACTIVE).await.unwrap(), "stored");
    assert_eq!(*store.names.lock().unwrap(), vec!["kimi-api"]);
}
#[tokio::test]
async fn empty_environment_uses_actual_store_and_preserves_version_type_and_mixed_payload_rules() {
    let mut old = Credential::for_api_key("old");
    old.version = 0;
    let mut mixed = Credential::for_api_key("mixed");
    mixed.oauth = Some(oauth("oauth"));
    for (value, available, key_value) in [
        (Some(Credential::for_api_key("stored")), true, "stored"),
        (Some(old), false, "old"),
        (Some(mixed), false, "mixed"),
        (Some(Credential::for_oauth(oauth("oauth"))), false, ""),
        (None, false, ""),
    ] {
        let store = Arc::new(Store::new(value));
        let c = credentials(store.clone(), Arc::new(Client::new()), "");
        let key = c.api_key("custom", "SYNTHETIC_KEY");
        assert_eq!(key.has_credential(&ACTIVE).await.unwrap(), available);
        assert_eq!(key.api_key(&ACTIVE).await.unwrap(), key_value);
        assert_eq!(store.calls.load(Ordering::SeqCst), 2);
    }
}
#[tokio::test]
async fn stored_auth_errors_are_retained_as_the_original_enum_and_cancellation_is_not_stringified()
{
    for error in [
        AuthError::Message("auth sentinel"),
        AuthError::Transport,
        AuthError::Io,
        AuthError::InvalidOperation("operation sentinel"),
        AuthError::Credential(parrot_foundation::credentials::CredentialError::MalformedStore),
        AuthError::Credential(
            parrot_foundation::credentials::CredentialError::UnsupportedVersion(9),
        ),
        AuthError::Http {
            operation: "refresh",
            status: 401,
        },
    ] {
        let store = Arc::new(Store::new(None));
        *store.value.lock().unwrap() = Err(error.clone());
        let c = credentials(store, Arc::new(Client::new()), "");
        let source = c.api_key("custom", "");
        assert_eq!(
            source.api_key(&ACTIVE).await.unwrap_err(),
            Error::Authentication(error.clone())
        );
        assert_eq!(
            source.has_credential(&ACTIVE).await.unwrap_err(),
            Error::Authentication(error)
        );
        assert_eq!(
            source.api_key(&CANCELLED).await.unwrap_err(),
            Error::Cancelled
        );
    }
}
#[tokio::test]
async fn oauth_reuses_actual_foundation_source_without_refresh_for_fresh_credentials() {
    let store = Arc::new(Store::new(Some(Credential::for_oauth(oauth("fresh")))));
    let client = Arc::new(Client::new());
    let c = credentials(store.clone(), client.clone(), "ignored");
    let source = c.oauth("chatgpt");
    assert!(source.has_credential(&ACTIVE).await.unwrap());
    assert_eq!(
        source.token(&ACTIVE).await.unwrap(),
        OAuthAccess {
            access_token: "fresh".into(),
            account_id: "synthetic-account".into()
        }
    );
    assert_eq!(client.calls.load(Ordering::SeqCst), 0);
    assert!(
        store
            .names
            .lock()
            .unwrap()
            .iter()
            .all(|name| name == "chatgpt")
    );
    c.settle().await.unwrap();
}
#[tokio::test]
async fn oauth_refresh_and_original_auth_errors_are_delegated_to_foundation() {
    for error in [
        None,
        Some(AuthError::Message("refresh rejected")),
        Some(AuthError::Transport),
    ] {
        let mut stale = oauth("stale");
        stale.expires_at = "2020-01-01T00:00:00Z".into();
        let store = Arc::new(Store::new(Some(Credential::for_oauth(stale))));
        let mut client = Client::new();
        if let Some(error) = error.clone() {
            client.result = Err(error);
        }
        let client = Arc::new(client);
        let c = credentials(store, client.clone(), "");
        let source = c.oauth("chatgpt");
        let result = source.token(&ACTIVE).await;
        match error {
            None => assert_eq!(result.unwrap().access_token, "refreshed"),
            Some(error) => assert_eq!(result.unwrap_err(), Error::Authentication(error)),
        };
        assert_eq!(client.calls.load(Ordering::SeqCst), 1);
        c.settle().await.unwrap();
    }
}
#[tokio::test]
async fn oauth_initial_cancellation_is_visible_to_the_actual_store() {
    let store = Arc::new(Store::new(Some(Credential::for_oauth(oauth("fresh")))));
    let c = credentials(store, Arc::new(Client::new()), "");
    let source = c.oauth("chatgpt");
    assert_eq!(
        source.token(&CANCELLED).await.unwrap_err(),
        Error::Cancelled
    );
    c.settle().await.unwrap();
}
#[tokio::test]
async fn oauth_ready_credential_store_that_ignores_cancellation_retains_source_ready_result() {
    let mut store = Store::new(Some(Credential::for_oauth(oauth("fresh"))));
    store.honor_cancellation = false;
    let c = credentials(Arc::new(store), Arc::new(Client::new()), "");
    let source = c.oauth("chatgpt");
    assert_eq!(
        source.token(&CANCELLED).await.unwrap().access_token,
        "fresh"
    );
    c.settle().await.unwrap();
}
#[tokio::test]
async fn oauth_pending_operation_observes_caller_cancellation_and_settles_without_detached_watcher()
{
    struct Signal(tokio::sync::watch::Sender<bool>);
    impl Cancellation for Signal {
        fn is_cancelled(&self) -> bool {
            *self.0.borrow()
        }
        fn cancelled(&self) -> BoxFuture<'_, ()> {
            let mut receiver = self.0.subscribe();
            Box::pin(async move {
                while !*receiver.borrow_and_update() {
                    if receiver.changed().await.is_err() {
                        return;
                    }
                }
            })
        }
    }
    let mut store = Store::new(None);
    store.pending = true;
    let store = Arc::new(store);
    let c = credentials(store.clone(), Arc::new(Client::new()), "");
    let source = c.oauth("chatgpt");
    let token = Signal(tokio::sync::watch::channel(false).0);
    let mut future = source.token(&token);
    tokio::select! {biased; result=&mut future=>panic!("unexpected token {result:?}"), _=store.entered.notified()=>{}}
    token.0.send_replace(true);
    assert_eq!(
        tokio::time::timeout(Duration::from_secs(1), future)
            .await
            .unwrap()
            .unwrap_err(),
        Error::Cancelled
    );
    c.settle().await.unwrap();
    assert_eq!(store.exits.load(Ordering::SeqCst), 1);
}
#[tokio::test]
async fn dropping_oauth_observation_cancels_its_owned_request_and_scope_settlement_joins_it() {
    let mut store = Store::new(None);
    store.pending = true;
    let store = Arc::new(store);
    let c = credentials(store.clone(), Arc::new(Client::new()), "");
    let source = c.oauth("chatgpt");
    let mut future = source.token(&ACTIVE);
    tokio::select! {biased; result=&mut future=>panic!("unexpected token {result:?}"), _=store.entered.notified()=>{}}
    drop(future);
    tokio::time::timeout(Duration::from_secs(1), c.settle())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(store.exits.load(Ordering::SeqCst), 1);
}
