//! Original InputAdmissionTests assertions against the real repository/actor.
use parrot_domain::protocol::messages::parrot::{
    Delivery, Event, InputAdmitted, InputCanceled, InputSource,
};
use parrot_domain::values::identifiers::Identifier;
use parrot_storage::database::{DatabaseError, SessionDatabase};
use parrot_storage::events::{
    Admission, AdmittedInput, ConversationOrigin, ConversationPart, EventError, EventRepository,
    NeverCancelled, projection,
};
use protobuf::Message;

const SESSION: &str = "agent";

fn repository(fixture: &super::Fixture) -> EventRepository {
    EventRepository::new(SessionDatabase::open_memory(fixture.root()).unwrap())
}
fn event(agent: &str) -> Event {
    Event {
        id: Identifier::event_id().unwrap(),
        agent_session_id: agent.into(),
        ..Event::new()
    }
}
async fn admit_parts<F>(
    repo: &EventRepository,
    agent: &str,
    message: &str,
    parts: Vec<ConversationPart>,
    delivery: Delivery,
    compose: F,
) -> Result<Admission, EventError>
where
    F: FnOnce(&AdmittedInput) -> Result<Event, EventError> + Send + 'static,
{
    repo.admit_input(
        agent.into(),
        message.into(),
        parts,
        delivery,
        compose,
        &NeverCancelled,
    )
    .unwrap()
    .into_completion()
    .await
    .map(|result| result.into_value().into_output())
}
async fn admit(
    repo: &EventRepository,
    agent: &str,
    message: &str,
    content: &str,
    delivery: Delivery,
) -> Admission {
    let event_agent = agent.to_owned();
    admit_parts(
        repo,
        agent,
        message,
        vec![ConversationPart::text(content)],
        delivery,
        move |_| Ok(event(&event_agent)),
    )
    .await
    .unwrap()
}
async fn idle(repo: &EventRepository, content: &str) -> Result<Option<Admission>, EventError> {
    repo.admit_steer_if_idle(
        SESSION.into(),
        "delivery".into(),
        content.into(),
        |_| Ok(event(SESSION)),
        &NeverCancelled,
    )
    .unwrap()
    .into_completion()
    .await
    .map(|result| result.into_value().into_output())
}
fn other(delivery: Delivery) -> Delivery {
    if delivery == Delivery::DELIVERY_STEER {
        Delivery::DELIVERY_QUEUE
    } else {
        Delivery::DELIVERY_STEER
    }
}
async fn promote(repo: &EventRepository, delivery: Delivery) -> usize {
    if delivery == Delivery::DELIVERY_STEER {
        repo.promote_steers(SESSION.into(), |_| Ok(event(SESSION)))
            .await
            .unwrap()
            .len()
    } else {
        repo.promote_next_queue(SESSION.into(), |_| Ok(event(SESSION)))
            .await
            .unwrap()
            .len()
    }
}
fn single<T>(mut values: Vec<T>) -> T {
    assert_eq!(values.len(), 1, "source Single() cardinality");
    values.remove(0)
}

