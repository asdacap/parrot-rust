use super::*;

#[derive(Default)]
struct AskDelivery {
    steers: Mutex<Vec<PendingAgentAsk>>,
    fail: AtomicBool,
    block: AtomicBool,
}

impl AgentAskDelivery for AskDelivery {
    fn steer<'a>(
        &'a self,
        request: &'a PendingAgentAsk,
        text: String,
    ) -> BoxFuture<'a, Result<(), InteractionError>> {
        Box::pin(async move {
            assert!(text.contains(&request.id));
            assert!(text.contains(&request.message));
            self.steers.lock().unwrap().push(request.clone());
            if self.block.load(Ordering::SeqCst) {
                std::future::pending::<()>().await;
            }
            if self.fail.load(Ordering::SeqCst) {
                return Err(InteractionError::new(
                    InteractionErrorKind::Delivery,
                    "delivery failed",
                ));
            }
            Ok(())
        })
    }
}

#[derive(Default)]
struct AskTemplates {
    fail: AtomicBool,
}

impl AgentAskTemplates for AskTemplates {
    fn ask(&self, request: &PendingAgentAsk) -> Result<String, InteractionError> {
        if self.fail.load(Ordering::SeqCst) {
            return Err(InteractionError::new(
                InteractionErrorKind::Argument,
                "template failed",
            ));
        }
        Ok(format!(
            "{} asks {}: {}. Reply to {}",
            request.asker.name, request.recipient.name, request.message, request.id
        ))
    }

    fn reminder(&self, requests: &[PendingAgentAsk]) -> Result<String, InteractionError> {
        Ok(requests
            .iter()
            .map(|request| format!("reply {} from {}", request.id, request.asker.name))
            .collect::<Vec<_>>()
            .join("\n"))
    }
}

struct Fixture {
    broker: Arc<AgentAskBroker>,
    delivery: Arc<AskDelivery>,
    templates: Arc<AskTemplates>,
}

impl Fixture {
    fn new() -> Self {
        Self {
            broker: Arc::new(AgentAskBroker::new()),
            delivery: Arc::new(AskDelivery::default()),
            templates: Arc::new(AskTemplates::default()),
        }
    }

    fn agent(&self, id: &str) -> AgentAsks {
        self.broker
            .register(
                AgentAskIdentity {
                    session_id: id.into(),
                    name: id.into(),
                },
                self.delivery.clone(),
                self.templates.clone(),
            )
            .unwrap()
    }
}

fn cycle_error(result: Result<String, OperationError<InteractionError>>) {
    assert!(
        matches!(result, Err(OperationError::Failed(error)) if error.message.contains("cycle") && error.message.contains("reply to existing asks first"))
    );
}

#[tokio::test(flavor = "current_thread")]
async fn delivery_uses_the_resolved_recipient_scope_adapter() {
    let fixture = Fixture::new();
    let asker = fixture.agent("asker");
    let delivery = Arc::new(AskDelivery::default());
    let recipient = fixture
        .broker
        .register(
            AgentAskIdentity {
                session_id: "recipient".into(),
                name: "recipient".into(),
            },
            delivery.clone(),
            fixture.templates.clone(),
        )
        .unwrap();
    let token = Token::default();
    let mut ask = Box::pin(asker.ask(&recipient, "local delivery".into(), &token));
    assert_pending(ask.as_mut());
    assert!(fixture.delivery.steers.lock().unwrap().is_empty());
    assert_eq!(delivery.steers.lock().unwrap().len(), 1);
    recipient
        .reply(&recipient.pending()[0].id, "answer".into())
        .unwrap();
    assert_eq!(ask.await.unwrap(), "answer");
}

