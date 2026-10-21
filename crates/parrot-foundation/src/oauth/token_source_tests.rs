use super::*;
use crate::oauth::DeviceAuthorization;
use std::collections::BTreeMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::task::Poll;
use tokio::sync::{Notify, Semaphore};

const NOW_SECONDS: u64 = 1_767_225_600;

#[derive(Default)]
struct TestCancellation {
    cancelled: AtomicBool,
    wake: Notify,
    polled: Notify,
}
impl TestCancellation {
    fn cancel(&self) {
        self.cancelled.store(true, Ordering::Release);
        self.wake.notify_waiters();
    }
}
impl Cancellation for TestCancellation {
    fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::Acquire)
    }
    fn cancelled(&self) -> BoxFuture<'_, ()> {
        Box::pin(async move {
            self.polled.notify_one();
            loop {
                let notified = self.wake.notified();
                if self.is_cancelled() {
                    return;
                }
                notified.await;
            }
        })
    }
}

struct Gate {
    entered: Notify,
    release: Semaphore,
}
impl Gate {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            entered: Notify::new(),
            release: Semaphore::new(0),
        })
    }
    async fn wait(&self, cancellation: &dyn Cancellation) -> OperationResult<(), AuthError> {
        self.entered.notify_one();
        tokio::select! {
            biased;
            _ = cancellation.cancelled() => Err(OperationError::Cancelled),
            permit = self.release.acquire() => {
                permit.expect("test gate open").forget();
                Ok(())
            }
        }
    }
    fn open(&self) {
        self.release.add_permits(1);
    }
}

struct MemoryStore {
    value: Mutex<Option<Credential>>,
    gets: AtomicUsize,
    sets: AtomicUsize,
    get_gates: BTreeMap<usize, Arc<Gate>>,
    set_gate: Option<Arc<Gate>>,
    fail_set: AtomicBool,
}
impl MemoryStore {
    fn new(expiry: &str) -> Self {
        Self {
            value: Mutex::new(Some(Credential::for_oauth(oauth("stale", expiry)))),
            gets: AtomicUsize::new(0),
            sets: AtomicUsize::new(0),
            get_gates: BTreeMap::new(),
            set_gate: None,
            fail_set: AtomicBool::new(false),
        }
    }
    fn saved(&self) -> OAuthCredential {
        self.value
            .lock()
            .unwrap()
            .as_ref()
            .unwrap()
            .oauth
            .clone()
            .unwrap()
    }
}
impl CredentialStorage for MemoryStore {
    fn get<'a>(
        &'a self,
        name: &'a str,
        cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, OperationResult<Option<Credential>, AuthError>> {
        Box::pin(async move {
            assert_eq!(name, "openai");
            let index = self.gets.fetch_add(1, Ordering::AcqRel) + 1;
            if let Some(gate) = self.get_gates.get(&index) {
                gate.wait(cancellation).await?;
            }
            if cancellation.is_cancelled() {
                return Err(OperationError::Cancelled);
            }
            Ok(self.value.lock().unwrap().clone())
        })
    }
    fn set<'a>(
        &'a self,
        name: &'a str,
        credential: &'a Credential,
        cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, OperationResult<(), AuthError>> {
        Box::pin(async move {
            assert_eq!(name, "openai");
            self.sets.fetch_add(1, Ordering::AcqRel);
            if let Some(gate) = &self.set_gate {
                gate.wait(cancellation).await?;
            }
            if cancellation.is_cancelled() {
                return Err(OperationError::Cancelled);
            }
            if self.fail_set.load(Ordering::Acquire) {
                return Err(OperationError::Failed(AuthError::Message(
                    "test persistence failure",
                )));
            }
            *self.value.lock().unwrap() = Some(credential.clone());
            Ok(())
        })
    }
}