// These adapters observe actual persisted projection rows. They do not replace
// the later T18 production Conversation/ModelHistory APIs or materialization.
async fn messages(repo: &EventRepository) -> Vec<String> {
    repo.transaction(|tx| {
        projection::ensure_conversation_projection(tx)?;
        let mut read = tx.prepare("SELECT sequence, role FROM conversation_item WHERE agent_session = ?1 ORDER BY sequence")?;
        let rows = read.query_map([SESSION], |row| Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?)))?.collect::<Result<Vec<_>, _>>()?;
        rows.into_iter().map(|(sequence, role)| {
            let parts = projection::read_parts(tx, "conversation_part", "item_sequence", &sequence)?;
            if parts.iter().any(|part| matches!(part, ConversationPart::ImageArtifact { .. })) {
                return Err(EventError::Invalid("artifact references must be materialized before model history is read".into()));
            }
            Ok(format!("{role}: {}", parrot_storage::events::content(&parts)))
        }).collect()
    }).await.unwrap()
}
struct ObservedConversation {
    origin: ConversationOrigin,
    parts: Vec<ConversationPart>,
}
async fn conversation(repo: &EventRepository) -> Vec<ObservedConversation> {
    repo.transaction(|tx| {
        projection::ensure_conversation_projection(tx)?;
        let mut read = tx.prepare("SELECT sequence, origin FROM conversation_item WHERE agent_session = ?1 ORDER BY sequence")?;
        let rows = read.query_map([SESSION], |row| Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?)))?.collect::<Result<Vec<_>, _>>()?;
        rows.into_iter().map(|(sequence, origin)| {
            let origin = match origin.as_str() {
                "user_input" => ConversationOrigin::UserInput, "model" => ConversationOrigin::Model,
                "system" => ConversationOrigin::System, "tool" => ConversationOrigin::Tool,
                _ => return Err(EventError::Invalid(format!("unknown conversation origin {origin}"))),
            };
            Ok(ObservedConversation { origin, parts: projection::read_parts(tx, "conversation_part", "item_sequence", &sequence)? })
        }).collect()
    }).await.unwrap()
}

async fn boundary(delivery: Delivery) {
    let fixture = super::Fixture::new();
    let repo = repository(&fixture);
    let admitted = admit(&repo, SESSION, "msg-1", "hello", delivery).await;
    // source-assertion-line: 27
    assert!(admitted.created());
    // source-assertion-line: 28
    assert!(repo.has_pending_inputs(SESSION.into()).await.unwrap());
    // source-assertion-line: 31
    assert!(messages(&repo).await.is_empty());
    // source-assertion-line: 34
    assert_eq!(promote(&repo, other(delivery)).await, 0);
    // source-assertion-line: 35
    assert!(repo.has_pending_inputs(SESSION.into()).await.unwrap());
    // source-assertion-line: 37
    assert_eq!(promote(&repo, delivery).await, 1);
    // source-assertion-line: 38
    assert!(!repo.has_pending_inputs(SESSION.into()).await.unwrap());
    // source-assertion-line: 39
    assert!(messages(&repo).await.contains(&"user: hello".to_owned()));
    // source-assertion-line: 42
    assert_eq!(promote(&repo, delivery).await, 0);
    repo.database().close().await.unwrap();
}
#[tokio::test]
async fn boundary_steer() {
    boundary(Delivery::DELIVERY_STEER).await;
}
#[tokio::test]
async fn boundary_queue() {
    boundary(Delivery::DELIVERY_QUEUE).await;
}

#[tokio::test]
async fn message_identity_and_resend() {
    let fixture = super::Fixture::new();
    let repo = repository(&fixture);
    let first = admit(&repo, SESSION, "msg-1", "hello", Delivery::DELIVERY_STEER).await;
    let again = admit(&repo, SESSION, "msg-1", "hello", Delivery::DELIVERY_STEER).await;
    // source-assertion-line: 51
    assert!(!again.created());
    // source-assertion-line: 52
    assert_eq!(again.input.id, first.input.id);
    // source-assertion-line: 53
    assert_eq!(promote(&repo, Delivery::DELIVERY_STEER).await, 1);
    // source-assertion-line: 55
    assert!(
        matches!(admit_parts(&repo, SESSION, "msg-1", vec![ConversationPart::text("different")], Delivery::DELIVERY_STEER, |_| Ok(event(SESSION))).await, Err(EventError::Conflict(message)) if message == "message msg-1 was already admitted with different content")
    );
    repo.database().close().await.unwrap();
}

