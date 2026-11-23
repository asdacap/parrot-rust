//! Durable filter watermark and in-place replay amendment of one history batch.
use super::{ConversationRepository, EventError};
use crate::events::{record, timestamp};
use parrot_core::FilterAmendment;
use parrot_domain::protocol::messages::parrot::Event;
use rusqlite::{Transaction, params};

fn read_watermark(tx: &Transaction<'_>, agent: &str) -> Result<i64, EventError> {
    Ok(tx
        .query_row(
            "SELECT COALESCE((SELECT filter_watermark FROM conversation_item WHERE agent_session = ?1 AND kind = 'filter' ORDER BY sequence DESC LIMIT 1), 0);",
            [agent],
            |row| row.get(0),
        )?)
}

fn already_committed(
    tx: &Transaction<'_>,
    agent: &str,
    watermark: i64,
) -> Result<bool, EventError> {
    Ok(tx.query_row(
        "SELECT EXISTS (SELECT 1 FROM conversation_item WHERE agent_session = ?1 AND kind = 'filter' AND filter_watermark = ?2);",
        params![agent, watermark],
        |row| row.get(0),
    )?)
}

fn save_watermark(
    tx: &Transaction<'_>,
    agent: &str,
    watermark: i64,
    at: &str,
) -> Result<(), EventError> {
    tx.execute(
        "INSERT INTO conversation_item (agent_session, kind, origin, role, tool_call_id, filter_watermark, created_at) VALUES (?1, 'filter', 'system', 'system', '', ?2, ?3);",
        params![agent, watermark, at],
    )?;
    Ok(())
}

impl ConversationRepository {
    /// Reads the durable filter watermark; zero before the first commit.
    pub async fn filter_watermark(&self, agent: String) -> Result<i64, EventError> {
        self.events
            .transaction(move |tx| read_watermark(tx, &agent))
            .await
    }

    /// Commits one filter batch inside a single transaction: it rewrites the
    /// replay payloads of the superseded rows and advances the filter watermark.
    /// Message parts, tool calls, and settlement metadata are preserved from the
    /// durable rows because the filter only rewrites replay payloads. Every
    /// batch advances the watermark, including a skipped batch, whose
    /// replacement messages resubmit their original replay payloads. Returns
    /// false when this watermark is already committed.
    pub async fn commit_filter_amendment(
        &self,
        agent: String,
        published: Event,
        amendment: FilterAmendment,
    ) -> Result<bool, EventError> {
        self.validate(&agent)?;
        let committed = self
            .events
            .transaction(move |tx| {
                if amendment.watermark < read_watermark(tx, &agent)? {
                    return Err(EventError::Argument(
                        "filter watermark must not move backwards".into(),
                    ));
                }
                if amendment.watermark < amendment.end_sequence {
                    return Err(EventError::Argument(
                        "filter watermark must cover the amended batch".into(),
                    ));
                }
                if already_committed(tx, &agent, amendment.watermark)? {
                    return Ok(false);
                }
                let mut read = tx.prepare(
                    "SELECT sequence FROM conversation_item WHERE agent_session = ?1 AND kind NOT IN ('compaction', 'filter') AND sequence BETWEEN ?2 AND ?3 ORDER BY sequence;",
                )?;
                let rows = read
                    .query_map(
                        params![agent, amendment.start_sequence, amendment.end_sequence],
                        |row| row.get(0),
                    )?
                    .collect::<Result<Vec<i64>, _>>()?;
                if rows.len() != amendment.messages.len() {
                    return Err(EventError::Argument(format!(
                        "filter amendment carries {} replacement messages for {} superseded rows",
                        amendment.messages.len(),
                        rows.len()
                    )));
                }
                for (sequence, message) in rows.into_iter().zip(&amendment.messages) {
                    let provider_id = if message.provider_content.is_empty() {
                        ""
                    } else {
                        message.provider_id.as_str()
                    };
                    tx.execute(
                        "UPDATE conversation_item SET provider_id = ?1, provider_content = ?2 WHERE sequence = ?3;",
                        params![provider_id, message.provider_content, sequence],
                    )?;
                }
                save_watermark(tx, &agent, amendment.watermark, &timestamp())?;
                record(tx, &published, &timestamp())?;
                Ok(true)
            })
            .await?;
        Ok(committed)
    }
}
