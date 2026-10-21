use super::Fixture;
use parrot_domain::contracts::async_ops::{BoxFuture, Cancellation};
use parrot_domain::contracts::storage::AdmissionError;
use parrot_domain::protocol::messages::parrot::{Delivery, Event, InputAdmitted, InputPromoted};
use parrot_storage::events::{
    AdmittedInput, ConversationPart, EventError, EventRepository, NeverCancelled,
};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, mpsc};

struct Cancel(AtomicBool);
impl Cancellation for Cancel {
    fn is_cancelled(&self) -> bool {
        self.0.load(Ordering::SeqCst)
    }
    fn cancelled(&self) -> BoxFuture<'_, ()> {
        Box::pin(std::future::pending())
    }
}
fn admitted(id: &str, agent: &str, input: &AdmittedInput) -> Result<Event, EventError> {
    let mut event = Event::new();
    event.id = id.into();
    event.agent_session_id = agent.into();
    event.set_input_admitted(InputAdmitted {
        input_id: input.id.clone(),
        message_id: input.message_id.clone(),
        content: input.content(),
        delivery: input.delivery.into(),
        ..Default::default()
    });
    Ok(event)
}
async fn admit(
    repository: &EventRepository,
    agent: &str,
    message: &str,
    id: &str,
    delivery: Delivery,
) -> parrot_storage::events::Admission {
    let a = agent.to_owned();
    let id = id.to_owned();
    repository
        .admit_input(
            agent.into(),
            message.into(),
            vec![ConversationPart::text(message)],
            delivery,
            move |input| admitted(&id, &a, input),
            &NeverCancelled,
        )
        .unwrap()
        .into_completion()
        .await
        .unwrap()
        .into_value()
        .into_output()
}
#[tokio::test]
async fn actual_input_admission_survives_dropped_completion_and_retry_keeps_compound_key() {
    let fixture = Fixture::new();
    let repository = fixture.repository();
    let cancel = Arc::new(Cancel(AtomicBool::new(false)));
    let (entered_tx, entered_rx) = tokio::sync::oneshot::channel();
    let (release_tx, release_rx) = mpsc::channel();
    let blocker = repository
        .admit("block", &NeverCancelled, move |_| {
            entered_tx.send(()).unwrap();
            release_rx.recv().unwrap();
            Ok(())
        })
        .unwrap();
    entered_rx.await.unwrap();
    let ticket = repository
        .admit_input(
            "agent".into(),
            "message".into(),
            vec![ConversationPart::text("content")],
            Delivery::DELIVERY_QUEUE,
            |input| admitted("event", "agent", input),
            cancel.as_ref(),
        )
        .unwrap();
    assert_eq!(ticket.key(), &("agent".to_string(), "message".to_string()));
    drop(ticket.into_completion());
    cancel.0.store(true, Ordering::SeqCst);
    release_tx.send(()).unwrap();
    blocker.into_completion().await.unwrap();
    let replay = repository.replay().await.unwrap();
    assert_eq!(replay.len(), 1);
    let original = replay[0].input_admitted().input_id.clone();
    let retry = repository
        .admit_input(
            "agent".into(),
            "message".into(),
            vec![ConversationPart::text("content")],
            Delivery::DELIVERY_QUEUE,
            |_| panic!("retry compose must not run"),
            &NeverCancelled,
        )
        .unwrap()
        .into_completion()
        .await
        .unwrap()
        .into_value()
        .into_output();
    assert!(!retry.created());
    assert_eq!(retry.input.id, original);
    assert_eq!(repository.replay().await.unwrap().len(), 1);
    repository.database().close().await.unwrap();
    let reopened = fixture.repository();
    assert_eq!(
        reopened
            .inputs_for_next_promotion("agent".into())
            .await
            .unwrap()[0]
            .id,
        original
    );
    reopened.database().close().await.unwrap();
}
#[tokio::test]
async fn preadmission_cancel_and_compose_failure_have_no_durable_effect() {
    let fixture = Fixture::new();
    let repository = fixture.repository();
    let cancelled = Cancel(AtomicBool::new(true));
    assert!(matches!(
        repository.admit_input(
            "agent".into(),
            "message".into(),
            vec![],
            Delivery::DELIVERY_QUEUE,
            |_| panic!("cancelled compose"),
            &cancelled
        ),
        Err(AdmissionError::Cancelled)
    ));
    let ticket = repository
        .admit_input(
            "agent".into(),
            "message".into(),
            vec![ConversationPart::text("content")],
            Delivery::DELIVERY_QUEUE,
            |_| Err(EventError::Invalid("compose failure".into())),
            &NeverCancelled,
        )
        .unwrap();
    assert!(
        matches!(ticket.into_completion().await,Err(EventError::Invalid(message))if message=="compose failure")
    );
    assert!(repository.replay().await.unwrap().is_empty());
    assert!(!repository.has_pending_inputs("agent".into()).await.unwrap());
    repository.database().close().await.unwrap();
}
#[tokio::test]
async fn promotion_second_compose_failure_rolls_back_all_inputs_events_and_projections() {
    let fixture = Fixture::new();
    let repository = fixture.repository();
    let first = admit(
        &repository,
        "agent",
        "one",
        "admit-one",
        Delivery::DELIVERY_STEER,
    )
    .await;
    let second = admit(
        &repository,
        "agent",
        "two",
        "admit-two",
        Delivery::DELIVERY_STEER,
    )
    .await;
    let mut count = 0;
    assert!(
        repository
            .promote_steers("agent".into(), move |input| {
                count += 1;
                if count == 2 {
                    return Err(EventError::Invalid("second compose".into()));
                }
                let mut event = Event::new();
                event.id = "promotion".into();
                event.agent_session_id = "agent".into();
                event.set_input_promoted(InputPromoted {
                    input_id: input.id.clone(),
                    message_id: input.message_id.clone(),
                    ..Default::default()
                });
                Ok(event)
            })
            .await
            .is_err()
    );
    let pending = repository
        .inputs_for_next_promotion("agent".into())
        .await
        .unwrap();
    assert_eq!(
        pending.iter().map(|input| &input.id).collect::<Vec<_>>(),
        vec![&first.input.id, &second.input.id]
    );
    assert_eq!(repository.replay().await.unwrap().len(), 2);
    repository
        .transaction(|tx| {
            for table in [
                "message",
                "conversation_item",
                "conversation_part",
                "agent_history",
            ] {
                let count: i64 =
                    tx.query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |row| {
                        row.get(0)
                    })?;
                assert_eq!(count, 0, "{table}");
            }
            Ok(())
        })
        .await
        .unwrap();
    repository.database().close().await.unwrap();
}
