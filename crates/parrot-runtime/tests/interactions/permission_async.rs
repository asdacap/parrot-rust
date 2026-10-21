use super::*;
use parrot_domain::contracts::events::{EventPublisher, EventSubscription};
use parrot_runtime::event_broker::EventBroker;

#[derive(Default)]
struct Barrier {
    released: AtomicBool,
    changed: Notify,
}
impl Barrier {
    fn release(&self) {
        self.released.store(true, Ordering::SeqCst);
        self.changed.notify_waiters();
    }
    async fn wait(&self) {
        loop {
            let changed = self.changed.notified();
            tokio::pin!(changed);
            changed.as_mut().enable();
            if self.released.load(Ordering::SeqCst) {
                return;
            }
            changed.await;
        }
    }
}
#[derive(Default)]
struct AsyncEvents {
    append: Barrier,
    publish: Barrier,
    pending: Mutex<Option<PendingPermission<String>>>,
    receipt: Mutex<Option<Event>>,
    published: Mutex<Option<Event>>,
    fail_append: AtomicBool,
    fail_publish: AtomicBool,
    fanout: EventBroker,
    publication_token: Token,
}
impl AsyncEvents {
    fn id(&self) -> String {
        self.pending.lock().unwrap().as_ref().unwrap().id.clone()
    }
}
impl PermissionEvents<String> for AsyncEvents {
    fn append<'a>(
        &'a self,
        pending: &'a PendingPermission<String>,
    ) -> BoxFuture<'a, Result<Committed<Event>, InteractionError>> {
        Box::pin(async move {
            *self.pending.lock().unwrap() = Some(pending.clone());
            self.append.wait().await;
            if self.fail_append.load(Ordering::SeqCst) {
                return Err(delivery_error("append"));
            }
            // Fixture attestation only: this test does not claim SQLite durability.
            let event = Event {
                id: format!("stored/{}", pending.id),
                agent_session_id: pending.agent_session_id.clone(),
                ..Event::new()
            };
            *self.receipt.lock().unwrap() = Some(event.clone());
            Ok(Committed::after_commit(event))
        })
    }
    fn publish<'a>(
        &'a self,
        event: &'a Committed<Event>,
    ) -> BoxFuture<'a, Result<(), InteractionError>> {
        Box::pin(async move {
            self.publish.wait().await;
            if self.fail_publish.load(Ordering::SeqCst) {
                return Err(delivery_error("publish"));
            }
            *self.published.lock().unwrap() = Some(event.value().clone());
            self.fanout
                .publish_committed(event, &self.publication_token)
                .await
                .map_err(|_| delivery_error("fanout"))
        })
    }
}
fn delivery_error(stage: &str) -> InteractionError {
    InteractionError {
        kind: InteractionErrorKind::Delivery,
        message: stage.into(),
    }
}
fn setup(
    timeout: Option<Duration>,
) -> (PermissionBroker<String>, Arc<AsyncEvents>, Arc<ManualClock>) {
    let events = Arc::new(AsyncEvents::default());
    let clock = Arc::new(ManualClock::new());
    let broker = PermissionBroker::with_clock(
        true,
        timeout,
        clock.clone(),
        Arc::new(Diagnostics::default()),
        events.clone(),
    )
    .unwrap();
    (broker, events, clock)
}

#[tokio::test(flavor = "current_thread")]
async fn permission_async_snapshot_and_reply_wait_for_append_and_publication() {
    let (broker, events, _) = setup(None);
    let approval = Arc::new(Approval::default());
    let token = Token::default();
    let subscription = events.fanout.subscribe();
    let mut wait = Box::pin(broker.request(request(approval.clone(), "one"), &token));
    assert_pending(wait.as_mut());
    let id = events.id();
    let mut snapshot = Box::pin(broker.pending());
    let mut reply = Box::pin(broker.reply(&id, "grant", ""));
    assert_pending(snapshot.as_mut());
    assert_pending(reply.as_mut());
    assert!(approval.grants.lock().unwrap().is_empty());
    events.append.release();
    assert_pending(wait.as_mut());
    assert_pending(snapshot.as_mut());
    assert_pending(reply.as_mut());
    assert!(events.receipt.lock().unwrap().is_some());
    assert!(events.published.lock().unwrap().is_none());
    events.publish.release();
    assert_pending(wait.as_mut());
    assert_eq!(snapshot.await.len(), 1);
    reply.await.unwrap();
    assert_eq!(wait.await.unwrap().decision, PermissionDecision::Grant);
    let committed = events.receipt.lock().unwrap().clone().unwrap();
    assert_eq!(*events.published.lock().unwrap(), Some(committed.clone()));
    assert_ne!(committed.id, id);
    assert_eq!(subscription.next(&token).await.unwrap(), Some(committed));
}

#[tokio::test(flavor = "current_thread")]
async fn permission_async_close_cannot_overtake_publication() {
    let (broker, events, _) = setup(None);
    let token = Token::default();
    let mut wait = Box::pin(broker.request(request(Arc::new(Approval::default()), "one"), &token));
    assert_pending(wait.as_mut());
    let mut close = Box::pin(broker.close());
    assert_pending(close.as_mut());
    events.append.release();
    assert_pending(wait.as_mut());
    assert_pending(close.as_mut());
    events.publish.release();
    assert_pending(wait.as_mut());
    close.await;
    assert!(events.published.lock().unwrap().is_some());
    assert!(
        matches!(wait.await, Err(OperationError::Failed(error)) if error.message == "permission session closed")
    );
    assert!(broker.pending().await.is_empty());
    broker.close().await;
}

