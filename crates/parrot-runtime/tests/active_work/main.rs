// One shared identity counter prevents independent fixture imports reusing paths.
#[allow(dead_code)]
#[path = "../children/admission_publisher.rs"]
mod admission_publisher;
#[allow(dead_code)]
#[path = "../children/support.rs"]
mod support;

mod blockers;
mod runtime;
mod scheduler;
mod status;

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use parrot_domain::contracts::async_ops::{BoxFuture, Cancellation, OperationError};
use parrot_domain::contracts::events::EventPublisher;
use parrot_domain::contracts::paths::{RootOrigin, SelectedSessionRoot};
use parrot_domain::contracts::storage::Committed;
use parrot_domain::protocol::messages::parrot::{AgentNotice, Event, ExitReminderChanged};
use parrot_foundation::template_catalog::PromptTemplateCatalog;
use parrot_runtime::active_work::exit_reminders::{
    CHANGED_NOTICE_KIND, ExitReminders, ReminderError,
};
use parrot_runtime::event_broker::{BrokerError, EventBroker};
use parrot_storage::conversation::{AgentHistoryView, ConversationRepository};
use parrot_storage::database::SessionDatabase;
use parrot_storage::events::{EventRepository, NeverCancelled};
use parrot_storage::history_files::AgentHistoryFile;
use protobuf::Message;

static NEXT: AtomicU64 = AtomicU64::new(0);

struct Fixture {
    path: PathBuf,
    repository: ConversationRepository,
}

impl Fixture {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!(
            "parrot-t52-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&path).unwrap();
        let repository = Self::open(&path);
        Self { path, repository }
    }

    fn open(path: &std::path::Path) -> ConversationRepository {
        let root = SelectedSessionRoot::new(path.to_owned(), RootOrigin::Current).unwrap();
        ConversationRepository::new(EventRepository::new(SessionDatabase::open(root).unwrap()))
    }

    fn history_for(repository: &ConversationRepository) -> AgentHistoryView {
        let root = repository.events().database().selected_root().clone();
        let projection = AgentHistoryFile::new(
            root.clone(),
            root.path().join("history.jsonl"),
            vec!["agent".into()],
        )
        .unwrap();
        repository
            .bind_agent_history("agent".into(), Arc::new(projection))
            .unwrap()
    }

    fn history(&self) -> AgentHistoryView {
        Self::history_for(&self.repository)
    }

