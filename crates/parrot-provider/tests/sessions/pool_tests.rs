use parrot_domain::contracts::async_ops::{BoxFuture, Cancellation};
use parrot_provider::sessions::{
    Error, Event, EventStream, Provider, ProviderSessions, Request, Session,
    diagnostics::{DiagnosticEvent, DiagnosticSink, MonotonicDiagnosticClock},
};
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, AtomicUsize, Ordering},
};
use tokio::sync::Semaphore;

struct EmptyStream;
impl EventStream for EmptyStream {
    fn next<'a>(
        &'a mut self,
        _: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<Option<Event>, Error>> {
        Box::pin(async { Ok(None) })
    }
    fn close(&mut self) -> BoxFuture<'_, Result<(), Error>> {
        Box::pin(async { Ok(()) })
    }
}

struct TestSession {
    calls: AtomicUsize,
    fallback_calls: AtomicUsize,
    supports_fallback: AtomicBool,
    turns: AtomicUsize,
    closes: AtomicUsize,
    failure: Option<Error>,
    blocking: bool,
    started: Semaphore,
    release: Semaphore,
}
impl Session for TestSession {
    fn begin_turn(&self) -> BoxFuture<'_, Result<(), Error>> {
        Box::pin(async {
            self.turns.fetch_add(1, Ordering::SeqCst);
            Ok(())
        })
    }
    fn call(&self, _: Request) -> Result<Box<dyn EventStream>, Error> {
        if self.closes.load(Ordering::SeqCst) != 0 {
            return Err(Error::Disposed);
        }
        self.calls.fetch_add(1, Ordering::SeqCst);
        Ok(Box::new(EmptyStream))
    }
    fn try_fall_back_to_http(&self) -> BoxFuture<'_, Result<bool, Error>> {
        Box::pin(async {
            self.fallback_calls.fetch_add(1, Ordering::SeqCst);
            Ok(self.supports_fallback.load(Ordering::SeqCst))
        })
    }
    fn close(&self) -> BoxFuture<'_, Result<(), Error>> {
        Box::pin(async {
            self.closes.fetch_add(1, Ordering::SeqCst);
            self.started.add_permits(1);
            if self.blocking {
                self.release.acquire().await.unwrap().forget();
            }
            self.failure.clone().map_or(Ok(()), Err)
        })
    }
}

struct TestProvider {
    opened: AtomicUsize,
    direct_calls: AtomicUsize,
    sessions: Mutex<Vec<Arc<TestSession>>>,
    failure: Option<Error>,
    blocking: bool,
}
impl TestProvider {
    fn new(failure: Option<Error>, blocking: bool) -> Arc<Self> {
        Arc::new(Self {
            opened: AtomicUsize::new(0),
            direct_calls: AtomicUsize::new(0),
            sessions: Mutex::new(Vec::new()),
            failure,
            blocking,
        })
    }
    fn session(&self, index: usize) -> Arc<TestSession> {
        self.sessions.lock().unwrap()[index].clone()
    }
}
impl Provider for TestProvider {
    fn id(&self) -> &str {
        "session-provider"
    }
    fn call(&self, _: Request) -> Result<Box<dyn EventStream>, Error> {
        self.direct_calls.fetch_add(1, Ordering::SeqCst);
        Err(Error::InvalidOperation(
            "Scoped calls must not use the shared provider directly.".into(),
        ))
    }
    fn has_credential<'a>(&'a self, _: &'a dyn Cancellation) -> BoxFuture<'a, Result<bool, Error>> {
        Box::pin(async { Ok(true) })
    }
    fn seed_models(&self) -> Vec<parrot_provider::catalogs::Model> {
        Vec::new()
    }
    fn list_models<'a>(
        &'a self,
        _: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<Vec<parrot_provider::catalogs::Model>, Error>> {
        Box::pin(async { Ok(Vec::new()) })
    }
    fn calculate_image_tokens(
        &self,
        _: &parrot_provider::catalogs::Model,
        _: &parrot_provider::sessions::Content,
    ) -> i64 {
        0
    }
    fn open_session(&self) -> Result<Arc<dyn Session>, Error> {
        self.opened.fetch_add(1, Ordering::SeqCst);
        let session = Arc::new(TestSession {
            calls: AtomicUsize::new(0),
            fallback_calls: AtomicUsize::new(0),
            supports_fallback: AtomicBool::new(false),
            turns: AtomicUsize::new(0),
            closes: AtomicUsize::new(0),
            failure: self.failure.clone(),
            blocking: self.blocking,
            started: Semaphore::new(0),
            release: Semaphore::new(0),
        });
        self.sessions.lock().unwrap().push(session.clone());
        Ok(session)
    }
}

#[derive(Default)]
struct RecordingDiagnostics(Mutex<Vec<DiagnosticEvent>>);
impl DiagnosticSink for RecordingDiagnostics {
    fn write(&self, event: DiagnosticEvent) {
        self.0.lock().unwrap().push(event);
    }
}

