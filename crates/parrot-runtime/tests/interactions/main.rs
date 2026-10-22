mod asks;
mod permission_async;
mod protocol;

use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll, Wake, Waker};
use std::time::{Duration, Instant, SystemTime};

use parrot_domain::contracts::async_ops::{
    BoxFuture, Cancellation, Cancelled, Clock, OperationError,
};
use parrot_domain::contracts::storage::Committed;
use parrot_domain::protocol::messages::parrot::Event;
use parrot_runtime::interactions::*;
use tokio::sync::Notify;

fn committed_event(id: &str) -> Committed<Event> {
    Committed::after_commit(Event {
        id: id.to_owned(),
        ..Event::new()
    })
}

#[derive(Default)]
struct Token {
    cancelled: AtomicBool,
    changed: Notify,
}
impl Token {
    fn cancel(&self) {
        self.cancelled.store(true, Ordering::SeqCst);
        self.changed.notify_waiters();
    }
}
impl Cancellation for Token {
    fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::SeqCst)
    }
    fn cancelled(&self) -> BoxFuture<'_, ()> {
        Box::pin(async move {
            loop {
                let changed = self.changed.notified();
                tokio::pin!(changed);
                changed.as_mut().enable();
                if self.is_cancelled() {
                    return;
                }
                changed.await;
            }
        })
    }
}
struct ManualClock {
    base: Instant,
    nanos: AtomicU64,
    changed: Notify,
}
impl ManualClock {
    fn new() -> Self {
        Self {
            base: Instant::now(),
            nanos: AtomicU64::new(0),
            changed: Notify::new(),
        }
    }
    fn advance(&self, duration: Duration) {
        self.nanos
            .fetch_add(duration.as_nanos().try_into().unwrap(), Ordering::SeqCst);
        self.changed.notify_waiters();
    }
}
impl Clock for ManualClock {
    fn now(&self) -> Instant {
        self.base + Duration::from_nanos(self.nanos.load(Ordering::SeqCst))
    }
    fn utc_now(&self) -> SystemTime {
        SystemTime::UNIX_EPOCH
    }
    fn sleep_until<'a>(
        &'a self,
        deadline: Instant,
        token: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<(), Cancelled>> {
        Box::pin(async move {
            loop {
                let changed = self.changed.notified();
                tokio::pin!(changed);
                changed.as_mut().enable();
                if token.is_cancelled() {
                    return Err(Cancelled);
                }
                if self.now() >= deadline {
                    return Ok(());
                }
                tokio::select! { _ = token.cancelled() => return Err(Cancelled), _ = changed => {} }
            }
        })
    }
}
#[derive(Default)]
struct Diagnostics(Mutex<Vec<BrokerDiagnostic>>);
impl InteractionDiagnostics for Diagnostics {
    fn write(&self, event: BrokerDiagnostic) {
        self.0.lock().unwrap().push(event);
    }
}
struct NoWake;
impl Wake for NoWake {
    fn wake(self: Arc<Self>) {}
}
fn assert_pending<F: Future + ?Sized>(future: Pin<&mut F>) {
    assert!(
        future
            .poll(&mut Context::from_waker(&Waker::from(Arc::new(NoWake))))
            .is_pending()
    );
}
fn question() -> QuestionDefinition {
    QuestionDefinition {
        header: "header".into(),
        prompt: "private-sentinel prompt".into(),
        options: vec![
            QuestionOption {
                label: "first".into(),
                description: "description".into(),
            },
            QuestionOption {
                label: "second".into(),
                description: String::new(),
            },
        ],
        multiple: false,
        custom: true,
    }
}
fn answer() -> QuestionReply {
    QuestionReply::answered(vec!["first".into()])
}
fn broker(timeout: Option<Duration>) -> (QuestionBroker, Arc<ManualClock>, Arc<Diagnostics>) {
    let clock = Arc::new(ManualClock::new());
    let diagnostics = Arc::new(Diagnostics::default());
    (
        QuestionBroker::with_clock(timeout, clock.clone(), diagnostics.clone()).unwrap(),
        clock,
        diagnostics,
    )
}