#[tokio::test(flavor = "current_thread")]
async fn direct_indirect_and_self_cycles_are_rejected_without_delivery() {
    let fixture = Fixture::new();
    let a = fixture.agent("a");
    let b = fixture.agent("b");
    let c = fixture.agent("c");
    let token = Token::default();
    cycle_error(a.ask(&a, "self".into(), &token).await);
    let mut ab = Box::pin(a.ask(&b, "ab".into(), &token));
    assert_pending(ab.as_mut());
    cycle_error(b.ask(&a, "ba".into(), &token).await);
    let mut bc = Box::pin(b.ask(&c, "bc".into(), &token));
    assert_pending(bc.as_mut());
    cycle_error(c.ask(&a, "ca".into(), &token).await);
    assert!(a.pending().is_empty());
    assert_eq!(b.pending().len(), 1);
    assert_eq!(c.pending().len(), 1);
    assert_eq!(fixture.delivery.steers.lock().unwrap().len(), 2);
    c.reply(&c.pending()[0].id, "c replied".into()).unwrap();
    assert_eq!(bc.await.unwrap(), "c replied");
    b.reply(&b.pending()[0].id, "b replied".into()).unwrap();
    assert_eq!(ab.await.unwrap(), "b replied");
    let mut ca = Box::pin(c.ask(&a, "now acyclic".into(), &token));
    assert_pending(ca.as_mut());
    a.reply(&a.pending()[0].id, "a replied".into()).unwrap();
    assert_eq!(ca.await.unwrap(), "a replied");
}

#[tokio::test(flavor = "current_thread")]
async fn fan_in_replies_are_exact_authorized_and_retryable() {
    let fixture = Fixture::new();
    let a = fixture.agent("a");
    let b = fixture.agent("b");
    let c = fixture.agent("c");
    let token = Token::default();
    let mut ac = Box::pin(a.ask(&c, "first".into(), &token));
    let mut bc = Box::pin(b.ask(&c, "second".into(), &token));
    assert_pending(ac.as_mut());
    assert_pending(bc.as_mut());
    assert!(
        matches!(a.ask(&b, "duplicate".into(), &token).await, Err(OperationError::Failed(error)) if error.message.contains("already has a pending ask"))
    );
    let mut pending = c.pending();
    assert_eq!(pending.len(), 2);
    let first = pending
        .iter()
        .find(|request| request.message == "first")
        .unwrap()
        .id
        .clone();
    let second = pending
        .iter()
        .find(|request| request.message == "second")
        .unwrap()
        .id
        .clone();
    pending[0].message.clear();
    assert!(
        c.pending()
            .iter()
            .all(|request| !request.message.is_empty())
    );
    let reminder = c.completion_reminder().unwrap().unwrap();
    assert!(reminder.contains(&first));
    assert!(reminder.contains(&second));
    assert!(a.completion_reminder().unwrap().is_none());
    assert!(a.reply(&first, "wrong recipient".into()).is_err());
    assert!(b.reply(&first, "wrong recipient".into()).is_err());
    assert!(c.reply(&first, " \n ".into()).is_err());
    assert!(c.reply("missing", "stale".into()).is_err());
    assert_eq!(c.pending().len(), 2);
    c.reply(&second, "second answer".into()).unwrap();
    assert_eq!(bc.await.unwrap(), "second answer");
    assert_eq!(c.pending().len(), 1);
    assert!(c.reply(&second, "duplicate".into()).is_err());
    c.reply(&first, "first answer".into()).unwrap();
    assert_eq!(ac.await.unwrap(), "first answer");
    assert!(c.completion_reminder().unwrap().is_none());
}

#[tokio::test(flavor = "current_thread")]
async fn cancellation_and_future_drop_remove_edges_even_during_delivery() {
    let fixture = Fixture::new();
    let a = fixture.agent("a");
    let b = fixture.agent("b");
    for block in [false, true] {
        fixture.delivery.block.store(block, Ordering::SeqCst);
        let token = Token::default();
        let mut ab = Box::pin(a.ask(&b, "cancel".into(), &token));
        assert_pending(ab.as_mut());
        let old_id = b.pending()[0].id.clone();
        token.cancel();
        assert_eq!(ab.await.unwrap_err(), OperationError::Cancelled);
        assert!(b.pending().is_empty());
        assert!(b.reply(&old_id, "stale".into()).is_err());
        let token = Token::default();
        let mut ba = Box::pin(b.ask(&a, "reverse".into(), &token));
        assert_pending(ba.as_mut());
        drop(ba);
        assert!(a.pending().is_empty());
        let mut ab = Box::pin(a.ask(&b, "drop".into(), &token));
        assert_pending(ab.as_mut());
        drop(ab);
        assert!(b.pending().is_empty());
    }
    let cancelled = Token::default();
    cancelled.cancel();
    let delivered = fixture.delivery.steers.lock().unwrap().len();
    assert_eq!(
        a.ask(&b, "already cancelled".into(), &cancelled).await,
        Err(OperationError::Cancelled)
    );
    assert_eq!(fixture.delivery.steers.lock().unwrap().len(), delivered);
}