#[tokio::test]
async fn structured_parts_and_idempotent_retry() {
    let fixture = super::Fixture::new();
    let repo = repository(&fixture);
    let parts = vec![
        ConversationPart::text("before"),
        ConversationPart::image("artifact-1", "image/png", "pixel.png"),
        ConversationPart::text("after"),
    ];
    let admitted = admit_parts(
        &repo,
        SESSION,
        "msg-structured",
        parts.clone(),
        Delivery::DELIVERY_STEER,
        |_| Ok(event(SESSION)),
    )
    .await
    .unwrap();
    let retried = admit_parts(
        &repo,
        SESSION,
        "msg-structured",
        parts,
        Delivery::DELIVERY_STEER,
        |_| Ok(event(SESSION)),
    )
    .await
    .unwrap();
    // source-assertion-line: 83
    assert!(admitted.created());
    // source-assertion-line: 84
    assert!(!retried.created());
    // source-assertion-line: 85
    assert_eq!(retried.input.id, admitted.input.id);
    // source-assertion-line: 86
    assert_eq!(promote(&repo, Delivery::DELIVERY_STEER).await, 1);
    let conversation = single(conversation(&repo).await);
    // source-assertion-line: 89
    assert_eq!(conversation.origin, ConversationOrigin::UserInput);
    // source-assertion-line: 90
    assert_eq!(
        conversation
            .parts
            .iter()
            .map(|part| match part {
                ConversationPart::Text(_) => "Text",
                ConversationPart::ImageArtifact { .. } => "ImageArtifact",
            })
            .collect::<Vec<_>>()
            .join(","),
        "Text,ImageArtifact,Text"
    );
    // source-assertion-line: 92
    assert!(matches!(&conversation.parts[0], ConversationPart::Text(text) if text == "before"));
    let ConversationPart::ImageArtifact {
        artifact_id,
        media_type,
        display_name,
    } = &conversation.parts[1]
    else {
        panic!("source image part kind");
    };
    // source-assertion-line: 93
    assert_eq!(artifact_id, "artifact-1");
    // source-assertion-line: 94
    assert_eq!(media_type, "image/png");
    // source-assertion-line: 95
    assert_eq!(display_name, "pixel.png");
    // source-assertion-line: 96
    assert!(matches!(&conversation.parts[2], ConversationPart::Text(text) if text == "after"));
    repo.database().close().await.unwrap();
}

#[tokio::test]
async fn conditional_steer_yields_and_retries() {
    let fixture = super::Fixture::new();
    let repo = repository(&fixture);
    admit(
        &repo,
        SESSION,
        "normal",
        "normal prompt",
        Delivery::DELIVERY_QUEUE,
    )
    .await;
    let blocked = idle(&repo, "notification").await.unwrap();
    // source-assertion-line: 115
    assert!(blocked.is_none());
    promote(&repo, Delivery::DELIVERY_QUEUE).await;
    let admitted = idle(&repo, "notification")
        .await
        .unwrap()
        .expect("conditional admission unexpectedly failed");
    let retried = idle(&repo, "notification")
        .await
        .unwrap()
        .expect("conditional admission unexpectedly failed");
    // source-assertion-line: 134
    assert!(admitted.created());
    // source-assertion-line: 135
    assert!(!retried.created());
    // source-assertion-line: 136
    assert_eq!(retried.input.id, admitted.input.id);
    repo.database().close().await.unwrap();
}
#[tokio::test]
async fn conditional_steer_conflict() {
    let fixture = super::Fixture::new();
    let repo = repository(&fixture);
    idle(&repo, "notification").await.unwrap();
    // source-assertion-line: 148
    assert!(
        matches!(idle(&repo, "different").await, Err(EventError::Conflict(message)) if message == "message delivery was already admitted with different content")
    );
    repo.database().close().await.unwrap();
}
#[tokio::test]
async fn steers_together_queues_one_at_a_time() {
    let fixture = super::Fixture::new();
    let repo = repository(&fixture);
    admit(
        &repo,
        SESSION,
        "msg-1",
        "first steer",
        Delivery::DELIVERY_STEER,
    )
    .await;
    admit(
        &repo,
        SESSION,
        "msg-2",
        "second steer",
        Delivery::DELIVERY_STEER,
    )
    .await;
    admit(
        &repo,
        SESSION,
        "msg-3",
        "first queued",
        Delivery::DELIVERY_QUEUE,
    )
    .await;
    admit(
        &repo,
        SESSION,
        "msg-4",
        "second queued",
        Delivery::DELIVERY_QUEUE,
    )
    .await;
    let steers = repo
        .promote_steers(SESSION.into(), |_| Ok(event(SESSION)))
        .await
        .unwrap();
    // source-assertion-line: 166
    assert_eq!(
        steers
            .iter()
            .map(|promotion| promotion.input.content())
            .collect::<Vec<_>>()
            .join(" | "),
        "first steer | second steer"
    );
    // source-assertion-line: 169
    assert_eq!(
        single(
            repo.promote_next_queue(SESSION.into(), |_| Ok(event(SESSION)))
                .await
                .unwrap()
        )
        .input
        .content(),
        "first queued"
    );
    // source-assertion-line: 170
    assert_eq!(
        single(
            repo.promote_next_queue(SESSION.into(), |_| Ok(event(SESSION)))
                .await
                .unwrap()
        )
        .input
        .content(),
        "second queued"
    );
    // source-assertion-line: 171
    assert!(
        repo.promote_next_queue(SESSION.into(), |_| Ok(event(SESSION)))
            .await
            .unwrap()
            .is_empty()
    );
    repo.database().close().await.unwrap();
}