#[test]
fn question_validation_preserves_source_literals_and_boundaries() {
    assert_eq!(
        validate_questions(&[]).unwrap_err().message,
        "question requests require between 1 and 32 questions"
    );
    assert!(validate_questions(&vec![question(); 32]).is_ok());
    assert!(validate_questions(&vec![question(); 33]).is_err());
    let mut q = question();
    q.prompt = " \n".into();
    assert_eq!(
        validate_questions(&[q]).unwrap_err().message,
        "question prompts cannot be empty"
    );
    let mut q = question();
    q.options[0].label = " ".into();
    assert_eq!(
        validate_questions(&[q]).unwrap_err().message,
        "question options cannot be empty"
    );
    let mut q = question();
    q.options[1].label = "first".into();
    assert_eq!(
        validate_questions(&[q]).unwrap_err().message,
        "duplicate question option: first"
    );
    let mut q = question();
    q.options.clear();
    q.custom = false;
    assert_eq!(
        validate_questions(&[q]).unwrap_err().message,
        "question requires options or custom answers"
    );
    let mut q = question();
    q.multiple = true;
    assert!(validate_reply(&[q], &QuestionReply::answered(vec!["first, second".into()])).is_ok());
    assert_eq!(
        validate_reply(&[question()], &QuestionReply::user_away())
            .unwrap_err()
            .message,
        "question replies require user answers"
    );
    assert_eq!(
        validate_reply(&[question()], &QuestionReply::answered(vec![]))
            .unwrap_err()
            .message,
        "question replies must contain one answer for every question in order"
    );
    assert_eq!(
        validate_reply(&[question()], &QuestionReply::answered(vec![" \n".into()]))
            .unwrap_err()
            .message,
        "question answers cannot be empty"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn valid_multi_reply_is_ordered_copied_and_settled_once() {
    let (broker, _, _) = broker(None);
    let token = Token::default();
    let questions = vec![question(), question()];
    let mut wait = Box::pin(broker.ask(questions, &token));
    assert_pending(wait.as_mut());
    let mut snapshot = broker.pending();
    let id = snapshot[0].id.clone();
    snapshot[0].questions[0].prompt.clear();
    assert_eq!(
        broker.pending()[0].questions[0].prompt,
        "private-sentinel prompt"
    );
    assert!(broker.reply(&id, answer()).is_err());
    assert_eq!(broker.pending().len(), 1);
    let reply = QuestionReply::answered(vec!["second".into(), "custom".into()]);
    broker.reply(&id, reply.clone()).unwrap();
    assert_eq!(wait.await.unwrap(), reply);
    assert!(broker.pending().is_empty());
    assert_eq!(
        broker.reply(&id, answer()).unwrap_err().kind,
        InteractionErrorKind::QuestionNotFound
    );
}

#[tokio::test(flavor = "current_thread")]
async fn invalid_replies_leave_question_pending() {
    let (broker, _, _) = broker(None);
    let token = Token::default();
    let mut wait = Box::pin(broker.ask(vec![question()], &token));
    assert_pending(wait.as_mut());
    let id = broker.pending()[0].id.clone();
    for bad in [
        QuestionReply::user_away(),
        QuestionReply::answered(vec![]),
        QuestionReply::answered(vec![" ".into()]),
    ] {
        assert!(broker.reply(&id, bad).is_err());
        assert_eq!(broker.pending().len(), 1);
    }
    broker.reply(&id, answer()).unwrap();
    assert_eq!(wait.await.unwrap(), answer());
}

#[tokio::test(flavor = "current_thread")]
async fn timeout_removes_question_and_late_reply_and_reject_are_not_found() {
    let (broker, clock, _) = broker(Some(Duration::from_secs(10)));
    let token = Token::default();
    let mut wait = Box::pin(broker.ask(vec![question()], &token));
    assert_pending(wait.as_mut());
    let id = broker.pending()[0].id.clone();
    clock.advance(Duration::from_secs(10));
    assert_eq!(wait.await.unwrap(), QuestionReply::user_away());
    assert!(broker.pending().is_empty());
    assert_eq!(
        broker.reply(&id, answer()).unwrap_err().message,
        format!("question request not found: {id}")
    );
    assert!(broker.reject(&id).is_err());
}

#[tokio::test(flavor = "current_thread")]
async fn infinite_timeout_and_pending_timing_preserve_original_deadline() {
    let (broker, clock, _) = broker(Some(Duration::from_secs(10)));
    let token = Token::default();
    let mut wait = Box::pin(broker.ask(vec![question()], &token));
    assert_pending(wait.as_mut());
    let id = broker.pending()[0].id.clone();
    clock.advance(Duration::from_nanos(1));
    assert_eq!(broker.pending()[0].remaining_milliseconds, Some(10_000));
    clock.advance(Duration::from_secs(3));
    assert_eq!(broker.pending()[0].remaining_milliseconds, Some(7_000));
    assert!(broker.reply(&id, QuestionReply::answered(vec![])).is_err());
    assert_eq!(broker.pending()[0].remaining_milliseconds, Some(7_000));
    clock.advance(Duration::from_secs(7));
    assert_eq!(wait.await.unwrap().kind, QuestionReplyKind::UserAway);
    let (infinite, clock, _) = self::broker(None);
    let mut wait = Box::pin(infinite.ask(vec![question()], &token));
    assert_pending(wait.as_mut());
    clock.advance(Duration::from_secs(86_400));
    assert_pending(wait.as_mut());
    assert_eq!(infinite.pending()[0].remaining_milliseconds, None);
    infinite.reply(&infinite.pending()[0].id, answer()).unwrap();
    assert_eq!(wait.await.unwrap(), answer());
}

#[tokio::test(flavor = "current_thread")]
async fn reply_wins_later_timeout_and_cancellation() {
    for cancel in [false, true] {
        let (broker, clock, _) = broker(Some(Duration::from_secs(1)));
        let token = Token::default();
        let mut wait = Box::pin(broker.ask(vec![question()], &token));
        assert_pending(wait.as_mut());
        broker.reply(&broker.pending()[0].id, answer()).unwrap();
        if cancel {
            token.cancel();
        } else {
            clock.advance(Duration::from_secs(1));
        }
        assert_eq!(wait.await.unwrap(), answer());
    }
}

#[tokio::test(flavor = "current_thread")]
async fn cancellation_rejection_disposal_and_dropped_observer_remove_waiters() {
    let (broker, _, _) = broker(None);
    let token = Token::default();
    let mut wait = Box::pin(broker.ask(vec![question()], &token));
    assert_pending(wait.as_mut());
    token.cancel();
    assert_eq!(wait.await.unwrap_err(), OperationError::Cancelled);
    assert!(broker.pending().is_empty());
    let token = Token::default();
    let mut wait = Box::pin(broker.ask(vec![question()], &token));
    assert_pending(wait.as_mut());
    broker.reject(&broker.pending()[0].id).unwrap();
    assert_eq!(
        wait.await.unwrap_err(),
        OperationError::Failed(InteractionError {
            kind: InteractionErrorKind::QuestionRejected,
            message: "question request rejected".into()
        })
    );
    let mut wait = Box::pin(broker.ask(vec![question()], &token));
    assert_pending(wait.as_mut());
    drop(wait);
    assert!(broker.pending().is_empty());
    let mut wait = Box::pin(broker.ask(vec![question()], &token));
    assert_pending(wait.as_mut());
    broker.close();
    broker.close();
    assert!(broker.pending().is_empty());
    assert!(
        matches!(wait.await, Err(OperationError::Failed(error)) if error.message == "question session closed")
    );
    assert!(
        matches!(broker.ask(vec![question()], &token).await, Err(OperationError::Failed(error)) if error.kind == InteractionErrorKind::Disposed)
    );
}

#[tokio::test(flavor = "current_thread")]
async fn brokers_are_session_local_pending_order_is_ordinal_and_diagnostics_are_safe() {
    let (left, _, diagnostics) = broker(None);
    let (right, _, _) = broker(None);
    let token = Token::default();
    let mut one = Box::pin(left.ask(vec![question()], &token));
    let mut two = Box::pin(left.ask(vec![question()], &token));
    assert_pending(one.as_mut());
    assert_pending(two.as_mut());
    let pending = left.pending();
    assert!(pending[0].id < pending[1].id);
    assert!(right.pending().is_empty());
    assert!(right.reply(&pending[0].id, answer()).is_err());
    for request in pending {
        left.reply(&request.id, answer()).unwrap();
    }
    one.await.unwrap();
    two.await.unwrap();
    let events = diagnostics.0.lock().unwrap();
    assert_eq!(events.len(), 4);
    assert_eq!(
        events
            .iter()
            .filter(|event| event.outcome == Some("answered"))
            .count(),
        2
    );
    assert!(!format!("{events:?}").contains("private-sentinel"));
}

#[derive(Default)]
struct Approval {
    fail: AtomicBool,
    grants: Mutex<Vec<Vec<String>>>,
}
impl TargetApproval<String> for Approval {
    fn validate(&self, _: &[String]) -> Result<(), InteractionError> {
        Ok(())
    }
    fn approve(&self, targets: &[String]) -> Result<(), InteractionError> {
        if self.fail.load(Ordering::SeqCst) {
            return Err(InteractionError {
                kind: InteractionErrorKind::Permission,
                message: "permission request targets changed before approval".into(),
            });
        }
        self.grants.lock().unwrap().push(targets.to_vec());
        Ok(())
    }
}
#[derive(Default)]
struct Events {
    log: Mutex<Vec<&'static str>>,
    fail_append: AtomicBool,
    fail_publish: AtomicBool,
}
impl PermissionEvents<String> for Events {
    fn append<'a>(
        &'a self,
        pending: &'a PendingPermission<String>,
    ) -> BoxFuture<'a, Result<Committed<Event>, InteractionError>> {
        Box::pin(async move {
            self.log.lock().unwrap().push("append");
            if self.fail_append.load(Ordering::SeqCst) {
                Err(InteractionError {
                    kind: InteractionErrorKind::Delivery,
                    message: "append failed".into(),
                })
            } else {
                Ok(committed_event(&pending.id))
            }
        })
    }
    fn publish<'a>(
        &'a self,
        _: &'a Committed<Event>,
    ) -> BoxFuture<'a, Result<(), InteractionError>> {
        Box::pin(async move {
            self.log.lock().unwrap().push("publish");
            if self.fail_publish.load(Ordering::SeqCst) {
                Err(InteractionError {
                    kind: InteractionErrorKind::Delivery,
                    message: "publish failed".into(),
                })
            } else {
                Ok(())
            }
        })
    }
}
fn request(approval: Arc<Approval>, session: &str) -> PermissionRequest<String> {
    PermissionRequest {
        agent_session_id: session.into(),
        reason: "private-sentinel reason".into(),
        targets: vec!["opaque-one".into(), "opaque-two".into()],
        approval,
    }
}
fn permissions(
    interactive: bool,
    timeout: Option<Duration>,
) -> (
    PermissionBroker<String>,
    Arc<ManualClock>,
    Arc<Diagnostics>,
    Arc<Events>,
) {
    let clock = Arc::new(ManualClock::new());
    let diagnostics = Arc::new(Diagnostics::default());
    let events = Arc::new(Events::default());
    (
        PermissionBroker::with_clock(
            interactive,
            timeout,
            clock.clone(),
            diagnostics.clone(),
            events.clone(),
        )
        .unwrap(),
        clock,
        diagnostics,
        events,
    )
}

