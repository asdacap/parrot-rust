//! Source-compatible durable input identity, pending selection and promotion.
use super::{EventError, RepositoryContext, projection, record, types::*};
use crate::database::Transaction;
use parrot_domain::protocol::messages::parrot::{Delivery, Event};
use rusqlite::{OptionalExtension, params};

pub fn delivery_text(delivery: Delivery) -> Result<&'static str, EventError> {
    match delivery {
        Delivery::DELIVERY_STEER => Ok("steer"),
        Delivery::DELIVERY_QUEUE => Ok("queue"),
        _ => Err(EventError::Argument("delivery is out of range".into())),
    }
}
fn parse_delivery(value: &str) -> Result<Delivery, EventError> {
    match value {
        "steer" => Ok(Delivery::DELIVERY_STEER),
        "queue" => Ok(Delivery::DELIVERY_QUEUE),
        _ => Err(EventError::Invalid(format!(
            "the input table holds an unknown delivery {value}"
        ))),
    }
}
fn input_parts(
    tx: &Transaction<'_>,
    id: &str,
    legacy: &str,
) -> Result<Vec<ConversationPart>, EventError> {
    projection::read_input_parts(tx, id, legacy)
}
pub fn existing(
    tx: &Transaction<'_>,
    agent: &str,
    message: &str,
) -> Result<Option<AdmittedInput>, EventError> {
    let row = tx
        .query_row(
            "SELECT id, content, delivery FROM input WHERE agent_session = ?1 AND message_id = ?2",
            params![agent, message],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                ))
            },
        )
        .optional()?;
    row.map(|(id, content, delivery)| {
        Ok(AdmittedInput {
            id: id.clone(),
            message_id: message.into(),
            parts: input_parts(tx, &id, &content)?,
            delivery: parse_delivery(&delivery)?,
        })
    })
    .transpose()
}
pub fn pending(
    tx: &Transaction<'_>,
    agent: &str,
    delivery: Delivery,
    limit: i64,
) -> Result<Vec<AdmittedInput>, EventError> {
    let mut read=tx.prepare("SELECT id, message_id, content FROM input WHERE agent_session = ?1 AND delivery = ?2 AND status = 'pending' ORDER BY sequence LIMIT ?3")?;
    let rows = read
        .query_map(params![agent, delivery_text(delivery)?, limit], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
            ))
        })?
        .collect::<Result<Vec<_>, _>>()?;
    rows.into_iter()
        .map(|(id, message_id, legacy)| {
            Ok(AdmittedInput {
                parts: input_parts(tx, &id, &legacy)?,
                id,
                message_id,
                delivery,
            })
        })
        .collect()
}
pub fn has_pending(tx: &Transaction<'_>, agent: &str) -> Result<bool, EventError> {
    Ok(tx.query_row(
        "SELECT EXISTS (SELECT 1 FROM input WHERE agent_session = ?1 AND status = 'pending')",
        [agent],
        |row| row.get(0),
    )?)
}
pub fn inputs_for_next_promotion(
    tx: &Transaction<'_>,
    agent: &str,
) -> Result<Vec<AdmittedInput>, EventError> {
    let steers = pending(tx, agent, Delivery::DELIVERY_STEER, -1)?;
    if steers.is_empty() {
        pending(tx, agent, Delivery::DELIVERY_QUEUE, 1)
    } else {
        Ok(steers)
    }
}
pub fn admit_parts<F>(
    tx: &Transaction<'_>,
    context: &RepositoryContext,
    agent: &str,
    message: &str,
    parts: Vec<ConversationPart>,
    delivery: Delivery,
    compose: F,
) -> Result<Admission, EventError>
where
    F: FnOnce(&AdmittedInput) -> Result<Event, EventError>,
{
    if let Some(already) = existing(tx, agent, message)? {
        return if already.parts == parts && already.delivery == delivery {
            Ok(Admission {
                input: already,
                published: None,
            })
        } else {
            Err(EventError::Conflict(format!(
                "message {message} was already admitted with different content"
            )))
        };
    }
    let admitted = AdmittedInput {
        id: (context.input_id)()?,
        message_id: message.into(),
        parts,
        delivery,
    };
    let event = compose(&admitted)?;
    tx.execute("INSERT INTO input (id, agent_session, message_id, content, delivery, status, created_at) VALUES (?1,?2,?3,?4,?5,'pending',?6)",params![admitted.id,agent,message,admitted.content(),delivery_text(delivery)?,(context.timestamp)()])?;
    projection::insert_input_parts(tx, &admitted.id, &admitted.parts)?;
    record(tx, &event, None, &(context.timestamp)())?;
    Ok(Admission {
        input: admitted,
        published: Some(event),
    })
}
pub fn admit_steer_if_idle<F>(
    tx: &Transaction<'_>,
    context: &RepositoryContext,
    agent: &str,
    message: &str,
    content: String,
    compose: F,
) -> Result<Option<Admission>, EventError>
where
    F: FnOnce(&AdmittedInput) -> Result<Event, EventError>,
{
    if let Some(already) = existing(tx, agent, message)? {
        return if already.content() == content && already.delivery == Delivery::DELIVERY_STEER {
            Ok(Some(Admission {
                input: already,
                published: None,
            }))
        } else {
            Err(EventError::Conflict(format!(
                "message {message} was already admitted with different content"
            )))
        };
    }
    if has_pending(tx, agent)? {
        return Ok(None);
    }
    let admitted = AdmittedInput {
        id: (context.input_id)()?,
        message_id: message.into(),
        parts: vec![ConversationPart::text(content)],
        delivery: Delivery::DELIVERY_STEER,
    };
    let event = compose(&admitted)?;
    tx.execute("INSERT INTO input (id, agent_session, message_id, content, delivery, status, created_at) VALUES (?1,?2,?3,?4,'steer','pending',?5)",params![admitted.id,agent,message,admitted.content(),(context.timestamp)()])?;
    // Source idle-steer path intentionally has no input_part insertion.
    record(tx, &event, None, &(context.timestamp)())?;
    Ok(Some(Admission {
        input: admitted,
        published: Some(event),
    }))
}
pub fn cancel_pending<F>(
    tx: &Transaction<'_>,
    context: &RepositoryContext,
    agent: &str,
    input: &str,
    compose: F,
) -> Result<Option<Event>, EventError>
where
    F: FnOnce() -> Result<Event, EventError>,
{
    if tx.execute("UPDATE input SET status = 'canceled' WHERE agent_session = ?1 AND id = ?2 AND status = 'pending'",params![agent,input])?==0{return Ok(None);}
    let event = compose()?;
    record(tx, &event, None, &(context.timestamp)())?;
    Ok(Some(event))
}
pub fn promote<F>(
    tx: &Transaction<'_>,
    context: &RepositoryContext,
    agent: &str,
    delivery: Delivery,
    limit: i64,
    mut compose: F,
) -> Result<Vec<Promotion>, EventError>
where
    F: FnMut(&AdmittedInput) -> Result<Event, EventError>,
{
    let waiting = pending(tx, agent, delivery, limit)?;
    if waiting.is_empty() {
        return Ok(Vec::new());
    }
    let at = (context.timestamp)();
    let mut promoted = Vec::with_capacity(waiting.len());
    for input in waiting {
        let event = compose(&input)?;
        if tx.execute("UPDATE input SET status = 'promoted', promoted_at = ?1 WHERE id = ?2 AND status = 'pending'",params![at,input.id])?!=1{return Err(EventError::Invalid(format!("input {} changed during promotion",input.id)));}
        let message = ConversationMessage {
            origin: ConversationOrigin::UserInput,
            role: MessageRole::User,
            parts: input.parts.clone(),
            tool_calls: Vec::new(),
            tool_call_id: String::new(),
            provider_content: ProviderContent::default(),
        };
        projection::project(tx, agent, &message, &(context.timestamp)())?;
        record(tx, &event, None, &(context.timestamp)())?;
        promoted.push(Promotion {
            input,
            published: event,
        });
    }
    Ok(promoted)
}