#[tokio::test]
async fn configured_sessions_trace_once_per_call_and_retain_the_real_owner() {
    let provider = TestProvider::new(None, false);
    let sink = Arc::new(RecordingDiagnostics::default());
    let owner = ProviderSessions::with_diagnostics(
        sink.clone(),
        Arc::new(MonotonicDiagnosticClock::default()),
        "diagnostic-agent".into(),
        None,
    );
    owner.begin_turn().await.unwrap();
    let session = owner.get(provider.clone()).await.unwrap();
    assert!(Arc::ptr_eq(
        &session,
        &owner.get(provider.clone()).await.unwrap()
    ));
    assert_eq!(provider.session(0).turns.load(Ordering::SeqCst), 1);
    drop(session.call(Request::default()).unwrap());
    assert!(
        sink.0.lock().unwrap().is_empty(),
        "unpulled calls stay lazy"
    );
    drain_session(session.as_ref()).await;
    drain_session(session.as_ref()).await;
    owner.close().await.unwrap();
    owner.close().await.unwrap();
    assert_eq!(provider.opened.load(Ordering::SeqCst), 1);
    assert_eq!(provider.session(0).calls.load(Ordering::SeqCst), 2);
    assert_eq!(provider.session(0).closes.load(Ordering::SeqCst), 1);
    let events = sink.0.lock().unwrap();
    assert_eq!(events.len(), 4);
    for call in events.chunks_exact(2) {
        assert_eq!(call[0].operation, "call_started");
        assert_eq!(call[1].operation, "call_finished");
        assert_eq!(call[1].outcome.as_deref(), Some("completed"));
        assert_eq!(call[0].correlation_id, call[1].correlation_id);
        for event in call {
            assert_eq!(event.agent_session_id.as_deref(), Some("diagnostic-agent"));
            assert_eq!(event.provider_id.as_deref(), Some("session-provider"));
            assert_eq!(event.model_id.as_deref(), Some("model"));
        }
    }
    assert_ne!(events[0].correlation_id, events[2].correlation_id);
}