#[tokio::test(flavor = "current_thread")]
async fn permission_choices_copy_targets_and_validate_before_removal() {
    let (broker, _, diagnostics, events) = permissions(true, None);
    let approval = Arc::new(Approval::default());
    let token = Token::default();
    let mut wait = Box::pin(broker.request(request(approval.clone(), "agent-one"), &token));
    assert_pending(wait.as_mut());
    let pending = broker.pending().await;
    let id = &pending[0].id;
    assert_eq!(pending[0].choices, PERMISSION_CHOICES);
    assert_eq!(pending[0].agent_session_id, "agent-one");
    assert_eq!(*events.log.lock().unwrap(), ["append", "publish"]);
    for (choice, reason, message) in [
        ("unknown", "", "unknown permission choice: unknown"),
        (
            "reject with reason",
            " \n",
            "the selected permission choice requires a reason",
        ),
        (
            "reject",
            "why",
            "the selected permission choice does not accept a reason",
        ),
        (
            "grant",
            "why",
            "the selected permission choice does not accept a reason",
        ),
    ] {
        assert_eq!(
            broker.reply(id, choice, reason).await.unwrap_err().message,
            message
        );
        assert_eq!(broker.pending().await.len(), 1);
    }
    broker
        .reply(id, "reject with reason", "  no thanks \n")
        .await
        .unwrap();
    assert_eq!(
        wait.await.unwrap(),
        PermissionReply::decided(PermissionDecision::Reject, "no thanks")
    );
    assert!(approval.grants.lock().unwrap().is_empty());
    assert!(broker.reply(id, "reject", "").await.is_err());
    let events = diagnostics.0.lock().unwrap();
    assert_eq!(events[1].outcome, Some("rejected"));
    assert_eq!(events[1].agent_session_id.as_deref(), Some("agent-one"));
    assert!(!format!("{events:?}").contains("private-sentinel"));
}