async fn owned_cancel(delivery: Delivery) {
    let fixture = super::Fixture::new();
    let repo = repository(&fixture);
    let admitted = admit(&repo, SESSION, "cancel", "canceled prompt", delivery).await;
    let unrelated = admit(&repo, SESSION, "unrelated", "unrelated prompt", delivery).await;
    let other_session = admit(&repo, "other-agent", "other", "other prompt", delivery).await;
    let mut canceled_event = event(SESSION);
    canceled_event.set_input_canceled(InputCanceled {
        input_id: admitted.input.id.clone(),
        ..Default::default()
    });
    // source-assertion-line: 189
    assert!(
        repo.cancel_pending_input("other-agent".into(), admitted.input.id.clone(), || Err(
            EventError::Invalid("wrong session".into())
        ))
        .await
        .unwrap()
        .is_none()
    );
    // source-assertion-line: 190
    assert!(
        repo.cancel_pending_input(SESSION.into(), other_session.input.id.clone(), || Err(
            EventError::Invalid("wrong owner".into())
        ))
        .await
        .unwrap()
        .is_none()
    );
    // source-assertion-line: 191
    assert!(
        repo.cancel_pending_input(SESSION.into(), "missing".into(), || Err(
            EventError::Invalid("missing input".into())
        ))
        .await
        .unwrap()
        .is_none()
    );
    let proposed = canceled_event.clone();
    // source-assertion-line: 192
    assert_eq!(
        repo.cancel_pending_input(SESSION.into(), admitted.input.id.clone(), move || Ok(
            proposed
        ))
        .await
        .unwrap(),
        Some(canceled_event.clone())
    );
    // source-assertion-line: 193
    assert!(
        repo.cancel_pending_input(SESSION.into(), admitted.input.id.clone(), || Err(
            EventError::Invalid("already canceled".into())
        ))
        .await
        .unwrap()
        .is_none()
    );
    let recorded = single(
        repo.replay()
            .await
            .unwrap()
            .into_iter()
            .filter(Event::has_input_canceled)
            .collect(),
    );
    // source-assertion-line: 196
    assert_eq!(recorded, canceled_event);
    // source-assertion-line: 197
    assert_eq!(
        single(
            repo.inputs_for_next_promotion(SESSION.into())
                .await
                .unwrap()
        )
        .id,
        unrelated.input.id
    );
    // source-assertion-line: 198
    assert_eq!(
        single(
            repo.inputs_for_next_promotion("other-agent".into())
                .await
                .unwrap()
        )
        .id,
        other_session.input.id
    );
    // source-assertion-line: 199
    assert_eq!(promote(&repo, delivery).await, 1);
    // source-assertion-line: 200
    assert_eq!(single(messages(&repo).await), "user: unrelated prompt");
    // source-assertion-line: 201
    assert!(!repo.has_pending_inputs(SESSION.into()).await.unwrap());
    // source-assertion-line: 202
    assert!(
        repo.cancel_pending_input(SESSION.into(), unrelated.input.id, || Err(
            EventError::Invalid("already promoted".into())
        ))
        .await
        .unwrap()
        .is_none()
    );
    // source-assertion-line: 203
    assert_eq!(
        repo.replay()
            .await
            .unwrap()
            .iter()
            .filter(|event| event.has_input_canceled())
            .count(),
        1
    );
    repo.database().close().await.unwrap();
}
#[tokio::test]
async fn owned_cancel_steer() {
    owned_cancel(Delivery::DELIVERY_STEER).await;
}
#[tokio::test]
async fn owned_cancel_queue() {
    owned_cancel(Delivery::DELIVERY_QUEUE).await;
}