#[tokio::test(flavor = "current_thread")]
async fn failed_validation_templates_and_delivery_leave_no_pending_state() {
    let fixture = Fixture::new();
    let a = fixture.agent("a");
    let b = fixture.agent("b");
    let token = Token::default();
    assert!(a.ask(&b, " \n".into(), &token).await.is_err());
    fixture.templates.fail.store(true, Ordering::SeqCst);
    assert!(a.ask(&b, "template failure".into(), &token).await.is_err());
    assert!(b.pending().is_empty());
    assert!(fixture.delivery.steers.lock().unwrap().is_empty());
    fixture.templates.fail.store(false, Ordering::SeqCst);
    fixture.delivery.fail.store(true, Ordering::SeqCst);
    assert!(a.ask(&b, "delivery failure".into(), &token).await.is_err());
    assert!(b.pending().is_empty());
    fixture.delivery.fail.store(false, Ordering::SeqCst);
    let mut ba = Box::pin(b.ask(&a, "no phantom edge".into(), &token));
    assert_pending(ba.as_mut());
    a.reply(&a.pending()[0].id, "ok".into()).unwrap();
    assert_eq!(ba.await.unwrap(), "ok");
}

#[tokio::test(flavor = "current_thread")]
async fn disposal_shutdown_and_identity_reuse_withdraw_incoming_and_outgoing() {
    let fixture = Fixture::new();
    let a = fixture.agent("a");
    let b = fixture.agent("b");
    let c = fixture.agent("c");
    let token = Token::default();
    let mut ab = Box::pin(a.ask(&b, "ab".into(), &token));
    let mut bc = Box::pin(b.ask(&c, "bc".into(), &token));
    assert_pending(ab.as_mut());
    assert_pending(bc.as_mut());
    let stale_id = b.pending()[0].id.clone();
    b.close();
    let replacement = fixture.agent("b");
    b.close();
    assert!(replacement.pending().is_empty());
    assert!(replacement.reply(&stale_id, "stale".into()).is_err());
    assert!(b.ask(&a, "old identity".into(), &token).await.is_err());
    assert!(a.ask(&b, "stale recipient".into(), &token).await.is_err());
    assert!(c.pending().is_empty());
    let mut fresh = Box::pin(a.ask(&replacement, "fresh".into(), &token));
    assert_pending(fresh.as_mut());
    assert!(ab.await.is_err());
    assert!(bc.await.is_err());
    assert_eq!(replacement.pending().len(), 1);
    replacement
        .reply(&replacement.pending()[0].id, "fresh answer".into())
        .unwrap();
    assert_eq!(fresh.await.unwrap(), "fresh answer");
    let mut ca = Box::pin(c.ask(&a, "shutdown".into(), &token));
    assert_pending(ca.as_mut());
    fixture.broker.close();
    fixture.broker.close();
    assert!(ca.await.is_err());
    assert!(a.pending().is_empty());
    assert!(a.ask(&c, "closed".into(), &token).await.is_err());
    assert!(
        fixture
            .broker
            .register(
                AgentAskIdentity {
                    session_id: "new".into(),
                    name: "new".into()
                },
                fixture.delivery.clone(),
                fixture.templates.clone()
            )
            .is_err()
    );
}

#[tokio::test(flavor = "current_thread")]
async fn user_sessions_are_isolated_and_registration_is_unique() {
    let first = Fixture::new();
    let second = Fixture::new();
    let a = first.agent("same-id");
    let b = second.agent("same-id");
    let token = Token::default();
    assert!(a.ask(&b, "cross session".into(), &token).await.is_err());
    assert!(first.delivery.steers.lock().unwrap().is_empty());
    assert!(
        first
            .broker
            .register(
                a.identity().clone(),
                first.delivery.clone(),
                first.templates.clone()
            )
            .is_err()
    );
    assert!(
        first
            .broker
            .register(
                AgentAskIdentity {
                    session_id: " ".into(),
                    name: "name".into()
                },
                first.delivery.clone(),
                first.templates.clone()
            )
            .is_err()
    );
    assert!(
        first
            .broker
            .register(
                AgentAskIdentity {
                    session_id: "other".into(),
                    name: " ".into()
                },
                first.delivery.clone(),
                first.templates.clone()
            )
            .is_err()
    );
}