#[tokio::test(flavor = "current_thread")]
async fn approval_is_only_delegated_to_request_adapter_and_failure_keeps_pending() {
    let (broker, _, _, _) = permissions(true, None);
    let approval = Arc::new(Approval::default());
    let other = Arc::new(Approval::default());
    let token = Token::default();
    let mut wait = Box::pin(broker.request(request(approval.clone(), "one"), &token));
    assert_pending(wait.as_mut());
    let id = broker.pending().await[0].id.clone();
    approval.fail.store(true, Ordering::SeqCst);
    assert!(broker.reply(&id, "grant", "").await.is_err());
    assert_eq!(broker.pending().await.len(), 1);
    approval.fail.store(false, Ordering::SeqCst);
    broker.reply(&id, "grant", "").await.unwrap();
    assert_eq!(wait.await.unwrap().decision, PermissionDecision::Grant);
    assert_eq!(
        *approval.grants.lock().unwrap(),
        [vec!["opaque-one".to_owned(), "opaque-two".to_owned()]]
    );
    assert!(other.grants.lock().unwrap().is_empty());
}

#[tokio::test(flavor = "current_thread")]
async fn noninteractive_permission_rejects_without_pending_or_publication() {
    let (broker, _, _, events) = permissions(false, None);
    let approval = Arc::new(Approval::default());
    let token = Token::default();
    assert_eq!(
        broker
            .request(request(approval.clone(), "one"), &token)
            .await
            .unwrap(),
        PermissionReply::decided(PermissionDecision::Reject, "")
    );
    assert!(broker.pending().await.is_empty());
    assert!(events.log.lock().unwrap().is_empty());
    assert!(approval.grants.lock().unwrap().is_empty());
    let mut empty = request(approval, "one");
    empty.targets.clear();
    assert!(
        matches!(broker.request(empty, &token).await, Err(OperationError::Failed(error)) if error.message == "permission requests require at least one target")
    );
}