#[tokio::test(flavor = "current_thread")]
async fn permission_async_close_rechecks_queued_request_admission() {
    let (broker, events, _) = setup(None);
    let token = Token::default();
    let mut first = Box::pin(broker.request(request(Arc::new(Approval::default()), "one"), &token));
    assert_pending(first.as_mut());
    let first_id = events.id();
    let mut close = Box::pin(broker.close());
    assert_pending(close.as_mut());
    let mut second =
        Box::pin(broker.request(request(Arc::new(Approval::default()), "two"), &token));
    assert_pending(second.as_mut());
    events.append.release();
    events.publish.release();
    assert_pending(first.as_mut());
    close.await;
    assert!(
        matches!(second.await, Err(OperationError::Failed(error)) if error.kind == InteractionErrorKind::Disposed)
    );
    assert!(first.await.is_err());
    assert_eq!(events.id(), first_id);
    assert!(broker.pending().await.is_empty());
}

#[tokio::test(flavor = "current_thread")]
async fn permission_async_reject_and_withdraw_cannot_overtake_publication() {
    for reject in [false, true] {
        let (broker, events, _) = setup(None);
        let token = Token::default();
        let mut wait =
            Box::pin(broker.request(request(Arc::new(Approval::default()), "one"), &token));
        assert_pending(wait.as_mut());
        let id = events.id();
        let mut settle: BoxFuture<'_, Result<(), InteractionError>> = if reject {
            Box::pin(broker.reject(&id))
        } else {
            Box::pin(broker.withdraw(&id))
        };
        assert_pending(settle.as_mut());
        events.append.release();
        assert_pending(wait.as_mut());
        assert_pending(settle.as_mut());
        events.publish.release();
        assert_pending(wait.as_mut());
        settle.await.unwrap();
        if reject {
            assert_eq!(wait.await.unwrap().decision, PermissionDecision::Reject);
        } else {
            assert!(
                matches!(wait.await, Err(OperationError::Failed(error)) if error.message == "permission request withdrawn")
            );
        }
        assert!(events.published.lock().unwrap().is_some());
        assert!(broker.pending().await.is_empty());
    }
}

#[tokio::test(flavor = "current_thread")]
async fn permission_async_failures_remove_pending_but_not_committed_event() {
    for fail_publish in [false, true] {
        let (broker, events, _) = setup(None);
        let token = Token::default();
        events.fail_append.store(!fail_publish, Ordering::SeqCst);
        events.fail_publish.store(fail_publish, Ordering::SeqCst);
        events.append.release();
        events.publish.release();
        assert!(
            broker
                .request(request(Arc::new(Approval::default()), "one"), &token)
                .await
                .is_err()
        );
        assert!(broker.pending().await.is_empty());
        assert_eq!(events.receipt.lock().unwrap().is_some(), fail_publish);
        assert!(events.published.lock().unwrap().is_none());
        assert!(broker.reject(&events.id()).await.is_err());
    }
}

#[tokio::test(flavor = "current_thread")]
async fn permission_async_timeout_starts_after_publication() {
    let (broker, events, clock) = setup(Some(Duration::from_secs(1)));
    let token = Token::default();
    let mut wait = Box::pin(broker.request(request(Arc::new(Approval::default()), "one"), &token));
    assert_pending(wait.as_mut());
    clock.advance(Duration::from_secs(20));
    events.append.release();
    assert_pending(wait.as_mut());
    clock.advance(Duration::from_secs(20));
    events.publish.release();
    assert_pending(wait.as_mut());
    clock.advance(Duration::from_millis(999));
    assert_pending(wait.as_mut());
    clock.advance(Duration::from_millis(1));
    assert_eq!(wait.await.unwrap().kind, PermissionReplyKind::UserAway);
    assert!(broker.pending().await.is_empty());
}

#[tokio::test(flavor = "current_thread")]
async fn permission_async_cancelled_request_publishes_before_wait_cancellation() {
    let (broker, events, _) = setup(None);
    let token = Token::default();
    let mut wait = Box::pin(broker.request(request(Arc::new(Approval::default()), "one"), &token));
    assert_pending(wait.as_mut());
    token.cancel();
    assert_pending(wait.as_mut());
    events.append.release();
    assert_pending(wait.as_mut());
    events.publish.release();
    assert_eq!(wait.await.unwrap_err(), OperationError::Cancelled);
    assert!(events.published.lock().unwrap().is_some());
    assert!(broker.pending().await.is_empty());
}

#[tokio::test(flavor = "current_thread")]
async fn permission_async_dropped_producer_cleans_identity_at_each_await() {
    for during_publish in [false, true] {
        let (broker, events, _) = setup(None);
        let token = Token::default();
        let mut producer =
            Box::pin(broker.request(request(Arc::new(Approval::default()), "one"), &token));
        assert_pending(producer.as_mut());
        if during_publish {
            events.append.release();
            assert_pending(producer.as_mut());
        }
        // This drops the executing producer, NOT a host's separate request observer.
        drop(producer);
        assert!(broker.pending().await.is_empty());
        assert_eq!(events.receipt.lock().unwrap().is_some(), during_publish);
        events.append.release();
        events.publish.release();
        let mut successor =
            Box::pin(broker.request(request(Arc::new(Approval::default()), "two"), &token));
        assert_pending(successor.as_mut());
        assert_eq!(broker.pending().await.len(), 1);
        broker.reject(&events.id()).await.unwrap();
        assert_eq!(
            successor.await.unwrap().decision,
            PermissionDecision::Reject
        );
    }
}