fn sqlite_failure(error: EventError, extended_code: i32, message: Option<&str>) {
    let EventError::Database(DatabaseError::Sqlite(rusqlite::Error::SqliteFailure(
        code,
        actual_message,
    ))) = error
    else {
        panic!("expected actual SQLite failure, got {error:?}");
    };
    assert_eq!(code.code, rusqlite::ErrorCode::ConstraintViolation);
    assert_eq!(code.extended_code, extended_code);
    if let Some(expected) = message {
        assert_eq!(actual_message.as_deref(), Some(expected));
    }
}
async fn cancellation_failure(fail_composition: bool) {
    let fixture = super::Fixture::new();
    let repo = repository(&fixture);
    let admitted_event = event(SESSION);
    let initial = admitted_event.clone();
    let admitted = admit_parts(
        &repo,
        SESSION,
        "cancel",
        vec![ConversationPart::text("pending prompt")],
        Delivery::DELIVERY_QUEUE,
        move |_| Ok(initial),
    )
    .await
    .unwrap();
    let mut canceled_event = Event {
        id: admitted_event.id.clone(),
        agent_session_id: SESSION.into(),
        ..Event::new()
    };
    canceled_event.set_input_canceled(InputCanceled {
        input_id: admitted.input.id.clone(),
        ..Default::default()
    });
    if fail_composition {
        // source-assertion-line: 222
        assert!(
            matches!(repo.cancel_pending_input(SESSION.into(), admitted.input.id.clone(), || Err(EventError::Invalid("composition failed".into()))).await, Err(EventError::Invalid(message)) if message == "composition failed")
        );
    } else {
        let proposed = canceled_event.clone();
        // source-assertion-line: 227
        sqlite_failure(
            repo.cancel_pending_input(SESSION.into(), admitted.input.id.clone(), move || {
                Ok(proposed)
            })
            .await
            .unwrap_err(),
            2067,
            Some("UNIQUE constraint failed: event.id"),
        );
    }
    // source-assertion-line: 231
    assert_eq!(
        single(
            repo.inputs_for_next_promotion(SESSION.into())
                .await
                .unwrap()
        )
        .id,
        admitted.input.id
    );
    // source-assertion-line: 232
    assert_eq!(single(repo.replay().await.unwrap()), admitted_event);
    canceled_event.id = Identifier::event_id().unwrap();
    let proposed = canceled_event.clone();
    // source-assertion-line: 234
    assert_eq!(
        repo.cancel_pending_input(SESSION.into(), admitted.input.id, move || Ok(proposed))
            .await
            .unwrap(),
        Some(canceled_event.clone())
    );
    // source-assertion-line: 235
    assert!(!repo.has_pending_inputs(SESSION.into()).await.unwrap());
    // source-assertion-line: 236
    assert_eq!(repo.replay().await.unwrap().last(), Some(&canceled_event));
    // source-assertion-line: 237
    assert_eq!(promote(&repo, Delivery::DELIVERY_QUEUE).await, 0);
    // source-assertion-line: 238
    assert!(messages(&repo).await.is_empty());
    repo.database().close().await.unwrap();
}
#[tokio::test]
async fn cancel_composition_failure() {
    cancellation_failure(true).await;
}
#[tokio::test]
async fn cancel_duplicate_event_failure() {
    cancellation_failure(false).await;
}