#[tokio::test(flavor = "current_thread")]
async fn permission_timeout_cancellation_and_reply_races_preserve_outcome() {
    for win in ["timeout", "cancel", "reply-timeout", "reply-cancel"] {
        let (broker, clock, _, _) = permissions(true, Some(Duration::from_secs(1)));
        let token = Token::default();
        let mut wait =
            Box::pin(broker.request(request(Arc::new(Approval::default()), "one"), &token));
        assert_pending(wait.as_mut());
        let id = broker.pending().await[0].id.clone();
        if win.starts_with("reply") {
            broker.reply(&id, "reject", "").await.unwrap();
        }
        if win.ends_with("cancel") {
            token.cancel();
        } else {
            clock.advance(Duration::from_secs(1));
        }
        match win {
            "timeout" => assert_eq!(wait.await.unwrap().kind, PermissionReplyKind::UserAway),
            "cancel" => assert_eq!(wait.await.unwrap_err(), OperationError::Cancelled),
            _ => assert_eq!(wait.await.unwrap().kind, PermissionReplyKind::Decided),
        }
        assert!(broker.pending().await.is_empty());
        assert_eq!(
            broker.reply(&id, "reject", "").await.unwrap_err().kind,
            InteractionErrorKind::PermissionNotFound
        );
    }
}

