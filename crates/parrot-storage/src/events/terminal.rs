//! A003 existing-schema terminal arbitration, not generic event deduplication.
use super::*;
use parrot_domain::contracts::async_ops::OperationError;
use parrot_domain::contracts::events::EventPublisher;
use rusqlite::{OptionalExtension, params};

/// Source Drain keeps the persisted TurnStarted event ID as its turnId.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TurnKey {
    pub agent_session_id: String,
    pub start_event_id: String,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TurnClosure {
    NewlyClosed,
    AlreadyClosed,
}
#[derive(Clone, Debug, PartialEq)]
pub struct TurnCommit {
    pub key: TurnKey,
    pub closure: TurnClosure,
    pub terminal: StoredEvent,
    pub conversation: Option<ConversationItem>,
}

/// The committed event stream is the durable closed-turn state. Existing
/// terminals win unchanged; a losing proposal writes no event or projection.
pub fn close_turn_if_open(
    tx: &Transaction<'_>,
    context: &RepositoryContext,
    key: &TurnKey,
    proposed: &Event,
    raw: Option<&[u8]>,
    message: Option<&ConversationMessage>,
) -> Result<TurnCommit, EventError> {
    if proposed.agent_session_id != key.agent_session_id || !proposed.has_turn_ended() {
        return Err(EventError::Invalid(
            "terminal proposal must be TurnEnded for the keyed agent".into(),
        ));
    }
    let start: Option<(i64, String, Vec<u8>)> = tx
        .query_row(
            "SELECT sequence, agent_session, payload FROM event WHERE id = ?1",
            [&key.start_event_id],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .optional()?;
    let (sequence, agent, bytes) =
        start.ok_or_else(|| EventError::Invalid("turn start does not exist".into()))?;
    let start = Event::parse_from_bytes(&bytes)?;
    if agent != key.agent_session_id
        || start.agent_session_id != key.agent_session_id
        || !start.has_turn_started()
    {
        return Err(EventError::Invalid(
            "turn key does not name that agent's TurnStarted".into(),
        ));
    }
    let mut read=tx.prepare("SELECT sequence,payload FROM event WHERE agent_session = ?1 AND sequence > ?2 ORDER BY sequence")?;
    let rows = read.query_map(params![key.agent_session_id, sequence], |row| {
        Ok((row.get::<_, i64>(0)?, row.get::<_, Vec<u8>>(1)?))
    })?;
    for row in rows {
        let (sequence, raw) = row?;
        let event = Event::parse_from_bytes(&raw)?;
        if event.has_turn_ended() {
            return Ok(TurnCommit {
                key: key.clone(),
                closure: TurnClosure::AlreadyClosed,
                terminal: StoredEvent {
                    sequence,
                    event,
                    raw,
                },
                conversation: None,
            });
        }
        if event.has_turn_started() {
            return Err(EventError::Invalid(
                "turn key is stale: a later turn has started".into(),
            ));
        }
    }
    drop(read);
    let outcome = repository::append(tx, context, proposed, raw, message)?;
    Ok(TurnCommit {
        key: key.clone(),
        closure: TurnClosure::NewlyClosed,
        terminal: outcome.stored,
        conversation: outcome.conversation,
    })
}

#[derive(Debug)]
pub enum CloseTurnError<E> {
    AdmissionCancelled,
    Repository(EventError),
    Publication {
        committed: Box<TurnCommit>,
        error: OperationError<E>,
    },
}
impl EventRepository {
    pub fn admit_close_turn(
        &self,
        key: TurnKey,
        proposed: Event,
        raw: Option<Vec<u8>>,
        message: Option<ConversationMessage>,
        cancellation: &dyn Cancellation,
    ) -> Result<CommitTicket<TurnKey, TurnCommit, EventError>, AdmissionError<EventError>> {
        let context = self.context().clone();
        let command_key = key.clone();
        self.admit(key, cancellation, move |tx| {
            close_turn_if_open(
                tx,
                &context,
                &command_key,
                &proposed,
                raw.as_deref(),
                message.as_ref(),
            )
        })
    }
    /// Reconcile only the receipt's key, before cancellable publication. A
    /// dropped observer need not run reconciliation: durable retry is authoritative.
    /// AlreadyClosed must not publish an old terminal or apply losing message text.
    /// T46 must route BOTH normal and interrupted writers through this boundary.
    pub async fn close_turn_and_publish<P, R>(
        &self,
        key: TurnKey,
        proposed: Event,
        message: Option<ConversationMessage>,
        publisher: &P,
        cancellation: &dyn Cancellation,
        reconcile: R,
    ) -> Result<TurnCommit, CloseTurnError<P::Error>>
    where
        P: EventPublisher<Event = Event>,
        R: FnOnce(&TurnCommit),
    {
        let ticket = self
            .admit_close_turn(key, proposed, None, message, cancellation)
            .map_err(|error| match error {
                AdmissionError::Cancelled => CloseTurnError::AdmissionCancelled,
                AdmissionError::Rejected(error) => CloseTurnError::Repository(error),
            })?;
        let committed = ticket
            .into_completion()
            .await
            .map_err(CloseTurnError::Repository)?
            .into_value()
            .into_output();
        reconcile(&committed);
        if committed.closure == TurnClosure::NewlyClosed {
            let event = Committed::after_commit(committed.terminal.event.clone());
            if let Err(error) = publisher.publish_committed(&event, cancellation).await {
                return Err(CloseTurnError::Publication {
                    committed: Box::new(committed),
                    error,
                });
            }
        }
        Ok(committed)
    }
}
