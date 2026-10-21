use super::*;

#[derive(Default)]
struct Registry(Mutex<Vec<Arc<ChildQuestion>>>);
impl Registry {
    fn add(&self, name: &str, parent: &str) -> Arc<ChildIdentity> {
        let identity = Arc::new(ChildIdentity {
            session_id: format!("session-{name}"),
            name: name.into(),
            parent_session_id: parent.into(),
        });
        self.0
            .lock()
            .unwrap()
            .push(Arc::new(ChildQuestion::new(identity.clone())));
        identity
    }
}
impl ChildQuestionRegistry for Registry {
    fn find(&self, name: &str) -> Option<Arc<ChildQuestion>> {
        self.0
            .lock()
            .unwrap()
            .iter()
            .find(|child| child.identity.name == name)
            .cloned()
    }
    fn snapshot(&self) -> Vec<Arc<ChildQuestion>> {
        self.0.lock().unwrap().clone()
    }
}
#[derive(Default)]
struct Delivery {
    steers: Mutex<Vec<(String, String, String)>>,
    fail: AtomicBool,
    block: AtomicBool,
    released: Notify,
}
impl ChildQuestionDelivery for Delivery {
    fn steer<'a>(
        &'a self,
        parent: &'a str,
        id: &'a str,
        text: String,
    ) -> BoxFuture<'a, Result<(), InteractionError>> {
        Box::pin(async move {
            self.steers
                .lock()
                .unwrap()
                .push((parent.into(), id.into(), text));
            if self.block.load(Ordering::SeqCst) {
                self.released.notified().await;
            }
            if self.fail.load(Ordering::SeqCst) {
                Err(InteractionError {
                    kind: InteractionErrorKind::Delivery,
                    message: "delivery failed".into(),
                })
            } else {
                Ok(())
            }
        })
    }
}
struct Templates;
impl ChildQuestionTemplates for Templates {
    fn question(&self, name: &str, questions: &str) -> Result<String, InteractionError> {
        Ok(format!("child {name}\n{questions}"))
    }
    fn reminder(&self, children: &str) -> Result<String, InteractionError> {
        Ok(format!("pending:{children}"))
    }
}
fn coordinator(
    owner: &str,
    registry: Arc<Registry>,
    delivery: Arc<Delivery>,
) -> ChildQuestionCoordinator {
    ChildQuestionCoordinator::new(owner.into(), registry, delivery, Arc::new(Templates))
}