#[tokio::test(flavor = "current_thread")]
async fn permission_publication_failures_disposal_and_infinite_wait_leave_no_waiter() {
    for publish in [false, true] {
        let (broker, _, _, events) = permissions(true, None);
        let token = Token::default();
        if publish {
            events.fail_publish.store(true, Ordering::SeqCst);
        } else {
            events.fail_append.store(true, Ordering::SeqCst);
        }
        assert!(
            broker
                .request(request(Arc::new(Approval::default()), "one"), &token)
                .await
                .is_err()
        );
        assert!(broker.pending().await.is_empty());
        assert_eq!(
            events.log.lock().unwrap().len(),
            if publish { 2 } else { 1 }
        );
    }
    let (broker, clock, _, _) = permissions(true, None);
    let token = Token::default();
    let mut wait = Box::pin(broker.request(request(Arc::new(Approval::default()), "one"), &token));
    assert_pending(wait.as_mut());
    clock.advance(Duration::from_secs(86_400));
    assert_pending(wait.as_mut());
    broker.close().await;
    broker.close().await;
    assert!(
        matches!(wait.await, Err(OperationError::Failed(error)) if error.message == "permission session closed")
    );
    assert!(broker.pending().await.is_empty());
    assert!(
        matches!(broker.request(request(Arc::new(Approval::default()), "one"), &token).await, Err(OperationError::Failed(error)) if error.kind == InteractionErrorKind::Disposed)
    );
}

#[tokio::test(flavor = "current_thread")]
async fn source_observed_option_flags_are_not_selection_constraints() {
    let (broker, _, _) = broker(None);
    let token = Token::default();
    let mut first = question();
    first.multiple = false;
    first.custom = false;
    let mut second = first.clone();
    second.options[0].label = "First".into();
    let mut wait = Box::pin(broker.ask(vec![first, second], &token));
    assert_pending(wait.as_mut());
    let reply = QuestionReply::answered(vec![
        "Green, Violet".into(),
        "arbitrary private text".into(),
    ]);
    broker
        .reply(&broker.pending()[0].id, reply.clone())
        .unwrap();
    assert_eq!(wait.await.unwrap(), reply);
    let mut custom = question();
    custom.options.clear();
    let mut wait = Box::pin(broker.ask(vec![custom], &token));
    assert_pending(wait.as_mut());
    broker.reply(&broker.pending()[0].id, answer()).unwrap();
    wait.await.unwrap();
}

#[tokio::test(flavor = "current_thread")]
async fn concurrent_question_replies_have_exactly_one_winner() {
    let (broker, _, _) = broker(None);
    let token = Token::default();
    let mut wait = Box::pin(broker.ask(vec![question()], &token));
    assert_pending(wait.as_mut());
    let id = broker.pending()[0].id.clone();
    let winners = std::thread::scope(|scope| {
        let one = scope.spawn(|| broker.reply(&id, answer()).is_ok());
        let two = scope.spawn(|| broker.reply(&id, answer()).is_ok());
        usize::from(one.join().unwrap()) + usize::from(two.join().unwrap())
    });
    assert_eq!(winners, 1);
    assert_eq!(wait.await.unwrap(), answer());
}

#[tokio::test(flavor = "current_thread")]
async fn question_diagnostics_all_terminal_outcomes_are_safe_and_correlated() {
    for expected in ["answered", "rejected", "timeout", "cancelled", "failed"] {
        let (broker, clock, diagnostics) = broker(Some(Duration::from_secs(60)));
        let token = Token::default();
        if expected == "failed" {
            broker.close();
        }
        let mut wait = Box::pin(broker.ask(vec![question()], &token));
        if expected != "failed" {
            assert_pending(wait.as_mut());
            let id = broker.pending()[0].id.clone();
            match expected {
                "answered" => broker.reply(&id, answer()).unwrap(),
                "rejected" => broker.reject(&id).unwrap(),
                "timeout" => clock.advance(Duration::from_secs(60)),
                "cancelled" => token.cancel(),
                _ => unreachable!(),
            }
        }
        let _ = wait.await;
        let events = diagnostics.0.lock().unwrap();
        assert_eq!(events.len(), 2);
        assert_eq!(events[0].category, "question");
        assert_eq!(events[0].event, "wait_started");
        assert_eq!(events[1].event, "wait_completed");
        assert_eq!(events[0].correlation_id, events[1].correlation_id);
        assert!(!events[0].correlation_id.is_empty());
        assert_eq!(events[1].outcome, Some(expected));
        assert!(events[1].duration_milliseconds.is_some());
        assert_eq!(
            events[1].severity,
            if expected == "failed" {
                DiagnosticSeverity::Error
            } else {
                DiagnosticSeverity::Information
            }
        );
        assert!(!format!("{events:?}").contains("private-sentinel"));
    }
}