#[tokio::test]
async fn sessions_are_lazy_shared_within_an_owner_and_isolated_across_owners() {
    let provider = TestProvider::new(None, false);
    let first_owner = ProviderSessions::new();
    let second_owner = ProviderSessions::new();
    assert_eq!(provider.opened.load(Ordering::SeqCst), 0);
    let first = first_owner.get(provider.clone()).await.unwrap();
    let again = first_owner.get(provider.clone()).await.unwrap();
    let second = second_owner.get(provider.clone()).await.unwrap();
    assert!(Arc::ptr_eq(&first, &again));
    assert!(!Arc::ptr_eq(&first, &second));
    for session in [&first, &again, &second] {
        drain_session(session.as_ref()).await;
    }
    assert_eq!(provider.opened.load(Ordering::SeqCst), 2);
    assert_eq!(provider.direct_calls.load(Ordering::SeqCst), 0);
    assert_eq!(provider.session(0).calls.load(Ordering::SeqCst), 2);
    assert_eq!(provider.session(1).calls.load(Ordering::SeqCst), 1);
    first_owner.close().await.unwrap();
    assert_eq!(provider.session(0).closes.load(Ordering::SeqCst), 1);
    assert_eq!(provider.session(1).closes.load(Ordering::SeqCst), 0);
    assert!(matches!(
        first_owner.get(provider.clone()).await,
        Err(Error::Disposed)
    ));
    assert_eq!(first_owner.begin_turn().await, Err(Error::Disposed));
    second_owner.close().await.unwrap();
    second_owner.close().await.unwrap();
    assert_eq!(provider.session(1).closes.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn provider_identity_not_equal_ids_keys_the_cache_and_is_retained() {
    let first_provider = TestProvider::new(None, false);
    let second_provider = TestProvider::new(None, false);
    let weak = Arc::downgrade(&first_provider);
    let owner = ProviderSessions::new();
    let first = owner.get(first_provider.clone()).await.unwrap();
    let second = owner.get(second_provider.clone()).await.unwrap();
    assert_eq!(first_provider.id(), second_provider.id());
    assert!(!Arc::ptr_eq(&first, &second));
    drop(first_provider);
    assert!(weak.upgrade().is_some());
    owner.close().await.unwrap();
    assert!(weak.upgrade().is_none());
    assert_eq!(second_provider.session(0).closes.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn begin_turn_notifies_existing_and_lazy_sessions() {
    let first = TestProvider::new(None, false);
    let second = TestProvider::new(None, false);
    let owner = ProviderSessions::new();
    owner.get(first.clone()).await.unwrap();
    owner.begin_turn().await.unwrap();
    owner.get(second.clone()).await.unwrap();
    owner.begin_turn().await.unwrap();
    owner.close().await.unwrap();
    for provider in [first, second] {
        assert_eq!(provider.session(0).turns.load(Ordering::SeqCst), 2);
        assert_eq!(provider.opened.load(Ordering::SeqCst), 1);
    }
    assert!(!Never.is_cancelled());
}

async fn concurrent_closes(fail: bool) {
    let expected = if fail {
        Err(Error::InvalidOperation("dispose failed".into()))
    } else {
        Ok(())
    };
    let provider = TestProvider::new(expected.clone().err(), true);
    let owner = Arc::new(ProviderSessions::new());
    owner.get(provider.clone()).await.unwrap();
    let session = provider.session(0);
    let first = {
        let owner = owner.clone();
        tokio::spawn(async move { owner.close().await })
    };
    session.started.acquire().await.unwrap().forget();
    let second = {
        let owner = owner.clone();
        tokio::spawn(async move { owner.close().await })
    };
    tokio::task::yield_now().await;
    assert!(!first.is_finished());
    assert!(!second.is_finished());
    assert_eq!(session.closes.load(Ordering::SeqCst), 1);
    assert!(matches!(
        owner.get(provider.clone()).await,
        Err(Error::Disposed)
    ));
    assert_eq!(owner.begin_turn().await, Err(Error::Disposed));
    session.release.add_permits(1);
    assert_eq!(first.await.unwrap(), expected);
    assert_eq!(second.await.unwrap(), expected);
    assert_eq!(owner.close().await, expected);
    assert_eq!(session.closes.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn concurrent_closes_await_one_completed_disposal_and_repeat_success() {
    concurrent_closes(false).await;
}

#[tokio::test]
async fn concurrent_closes_await_one_completed_disposal_and_repeat_failure() {
    concurrent_closes(true).await;
}

#[tokio::test]
async fn close_attempts_all_sessions_sequentially_and_caches_the_first_failure() {
    let first = TestProvider::new(Some(Error::Provider("first failure".into())), true);
    let second = TestProvider::new(Some(Error::Provider("second failure".into())), false);
    let third = TestProvider::new(None, false);
    let owner = Arc::new(ProviderSessions::new());
    for provider in [&first, &second, &third] {
        owner.get(provider.clone()).await.unwrap();
    }
    let close = {
        let owner = owner.clone();
        tokio::spawn(async move { owner.close().await })
    };
    first.session(0).started.acquire().await.unwrap().forget();
    assert_eq!(second.session(0).closes.load(Ordering::SeqCst), 0);
    assert_eq!(third.session(0).closes.load(Ordering::SeqCst), 0);
    first.session(0).release.add_permits(1);
    let expected = Err(Error::Provider("first failure".into()));
    assert_eq!(close.await.unwrap(), expected);
    assert_eq!(owner.close().await, expected);
    for provider in [first, second, third] {
        assert_eq!(provider.session(0).closes.load(Ordering::SeqCst), 1);
    }
}

#[tokio::test]
async fn dropping_a_close_observer_does_not_cancel_owned_cleanup() {
    let provider = TestProvider::new(None, true);
    let owner = Arc::new(ProviderSessions::new());
    owner.get(provider.clone()).await.unwrap();
    let close = {
        let owner = owner.clone();
        tokio::spawn(async move { owner.close().await })
    };
    let session = provider.session(0);
    session.started.acquire().await.unwrap().forget();
    close.abort();
    assert!(close.await.unwrap_err().is_cancelled());
    session.release.add_permits(1);
    owner.close().await.unwrap();
    assert_eq!(session.closes.load(Ordering::SeqCst), 1);
}

struct Never;
impl Cancellation for Never {
    fn is_cancelled(&self) -> bool {
        false
    }
    fn cancelled(&self) -> BoxFuture<'_, ()> {
        Box::pin(std::future::pending())
    }
}
async fn drain_session(session: &dyn Session) {
    let mut stream = session
        .call(Request {
            model: "model".into(),
            ..Request::default()
        })
        .unwrap();
    while stream.next(&Never).await.unwrap().is_some() {}
    stream.close().await.unwrap();
}

#[tokio::test]
async fn retrying_provider_opens_and_disposes_one_inner_session() {
    let provider = TestProvider::new(None, false);
    let owner = ProviderSessions::new();
    let retrying =
        Arc::new(parrot_provider::sessions::decorators::RetryingProvider::new(provider.clone(), 5));
    let session = owner.get(retrying).await.unwrap();
    drain_session(session.as_ref()).await;
    drain_session(session.as_ref()).await;
    owner.close().await.unwrap();
    assert_eq!(provider.opened.load(Ordering::SeqCst), 1);
    assert_eq!(provider.direct_calls.load(Ordering::SeqCst), 0);
    assert_eq!(provider.session(0).calls.load(Ordering::SeqCst), 2);
    assert_eq!(provider.session(0).closes.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn retrying_session_forwards_the_inner_fallback_outcome_for_both_source_arguments() {
    for supported in [false, true] {
        let provider = TestProvider::new(None, false);
        let retrying =
            parrot_provider::sessions::decorators::RetryingProvider::new(provider.clone(), 5);
        let session = retrying.open_session().unwrap();
        let recorded = provider.session(0);
        recorded
            .supports_fallback
            .store(supported, Ordering::SeqCst);
        assert_eq!(session.try_fall_back_to_http().await.unwrap(), supported);
        assert_eq!(recorded.fallback_calls.load(Ordering::SeqCst), 1);
        assert_eq!(recorded.calls.load(Ordering::SeqCst), 0);
        session.close().await.unwrap();
    }
}