struct RefreshClient {
    refreshes: AtomicUsize,
    refresh_gate: Arc<Gate>,
}
impl RefreshClient {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            refreshes: AtomicUsize::new(0),
            refresh_gate: Gate::new(),
        })
    }
}
impl OAuthClient for RefreshClient {
    fn now(&self) -> SystemTime {
        SystemTime::UNIX_EPOCH + Duration::from_secs(NOW_SECONDS)
    }
    fn authorization_url(&self, _: &str, _: &str, _: &str) -> String {
        panic!("not part of token-source test")
    }
    fn browser_login<'a>(
        &'a self,
        _: &'a dyn Cancellation,
    ) -> BoxFuture<'a, OperationResult<OAuthCredential, AuthError>> {
        panic!("not part of token-source test")
    }
    fn start_device_authorization<'a>(
        &'a self,
        _: &'a dyn Cancellation,
    ) -> BoxFuture<'a, OperationResult<DeviceAuthorization, AuthError>> {
        panic!("not part of token-source test")
    }
    fn await_device_authorization<'a>(
        &'a self,
        _: &'a DeviceAuthorization,
        _: &'a dyn Cancellation,
    ) -> BoxFuture<'a, OperationResult<OAuthCredential, AuthError>> {
        panic!("not part of token-source test")
    }
    fn refresh<'a>(
        &'a self,
        current: &'a OAuthCredential,
        cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, OperationResult<OAuthCredential, AuthError>> {
        Box::pin(async move {
            self.refreshes.fetch_add(1, Ordering::AcqRel);
            self.refresh_gate.wait(cancellation).await?;
            let mut rotated = oauth("rotated", "2026-01-01T01:00:00+00:00");
            rotated.refresh_token = crate::credentials::Secret::new("rotated-refresh");
            rotated.account_id.clone_from(&current.account_id);
            Ok(rotated)
        })
    }
}

fn oauth(access: &str, expiry: &str) -> OAuthCredential {
    OAuthCredential::create(access, "refresh", expiry, "acct")
}
fn source(store: &Arc<MemoryStore>, client: &Arc<RefreshClient>) -> OAuthTokenSource {
    OAuthTokenSource::new(store.clone(), client.clone(), "openai")
}
async fn bounded<T>(future: impl Future<Output = T>) -> T {
    tokio::time::timeout(Duration::from_secs(5), future)
        .await
        .expect("controlled test operation settled")
}
async fn pending<T>(mut future: Pin<&mut (impl Future<Output = T> + ?Sized)>) {
    std::future::poll_fn(|context| match future.as_mut().poll(context) {
        Poll::Pending => Poll::Ready(()),
        Poll::Ready(_) => panic!("operation completed before controlled release"),
    })
    .await;
}
async fn joined(source: &OAuthTokenSource, cancellation: &TestCancellation) {
    bounded(cancellation.polled.notified()).await;
    let gate = source.inner.flight.lock().await;
    let flight = gate.as_ref().expect("refresh flight installed");
    // Current-thread tests cannot resume here until the token task's admission poll
    // reaches its pending flight wait. The extra Arc is held by that real waiter.
    assert!(Arc::strong_count(flight) >= 3);
}

#[tokio::test]
async fn fresh_token_is_returned_unchanged_without_refresh() {
    let store = Arc::new(MemoryStore::new("2026-01-01T00:30:00+00:00"));
    let client = RefreshClient::new();
    let source = source(&store, &client);
    let access = bounded(source.token(Arc::new(TestCancellation::default())))
        .await
        .unwrap();
    assert_eq!(access.access_token.value(), "stale");
    assert_eq!(access.account_id.as_deref(), Some("acct"));
    assert_eq!(client.refreshes.load(Ordering::Acquire), 0);
    assert_eq!(store.sets.load(Ordering::Acquire), 0);
    bounded(source.settle()).await.unwrap();
}

#[tokio::test]
async fn expired_structurally_valid_credential_is_available_without_refresh() {
    let store = Arc::new(MemoryStore::new("2025-12-31T23:59:00+00:00"));
    let client = RefreshClient::new();
    let source = source(&store, &client);
    assert!(
        bounded(source.has_credential(&TestCancellation::default()))
            .await
            .unwrap()
    );
    assert_eq!(client.refreshes.load(Ordering::Acquire), 0);
    assert_eq!(store.sets.load(Ordering::Acquire), 0);
}