#[test]
fn steer_formats_original_labels_options_and_lowercase_booleans() {
    let mut second = question();
    second.header = "other".into();
    second.multiple = true;
    second.custom = false;
    assert_eq!(
        format_questions(&[question(), second]),
        "Question 1:\nHeader: header\nPrompt: private-sentinel prompt\nMultiple: false\nCustom: true\nOptions:\n- first — description\n- second\n\nQuestion 2:\nHeader: other\nPrompt: private-sentinel prompt\nMultiple: true\nCustom: false\nOptions:\n- first — description\n- second"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn parent_scoped_questions_copy_validate_and_settle_once() {
    let registry = Arc::new(Registry::default());
    let child = registry.add("child", "parent");
    let delivery = Arc::new(Delivery::default());
    let coordinator = coordinator("parent", registry.clone(), delivery.clone());
    let token = Token::default();
    let mut wait = Box::pin(coordinator.ask(child.clone(), vec![question()], &token));
    assert_pending(wait.as_mut());
    assert_eq!(coordinator.pending_for_parent("other").len(), 0);
    assert_eq!(coordinator.pending_for_parent("parent").len(), 1);
    let mut snapshot = coordinator.pending();
    snapshot[0].questions[0].prompt.clear();
    assert_eq!(
        coordinator.pending()[0].questions[0].prompt,
        "private-sentinel prompt"
    );
    assert!(
        coordinator
            .reply("child", QuestionReply::answered(vec![]))
            .is_err()
    );
    assert_eq!(coordinator.pending().len(), 1);
    let mut duplicate = Box::pin(coordinator.ask(child, vec![question()], &token));
    assert!(
        matches!(duplicate.as_mut().poll(&mut Context::from_waker(&Waker::from(Arc::new(NoWake)))), Poll::Ready(Err(OperationError::Failed(error))) if error.message == "the child already has a pending question")
    );
    coordinator
        .reply_from_parent("parent", registry.as_ref(), "child", answer())
        .unwrap();
    assert_eq!(wait.await.unwrap(), answer());
    assert!(coordinator.pending().is_empty());
    assert_eq!(
        coordinator.reply("child", answer()).unwrap_err().message,
        "child question request is no longer pending"
    );
    let steers = delivery.steers.lock().unwrap();
    assert_eq!(steers.len(), 1);
    assert_eq!(steers[0].0, "parent");
    assert_eq!(steers[0].1, snapshot[0].id);
    assert!(steers[0].2.starts_with("child child\nQuestion 1:"));
}

#[tokio::test(flavor = "current_thread")]
async fn wrong_parent_stale_session_and_root_are_rejected() {
    let registry = Arc::new(Registry::default());
    let child = registry.add("child", "parent");
    let delivery = Arc::new(Delivery::default());
    let coordinator = coordinator("parent", registry.clone(), delivery);
    let token = Token::default();
    let impostor = Arc::new((*child).clone());
    assert!(
        matches!(coordinator.ask(impostor, vec![question()], &token).await, Err(OperationError::Failed(error)) if error.message == "child agent not found: child")
    );
    let root = Arc::new(ChildIdentity {
        session_id: "root".into(),
        name: "root".into(),
        parent_session_id: String::new(),
    });
    assert!(
        matches!(coordinator.ask(root, vec![question()], &token).await, Err(OperationError::Failed(error)) if error.message == "only child agents may ask questions")
    );
    let mut wait = Box::pin(coordinator.ask(child, vec![question()], &token));
    assert_pending(wait.as_mut());
    let unrelated = Registry::default();
    assert_eq!(
        coordinator
            .reply_from_parent("other", &unrelated, "child", answer())
            .unwrap_err()
            .kind,
        InteractionErrorKind::Registry
    );
    unrelated.add("child", "other");
    assert_eq!(
        coordinator
            .reply_from_parent("other", &unrelated, "child", answer())
            .unwrap_err()
            .message,
        "only the owning parent may answer a child question"
    );
    assert_eq!(coordinator.pending().len(), 1);
    coordinator.reply("child", answer()).unwrap();
    assert_eq!(wait.await.unwrap(), answer());
}

#[tokio::test(flavor = "current_thread")]
async fn cancellation_disposal_and_dropped_ask_leave_no_child_waiter() {
    let registry = Arc::new(Registry::default());
    let child = registry.add("child", "parent");
    let delivery = Arc::new(Delivery::default());
    let coordinator = coordinator("parent", registry, delivery);
    let token = Token::default();
    let mut wait = Box::pin(coordinator.ask(child.clone(), vec![question()], &token));
    assert_pending(wait.as_mut());
    token.cancel();
    assert_eq!(wait.await.unwrap_err(), OperationError::Cancelled);
    assert!(coordinator.pending().is_empty());
    let token = Token::default();
    let mut wait = Box::pin(coordinator.ask(child.clone(), vec![question()], &token));
    assert_pending(wait.as_mut());
    drop(wait);
    assert!(coordinator.pending().is_empty());
    let mut wait = Box::pin(coordinator.ask(child.clone(), vec![question()], &token));
    assert_pending(wait.as_mut());
    coordinator.close();
    coordinator.close();
    assert!(coordinator.pending().is_empty());
    assert!(coordinator.completion_reminder().unwrap().is_none());
    assert!(
        matches!(wait.await, Err(OperationError::Failed(error)) if error.message == "question session closed")
    );
    assert!(
        matches!(coordinator.ask(child, vec![question()], &token).await, Err(OperationError::Failed(error)) if error.kind == InteractionErrorKind::Disposed)
    );
}

#[tokio::test(flavor = "current_thread")]
async fn child_reply_wins_cancellation_and_send_failure_withdraws() {
    let registry = Arc::new(Registry::default());
    let child = registry.add("child", "parent");
    let delivery = Arc::new(Delivery::default());
    let coordinator = coordinator("parent", registry, delivery.clone());
    let token = Token::default();
    let mut wait = Box::pin(coordinator.ask(child.clone(), vec![question()], &token));
    assert_pending(wait.as_mut());
    coordinator.reply("child", answer()).unwrap();
    token.cancel();
    assert_eq!(wait.await.unwrap(), answer());
    let token = Token::default();
    delivery.fail.store(true, Ordering::SeqCst);
    assert!(
        matches!(coordinator.ask(child, vec![question()], &token).await, Err(OperationError::Failed(error)) if error.kind == InteractionErrorKind::Delivery)
    );
    assert!(coordinator.pending().is_empty());
}

#[tokio::test(flavor = "current_thread")]
async fn multiple_children_are_independent_and_reminder_is_name_ordered() {
    let registry = Arc::new(Registry::default());
    let z = registry.add("z", "parent");
    let a = registry.add("a", "parent");
    let delivery = Arc::new(Delivery::default());
    let coordinator = coordinator("parent", registry, delivery);
    let token = Token::default();
    let mut first = Box::pin(coordinator.ask(z, vec![question()], &token));
    let mut second = Box::pin(coordinator.ask(a, vec![question()], &token));
    assert_pending(first.as_mut());
    assert_pending(second.as_mut());
    assert_eq!(coordinator.pending().len(), 2);
    assert_eq!(
        coordinator.completion_reminder().unwrap().as_deref(),
        Some("pending:\n- a\n- z")
    );
    coordinator.reply("a", answer()).unwrap();
    assert_eq!(second.await.unwrap(), answer());
    assert_eq!(coordinator.pending().len(), 1);
    coordinator.reply("z", answer()).unwrap();
    first.await.unwrap();
}

#[tokio::test(flavor = "current_thread")]
async fn nested_questions_deliver_to_immediate_parent_not_root_broker() {
    let registry = Arc::new(Registry::default());
    let child = registry.add("grandchild", "child");
    let delivery = Arc::new(Delivery::default());
    let coordinator = coordinator("child", registry, delivery.clone());
    let (root, _, _) = broker(None);
    let token = Token::default();
    let mut wait = Box::pin(coordinator.ask(child, vec![question()], &token));
    assert_pending(wait.as_mut());
    assert_eq!(delivery.steers.lock().unwrap()[0].0, "child");
    assert!(root.pending().is_empty());
    assert!(coordinator.pending_for_parent("root").is_empty());
    coordinator.reply("grandchild", answer()).unwrap();
    wait.await.unwrap();
}

#[tokio::test(flavor = "current_thread")]
async fn concurrent_child_replies_have_one_winner_and_do_not_settle_next_request() {
    let registry = Arc::new(Registry::default());
    let child = registry.add("child", "parent");
    let coordinator = coordinator("parent", registry, Arc::new(Delivery::default()));
    let token = Token::default();
    let mut wait = Box::pin(coordinator.ask(child.clone(), vec![question()], &token));
    assert_pending(wait.as_mut());
    let winners = std::thread::scope(|scope| {
        let one = scope.spawn(|| coordinator.reply("child", answer()).is_ok());
        let two = scope.spawn(|| coordinator.reply("child", answer()).is_ok());
        usize::from(one.join().unwrap()) + usize::from(two.join().unwrap())
    });
    assert_eq!(winners, 1);
    wait.await.unwrap();
    let mut next = Box::pin(coordinator.ask(child, vec![question()], &token));
    assert_pending(next.as_mut());
    assert_eq!(coordinator.pending().len(), 1);
    coordinator.reply("child", answer()).unwrap();
    next.await.unwrap();
}

#[tokio::test(flavor = "current_thread")]
async fn parent_delivery_does_not_inherit_child_cancellation_and_drop_cleans_open_request() {
    let registry = Arc::new(Registry::default());
    let child = registry.add("child", "parent");
    let delivery = Arc::new(Delivery::default());
    delivery.block.store(true, Ordering::SeqCst);
    let coordinator = coordinator("parent", registry, delivery.clone());
    let token = Token::default();
    let mut wait = Box::pin(coordinator.ask(child.clone(), vec![question()], &token));
    assert_pending(wait.as_mut());
    token.cancel();
    assert_pending(wait.as_mut());
    assert_eq!(coordinator.pending().len(), 1);
    delivery.released.notify_one();
    assert_eq!(wait.await.unwrap_err(), OperationError::Cancelled);
    assert!(coordinator.pending().is_empty());
    let token = Token::default();
    let mut wait = Box::pin(coordinator.ask(child, vec![question()], &token));
    assert_pending(wait.as_mut());
    drop(wait);
    assert!(coordinator.pending().is_empty());
}
