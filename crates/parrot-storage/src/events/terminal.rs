//! A003 existing-schema terminal arbitration, not generic event deduplication.
use super::*;
use parrot_domain::contracts::async_ops::OperationError;
use parrot_domain::contracts::events::EventPublisher;
use rusqlite::{OptionalExtension, params};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TurnClosure {
    NewlyClosed,
    AlreadyClosed,
}
#[derive(Clone, Debug, PartialEq)]
pub struct TurnCommit {
    pub agent_session_id: String,
    pub closure: TurnClosure,
    pub terminal: StoredEvent,
    pub conversation: Option<ConversationItem>,
}

/// The committed event stream is the durable closed-turn state. Existing
/// terminals win unchanged; a losing proposal writes no event or projection.
/// The open turn is the agent session's latest `turn_started` event, and the
/// proposal closes it when no terminal follows it.
pub fn close_turn_if_open(
    tx: &Transaction<'_>,
    agent_session_id: &str,
    proposed: &Event,
    message: Option<&ConversationMessage>,
) -> Result<TurnCommit, EventError> {
    if proposed.agent_session_id != agent_session_id || !proposed.has_turn_ended() {
        return Err(EventError::Invalid(
            "terminal proposal must be TurnEnded for the closing agent".into(),
        ));
    }
    let start: Option<(i64, Vec<u8>)> = tx
        .query_row(
            "SELECT sequence, payload FROM event
             WHERE agent_session = ?1 AND kind = 'turn_started'
             ORDER BY sequence DESC LIMIT 1",
            [agent_session_id],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()?;
    let (start_sequence, start_bytes) =
        start.ok_or_else(|| EventError::Invalid("no open turn for the agent".into()))?;
    let started = Event::parse_from_bytes(&start_bytes)?;
    if started.agent_session_id != agent_session_id {
        return Err(EventError::Invalid(
            "turn start does not name the agent".into(),
        ));
    }
    let terminal: Option<(i64, Vec<u8>)> = tx
        .query_row(
            "SELECT sequence, payload FROM event
             WHERE agent_session = ?1 AND sequence > ?2 AND kind = 'turn_ended'
             ORDER BY sequence LIMIT 1",
            params![agent_session_id, start_sequence],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()?;
    if let Some((sequence, raw)) = terminal {
        let event = Event::parse_from_bytes(&raw)?;
        return Ok(TurnCommit {
            agent_session_id: agent_session_id.to_owned(),
            closure: TurnClosure::AlreadyClosed,
            terminal: StoredEvent {
                sequence,
                event,
                raw,
            },
            conversation: None,
        });
    }
    let outcome = repository::append(tx, proposed, message)?;
    Ok(TurnCommit {
        agent_session_id: agent_session_id.to_owned(),
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
        agent_session_id: String,
        proposed: Event,
        message: Option<ConversationMessage>,
        cancellation: &dyn Cancellation,
    ) -> Result<CommitTicket<String, TurnCommit, EventError>, AdmissionError<EventError>> {
        let command_key = agent_session_id.clone();
        self.admit(agent_session_id, cancellation, move |tx| {
            close_turn_if_open(tx, &command_key, &proposed, message.as_ref())
        })
    }
    /// Reconcile only the receipt's key, before cancellable publication. A
    /// dropped observer need not run reconciliation: durable retry is authoritative.
    /// AlreadyClosed must not publish an old terminal or apply losing message text.
    /// T46 must route BOTH normal and interrupted writers through this boundary.
    pub async fn close_turn_and_publish<P, R>(
        &self,
        agent_session_id: String,
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
            .admit_close_turn(agent_session_id, proposed, message, cancellation)
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