#[tokio::test]
async fn twelve_concurrent_expiring_callers_share_one_persisted_refresh() {
    let store = Arc::new(MemoryStore::new("2026-01-01T00:01:00+00:00"));
    let client = RefreshClient::new();
    let source = source(&store, &client);
    let first = source.token(Arc::new(TestCancellation::default()));
    bounded(client.refresh_gate.entered.notified()).await;
    let mut observers = vec![first];
    for _ in 1..12 {
        let cancellation = Arc::new(TestCancellation::default());
        observers.push(source.token(cancellation.clone()));
        joined(&source, &cancellation).await;
    }
    assert_eq!(client.refreshes.load(Ordering::Acquire), 1);
    assert_eq!(store.sets.load(Ordering::Acquire), 0);
    client.refresh_gate.open();
    for observer in observers {
        let access = bounded(observer).await.unwrap();
        assert_eq!(access.access_token.value(), "rotated");
        assert_eq!(access.account_id.as_deref(), Some("acct"));
    }
    assert_eq!(store.sets.load(Ordering::Acquire), 1);
    assert_eq!(store.saved().refresh_token.value(), "rotated-refresh");
    assert_eq!(store.saved().account_id.as_deref(), Some("acct"));
    bounded(source.settle()).await.unwrap();
}

#[tokio::test]
async fn exactly_five_minutes_to_expiry_requires_refresh() {
    let store = Arc::new(MemoryStore::new("2026-01-01T00:05:00+00:00"));
    let client = RefreshClient::new();
    let source = source(&store, &client);
    let observer = source.token(Arc::new(TestCancellation::default()));
    bounded(client.refresh_gate.entered.notified()).await;
    assert_eq!(client.refreshes.load(Ordering::Acquire), 1);
    client.refresh_gate.open();
    assert_eq!(
        bounded(observer).await.unwrap().access_token.value(),
        "rotated"
    );
    bounded(source.settle()).await.unwrap();
}

#[tokio::test]
async fn initiator_cancellation_cancels_shared_flight_and_allows_retry() {
    let store = Arc::new(MemoryStore::new("2026-01-01T00:01:00+00:00"));
    let client = RefreshClient::new();
    let source = source(&store, &client);
    let initiator = Arc::new(TestCancellation::default());
    let first = source.token(initiator.clone());
    bounded(client.refresh_gate.entered.notified()).await;
    let joiner = Arc::new(TestCancellation::default());
    let second = source.token(joiner.clone());
    joined(&source, &joiner).await;
    initiator.cancel();
    assert_eq!(bounded(first).await, Err(OperationError::Cancelled));
    assert_eq!(bounded(second).await, Err(OperationError::Cancelled));
    assert!(source.inner.flight.lock().await.is_none());
    assert_eq!(store.sets.load(Ordering::Acquire), 0);
    let retry = source.token(Arc::new(TestCancellation::default()));
    bounded(client.refresh_gate.entered.notified()).await;
    client.refresh_gate.open();
    assert_eq!(
        bounded(retry).await.unwrap().access_token.value(),
        "rotated"
    );
    assert_eq!(client.refreshes.load(Ordering::Acquire), 2);
    assert_eq!(store.sets.load(Ordering::Acquire), 1);
    bounded(source.settle()).await.unwrap();
}

#[tokio::test]
async fn admitted_joiner_cancellation_does_not_cancel_shared_wait_or_refresh() {
    let store = Arc::new(MemoryStore::new("2026-01-01T00:01:00+00:00"));
    let client = RefreshClient::new();
    let source = source(&store, &client);
    let first = source.token(Arc::new(TestCancellation::default()));
    bounded(client.refresh_gate.entered.notified()).await;
    let joiner = Arc::new(TestCancellation::default());
    let mut second = source.token(joiner.clone());
    joined(&source, &joiner).await;
    joiner.cancel();
    pending(second.as_mut()).await;
    assert_eq!(client.refreshes.load(Ordering::Acquire), 1);
    client.refresh_gate.open();
    assert_eq!(
        bounded(first).await.unwrap().access_token.value(),
        "rotated"
    );
    assert_eq!(
        bounded(second).await.unwrap().access_token.value(),
        "rotated"
    );
    bounded(source.settle()).await.unwrap();
}