#[tokio::test(flavor = "current_thread")]
async fn reply_wins_observed_cancellation_and_clears_edge_before_waiter_resumes() {
    let fixture = Fixture::new();
    let a = fixture.agent("a");
    let b = fixture.agent("b");
    let token = Token::default();
    fixture.delivery.block.store(true, Ordering::SeqCst);
    let mut ab = Box::pin(a.ask(&b, "blocked delivery".into(), &token));
    assert_pending(ab.as_mut());
    b.reply(&b.pending()[0].id, "reply won".into()).unwrap();
    let next_token = Token::default();
    let mut ba = Box::pin(b.ask(&a, "reverse before observation".into(), &next_token));
    assert_pending(ba.as_mut());
    token.cancel();
    assert_eq!(ab.await.unwrap(), "reply won");
    a.reply(&a.pending()[0].id, "reverse answer".into())
        .unwrap();
    assert_eq!(ba.await.unwrap(), "reverse answer");
    assert!(a.pending().is_empty());
    assert!(b.pending().is_empty());
}

#[test]
fn concurrent_reply_and_cancellation_do_not_leak_requests() {
    let fixture = Fixture::new();
    let a = fixture.agent("a");
    let b = fixture.agent("b");
    let runtime = tokio::runtime::Builder::new_current_thread()
        .build()
        .unwrap();
    for _ in 0..32 {
        let token = Token::default();
        let mut ab = Box::pin(a.ask(&b, "race".into(), &token));
        assert_pending(ab.as_mut());
        let id = b.pending()[0].id.clone();
        let barrier = std::sync::Barrier::new(3);
        let result = std::thread::scope(|scope| {
            let barrier = &barrier;
            let token = &token;
            scope.spawn(move || {
                barrier.wait();
                token.cancel();
            });
            let recipient = &b;
            let request_id = &id;
            let reply = scope.spawn(move || {
                barrier.wait();
                recipient.reply(request_id, "answer".into())
            });
            barrier.wait();
            let result = runtime.block_on(ab);
            if let Err(error) = reply.join().unwrap() {
                assert_eq!(error.message, "ask is no longer pending");
            }
            result
        });
        assert!(
            matches!(result, Ok(ref answer) if answer == "answer")
                || result == Err(OperationError::Cancelled)
        );
        assert!(b.pending().is_empty());
        assert!(b.reply(&id, "stale".into()).is_err());
        let next_token = Token::default();
        let mut ba = Box::pin(b.ask(&a, "reverse".into(), &next_token));
        assert_pending(ba.as_mut());
        drop(ba);
        assert!(a.pending().is_empty());
    }
}

#[test]
fn concurrent_opposing_admissions_never_both_succeed() {
    for _ in 0..32 {
        let fixture = Fixture::new();
        let a = fixture.agent("a");
        let b = fixture.agent("b");
        let barrier = std::sync::Barrier::new(2);
        let winners = std::sync::atomic::AtomicUsize::new(0);
        let losers = std::sync::atomic::AtomicUsize::new(0);
        std::thread::scope(|scope| {
            for (asker, recipient) in [(&a, &b), (&b, &a)] {
                let barrier = &barrier;
                let winners = &winners;
                let losers = &losers;
                scope.spawn(move || {
                    tokio::runtime::Builder::new_current_thread()
                        .build()
                        .unwrap()
                        .block_on(async {
                            let token = Token::default();
                            let mut ask = Box::pin(asker.ask(recipient, "race".into(), &token));
                            barrier.wait();
                            match ask
                                .as_mut()
                                .poll(&mut Context::from_waker(&Waker::from(Arc::new(NoWake))))
                            {
                                Poll::Pending => {
                                    winners.fetch_add(1, Ordering::SeqCst);
                                }
                                Poll::Ready(result) => {
                                    cycle_error(result);
                                    losers.fetch_add(1, Ordering::SeqCst);
                                }
                            }
                            barrier.wait();
                        });
                });
            }
        });
        assert_eq!(winners.load(Ordering::SeqCst), 1);
        assert_eq!(losers.load(Ordering::SeqCst), 1);
        assert_eq!(fixture.delivery.steers.lock().unwrap().len(), 1);
        assert!(a.pending().is_empty());
        assert!(b.pending().is_empty());
    }
}