async fn source_preserved(source: InputSource) {
    let fixture = super::Fixture::new();
    let repo = repository(&fixture);
    let admitted = admit_parts(
        &repo,
        SESSION,
        "msg-source",
        vec![ConversationPart::text("notification")],
        Delivery::DELIVERY_STEER,
        move |input| {
            let mut event = event(SESSION);
            event.set_input_admitted(InputAdmitted {
                input_id: input.id.clone(),
                message_id: input.message_id.clone(),
                content: input.content(),
                delivery: input.delivery.into(),
                source: source.into(),
                ..Default::default()
            });
            Ok(event)
        },
    )
    .await
    .unwrap();
    let retried = admit_parts(
        &repo,
        SESSION,
        "msg-source",
        vec![ConversationPart::text("notification")],
        Delivery::DELIVERY_STEER,
        |_| {
            Err(EventError::Invalid(
                "A retry must not compose another event.".into(),
            ))
        },
    )
    .await
    .unwrap();
    // source-assertion-line: 271
    assert!(!retried.created());
    // source-assertion-line: 272
    assert_eq!(retried.input.id, admitted.input.id);
    // source-assertion-line: 273
    assert_eq!(
        single(repo.replay().await.unwrap())
            .input_admitted()
            .source
            .enum_value()
            .unwrap(),
        source
    );
    // source-assertion-line: 274
    assert_eq!(promote(&repo, Delivery::DELIVERY_STEER).await, 1);
    let replayed = single(
        repo.replay()
            .await
            .unwrap()
            .into_iter()
            .filter(Event::has_input_admitted)
            .collect(),
    );
    // source-assertion-line: 276
    assert_eq!(
        replayed.input_admitted().source.enum_value().unwrap(),
        source
    );
    // source-assertion-line: 277
    assert_eq!(Some(replayed), admitted.published);
    repo.database().close().await.unwrap();
}
#[tokio::test]
async fn source_unspecified() {
    source_preserved(InputSource::INPUT_SOURCE_UNSPECIFIED).await;
}
#[tokio::test]
async fn source_monitor() {
    source_preserved(InputSource::INPUT_SOURCE_MONITOR).await;
}
#[tokio::test]
async fn legacy_source_default_decode() {
    let admitted = InputAdmitted::parse_from_bytes(&[0x20, 0x01]).unwrap();
    // source-assertion-line: 285
    assert_eq!(
        admitted.delivery.enum_value().unwrap(),
        Delivery::DELIVERY_STEER
    );
    // source-assertion-line: 286
    assert_eq!(
        admitted.source.enum_value().unwrap(),
        InputSource::INPUT_SOURCE_UNSPECIFIED
    );
}

#[tokio::test]
async fn supplemental_sql_trigger_failure_preserves_pending_input_and_event() {
    let fixture = super::Fixture::new();
    let repo = repository(&fixture);
    let admitted_event = event(SESSION);
    let initial = admitted_event.clone();
    let admitted = admit_parts(
        &repo,
        SESSION,
        "cancel",
        vec![ConversationPart::text("pending prompt")],
        Delivery::DELIVERY_QUEUE,
        move |_| Ok(initial),
    )
    .await
    .unwrap();
    repo.transaction(|tx| { tx.execute_batch("CREATE TRIGGER cancel_insert_failure BEFORE INSERT ON event BEGIN SELECT RAISE(ABORT, 'cancel trigger blocked'); END;")?; Ok(()) }).await.unwrap();
    let mut canceled = event(SESSION);
    canceled.set_input_canceled(InputCanceled {
        input_id: admitted.input.id.clone(),
        ..Default::default()
    });
    sqlite_failure(
        repo.cancel_pending_input(SESSION.into(), admitted.input.id.clone(), move || {
            Ok(canceled)
        })
        .await
        .unwrap_err(),
        1811,
        Some("cancel trigger blocked"),
    );
    assert_eq!(
        single(
            repo.inputs_for_next_promotion(SESSION.into())
                .await
                .unwrap()
        )
        .id,
        admitted.input.id
    );
    assert_eq!(single(repo.replay().await.unwrap()), admitted_event);
    assert!(messages(&repo).await.is_empty());
    repo.database().close().await.unwrap();
}