#[tokio::test]
async fn dropped_refresh_observer_does_not_stop_persistence_or_settlement() {
    let store = Arc::new(MemoryStore::new("2026-01-01T00:01:00+00:00"));
    let client = RefreshClient::new();
    let source = source(&store, &client);
    let observer = source.token(Arc::new(TestCancellation::default()));
    bounded(client.refresh_gate.entered.notified()).await;
    drop(observer);
    let mut settlement = source.settle();
    pending(settlement.as_mut()).await;
    assert_eq!(store.sets.load(Ordering::Acquire), 0);
    client.refresh_gate.open();
    bounded(settlement).await.unwrap();
    assert_eq!(store.saved().access_token.value(), "rotated");
    assert_eq!(store.sets.load(Ordering::Acquire), 1);
    assert!(source.tasks.lock().unwrap().is_empty());
    assert!(source.inner.flight.lock().await.is_none());
}

async fn dropped_read_observer_keeps_operation_alive(read_index: usize) {
    let gate = Gate::new();
    let mut store = MemoryStore::new("2026-01-01T00:01:00+00:00");
    store.get_gates.insert(read_index, gate.clone());
    let store = Arc::new(store);
    let client = RefreshClient::new();
    let source = source(&store, &client);
    let observer = source.token(Arc::new(TestCancellation::default()));
    bounded(gate.entered.notified()).await;
    if read_index == 2 {
        assert!(source.inner.flight.try_lock().is_err());
    }
    drop(observer);
    assert!(!source.tasks.lock().unwrap()[0].is_finished());
    gate.open();
    bounded(client.refresh_gate.entered.notified()).await;
    client.refresh_gate.open();
    bounded(source.settle()).await.unwrap();
    assert_eq!(store.saved().access_token.value(), "rotated");
    assert_eq!(store.sets.load(Ordering::Acquire), 1);
    assert!(source.tasks.lock().unwrap().is_empty());
}

#[tokio::test]
async fn dropped_initial_read_observer_keeps_owned_operation_alive() {
    dropped_read_observer_keeps_operation_alive(1).await;
}

#[tokio::test]
async fn dropped_gated_reread_observer_keeps_owned_operation_alive() {
    dropped_read_observer_keeps_operation_alive(2).await;
}

#[tokio::test]
async fn persistence_must_settle_and_failure_is_shared_before_token_success() {
    let set_gate = Gate::new();
    let mut store = MemoryStore::new("2026-01-01T00:01:00+00:00");
    store.set_gate = Some(set_gate.clone());
    store.fail_set.store(true, Ordering::Release);
    let store = Arc::new(store);
    let client = RefreshClient::new();
    let source = source(&store, &client);
    let mut first = source.token(Arc::new(TestCancellation::default()));
    bounded(client.refresh_gate.entered.notified()).await;
    let joiner = Arc::new(TestCancellation::default());
    let mut second = source.token(joiner.clone());
    joined(&source, &joiner).await;
    client.refresh_gate.open();
    bounded(set_gate.entered.notified()).await;
    pending(first.as_mut()).await;
    pending(second.as_mut()).await;
    assert_eq!(store.saved().access_token.value(), "stale");
    set_gate.open();
    let expected = Err(OperationError::Failed(AuthError::Message(
        "test persistence failure",
    )));
    assert_eq!(bounded(first).await, expected);
    assert_eq!(bounded(second).await, expected);
    assert!(source.inner.flight.lock().await.is_none());
    assert_eq!(store.saved().access_token.value(), "stale");
    bounded(source.settle()).await.unwrap();
}

#[tokio::test]
async fn settlement_waits_for_all_admitted_owned_token_operations() {
    let store = Arc::new(MemoryStore::new("2026-01-01T00:01:00+00:00"));
    let client = RefreshClient::new();
    let source = source(&store, &client);
    let first = source.token(Arc::new(TestCancellation::default()));
    bounded(client.refresh_gate.entered.notified()).await;
    let joiner = Arc::new(TestCancellation::default());
    let second = source.token(joiner.clone());
    joined(&source, &joiner).await;
    assert_eq!(source.tasks.lock().unwrap().len(), 2);
    let mut settlement = source.settle();
    pending(settlement.as_mut()).await;
    client.refresh_gate.open();
    bounded(settlement).await.unwrap();
    assert_eq!(
        bounded(first).await.unwrap().access_token.value(),
        "rotated"
    );
    assert_eq!(
        bounded(second).await.unwrap().access_token.value(),
        "rotated"
    );
    assert!(source.tasks.lock().unwrap().is_empty());
    assert!(source.inner.flight.lock().await.is_none());
}
