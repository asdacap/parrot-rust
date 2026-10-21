//! Agent history is authoritative SQLite projection history plus anchored request facts.
use super::{ConversationRepository, EventError, reads::read_conversation_item, types::*};
use crate::database::Transaction;
use crate::events::projection::ensure_agent_history_projection;
use parrot_domain::protocol::messages::parrot::Event;
use protobuf::Message;
use rusqlite::params;

fn message_entry(sequence: i64, item: ConversationItem) -> AgentHistoryEntry {
    let message = item.message;
    AgentHistoryEntry::Message {
        sequence,
        conversation_sequence: item.sequence,
        origin: message.origin.as_str().into(),
        role: message.role.as_str().into(),
        parts: message.parts,
        tool_calls: message.tool_calls,
        tool_call_id: message.tool_call_id,
    }
}

pub fn include_request_history(
    tx: &Transaction<'_>,
    agent: &str,
    after_request_event_sequence: i64,
    mut entries: Vec<AgentHistoryEntry>,
) -> Result<Vec<AgentHistoryEntry>, EventError> {
    let mut read = tx.prepare("SELECT event.sequence, event.payload, COALESCE(anchor.history_sequence, 0) AS history_sequence FROM event LEFT JOIN request_history_anchor AS anchor ON anchor.event_sequence = event.sequence WHERE event.agent_session = ?1 AND event.request_id IS NOT NULL AND event.sequence > ?2 ORDER BY event.sequence;")?;
    let mut rows = read.query(params![agent, after_request_event_sequence])?;
    while let Some(row) = rows.next()? {
        let event = Event::parse_from_bytes(&row.get::<_, Vec<u8>>(1)?)?;
        if !event.has_request_usage_recorded() {
            return Err(EventError::Invalid(
                "request history event does not contain RequestUsageRecorded".into(),
            ));
        }
        let request = event.request_usage_recorded();
        entries.push(AgentHistoryEntry::Request {
            sequence: row.get(2)?,
            event_sequence: row.get(0)?,
            request_id: request.request_id.clone(),
            provider: request.provider.clone(),
            model: request.model.clone(),
            effort: request.effort.clone(),
            input_tokens: request.input_tokens,
            cached_input_tokens: request.cached_input_tokens,
            output_tokens: request.output_tokens,
            input_cost: request.input_cost,
            output_cost: request.output_cost,
            tool_calls: i64::try_from(request.tool_call_ids.len())
                .map_err(|_| EventError::Overflow)?,
        });
    }
    // Stable ordering is intentional: history entries precede requests at the
    // same anchor, and tied requests retain their original event sequence order.
    entries.sort_by_key(AgentHistoryEntry::sequence);
    Ok(entries)
}

impl ConversationRepository {
    pub async fn agent_history_session_ids(&self) -> Result<Vec<String>, EventError> {
        self.events.transaction(|tx| {
            ensure_agent_history_projection(tx)?;
            let mut read = tx.prepare("SELECT agent_session FROM agent_history UNION SELECT agent_session FROM request_history_anchor ORDER BY agent_session;")?;
            Ok(read.query_map([], |row| row.get(0))?.collect::<Result<Vec<_>, _>>()?)
        }).await
    }
    pub async fn agent_history(&self, agent: String) -> Result<Vec<AgentHistoryEntry>, EventError> {
        self.agent_history_after(agent, 0, 0).await
    }
    pub async fn agent_history_after(
        &self,
        agent: String,
        history_sequence: i64,
        request_event_sequence: i64,
    ) -> Result<Vec<AgentHistoryEntry>, EventError> {
        self.events.transaction(move |tx| {
            ensure_agent_history_projection(tx)?;
            let mut read = tx.prepare("SELECT sequence, kind, conversation_sequence, summary, watermark FROM agent_history WHERE agent_session = ?1 AND sequence > ?2 ORDER BY sequence;")?;
            let mut rows = read.query(params![agent, history_sequence])?;
            let mut entries = Vec::new();
            while let Some(row) = rows.next()? {
                let sequence = row.get(0)?;
                let kind: String = row.get(1)?;
                entries.push(if kind == "message" {
                    let conversation_sequence = row.get::<_, Option<i64>>(2)?.unwrap_or(0);
                    message_entry(sequence, read_conversation_item(tx, conversation_sequence)?)
                } else {
                    AgentHistoryEntry::Compaction { sequence, summary: row.get(3)?, watermark: row.get(4)? }
                });
            }
            include_request_history(tx, &agent, request_event_sequence, entries)
        }).await
    }
}