    async fn reminders(&self, publisher: Arc<DurablePublisher>) -> ExitReminders<DurablePublisher> {
        ExitReminders::new(self.history(), publisher, templates())
            .await
            .unwrap()
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

fn templates() -> Arc<PromptTemplateCatalog> {
    Arc::new(
        PromptTemplateCatalog::from_content(
            "",
            &BTreeMap::from([("HOME".to_owned(), "/synthetic-t52-home".to_owned())]),
        )
        .unwrap()
        .1,
    )
}

/// Checks actual database visibility at the publication boundary, then uses the
/// real broker. It is not a fake persistence service or a fixture commit token.
struct DurablePublisher {
    history: AgentHistoryView,
    broker: EventBroker,
    attempts: Mutex<Vec<Event>>,
    fail: AtomicBool,
}

impl DurablePublisher {
    fn new(history: AgentHistoryView) -> Arc<Self> {
        Arc::new(Self {
            history,
            broker: EventBroker::new(),
            attempts: Mutex::new(Vec::new()),
            fail: AtomicBool::new(false),
        })
    }
}

impl EventPublisher for DurablePublisher {
    type Event = Event;
    type Error = BrokerError;

    fn publish_committed<'a>(
        &'a self,
        event: &'a Committed<Event>,
        cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<(), OperationError<BrokerError>>> {
        Box::pin(async move {
            let notices = self
                .history
                .notices(CHANGED_NOTICE_KIND.into())
                .await
                .unwrap();
            assert_eq!(notices.last().unwrap(), event.value().agent_notice());
            self.attempts.lock().unwrap().push(event.value().clone());
            if self.fail.load(Ordering::SeqCst) {
                return Err(OperationError::Failed(BrokerError::InvalidCapacity));
            }
            self.broker.publish_committed(event, cancellation).await
        })
    }

    fn publish_transient<'a>(
        &'a self,
        event: Event,
        cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<(), OperationError<BrokerError>>> {
        self.broker.publish_transient(event, cancellation)
    }
}

struct Cancelled;
impl Cancellation for Cancelled {
    fn is_cancelled(&self) -> bool {
        true
    }
    fn cancelled(&self) -> BoxFuture<'_, ()> {
        Box::pin(async {})
    }
}

async fn append_notice(
    repository: &ConversationRepository,
    id: &str,
    agent: &str,
    kind: &str,
    data: Vec<u8>,
) {
    let mut event = Event {
        id: id.into(),
        agent_session_id: agent.into(),
        ..Event::new()
    };
    event.set_agent_notice(AgentNotice {
        kind: kind.into(),
        notice: id.into(),
        data,
        ..AgentNotice::new()
    });
    repository.append(event, None, None).await.unwrap();
}

#[tokio::test]
async fn source_set_and_clear_record_announce_and_restore_both_original_rows() {
    for clear in [false, true] {
        let fixture = Fixture::new();
        let publisher = DurablePublisher::new(fixture.history());
        let reminders = fixture.reminders(publisher.clone()).await;
        reminders
            .set("tests", "run the tests", &NeverCancelled)
            .await
            .unwrap();
        let listener = publisher.broker.subscribe();
        reminders
            .set("port", "finish the port", &NeverCancelled)
            .await
            .unwrap();
        if clear {
            assert!(listener.try_next().is_some());
            assert!(reminders.clear("port", &NeverCancelled).await.unwrap());
        }
        let announced = listener.try_next().unwrap();
        let notice = announced.agent_notice();
        let changed = ExitReminderChanged::parse_from_bytes(&notice.data).unwrap();
        assert_eq!(notice.kind, CHANGED_NOTICE_KIND);
        assert_eq!(
            notice.notice,
            if clear {
                "Exit reminder cleared: port"
            } else {
                "Exit reminder set: port: finish the port"
            }
        );
        assert_eq!(changed.title, "port");
        assert_eq!(announced.agent_session_id, "agent");
        assert!(!announced.id.is_empty());
        if clear {
            assert!(changed.has_cleared());
            assert!(changed.cleared());
        } else {
            assert!(changed.has_description());
            assert_eq!(changed.description(), "finish the port");
        }
        let restored = fixture.reminders(publisher).await;
        assert_eq!(restored.titles().await.len(), if clear { 1 } else { 2 });
    }
}

#[tokio::test]
async fn notices_filter_kind_and_agent_in_recorded_order() {
    let fixture = Fixture::new();
    for (id, agent, kind) in [
        ("first", "agent", "wanted"),
        ("other", "agent", "unwanted"),
        ("foreign", "another", "wanted"),
        ("second", "agent", "wanted"),
    ] {
        append_notice(&fixture.repository, id, agent, kind, vec![]).await;
    }
    let notices = fixture.history().notices("wanted".into()).await.unwrap();
    assert_eq!(
        notices
            .into_iter()
            .map(|notice| notice.notice)
            .collect::<Vec<_>>(),
        ["first", "second"]
    );
}

#[tokio::test]
async fn replacement_keeps_live_position_but_rehydration_moves_last_change_to_end() {
    let fixture = Fixture::new();
    let publisher = DurablePublisher::new(fixture.history());
    let reminders = fixture.reminders(publisher.clone()).await;
    reminders.set("a", "first", &NeverCancelled).await.unwrap();
    reminders.set("b", "second", &NeverCancelled).await.unwrap();
    reminders
        .set("a", "updated", &NeverCancelled)
        .await
        .unwrap();
    assert_eq!(reminders.titles().await, ["a", "b"]);
    let mut copied = reminders.titles().await;
    copied.clear();
    assert_eq!(reminders.titles().await, ["a", "b"]);
    assert_eq!(
        fixture.reminders(publisher).await.titles().await,
        ["b", "a"]
    );
}

#[tokio::test]
async fn build_counts_reset_only_on_effective_changes_and_clear() {
    let fixture = Fixture::new();
    let publisher = DurablePublisher::new(fixture.history());
    let reminders = fixture.reminders(publisher.clone()).await;
    assert_eq!(reminders.build().await.unwrap(), None);
    reminders
        .set("one", "finish", &NeverCancelled)
        .await
        .unwrap();
    let first = reminders.build().await.unwrap().unwrap();
    assert_eq!(
        first,
        "Exit reminders are set. You cannot finish until every one is cleared with clear_exit_reminder:\n- one: finish"
    );
    assert!(
        reminders
            .build()
            .await
            .unwrap()
            .unwrap()
            .starts_with("This is the 2nd exit reminder.")
    );
    reminders
        .set("one", "finish", &NeverCancelled)
        .await
        .unwrap();
    assert!(!reminders.clear("missing", &NeverCancelled).await.unwrap());
    assert!(
        reminders
            .build()
            .await
            .unwrap()
            .unwrap()
            .starts_with("This is the 3rd exit reminder.")
    );
    assert_eq!(
        publisher.attempts.lock().unwrap().len(),
        2,
        "identical Set still appends/publishes"
    );
    reminders
        .set("one", "changed", &NeverCancelled)
        .await
        .unwrap();
    assert!(
        reminders
            .build()
            .await
            .unwrap()
            .unwrap()
            .starts_with("Exit reminders are set.")
    );
    reminders
        .set("two", "other", &NeverCancelled)
        .await
        .unwrap();
    assert!(
        reminders
            .build()
            .await
            .unwrap()
            .unwrap()
            .starts_with("Exit reminders are set.")
    );
    reminders.build().await.unwrap();
    assert!(reminders.clear("two", &NeverCancelled).await.unwrap());
    assert!(
        reminders
            .build()
            .await
            .unwrap()
            .unwrap()
            .starts_with("Exit reminders are set.")
    );
    assert!(reminders.clear("one", &NeverCancelled).await.unwrap());
    assert_eq!(reminders.build().await.unwrap(), None);
}

#[tokio::test]
async fn ordinals_follow_original_teen_and_last_digit_rules() {
    let fixture = Fixture::new();
    let reminders = fixture
        .reminders(DurablePublisher::new(fixture.history()))
        .await;
    reminders
        .set("test", "done", &NeverCancelled)
        .await
        .unwrap();
    for count in 1..=123 {
        let output = reminders.build().await.unwrap().unwrap();
        for (number, expected) in [
            (2, "2nd"),
            (3, "3rd"),
            (4, "4th"),
            (11, "11th"),
            (12, "12th"),
            (13, "13th"),
            (21, "21st"),
            (22, "22nd"),
            (23, "23rd"),
            (111, "111th"),
            (112, "112th"),
            (113, "113th"),
            (121, "121st"),
            (122, "122nd"),
            (123, "123rd"),
        ] {
            if count == number {
                assert!(output.starts_with(&format!("This is the {expected} exit reminder.")));
            }
        }
    }
}

#[tokio::test]
async fn empty_description_retains_source_live_vs_durable_quirk_and_whitespace_is_literal() {
    let fixture = Fixture::new();
    let publisher = DurablePublisher::new(fixture.history());
    let reminders = fixture.reminders(publisher.clone()).await;
    reminders.set("empty", "", &NeverCancelled).await.unwrap();
    assert_eq!(reminders.titles().await, ["empty"]);
    assert!(
        reminders
            .build()
            .await
            .unwrap()
            .unwrap()
            .ends_with("- empty: ")
    );
    assert!(
        fixture
            .reminders(publisher.clone())
            .await
            .titles()
            .await
            .is_empty()
    );
    reminders.set(" \t", " \n", &NeverCancelled).await.unwrap();
    assert_eq!(fixture.reminders(publisher).await.titles().await, [" \t"]);
}

#[tokio::test]
async fn empty_title_errors_before_append_but_preserves_source_count_reset() {
    let fixture = Fixture::new();
    let publisher = DurablePublisher::new(fixture.history());
    let reminders = fixture.reminders(publisher.clone()).await;
    reminders.set("a", "x", &NeverCancelled).await.unwrap();
    reminders.build().await.unwrap();
    assert!(matches!(
        reminders.set("", "x", &NeverCancelled).await,
        Err(ReminderError::EmptyTitle)
    ));
    assert_eq!(reminders.titles().await, ["a"]);
    assert!(
        reminders
            .build()
            .await
            .unwrap()
            .unwrap()
            .starts_with("Exit reminders are set.")
    );
    assert!(!reminders.clear("", &NeverCancelled).await.unwrap());
    assert_eq!(publisher.attempts.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn cancelled_publication_does_not_cancel_commit_or_rehydrate() {
    let fixture = Fixture::new();
    let publisher = DurablePublisher::new(fixture.history());
    let listener = publisher.broker.subscribe();
    let reminders = fixture.reminders(publisher.clone()).await;
    assert!(matches!(
        reminders.set("kept", "durable", &Cancelled).await,
        Err(ReminderError::Publication(OperationError::Cancelled))
    ));
    assert!(listener.try_next().is_none());
    assert_eq!(reminders.titles().await, ["kept"]);
    assert_eq!(
        fixture.reminders(publisher.clone()).await.titles().await,
        ["kept"]
    );
    assert!(matches!(
        reminders.clear("kept", &Cancelled).await,
        Err(ReminderError::Publication(OperationError::Cancelled))
    ));
    assert!(fixture.reminders(publisher).await.titles().await.is_empty());
}

#[tokio::test]
async fn failed_publication_leaves_real_durable_change_and_live_state() {
    let fixture = Fixture::new();
    let publisher = DurablePublisher::new(fixture.history());
    publisher.fail.store(true, Ordering::SeqCst);
    let reminders = fixture.reminders(publisher.clone()).await;
    assert!(matches!(
        reminders.set("kept", "durable", &NeverCancelled).await,
        Err(ReminderError::Publication(OperationError::Failed(_)))
    ));
    assert_eq!(reminders.titles().await, ["kept"]);
    assert_eq!(fixture.reminders(publisher).await.titles().await, ["kept"]);
}

#[tokio::test]
async fn real_closed_database_prevents_publish_and_preserves_source_failure_mutations() {
    let fixture = Fixture::new();
    let publisher = DurablePublisher::new(fixture.history());
    let reminders = fixture.reminders(publisher.clone()).await;
    reminders
        .set("original", "first", &NeverCancelled)
        .await
        .unwrap();
    fixture
        .repository
        .events()
        .database()
        .close()
        .await
        .unwrap();
    assert!(matches!(
        reminders.set("new", "failed", &NeverCancelled).await,
        Err(ReminderError::Storage(_))
    ));
    assert_eq!(reminders.titles().await, ["original"]);
    assert!(matches!(
        reminders.clear("original", &NeverCancelled).await,
        Err(ReminderError::Storage(_))
    ));
    assert!(
        reminders.titles().await.is_empty(),
        "source removes before failed append"
    );
    assert_eq!(publisher.attempts.lock().unwrap().len(), 1);
    let reopened_repository = Fixture::open(&fixture.path);
    let history = Fixture::history_for(&reopened_repository);
    let restored = ExitReminders::new(history.clone(), DurablePublisher::new(history), templates())
        .await
        .unwrap();
    assert_eq!(
        restored.titles().await,
        ["original"],
        "failed clear never reached durable history"
    );
    reopened_repository
        .events()
        .database()
        .close()
        .await
        .unwrap();
}

#[tokio::test]
async fn real_reopen_restores_committed_notices_and_resets_transient_build_count() {
    let fixture = Fixture::new();
    let publisher = DurablePublisher::new(fixture.history());
    let reminders = fixture.reminders(publisher).await;
    reminders.set("a", "first", &NeverCancelled).await.unwrap();
    reminders.set("b", "second", &NeverCancelled).await.unwrap();
    reminders.set("a", "last", &NeverCancelled).await.unwrap();
    reminders.build().await.unwrap();
    reminders.build().await.unwrap();
    fixture
        .repository
        .events()
        .database()
        .close()
        .await
        .unwrap();
    let repository = Fixture::open(&fixture.path);
    let history = Fixture::history_for(&repository);
    let restored = ExitReminders::new(history.clone(), DurablePublisher::new(history), templates())
        .await
        .unwrap();
    assert_eq!(restored.titles().await, ["b", "a"]);
    assert!(
        restored
            .build()
            .await
            .unwrap()
            .unwrap()
            .starts_with("Exit reminders are set.")
    );
    repository.events().database().close().await.unwrap();
}

#[tokio::test]
async fn malformed_changed_payload_fails_rehydration_other_kinds_are_ignored() {
    let fixture = Fixture::new();
    append_notice(
        &fixture.repository,
        "ignored",
        "agent",
        "another_kind",
        vec![0xff],
    )
    .await;
    let publisher = DurablePublisher::new(fixture.history());
    assert!(
        fixture
            .reminders(publisher.clone())
            .await
            .titles()
            .await
            .is_empty()
    );
    append_notice(
        &fixture.repository,
        "bad",
        "agent",
        CHANGED_NOTICE_KIND,
        vec![0xff],
    )
    .await;
    assert!(matches!(
        ExitReminders::new(fixture.history(), publisher, templates()).await,
        Err(ReminderError::Encoding(_))
    ));
}

#[tokio::test]
async fn rehydrate_uses_oneof_presence_not_cleared_boolean_value() {
    let fixture = Fixture::new();
    let mut changed = ExitReminderChanged::new();
    changed.title = "a".into();
    changed.set_description(String::new());
    append_notice(
        &fixture.repository,
        "set",
        "agent",
        CHANGED_NOTICE_KIND,
        changed.write_to_bytes().unwrap(),
    )
    .await;
    let publisher = DurablePublisher::new(fixture.history());
    assert_eq!(
        fixture.reminders(publisher.clone()).await.titles().await,
        ["a"]
    );
    changed.set_cleared(false);
    append_notice(
        &fixture.repository,
        "remove",
        "agent",
        CHANGED_NOTICE_KIND,
        changed.write_to_bytes().unwrap(),
    )
    .await;
    assert!(fixture.reminders(publisher).await.titles().await.is_empty());
}

#[tokio::test]
async fn dropped_mutation_future_can_commit_but_requires_rehydration_and_later_owned_join() {
    use std::future::Future;
    use std::task::{Context, Poll, Wake, Waker};

    struct NoWake;
    impl Wake for NoWake {
        fn wake(self: Arc<Self>) {}
    }

    let fixture = Fixture::new();
    let publisher = DurablePublisher::new(fixture.history());
    let reminders = fixture.reminders(publisher.clone()).await;
    let (release, blocked) = std::sync::mpsc::channel();
    let barrier = fixture
        .repository
        .events()
        .database()
        .admit("barrier", &NeverCancelled, move |_| {
            blocked
                .recv_timeout(std::time::Duration::from_secs(5))
                .unwrap();
            Ok(())
        })
        .unwrap();
    let mut mutation = Box::pin(reminders.set("committed", "but unobserved", &NeverCancelled));
    let waker = Waker::from(Arc::new(NoWake));
    assert!(matches!(
        mutation.as_mut().poll(&mut Context::from_waker(&waker)),
        Poll::Pending
    ));
    drop(mutation);
    release.send(()).unwrap();
    barrier.into_completion().await.unwrap();
    // This queued read follows the admitted append on the real database worker.
    assert_eq!(
        fixture
            .history()
            .notices(CHANGED_NOTICE_KIND.into())
            .await
            .unwrap()
            .len(),
        1
    );
    assert!(
        reminders.titles().await.is_empty(),
        "observer drop left this instance stale"
    );
    assert_eq!(
        fixture.reminders(publisher.clone()).await.titles().await,
        ["committed"]
    );
    assert!(
        publisher.attempts.lock().unwrap().is_empty(),
        "publication was never reached"
    );
}