#[tokio::test(flavor = "current_thread")]
async fn permission_diagnostics_safe_decisions_without_filesystem_approval() {
    for expected in ["rejected", "timeout", "cancelled", "failed"] {
        let (broker, clock, diagnostics, _) = permissions(true, Some(Duration::from_secs(60)));
        let token = Token::default();
        let input = request(Arc::new(Approval::default()), "agent-private-id");
        let mut wait = Box::pin(broker.request(input, &token));
        assert_pending(wait.as_mut());
        let id = broker.pending().await[0].id.clone();
        match expected {
            "rejected" => broker.reply(&id, "reject", "").await.unwrap(),
            "timeout" => clock.advance(Duration::from_secs(60)),
            "cancelled" => token.cancel(),
            "failed" => broker.close().await,
            _ => unreachable!(),
        }
        let _ = wait.await;
        let events = diagnostics.0.lock().unwrap();
        assert_eq!(events.len(), 2);
        assert_eq!(events[0].event, "request_started");
        assert_eq!(events[1].event, "request_completed");
        assert_eq!(events[0].correlation_id, events[1].correlation_id);
        assert_eq!(events[1].outcome, Some(expected));
        assert_eq!(
            events[1].agent_session_id.as_deref(),
            Some("agent-private-id")
        );
        assert!(!format!("{events:?}").contains("private-sentinel"));
        assert!(!format!("{events:?}").contains("opaque-one"));
    }
}

#[tokio::test(flavor = "current_thread")]
async fn timeout_must_be_positive_and_argument_errors_do_not_settle_waiters() {
    assert!(
        matches!(QuestionBroker::new(Some(Duration::ZERO), Arc::new(Diagnostics::default())), Err(error) if error.message == "The question request timeout must be positive or infinite.")
    );
    assert!(
        matches!(PermissionBroker::<String>::new(true, Some(Duration::ZERO), Arc::new(Diagnostics::default()), Arc::new(Events::default())), Err(error) if error.message == "The permission request timeout must be positive or infinite.")
    );
    let (broker, _, _) = broker(None);
    assert_eq!(
        broker.reply("", answer()).unwrap_err().kind,
        InteractionErrorKind::Argument
    );
    assert_eq!(
        broker.reject("").unwrap_err().kind,
        InteractionErrorKind::Argument
    );
    let (broker, _, _, _) = permissions(true, None);
    let token = Token::default();
    let mut wait = Box::pin(broker.request(request(Arc::new(Approval::default()), "one"), &token));
    assert_pending(wait.as_mut());
    let id = broker.pending().await[0].id.clone();
    assert_eq!(
        broker.reply(&id, "", "").await.unwrap_err().kind,
        InteractionErrorKind::Argument
    );
    assert_eq!(broker.pending().await.len(), 1);
    assert_eq!(
        broker.reply(&id, "Reject", "").await.unwrap_err().message,
        "unknown permission choice: Reject"
    );
    broker.reject(&id).await.unwrap();
    wait.await.unwrap();
}

#[tokio::test(flavor = "current_thread")]
async fn original_five_pending_deadline_rows_are_not_reset_by_snapshot() {
    for (timeout_ms, elapsed_ticks, expected) in [
        (Some(1000), 1, Some(1000)),
        (Some(1000), 2_500_001, Some(750)),
        (Some(1000), 10_000_000, Some(0)),
        (Some(1000), 11_000_000, Some(0)),
        (None, 11_000_000, None),
    ] {
        let (broker, clock, _) = broker(timeout_ms.map(Duration::from_millis));
        let token = Token::default();
        let mut wait = Box::pin(broker.ask(vec![question()], &token));
        assert_pending(wait.as_mut());
        assert_eq!(
            broker.pending()[0].remaining_milliseconds,
            timeout_ms.map(u128::from)
        );
        clock.advance(Duration::from_nanos(elapsed_ticks * 100));
        assert_eq!(broker.pending()[0].remaining_milliseconds, expected);
        assert_eq!(broker.pending()[0].remaining_milliseconds, expected);
        if timeout_ms.is_none() {
            broker.reply(&broker.pending()[0].id, answer()).unwrap();
            assert_eq!(wait.await.unwrap(), answer());
        } else {
            clock.advance(Duration::from_secs(1));
            assert_eq!(wait.await.unwrap().kind, QuestionReplyKind::UserAway);
        }
        assert!(broker.pending().is_empty());
    }
}

#[tokio::test(flavor = "current_thread")]
async fn default_tokio_clock_and_domain_contract_preserve_timeout_kind() {
    use parrot_domain::contracts::interactions::{
        PermissionBroker as PermissionContract, QuestionBroker as QuestionContract,
    };
    let diagnostics = Arc::new(Diagnostics::default());
    let token = Token::default();
    let broker = QuestionBroker::new(Some(Duration::from_millis(1)), diagnostics.clone()).unwrap();
    assert_eq!(
        QuestionContract::ask(&broker, vec![question()], &token)
            .await
            .unwrap()
            .kind,
        QuestionReplyKind::UserAway
    );
    let broker = PermissionBroker::new(
        true,
        Some(Duration::from_millis(1)),
        diagnostics,
        Arc::new(Events::default()),
    )
    .unwrap();
    assert_eq!(
        PermissionContract::request(
            &broker,
            request(Arc::new(Approval::default()), "one"),
            &token
        )
        .await
        .unwrap()
        .kind,
        PermissionReplyKind::UserAway
    );
    let mut wait = PermissionContract::request(
        &broker,
        request(Arc::new(Approval::default()), "one"),
        &token,
    );
    assert_pending(wait.as_mut());
    let id = PermissionContract::pending(&broker).await.unwrap()[0]
        .id
        .clone();
    assert!(
        PermissionContract::reply(&broker, &id, PermissionReply::user_away())
            .await
            .is_err()
    );
    PermissionContract::reply(
        &broker,
        &id,
        PermissionReply::decided(PermissionDecision::Reject, "  reason  "),
    )
    .await
    .unwrap();
    assert_eq!(
        wait.await.unwrap(),
        PermissionReply::decided(PermissionDecision::Reject, "reason")
    );
}

#[tokio::test(flavor = "current_thread")]
async fn synchronous_diagnostics_and_publication_do_not_consume_request_wait_timeout() {
    struct SlowDiagnostics(Arc<ManualClock>);
    impl InteractionDiagnostics for SlowDiagnostics {
        fn write(&self, event: BrokerDiagnostic) {
            if event.event == "wait_started" {
                self.0.advance(Duration::from_secs(60));
            }
        }
    }
    struct SlowEvents(Arc<ManualClock>);
    impl PermissionEvents<String> for SlowEvents {
        fn append<'a>(
            &'a self,
            pending: &'a PendingPermission<String>,
        ) -> BoxFuture<'a, Result<Committed<Event>, InteractionError>> {
            Box::pin(async move {
                self.0.advance(Duration::from_secs(60));
                Ok(committed_event(&pending.id))
            })
        }
        fn publish<'a>(
            &'a self,
            _: &'a Committed<Event>,
        ) -> BoxFuture<'a, Result<(), InteractionError>> {
            Box::pin(async { Ok(()) })
        }
    }
    let clock = Arc::new(ManualClock::new());
    let token = Token::default();
    let broker = QuestionBroker::with_clock(
        Some(Duration::from_secs(1)),
        clock.clone(),
        Arc::new(SlowDiagnostics(clock.clone())),
    )
    .unwrap();
    let mut wait = Box::pin(broker.ask(vec![question()], &token));
    assert_pending(wait.as_mut());
    assert_eq!(broker.pending()[0].remaining_milliseconds, Some(1000));
    clock.advance(Duration::from_secs(1));
    assert_eq!(wait.await.unwrap().kind, QuestionReplyKind::UserAway);
    let broker = PermissionBroker::with_clock(
        true,
        Some(Duration::from_secs(1)),
        clock.clone(),
        Arc::new(Diagnostics::default()),
        Arc::new(SlowEvents(clock.clone())),
    )
    .unwrap();
    let mut wait = Box::pin(broker.request(request(Arc::new(Approval::default()), "one"), &token));
    assert_pending(wait.as_mut());
    clock.advance(Duration::from_secs(1));
    assert_eq!(wait.await.unwrap().kind, PermissionReplyKind::UserAway);
}

mod children;
